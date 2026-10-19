//! Two-list CLOCK replacement: which cached page reclaim takes next.
//!
//! Pages (any key `K`, such as an object and page number) sit on one of two lists:
//!
//! - *inactive*: pages referenced once since they came in or were demoted, where eviction
//!   happens;
//! - *active*: pages referenced again while inactive, which are protected from eviction until
//!   they go a full turn of the hand without a reference.
//!
//! [`Clock::insert`] puts a new page at the inactive tail. [`Clock::touch`] sets a page's
//! reference bit; the kernel calls it when a read hits the page, and when the accessed bit of a
//! mapping that shows it was found set. [`Clock::evict`] moves the hand: an inactive page with
//! its reference bit set is promoted to active (bit cleared); one without is taken if the
//! caller says it can be (it is clean and idle, [`crate::page::PageState::evictable`]) and
//! otherwise goes round again. When the active list is longer than the inactive one, its head is
//! looked at too: demoted to inactive if unreferenced, given another turn if referenced. This
//! keeps a scan of pages read once (a large file streamed through) from pushing out the pages
//! that are used again and again, which is the point of 2Q (Johnson and Shasha, VLDB 1994) and
//! CLOCK-Pro (Jiang, Chen and Zhang, USENIX ATC 2005); this is the simpler two-list form of
//! both, without their history of evicted pages.
//!
//! Every operation is O(log n) amortized: the lists hold `(key, generation)`, and a page
//! removed while it is not at the hand leaves a stale entry that the hand skips. So that pages
//! inserted and discarded without eviction do not grow the lists without bound, [`Clock::remove`]
//! drops the stale entries once they outnumber the pages (plus a small slack), which keeps the
//! lists below twice the page count and costs O(1) amortized per removal.

use alloc::collections::{BTreeMap, VecDeque};
use core::fmt;

/// Which list a page is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    /// Referenced at most once since it came in or was demoted.
    Inactive,
    /// Referenced again while inactive.
    Active,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    list: List,
    referenced: bool,
    /// Matches the list entry that is current for this page; older ones are stale.
    generation: u64,
}

/// The reclaim lists.
#[derive(Debug)]
pub struct Clock<K> {
    pages: BTreeMap<K, Entry>,
    inactive: VecDeque<(K, u64)>,
    active: VecDeque<(K, u64)>,
    next_generation: u64,
    inactive_len: usize,
}

impl<K: Ord + Copy> Default for Clock<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Copy> Clock<K> {
    /// Empty lists.
    pub const fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            inactive: VecDeque::new(),
            active: VecDeque::new(),
            next_generation: 0,
            inactive_len: 0,
        }
    }

    /// The number of pages on both lists.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// Whether there are no pages.
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// The list `key` is on, if any.
    pub fn list_of(&self, key: K) -> Option<List> {
        self.pages.get(&key).map(|entry| entry.list)
    }

    /// The number of pages on the active list.
    pub fn active_len(&self) -> usize {
        self.pages.len() - self.inactive_len
    }

    /// Adds a page that just came into the cache, at the inactive tail, unreferenced.
    ///
    /// # Errors
    ///
    /// [`AlreadyPresent`] when the page is on a list already.
    pub fn insert(&mut self, key: K) -> Result<(), AlreadyPresent> {
        if self.pages.contains_key(&key) {
            return Err(AlreadyPresent);
        }
        let generation = self.generation();
        self.pages.insert(
            key,
            Entry {
                list: List::Inactive,
                referenced: false,
                generation,
            },
        );
        self.inactive.push_back((key, generation));
        self.inactive_len += 1;
        Ok(())
    }

    /// Sets the reference bit of a page; returns whether it is on a list.
    pub fn touch(&mut self, key: K) -> bool {
        match self.pages.get_mut(&key) {
            Some(entry) => {
                entry.referenced = true;
                true
            }
            None => false,
        }
    }

    /// Takes a page off the lists (it was discarded, or evicted some other way); returns
    /// whether it was on one.
    pub fn remove(&mut self, key: K) -> bool {
        match self.pages.remove(&key) {
            Some(entry) => {
                if entry.list == List::Inactive {
                    self.inactive_len -= 1;
                }
                self.compact_if_stale();
                true
            }
            None => false,
        }
    }

    /// The list entries kept beyond the pages before [`Self::remove`] compacts.
    const STALE_SLACK: usize = 16;

    /// Drops the stale list entries once there are more of them than pages (plus
    /// [`Self::STALE_SLACK`]). Each compaction is O(n) and follows at least n removals since the
    /// last one, so it is O(1) amortized.
    fn compact_if_stale(&mut self) {
        let entries = self.inactive.len() + self.active.len();
        if entries <= 2 * self.pages.len() + Self::STALE_SLACK {
            return;
        }
        let pages = &self.pages;
        let current = |&(key, generation): &(K, u64)| {
            pages.get(&key).is_some_and(|e| e.generation == generation)
        };
        self.inactive.retain(current);
        self.active.retain(current);
    }

    /// Moves the hand until it finds a page to evict, removes it from the lists and returns
    /// it. `can_evict` says whether a page may go now (it is clean and not being filled or
    /// written back); a page it refuses stays, and goes round again.
    ///
    /// Returns `None` when there is no page, or when no page could go: every inactive page was
    /// refused, and the active pages were aged onto the inactive list and refused too.
    pub fn evict(&mut self, mut can_evict: impl FnMut(K) -> bool) -> Option<K> {
        // Without a victim, the hand sweeps the inactive list (promoting referenced pages,
        // refusing the rest), then the whole active list twice (clearing reference bits, then
        // demoting), refusing on the inactive list in between and after. Each list entry,
        // current or stale, is popped a bounded number of times, so this budget always lets a
        // victim be found when there is one and stops the hand when there is none.
        let mut budget = 6 * (self.inactive.len() + self.active.len()) + 6;
        let mut refused = 0;
        while budget > 0 && !self.pages.is_empty() {
            if refused >= self.inactive_len {
                // Every inactive page was refused (or there are none): age the whole active
                // list, so its pages get their turn.
                if self.active_len() == 0 {
                    return None;
                }
                for _ in 0..self.active_len() {
                    budget = budget.saturating_sub(self.age_active());
                }
                refused = 0;
                continue;
            }
            if self.active_len() > self.inactive_len {
                budget = budget.saturating_sub(self.age_active());
                continue;
            }
            budget -= 1;
            let Some((key, generation)) = self.inactive.pop_front() else {
                // Unreachable while `inactive_len` counts the pages on the list; stop rather
                // than spin if it does not.
                return None;
            };
            let Some(entry) = self.current(key, generation) else {
                continue;
            };
            if entry.referenced {
                self.move_to(key, List::Active);
            } else if can_evict(key) {
                self.remove(key);
                return Some(key);
            } else {
                // Refused (dirty, or busy): back to the tail with a fresh generation.
                self.move_to(key, List::Inactive);
                refused += 1;
            }
        }
        None
    }

    /// Looks at the head of the active list: demotes it if unreferenced, gives it another
    /// turn (bit cleared) if referenced. Returns the number of list entries popped, stale ones
    /// included.
    fn age_active(&mut self) -> usize {
        let mut popped = 0;
        while let Some((key, generation)) = self.active.pop_front() {
            popped += 1;
            let Some(entry) = self.current(key, generation) else {
                continue;
            };
            if entry.referenced {
                self.move_to(key, List::Active);
            } else {
                self.move_to(key, List::Inactive);
            }
            break;
        }
        popped
    }

    /// The page's entry, if `generation` is its current list entry.
    fn current(&self, key: K, generation: u64) -> Option<Entry> {
        self.pages
            .get(&key)
            .copied()
            .filter(|entry| entry.generation == generation)
    }

    /// Puts a page at the tail of `list` with its reference bit cleared.
    fn move_to(&mut self, key: K, list: List) {
        let generation = self.generation();
        let Some(entry) = self.pages.get_mut(&key) else {
            return;
        };
        match (entry.list, list) {
            (List::Inactive, List::Active) => self.inactive_len -= 1,
            (List::Active, List::Inactive) => self.inactive_len += 1,
            _ => {}
        }
        *entry = Entry {
            list,
            referenced: false,
            generation,
        };
        match list {
            List::Inactive => self.inactive.push_back((key, generation)),
            List::Active => self.active.push_back((key, generation)),
        }
    }

    fn generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        generation
    }
}

/// [`Clock::insert`] of a page that is already on a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlreadyPresent;

impl fmt::Display for AlreadyPresent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the page is already on the reclaim lists")
    }
}

impl core::error::Error for AlreadyPresent {}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::collections::BTreeSet;
    use std::vec::Vec;

    fn clock(pages: impl IntoIterator<Item = u32>) -> Clock<u32> {
        let mut clock = Clock::new();
        for page in pages {
            clock.insert(page).expect("a new page");
        }
        clock
    }

    #[test]
    fn discarded_pages_do_not_grow_the_lists() {
        // Pages come in and are discarded with no eviction, on both lists.
        let mut lists = clock(0..8);
        for page in 0..4 {
            lists.touch(page);
        }
        // The hand promotes 0..4 on its way to 4, which goes.
        assert_eq!(lists.evict(|_| true), Some(4));
        assert_eq!(lists.active_len(), 4);
        for round in 0..10_000u32 {
            let page = 100 + round;
            lists.insert(page).expect("a new page");
            assert!(lists.remove(page));
            let active = round % 4;
            assert!(lists.remove(active));
            lists.insert(active).expect("back again");
            lists.touch(active);
            let entries = lists.inactive.len() + lists.active.len();
            assert!(
                entries <= 2 * lists.len() + Clock::<u32>::STALE_SLACK,
                "{entries} entries for {} pages",
                lists.len()
            );
        }
        // The generation check still holds after compactions: every page comes out once.
        let mut out: Vec<u32> = core::iter::from_fn(|| lists.evict(|_| true)).collect();
        out.sort_unstable();
        assert_eq!(out, [0, 1, 2, 3, 5, 6, 7]);
    }

    #[test]
    fn unreferenced_pages_go_in_order() {
        let mut lists = clock(0..4);
        let order: Vec<u32> = core::iter::from_fn(|| lists.evict(|_| true)).collect();
        assert_eq!(order, [0, 1, 2, 3]);
        assert!(lists.is_empty());
        assert_eq!(lists.evict(|_| true), None);
    }

    #[test]
    fn a_referenced_page_is_promoted_and_survives() {
        let mut lists = clock(0..4);
        assert!(lists.touch(0));
        assert_eq!(lists.evict(|_| true), Some(1));
        assert_eq!(lists.list_of(0), Some(List::Active));
        assert_eq!(lists.evict(|_| true), Some(2));
    }

    #[test]
    fn refused_pages_stay_and_are_not_spun_on_forever() {
        let mut lists = clock(0..3);
        // Page 1 is dirty: the hand passes it.
        assert_eq!(lists.evict(|page| page != 0), Some(1));
        assert_eq!(lists.list_of(0), Some(List::Inactive));
        // Nothing can go: the hand gives up.
        assert_eq!(lists.evict(|_| false), None);
        assert_eq!(lists.len(), 2);
    }

    #[test]
    fn active_pages_go_when_every_inactive_page_is_refused() {
        // Page 0 is pinned on the inactive list; page 1 is active. The hand must age page 1
        // down and take it rather than keep refusing page 0.
        let mut lists = clock(0..2);
        lists.touch(0);
        lists.touch(1);
        assert_eq!(lists.evict(|_| false), None);
        assert_eq!(lists.active_len(), 0);
        lists.touch(1);
        assert_eq!(lists.evict(|page| page == 0), Some(0));
        let mut lists = clock(0..2);
        lists.touch(0);
        lists.touch(1);
        assert_eq!(lists.evict(|page| page != 0), Some(1));
        assert_eq!(lists.list_of(0), Some(List::Inactive));
    }

    #[test]
    fn a_scan_does_not_push_out_the_hot_set() {
        // Pages 0..8 are used again and again; 100..400 are read once (a large file streamed
        // through). Evicting to keep 16 pages must keep the hot set.
        let mut lists = Clock::new();
        let hot: Vec<u32> = (0..8).collect();
        for &page in &hot {
            lists.insert(page).expect("a new page");
            lists.touch(page);
        }
        for page in 100..400 {
            lists.insert(page).expect("a new page");
            for &h in &hot {
                lists.touch(h);
            }
            while lists.len() > 16 {
                lists.evict(|_| true).expect("something to evict");
            }
        }
        for page in hot {
            assert_eq!(lists.list_of(page), Some(List::Active), "hot page {page}");
        }
    }

    #[test]
    fn removed_and_duplicate_pages() {
        let mut lists = clock(0..3);
        assert_eq!(lists.insert(1), Err(AlreadyPresent));
        assert!(lists.remove(1));
        assert!(!lists.remove(1));
        assert!(!lists.touch(1));
        let order: Vec<u32> = core::iter::from_fn(|| lists.evict(|_| true)).collect();
        assert_eq!(order, [0, 2]);
    }

    /// A reference model: the set of pages present. Whatever sequence of operations runs, the
    /// lists hold exactly the pages inserted and not removed or evicted, an evicted page was
    /// one `can_evict` allowed, and eviction finds a page whenever one is allowed.
    #[test]
    fn matches_a_set_model() {
        let mut lists = Clock::new();
        let mut model = BTreeSet::new();
        // A small linear congruential generator, so the test is deterministic.
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as u32
        };
        for _ in 0..20_000 {
            let page = next() % 64;
            match next() % 4 {
                0 => assert_eq!(lists.insert(page).is_ok(), model.insert(page)),
                1 => assert_eq!(lists.touch(page), model.contains(&page)),
                2 => assert_eq!(lists.remove(page), model.remove(&page)),
                _ => {
                    let pinned = next() % 64;
                    let allowed = |p: u32| p != pinned;
                    let any_allowed = model.iter().any(|&p| allowed(p));
                    match lists.evict(allowed) {
                        Some(victim) => {
                            assert!(allowed(victim));
                            assert!(model.remove(&victim));
                        }
                        None => assert!(!any_allowed, "no victim among {model:?}"),
                    }
                }
            }
            assert_eq!(lists.len(), model.len());
            assert!(lists.active_len() <= lists.len());
        }
    }
}
