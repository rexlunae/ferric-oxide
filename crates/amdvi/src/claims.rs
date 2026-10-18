//! Which DMA domain holds each device table entry, and which functions use it.
//!
//! A function's DMA reaches the IOMMU with its own device ID or with the alias its IVHD
//! names (section 2.2.1), so attaching a function to a domain has to set both entries. An
//! alias can be shared: several functions behind one PCI Express to PCI bridge all arrive
//! with the bridge's alias. That makes the entry a resource of every function that uses it,
//! not of the one that wrote it last:
//!
//! - an entry already held by a different domain cannot be taken over, or a function would
//!   move another driver's DMA into its own domain ([`Conflict`]);
//! - attaching to an entry the same domain already holds, with the same root, changes
//!   nothing, so pcid can attach the alias itself after the function;
//! - detaching a function gives up its use of each entry, and only an entry nobody uses any
//!   more goes back to being blocked.
//!
//! [`Claims`] keeps that bookkeeping for one IOMMU; the kernel writes the entries it names.

use alloc::{collections::BTreeMap, vec::Vec};
use core::fmt;

use crate::DeviceId;

/// The DMA domain an entry translates through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The domain ID.
    pub domain: u16,
    /// The physical address of the domain's root table.
    pub root: u64,
}

/// An attach refused because an entry it needs belongs to another domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The entry.
    pub id: DeviceId,
    /// The domain holding it.
    pub holder: Holder,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "device ID {:#06x} is held by domain {}",
            self.id.0, self.holder.domain
        )
    }
}

impl core::error::Error for Conflict {}

/// One held entry.
#[derive(Clone, Debug)]
struct Claim {
    holder: Holder,
    /// The functions whose attach needs the entry.
    users: Vec<DeviceId>,
}

/// The entries of one IOMMU's device table that DMA domains hold.
#[derive(Clone, Debug, Default)]
pub struct Claims {
    entries: BTreeMap<DeviceId, Claim>,
}

impl Claims {
    /// No entry held.
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// The domain holding entry `id`, if any.
    pub fn holder(&self, id: DeviceId) -> Option<Holder> {
        self.entries.get(&id).map(|claim| claim.holder)
    }

    /// Whether any of `ids` is held by a domain.
    pub fn any_held(&self, ids: &[DeviceId]) -> bool {
        ids.iter().any(|id| self.entries.contains_key(id))
    }

    /// The entries of `ids` that attaching `function` to `holder` has to write: those no
    /// domain holds yet. Entries `holder` already holds stay as they are.
    ///
    /// # Errors
    ///
    /// [`Conflict`] when a different domain, or the same domain ID with another root, holds
    /// one of `ids`.
    pub fn check_attach(
        &self,
        ids: &[DeviceId],
        holder: Holder,
    ) -> Result<Vec<DeviceId>, Conflict> {
        let mut writes = Vec::new();
        for &id in ids {
            match self.entries.get(&id) {
                Some(claim) if claim.holder == holder => {}
                Some(claim) => {
                    return Err(Conflict {
                        id,
                        holder: claim.holder,
                    });
                }
                None if !writes.contains(&id) => writes.push(id),
                None => {}
            }
        }
        Ok(writes)
    }

    /// Records that `function` uses `ids` through `holder`, after
    /// [`check_attach`](Self::check_attach) accepted it and the entries were written.
    pub fn record_attach(&mut self, function: DeviceId, ids: &[DeviceId], holder: Holder) {
        for &id in ids {
            let claim = self.entries.entry(id).or_insert_with(|| Claim {
                holder,
                users: Vec::new(),
            });
            if !claim.users.contains(&function) {
                claim.users.push(function);
            }
        }
    }

    /// The entries of `ids` that only `function` uses, which detaching it has to block.
    pub fn check_release(&self, function: DeviceId, ids: &[DeviceId]) -> Vec<DeviceId> {
        let mut last = Vec::new();
        for &id in ids {
            let only = self
                .entries
                .get(&id)
                .is_some_and(|claim| claim.users.iter().all(|&user| user == function));
            if only && !last.contains(&id) {
                last.push(id);
            }
        }
        last
    }

    /// Records that `function` no longer uses `ids`, after the entries
    /// [`check_release`](Self::check_release) named were blocked. An entry nobody uses is
    /// no longer held.
    pub fn record_release(&mut self, function: DeviceId, ids: &[DeviceId]) {
        for id in ids {
            if let Some(claim) = self.entries.get_mut(id) {
                claim.users.retain(|&user| user != function);
                if claim.users.is_empty() {
                    self.entries.remove(id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    const D1: Holder = Holder {
        domain: 3,
        root: 0x10_0000,
    };
    const D2: Holder = Holder {
        domain: 4,
        root: 0x20_0000,
    };

    fn id(bus: u8, device: u8, function: u8) -> DeviceId {
        DeviceId::new(bus, device, function)
    }

    /// Attaches like the kernel does: check, write, record.
    fn attach(
        claims: &mut Claims,
        function: DeviceId,
        ids: &[DeviceId],
        holder: Holder,
    ) -> Result<Vec<DeviceId>, Conflict> {
        let writes = claims.check_attach(ids, holder)?;
        claims.record_attach(function, ids, holder);
        Ok(writes)
    }

    /// Detaches like the kernel does, returning the entries to block.
    fn detach(claims: &mut Claims, function: DeviceId, ids: &[DeviceId]) -> Vec<DeviceId> {
        let last = claims.check_release(function, ids);
        claims.record_release(function, ids);
        last
    }

    #[test]
    fn unaliased_function_attaches_and_detaches() {
        let mut claims = Claims::new();
        let f = id(0, 4, 0);
        assert_eq!(attach(&mut claims, f, &[f], D1), Ok(vec![f]));
        assert_eq!(claims.holder(f), Some(D1));
        assert_eq!(detach(&mut claims, f, &[f]), [f]);
        assert_eq!(claims.holder(f), None);
    }

    /// pcid attaches the function, then its alias, to the same domain: the second attach
    /// finds the alias already held by that domain and writes nothing.
    #[test]
    fn alias_attached_by_pcid_after_the_function_is_idempotent() {
        let mut claims = Claims::new();
        let (f, alias) = (id(3, 1, 0), id(2, 0, 0));
        assert_eq!(attach(&mut claims, f, &[alias, f], D1), Ok(vec![alias, f]));
        assert_eq!(attach(&mut claims, alias, &[alias], D1), Ok(vec![]));
        // Detaching the alias leaves the entry to the function that still needs it.
        assert_eq!(detach(&mut claims, alias, &[alias]), []);
        assert_eq!(claims.holder(alias), Some(D1));
        assert_eq!(detach(&mut claims, f, &[alias, f]), [alias, f]);
        assert!(!claims.any_held(&[alias, f]));
    }

    #[test]
    fn same_domain_id_with_another_root_conflicts() {
        let mut claims = Claims::new();
        let f = id(0, 4, 0);
        attach(&mut claims, f, &[f], D1).unwrap();
        let other_root = Holder {
            root: 0x30_0000,
            ..D1
        };
        assert_eq!(
            claims.check_attach(&[f], other_root),
            Err(Conflict { id: f, holder: D1 })
        );
    }

    /// Two functions behind one bridge share its alias: the second cannot pull the alias
    /// (and so the first function's DMA) into another domain.
    #[test]
    fn shared_alias_held_by_another_domain_is_refused() {
        let mut claims = Claims::new();
        let (f1, f2, alias) = (id(3, 1, 0), id(3, 2, 0), id(2, 0, 0));
        attach(&mut claims, f1, &[alias, f1], D1).unwrap();
        assert_eq!(
            attach(&mut claims, f2, &[alias, f2], D2),
            Err(Conflict {
                id: alias,
                holder: D1
            })
        );
        // The refused attach changed nothing.
        assert_eq!(claims.holder(alias), Some(D1));
        assert_eq!(claims.holder(f2), None);
    }

    /// Two functions in one domain share an alias: detaching one keeps the alias for the
    /// other, so neither its DMA nor the domain boundary changes.
    #[test]
    fn detach_keeps_a_shared_alias_until_its_last_user_leaves() {
        let mut claims = Claims::new();
        let (f1, f2, alias) = (id(3, 1, 0), id(3, 2, 0), id(2, 0, 0));
        attach(&mut claims, f1, &[alias, f1], D1).unwrap();
        assert_eq!(attach(&mut claims, f2, &[alias, f2], D1), Ok(vec![f2]));
        assert_eq!(detach(&mut claims, f1, &[alias, f1]), [f1]);
        assert_eq!(claims.holder(alias), Some(D1));
        // While f2 holds the alias, another domain still cannot take it.
        assert!(claims.check_attach(&[alias], D2).is_err());
        assert_eq!(detach(&mut claims, f2, &[alias, f2]), [alias, f2]);
        assert_eq!(claims.check_attach(&[alias], D2), Ok(vec![alias]));
    }

    /// A join moves a domain's function and the alias pcid attached for it into another
    /// domain. One at a time it cannot work: after the function leaves, its alias still holds
    /// the entry for the old domain. As a set, everything leaves first, then attaches.
    #[test]
    fn join_moves_a_function_and_its_alias_as_a_set() {
        let mut claims = Claims::new();
        let (f, alias) = (id(3, 1, 0), id(2, 0, 0));
        // pcid's create_domain: the function, then the alias, into D2.
        attach(&mut claims, f, &[alias, f], D2).unwrap();
        attach(&mut claims, alias, &[alias], D2).unwrap();

        // One device at a time: the function's attach to D1 meets the alias D2 still holds.
        let mut one_by_one = claims.clone();
        detach(&mut one_by_one, f, &[alias, f]);
        assert_eq!(
            one_by_one.check_attach(&[alias, f], D1),
            Err(Conflict {
                id: alias,
                holder: D2
            })
        );

        // As a set: both leave D2 (the last one out blocks the alias), then both attach.
        assert_eq!(detach(&mut claims, f, &[alias, f]), [f]);
        assert_eq!(detach(&mut claims, alias, &[alias]), [alias]);
        assert_eq!(attach(&mut claims, f, &[alias, f], D1), Ok(vec![alias, f]));
        assert_eq!(attach(&mut claims, alias, &[alias], D1), Ok(vec![]));
        assert_eq!(claims.holder(alias), Some(D1));
        assert_eq!(claims.holder(f), Some(D1));
    }

    #[test]
    fn detaching_an_unattached_function_changes_nothing() {
        let mut claims = Claims::new();
        let (f1, f2, alias) = (id(3, 1, 0), id(3, 2, 0), id(2, 0, 0));
        attach(&mut claims, f1, &[alias, f1], D1).unwrap();
        assert_eq!(detach(&mut claims, f2, &[alias, f2]), []);
        assert_eq!(claims.holder(alias), Some(D1));
        assert_eq!(claims.holder(f1), Some(D1));
    }
}
