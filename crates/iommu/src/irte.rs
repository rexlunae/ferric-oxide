//! pcid's record of the interrupt remapping entries it has made for each driver channel.
//!
//! An entry ([`crate::abi::Request::AllocIrte`]) is made for a route: a function's interrupt
//! with one vector on one logical CPU. A channel's entries are freed when the channel closes
//! ([`IrteBook::close`]). A driver that asks again for a route it already has, for example
//! after giving a vector back and reserving the same one again, gets the entry it already
//! holds, so retries do not use up the remapping table: a channel holds at most one entry per
//! (CPU, vector).

use alloc::collections::BTreeMap;
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
    channels: BTreeMap<usize, BTreeMap<Route, (u32, M)>>,
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
    /// # Errors
    ///
    /// The error of `alloc`; nothing is recorded then.
    pub fn get_or_alloc<E>(
        &mut self,
        channel: usize,
        route: Route,
        alloc: impl FnOnce() -> Result<(u32, M), E>,
    ) -> Result<(u32, M), E> {
        let routes = self.channels.entry(channel).or_default();
        if let Some(&found) = routes.get(&route) {
            return Ok(found);
        }
        let made = alloc()?;
        routes.insert(route, made);
        Ok(made)
    }

    /// Forgets `channel` and returns the IDs of its entries, for freeing.
    pub fn close(&mut self, channel: usize) -> Vec<u32> {
        self.channels
            .remove(&channel)
            .map(|routes| routes.into_values().map(|(id, _)| id).collect())
            .unwrap_or_default()
    }

    /// How many entries `channel` holds.
    #[must_use]
    pub fn len(&self, channel: usize) -> usize {
        self.channels.get(&channel).map_or(0, BTreeMap::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(cpu: u32, vector: u8) -> Route {
        Route { cpu, vector }
    }

    #[test]
    fn asking_again_for_a_route_reuses_its_entry() {
        let mut book = IrteBook::new();
        let mut next = 0x10_u32;
        let mut alloc = || -> Result<(u32, u64), ()> {
            next += 1;
            Ok((next, u64::from(next) << 4))
        };
        let first = book.get_or_alloc(1, route(1, 0x52), &mut alloc);
        // A failed batch gave the vector back and a retry reserved it again.
        let again = book.get_or_alloc(1, route(1, 0x52), &mut alloc);
        assert_eq!(first, again);
        assert_eq!(book.len(1), 1);
        // Another CPU, vector or channel is another entry.
        assert_ne!(book.get_or_alloc(1, route(2, 0x52), &mut alloc), first);
        assert_ne!(book.get_or_alloc(1, route(1, 0x53), &mut alloc), first);
        assert_ne!(book.get_or_alloc(2, route(1, 0x52), &mut alloc), first);
        assert_eq!(book.len(1), 3);
    }

    #[test]
    fn failed_allocations_record_nothing() {
        let mut book: IrteBook<u64> = IrteBook::new();
        assert_eq!(
            book.get_or_alloc(1, route(0, 0x40), || Err("full")),
            Err("full")
        );
        assert_eq!(book.len(1), 0);
        assert_eq!(book.close(1), Vec::<u32>::new());
    }

    #[test]
    fn closing_returns_every_entry_once() {
        let mut book = IrteBook::new();
        for (id, vector) in [(7_u32, 0x50_u8), (8, 0x51)] {
            let _ = book.get_or_alloc(3, route(0, vector), || Ok::<_, ()>((id, ())));
        }
        let mut ids = book.close(3);
        ids.sort_unstable();
        assert_eq!(ids, [7, 8]);
        assert!(book.close(3).is_empty());
    }
}
