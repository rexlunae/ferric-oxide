//! Grant-link lifecycle bookkeeping for strict DMA domain closure.
//!
//! The kernel serializes this registry with its domain handle table, blocks devices, and
//! then reconciles returned links with their regions after releasing the handle lock.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// Grant links indexed by unique mapping ticket and associated with a domain handle.
///
/// A successful domain close returns its links for reconciliation with their grants after
/// releasing the domain lock. Failed closes retain a terminal marker even if a concurrent
/// revoker already took its link, so a missing handle cannot imply successful invalidation.
#[derive(Debug)]
pub struct DomainLinks<T> {
    links: BTreeMap<u64, (usize, T)>,
    failed: BTreeSet<usize>,
}

impl<T> Default for DomainLinks<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> DomainLinks<T> {
    /// An empty registry. Domain handles must remain unique while failure markers exist.
    pub const fn new() -> Self {
        Self {
            links: BTreeMap::new(),
            failed: BTreeSet::new(),
        }
    }

    /// Records `link` for a fresh mapping ticket, returning an older link if it was reused.
    pub fn insert(&mut self, ticket: u64, domain: usize, link: T) -> Option<T> {
        self.links
            .insert(ticket, (domain, link))
            .map(|(_, old)| old)
    }

    /// Takes the link for a mapping being unmapped or revoked.
    pub fn remove(&mut self, ticket: &u64) -> Option<T> {
        self.links.remove(ticket).map(|(_, link)| link)
    }

    /// Records the hardware result of closing `domain` and returns links safe to forget.
    ///
    /// Call while holding the domain table's lock, before publishing its missing handle.
    /// When `blocked` is false no links are returned; later revokes must keep failing.
    /// Reconcile returned links after releasing the domain table lock to preserve lock order.
    /// A true result must mean all of the domain's devices were successfully blocked.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_iommu::dma::links::DomainLinks;
    /// let mut links = DomainLinks::new();
    /// links.insert(1, 7, "grant mapping");
    /// assert_eq!(links.close(7, true), ["grant mapping"]);
    /// assert_eq!(links.remove(&1), None);
    /// ```
    pub fn close(&mut self, domain: usize, blocked: bool) -> Vec<T> {
        if !blocked {
            self.failed.insert(domain);
            return Vec::new();
        }
        self.failed.remove(&domain);
        self.links
            .extract_if(.., |_, (owner, _)| *owner == domain)
            .map(|(_, (_, link))| link)
            .collect()
    }

    /// Whether a missing domain handle represents a failed close rather than safe teardown.
    pub fn closed_failed(&self, domain: usize) -> bool {
        self.failed.contains(&domain)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn repeated_domain_close_does_not_accumulate_links() {
        let mut links = DomainLinks::new();
        for domain in 1..=100 {
            links.insert(domain as u64 * 2, domain, "grant");
            links.insert(domain as u64 * 2 + 1, domain, "sub-grant");
            assert_eq!(links.close(domain, true), ["grant", "sub-grant"]);
            assert!(links.links.is_empty());
            assert!(links.failed.is_empty());
        }
    }

    #[test]
    fn domain_close_only_returns_its_remaining_links() {
        let mut links = DomainLinks::new();
        links.insert(10, 1, "explicitly unmapped");
        links.insert(11, 1, "closed");
        links.insert(12, 2, "other domain");
        assert_eq!(links.remove(&10), Some("explicitly unmapped"));
        assert_eq!(links.close(1, true), ["closed"]);
        assert_eq!(links.remove(&12), Some("other domain"));
        assert!(links.close(1, true).is_empty());
    }

    #[test]
    fn failed_close_is_not_a_successful_missing_domain() {
        let mut links = DomainLinks::new();
        links.insert(1, 7, "mapping");
        assert!(links.close(7, false).is_empty());
        assert!(links.closed_failed(7));
        assert_eq!(links.remove(&1), Some("mapping"));
        // A revoker can already own the link when closure fails. The marker must survive.
        assert!(links.closed_failed(7));
        assert!(links.close(8, false).is_empty());
        assert!(links.closed_failed(8));
        // Only confirmed blocking permits recovery.
        links.close(7, true);
        assert!(!links.closed_failed(7));
        assert!(links.closed_failed(8));
    }

    #[test]
    fn concurrent_close_and_revoke_retire_each_link_once() {
        use std::sync::{Barrier, Mutex};
        use std::thread;

        let mut links = DomainLinks::new();
        links.insert(1, 7, "mapping");
        let links = Mutex::new(links);
        let start = Barrier::new(3);
        thread::scope(|scope| {
            let close = scope.spawn(|| {
                start.wait();
                links.lock().unwrap().close(7, true).len()
            });
            let revoke = scope.spawn(|| {
                start.wait();
                usize::from(links.lock().unwrap().remove(&1).is_some())
            });
            start.wait();
            assert_eq!(close.join().unwrap() + revoke.join().unwrap(), 1);
        });
        assert!(links.lock().unwrap().links.is_empty());
    }
}
