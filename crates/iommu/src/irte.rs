//! pcid's record of the interrupt remapping entries it has made for each driver channel.
//!
//! An entry ([`crate::abi::Request::AllocIrte`]) is made for a route: a function's interrupt
//! with one vector on one logical CPU. A channel's entries are freed when the channel closes
//! ([`IrteBook::close`]). A driver that asks again for a route it already has, for example
//! after giving a vector back and reserving the same one again, gets the entry it already
//! holds, so retries do not use up the remapping table: a channel holds at most one entry per
//! (CPU, vector). An entry that was steered to another (CPU, vector) since is recorded under
//! the route it has now, and not given out for its old one.

use alloc::collections::{BTreeMap, btree_map::Entry};
use alloc::vec::Vec;

/// A route: the logical CPU and the vector an interrupt is delivered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Route {
    /// The logical CPU.
    pub cpu: u32,
    /// The IDT vector.
    pub vector: u8,
}

/// The remapping entries of every channel, with the message (`M`) each one gives.
#[derive(Debug)]
pub struct IrteBook<M> {
    channels: BTreeMap<usize, Channel<M>>,
}

/// One channel's entries.
#[derive(Debug)]
struct Channel<M> {
    /// The entry for each route, as far as pcid knows.
    routes: BTreeMap<Route, (u32, M)>,
    /// Entries that were steered to a route another entry of the channel already had: still
    /// the channel's to free, but no longer given out.
    moved: Vec<u32>,
}

impl<M> Default for Channel<M> {
    fn default() -> Self {
        Self {
            routes: BTreeMap::new(),
            moved: Vec::new(),
        }
    }
}

impl<M> Default for IrteBook<M> {
    fn default() -> Self {
        Self {
            channels: BTreeMap::new(),
        }
    }
}

impl<M: Copy> IrteBook<M> {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The message for `route` on `channel`: the one recorded, or the entry `alloc` makes,
    /// which is then recorded. Returns the entry ID with the message.
    ///
    /// Ferric Oxide: an entry can be steered after it was made (the kernel rewrites it when the
    /// vector it delivers moves to another CPU). So before reusing a recorded entry, this asks
    /// `target` where it delivers now. One that moved is recorded under the route it has now
    /// (or kept only for freeing, when the channel has an entry for that route already), and a
    /// new entry is made for `route`.
    ///
    /// # Errors
    ///
    /// The error of `target` or `alloc`; nothing new is recorded then.
    pub fn get_or_alloc<E>(
        &mut self,
        channel: usize,
        route: Route,
        target: impl FnOnce(u32) -> Result<Route, E>,
        alloc: impl FnOnce() -> Result<(u32, M), E>,
    ) -> Result<(u32, M), E> {
        let book = self.channels.entry(channel).or_default();
        if let Some(&(id, message)) = book.routes.get(&route) {
            let now = target(id)?;
            if now == route {
                return Ok((id, message));
            }
            book.routes.remove(&route);
            match book.routes.entry(now) {
                Entry::Vacant(slot) => {
                    slot.insert((id, message));
                }
                Entry::Occupied(_) => book.moved.push(id),
            }
        }
        let made = alloc()?;
        book.routes.insert(route, made);
        Ok(made)
    }

    /// Forgets `channel` and returns the IDs of its entries, for freeing.
    pub fn close(&mut self, channel: usize) -> Vec<u32> {
        self.channels
            .remove(&channel)
            .map(|book| {
                let mut ids: Vec<u32> = book.routes.into_values().map(|(id, _)| id).collect();
                ids.extend(book.moved);
                ids
            })
            .unwrap_or_default()
    }

    /// How many entries `channel` holds.
    #[must_use]
    pub fn len(&self, channel: usize) -> usize {
        self.channels
            .get(&channel)
            .map_or(0, |book| book.routes.len() + book.moved.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(cpu: u32, vector: u8) -> Route {
        Route { cpu, vector }
    }

    /// A `target` that reports every entry still at `route(cpu, vector)`.
    fn unmoved<E>(cpu: u32, vector: u8) -> impl FnOnce(u32) -> Result<Route, E> {
        move |_| Ok(route(cpu, vector))
    }

    #[test]
    fn a_steered_entry_is_not_given_out_for_its_old_route() {
        let mut book = IrteBook::new();
        let mut next = 0_u32;
        let mut alloc = || -> Result<(u32, u32), ()> {
            next += 1;
            Ok((next, next))
        };
        let (moved, _) = book
            .get_or_alloc(1, route(1, 0x52), unmoved(1, 0x52), &mut alloc)
            .unwrap();
        // The vector was steered to CPU 2 vector 0x60: asking for the old route again makes a
        // new entry, and the steered one now answers for its new route.
        let at_new = |_| Ok::<_, ()>(route(2, 0x60));
        let (fresh, _) = book
            .get_or_alloc(1, route(1, 0x52), at_new, &mut alloc)
            .unwrap();
        assert_ne!(fresh, moved);
        let (again, _) = book
            .get_or_alloc(1, route(2, 0x60), unmoved(2, 0x60), &mut alloc)
            .unwrap();
        assert_eq!(again, moved);
        assert_eq!(book.len(1), 2);
        // Steered onto a route the channel already has an entry for: kept only for freeing.
        let onto_fresh = |_| Ok::<_, ()>(route(1, 0x52));
        let (third, _) = book
            .get_or_alloc(1, route(2, 0x60), onto_fresh, &mut alloc)
            .unwrap();
        assert_eq!(book.len(1), 3);
        let mut ids = book.close(1);
        ids.sort_unstable();
        assert_eq!(ids, [moved, fresh, third]);
    }

    #[test]
    fn a_failed_target_query_changes_nothing() {
        let mut book = IrteBook::new();
        let made = book.get_or_alloc(1, route(0, 0x40), unmoved::<&str>(0, 0x40), || Ok((5, ())));
        assert_eq!(made, Ok((5, ())));
        let failed = book.get_or_alloc(1, route(0, 0x40), |_| Err("gone"), || Ok((6, ())));
        assert_eq!(failed, Err("gone"));
        assert_eq!(book.close(1), [5]);
    }

    #[test]
    fn asking_again_for_a_route_reuses_its_entry() {
        let mut book = IrteBook::new();
        let mut next = 0x10_u32;
        let mut alloc = || -> Result<(u32, u64), ()> {
            next += 1;
            Ok((next, u64::from(next) << 4))
        };
        let first = book.get_or_alloc(1, route(1, 0x52), unmoved(1, 0x52), &mut alloc);
        // A failed batch gave the vector back and a retry reserved it again.
        let again = book.get_or_alloc(1, route(1, 0x52), unmoved(1, 0x52), &mut alloc);
        assert_eq!(first, again);
        assert_eq!(book.len(1), 1);
        // Another CPU, vector or channel is another entry.
        assert_ne!(
            book.get_or_alloc(1, route(2, 0x52), unmoved(2, 0x52), &mut alloc),
            first
        );
        assert_ne!(
            book.get_or_alloc(1, route(1, 0x53), unmoved(1, 0x53), &mut alloc),
            first
        );
        assert_ne!(
            book.get_or_alloc(2, route(1, 0x52), unmoved(1, 0x52), &mut alloc),
            first
        );
        assert_eq!(book.len(1), 3);
    }

    #[test]
    fn failed_allocations_record_nothing() {
        let mut book: IrteBook<u64> = IrteBook::new();
        assert_eq!(
            book.get_or_alloc(1, route(0, 0x40), unmoved(0, 0x40), || Err("full")),
            Err("full")
        );
        assert_eq!(book.len(1), 0);
        assert_eq!(book.close(1), Vec::<u32>::new());
    }

    #[test]
    fn closing_returns_every_entry_once() {
        let mut book = IrteBook::new();
        for (id, vector) in [(7_u32, 0x50_u8), (8, 0x51)] {
            let _ = book.get_or_alloc(3, route(0, vector), unmoved(0, vector), || {
                Ok::<_, ()>((id, ()))
            });
        }
        let mut ids = book.close(3);
        ids.sort_unstable();
        assert_eq!(ids, [7, 8]);
        assert!(book.close(3).is_empty());
    }
}
