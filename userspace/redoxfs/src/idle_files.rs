//! Ferric Oxide: the closed files whose page-cache objects RedoxFS keeps (#286).
//!
//! Without this, a file's cached pages went with its last description, so every run of a
//! program read the whole executable through RedoxFS again. [`IdleFiles`] holds the files
//! no description has open, least recently closed first, within a file count and byte
//! budget. Each entry carries a stamp of the node as it was at close; a file whose node no
//! longer matches (its ID was freed and reused, or it changed without going through the
//! cache) is not reused.

#![forbid(unsafe_code)]

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Whether a file's idle object can serve an attachment being attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleMatch {
    /// It was idle with the same stamp: its object can serve the new description.
    Reusable,
    /// It was idle with a different stamp: its object must be closed.
    Stale,
    /// It was not idle.
    Absent,
}

/// Closed files kept for reuse, by node ID, evicted oldest first.
#[derive(Debug)]
pub struct IdleFiles<S> {
    by_node: BTreeMap<u32, Entry<S>>,
    by_age: BTreeMap<u64, u32>,
    bytes: u64,
    next_age: u64,
    max_files: usize,
    max_bytes: u64,
}

#[derive(Debug)]
struct Entry<S> {
    age: u64,
    bytes: u64,
    stamp: S,
}

impl<S: PartialEq> IdleFiles<S> {
    /// An empty set that keeps at most `max_files` files of at most `max_bytes` in all.
    pub fn new(max_files: usize, max_bytes: u64) -> Self {
        Self {
            by_node: BTreeMap::new(),
            by_age: BTreeMap::new(),
            bytes: 0,
            next_age: 0,
            max_files,
            max_bytes,
        }
    }

    /// Keeps `node`, `bytes` long and stamped `stamp`, as the most recently closed file, and
    /// returns the files evicted to stay within budget, oldest first. A file larger than the
    /// whole byte budget is evicted at once, so the result can name `node` itself.
    pub fn insert(&mut self, node: u32, bytes: u64, stamp: S) -> Vec<u32> {
        self.remove(node);
        let mut evicted = Vec::new();
        // Make room before adding the weight, so even an extreme file size cannot
        // overflow the accounting and bypass the byte limit in a release build.
        while self.by_node.len() >= self.max_files
            || bytes > self.max_bytes.saturating_sub(self.bytes)
        {
            let Some((_, oldest)) = self.by_age.first_key_value() else {
                break;
            };
            let oldest = *oldest;
            self.remove(oldest);
            evicted.push(oldest);
        }
        if self.max_files == 0 || bytes > self.max_bytes {
            evicted.push(node);
            return evicted;
        }
        let age = self.next_age;
        self.next_age += 1;
        self.by_age.insert(age, node);
        self.by_node.insert(node, Entry { age, bytes, stamp });
        self.bytes += bytes;
        evicted
    }

    /// Compare stamps without removing the idle entry or changing eviction order.
    /// Remove a reusable entry only after the attachment succeeds; a failed attempt
    /// must leave its object within the idle budget. Discard a stale object explicitly.
    pub fn check(&self, node: u32, stamp: &S) -> IdleMatch {
        match self.by_node.get(&node) {
            Some(entry) if entry.stamp == *stamp => IdleMatch::Reusable,
            Some(_) => IdleMatch::Stale,
            None => IdleMatch::Absent,
        }
    }

    /// Takes `node` out of the set; whether it was there.
    pub fn remove(&mut self, node: u32) -> bool {
        self.remove_entry(node).is_some()
    }

    /// Whether `node` is in the set.
    pub fn contains(&self, node: u32) -> bool {
        self.by_node.contains_key(&node)
    }

    fn remove_entry(&mut self, node: u32) -> Option<Entry<S>> {
        let entry = self.by_node.remove(&node)?;
        self.by_age.remove(&entry.age);
        self.bytes -= entry.bytes;
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oldest_file_goes_first_when_the_count_is_full() {
        let mut idle = IdleFiles::new(2, u64::MAX);
        assert!(idle.insert(1, 10, ()).is_empty());
        assert!(idle.insert(2, 10, ()).is_empty());
        assert_eq!(idle.insert(3, 10, ()), [1]);
        assert!(!idle.contains(1));
        assert!(idle.contains(2) && idle.contains(3));
    }

    #[test]
    fn files_are_evicted_until_the_bytes_fit() {
        let mut idle = IdleFiles::new(10, 100);
        idle.insert(1, 40, ());
        idle.insert(2, 40, ());
        assert_eq!(idle.insert(3, 70, ()), [1, 2]);
        assert!(idle.contains(3));
    }

    #[test]
    fn a_file_larger_than_the_budget_is_not_kept() {
        let mut idle = IdleFiles::new(10, 100);
        idle.insert(1, 40, ());
        assert_eq!(idle.insert(2, 101, ()), [1, 2]);
        assert!(!idle.contains(2));
    }

    #[test]
    fn reclosing_a_file_makes_it_the_newest() {
        let mut idle = IdleFiles::new(2, u64::MAX);
        idle.insert(1, 10, ());
        idle.insert(2, 10, ());
        assert_eq!(idle.check(1, &()), IdleMatch::Reusable);
        assert!(idle.remove(1));
        idle.insert(1, 10, ());
        assert_eq!(idle.insert(3, 10, ()), [2]);
    }

    #[test]
    fn a_changed_stamp_is_stale_until_discarded() {
        let mut idle = IdleFiles::new(4, u64::MAX);
        idle.insert(7, 10, (1, 2));
        assert_eq!(idle.check(7, &(1, 3)), IdleMatch::Stale);
        assert!(idle.remove(7));
        assert_eq!(idle.check(7, &(1, 3)), IdleMatch::Absent);
    }

    #[test]
    fn removal_returns_the_bytes_to_the_budget() {
        let mut idle = IdleFiles::new(10, 100);
        idle.insert(1, 60, ());
        assert!(idle.remove(1));
        assert!(!idle.remove(1));
        assert!(idle.insert(2, 100, ()).is_empty());
    }

    #[test]
    fn an_extreme_file_size_cannot_wrap_the_byte_budget() {
        let mut idle = IdleFiles::new(64, 256 << 20);
        idle.insert(1, 4096, ());
        assert_eq!(idle.insert(2, u64::MAX, ()), [1, 2]);
        assert!(!idle.contains(1) && !idle.contains(2));
        assert!(idle.insert(3, 4096, ()).is_empty());
    }

    #[test]
    fn zero_limits_keep_no_files() {
        for (files, bytes) in [(0, 100), (1, 0), (0, 0)] {
            let mut idle = IdleFiles::new(files, bytes);
            assert_eq!(idle.insert(1, 1, ()), [1]);
            assert!(!idle.contains(1));
        }
    }

    #[test]
    fn an_unfinished_reopen_keeps_its_budget_and_eviction_order() {
        let mut idle = IdleFiles::new(2, 100);
        idle.insert(1, 40, 11);
        idle.insert(2, 40, 22);
        assert_eq!(idle.check(1, &11), IdleMatch::Reusable);
        // The attachment failed, so it never committed remove(1).
        assert!(idle.contains(1));
        assert_eq!(idle.insert(3, 60, 33), [1]);
        assert!(idle.contains(2) && idle.contains(3));
    }
}
