//! An object's reverse map: from a file page to the mappings that show it.
//!
//! Each cache object keeps one [`Rmap`]. Every shared `mmap` of the object adds a [`Mapping`]
//! (the address space, a page-aligned virtual address, the file pages from there on, and the
//! access), and `munmap` takes it away again, whole or in part ([`Rmap::unmap`] splits a mapping
//! when the middle of it goes). When truncation, reclaim or invalidation must take a page away
//! from every process, [`Rmap::overlapping`] gives each virtual address the page is mapped at,
//! so the kernel can clear those page table entries and shoot down the TLBs; reclaim also uses
//! it to read the accessed bits of a mapped page.
//!
//! The address space type `S` is whatever the kernel uses to name one weakly (an ID it can look
//! up and find gone); when an address space exits, [`Rmap::remove_space`] drops its mappings.
//!
//! Within one address space, mappings never overlap in virtual addresses ([`Rmap::map`]
//! refuses one that would), so a mapping is named by its space and start address. The same
//! file page may be mapped many times, by one space or several. Lookups are O(log n) plus the
//! mappings that start within the longest mapping's length before the asked-for pages.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::fmt;

use crate::abi::{PAGE_SIZE, PageRange};

/// What a mapping allows its address space to do with the pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Read (and execute) only.
    Read,
    /// Read and write: writes make the pages dirty.
    ReadWrite,
}

/// One mapping of an object's pages into an address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping<S> {
    /// The address space.
    pub space: S,
    /// The virtual address of the first page, a multiple of [`PAGE_SIZE`].
    pub addr: u64,
    /// The file pages shown, from `addr` on, one after another.
    pub pages: PageRange,
    /// What the mapping allows.
    pub access: Access,
}

impl<S> Mapping<S> {
    /// The virtual address `page` is shown at, if the mapping shows it.
    pub const fn addr_of(&self, page: u64) -> Option<u64> {
        if !self.pages.contains(page) {
            return None;
        }
        // `Rmap::map` checked that the mapping's whole virtual range fits in a u64.
        Some(self.addr + (page - self.pages.first()) * PAGE_SIZE)
    }

    /// The virtual address after the last page.
    pub const fn end_addr(&self) -> u64 {
        self.addr + self.pages.count() as u64 * PAGE_SIZE
    }
}

/// A virtual address range that cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddressError {
    /// The address is not a multiple of [`PAGE_SIZE`].
    Unaligned {
        /// The address.
        addr: u64,
    },
    /// The range would pass the end of the address space.
    Overflow,
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unaligned { addr } => write!(f, "address {addr:#x} is not page-aligned"),
            Self::Overflow => f.write_str("the range passes the end of the address space"),
        }
    }
}

impl core::error::Error for AddressError {}

/// Why [`Rmap::map`] refused a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MapError {
    /// The virtual address range cannot be used.
    Address(AddressError),
    /// The address space already maps this object somewhere in the range.
    Overlap,
}

impl From<AddressError> for MapError {
    fn from(error: AddressError) -> Self {
        Self::Address(error)
    }
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address(_) => f.write_str("the mapping's address range cannot be used"),
            Self::Overlap => f.write_str("the address space already maps the object in the range"),
        }
    }
}

impl core::error::Error for MapError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Address(error) => Some(error),
            Self::Overlap => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Extent {
    pages: PageRange,
    access: Access,
}

/// The mappings of one object.
#[derive(Debug)]
pub struct Rmap<S> {
    /// Every mapping, by space and start address.
    by_addr: BTreeMap<(S, u64), Extent>,
    /// The mappings starting at each file page.
    by_page: BTreeMap<u64, BTreeSet<(S, u64)>>,
    /// The most pages any mapping has had: how far before a page a mapping showing it can
    /// start. A high-water mark, so removing the longest mapping costs nothing.
    longest: u32,
}

impl<S: Ord + Copy> Default for Rmap<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Ord + Copy> Rmap<S> {
    /// No mappings.
    pub const fn new() -> Self {
        Self {
            by_addr: BTreeMap::new(),
            by_page: BTreeMap::new(),
            longest: 0,
        }
    }

    /// The number of mappings.
    pub fn len(&self) -> usize {
        self.by_addr.len()
    }

    /// Whether there are no mappings.
    pub fn is_empty(&self) -> bool {
        self.by_addr.is_empty()
    }

    /// Records that `space` maps `pages` at `addr` with `access`.
    ///
    /// # Errors
    ///
    /// [`MapError::Address`] when `addr` is not page-aligned or the range passes the end of the
    /// address space, and [`MapError::Overlap`] when `space` already maps the object somewhere
    /// in the range (the kernel unmaps the old mapping first, as `MAP_FIXED` does).
    pub fn map(
        &mut self,
        space: S,
        addr: u64,
        pages: PageRange,
        access: Access,
    ) -> Result<(), MapError> {
        let end = end_addr(addr, u64::from(pages.count()))?;
        let overlaps = self
            .by_addr
            .range((space, 0)..(space, end))
            .next_back()
            .is_some_and(|(&(_, start), extent)| start + span(extent.pages) > addr);
        if overlaps {
            return Err(MapError::Overlap);
        }
        self.insert(space, addr, Extent { pages, access });
        Ok(())
    }

    /// Removes `count` pages of `space`'s virtual addresses from `addr` on, as `munmap` does:
    /// mappings inside the range go, and mappings that reach into it are cut, in two if the
    /// range is in the middle. Returns the number of pages that were mapped there.
    ///
    /// # Errors
    ///
    /// [`AddressError`] when `addr` is not page-aligned or the range passes the end of the
    /// address space.
    pub fn unmap(&mut self, space: S, addr: u64, count: u64) -> Result<u64, AddressError> {
        let end = end_addr(addr, count)?;
        // Mappings in one space do not overlap, so ordered by start they are ordered by end
        // too: walking back from the last one starting before `end`, stop at the first that
        // ends at or before `addr`.
        let hit: Vec<(u64, Extent)> = self
            .by_addr
            .range((space, 0)..(space, end))
            .rev()
            .take_while(|&(&(_, start), extent)| start + span(extent.pages) > addr)
            .map(|(&(_, start), &extent)| (start, extent))
            .collect();
        let mut removed = 0;
        for (start, extent) in hit {
            self.remove(space, start, extent.pages.first());
            let map_end = start + span(extent.pages);
            let cut_start = start.max(addr);
            let cut_end = map_end.min(end);
            removed += (cut_end - cut_start) / PAGE_SIZE;
            let first = extent.pages.first();
            if let Some(left) =
                PageRange::from_bounds(first, first + (cut_start - start) / PAGE_SIZE)
            {
                self.insert(
                    space,
                    start,
                    Extent {
                        pages: left,
                        ..extent
                    },
                );
            }
            let right_first = first + (cut_end - start) / PAGE_SIZE;
            if let Some(right) = PageRange::from_bounds(right_first, extent.pages.end()) {
                self.insert(
                    space,
                    cut_end,
                    Extent {
                        pages: right,
                        ..extent
                    },
                );
            }
        }
        Ok(removed)
    }

    /// Drops every mapping of `space` (the address space exited); returns how many there were.
    pub fn remove_space(&mut self, space: S) -> usize {
        let gone: Vec<(u64, u64)> = self
            .by_addr
            .range((space, 0)..=(space, u64::MAX))
            .map(|(&(_, start), extent)| (start, extent.pages.first()))
            .collect();
        for &(start, first) in &gone {
            self.remove(space, start, first);
        }
        gone.len()
    }

    /// Every mapping that shows any of `pages`, cut down to just those pages: what the kernel
    /// unmaps when the pages go, or whose accessed bits it reads. Pass a one-page range for a
    /// single page.
    pub fn overlapping(&self, pages: PageRange) -> impl Iterator<Item = Mapping<S>> + '_ {
        let lowest = pages
            .first()
            .saturating_sub(u64::from(self.longest.saturating_sub(1)));
        self.by_page
            .range(lowest..pages.end())
            .flat_map(|(_, starts)| starts.iter())
            .filter_map(move |&(space, start)| {
                let extent = self.by_addr.get(&(space, start))?;
                let shown = extent.pages.intersect(pages)?;
                let addr = start + (shown.first() - extent.pages.first()) * PAGE_SIZE;
                Some(Mapping {
                    space,
                    addr,
                    pages: shown,
                    access: extent.access,
                })
            })
    }

    /// Every mapping of `space`, in address order.
    pub fn mappings_of(&self, space: S) -> impl Iterator<Item = Mapping<S>> + '_ {
        self.by_addr
            .range((space, 0)..=(space, u64::MAX))
            .map(|(&(space, addr), extent)| Mapping {
                space,
                addr,
                pages: extent.pages,
                access: extent.access,
            })
    }

    fn insert(&mut self, space: S, addr: u64, extent: Extent) {
        self.longest = self.longest.max(extent.pages.count());
        self.by_page
            .entry(extent.pages.first())
            .or_default()
            .insert((space, addr));
        self.by_addr.insert((space, addr), extent);
    }

    fn remove(&mut self, space: S, addr: u64, first: u64) {
        self.by_addr.remove(&(space, addr));
        if let Some(starts) = self.by_page.get_mut(&first) {
            starts.remove(&(space, addr));
            if starts.is_empty() {
                self.by_page.remove(&first);
            }
        }
    }
}

/// The bytes `pages` take in an address space. [`end_addr`] checked it fits when the mapping
/// was made, and a cut mapping only gets shorter.
fn span(pages: PageRange) -> u64 {
    u64::from(pages.count()) * PAGE_SIZE
}

/// The address after `count` pages from `addr`.
fn end_addr(addr: u64, count: u64) -> Result<u64, AddressError> {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(AddressError::Unaligned { addr });
    }
    count
        .checked_mul(PAGE_SIZE)
        .and_then(|bytes| addr.checked_add(bytes))
        .ok_or(AddressError::Overflow)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::collections::BTreeSet;
    use std::string::ToString;
    use std::vec::Vec;

    const P: u64 = PAGE_SIZE;

    fn range(first: u64, count: u32) -> PageRange {
        PageRange::new(first, count).expect("a valid range")
    }

    fn shown(rmap: &Rmap<u8>, page: u64) -> Vec<(u8, u64)> {
        rmap.overlapping(range(page, 1))
            .map(|mapping| (mapping.space, mapping.addr))
            .collect()
    }

    #[test]
    fn a_page_is_found_in_every_mapping() {
        let mut rmap = Rmap::new();
        rmap.map(1, 0x10_000, range(0, 8), Access::Read)
            .expect("map");
        rmap.map(2, 0x40_000, range(4, 4), Access::ReadWrite)
            .expect("map");
        rmap.map(1, 0x80_000, range(5, 1), Access::Read)
            .expect("map");
        assert_eq!(shown(&rmap, 0), [(1, 0x10_000)]);
        let mut five = shown(&rmap, 5);
        five.sort_unstable();
        assert_eq!(
            five,
            [(1, 0x10_000 + 5 * P), (1, 0x80_000), (2, 0x40_000 + P)]
        );
        assert_eq!(shown(&rmap, 8), []);
        let cut: Vec<Mapping<u8>> = rmap.overlapping(range(6, 10)).collect();
        assert_eq!(cut.len(), 2);
        assert!(cut.iter().all(|mapping| mapping.pages == range(6, 2)));
        let writable = cut
            .iter()
            .find(|mapping| mapping.space == 2)
            .expect("space 2");
        assert_eq!(writable.access, Access::ReadWrite);
        assert_eq!(writable.addr_of(7), Some(0x40_000 + 3 * P));
        assert_eq!(writable.addr_of(5), None);
    }

    #[test]
    fn unmapping_the_middle_splits_a_mapping() {
        let mut rmap = Rmap::new();
        rmap.map(1, 0x10_000, range(100, 10), Access::Read)
            .expect("map");
        assert_eq!(rmap.unmap(1, 0x10_000 + 3 * P, 4), Ok(4));
        let left: Vec<Mapping<u8>> = rmap.mappings_of(1).collect();
        assert_eq!(left.len(), 2);
        assert_eq!((left[0].addr, left[0].pages), (0x10_000, range(100, 3)));
        assert_eq!(
            (left[1].addr, left[1].pages),
            (0x10_000 + 7 * P, range(107, 3))
        );
        assert_eq!(shown(&rmap, 104), []);
        assert_eq!(shown(&rmap, 108), [(1, 0x10_000 + 8 * P)]);
        // An unmap across both pieces and the hole between them.
        assert_eq!(rmap.unmap(1, 0x10_000 + 2 * P, 6), Ok(2));
        assert_eq!(rmap.unmap(1, 0x90_000, 6), Ok(0));
        assert_eq!(rmap.len(), 2);
    }

    #[test]
    fn overlaps_in_one_space_are_refused_but_not_across_spaces() {
        let mut rmap = Rmap::new();
        rmap.map(1, 0x10_000, range(0, 4), Access::Read)
            .expect("map");
        assert_eq!(
            rmap.map(1, 0x10_000 + 3 * P, range(0, 1), Access::Read),
            Err(MapError::Overlap)
        );
        assert_eq!(
            rmap.map(1, 0x10_000 - P, range(0, 2), Access::Read),
            Err(MapError::Overlap)
        );
        rmap.map(1, 0x10_000 + 4 * P, range(0, 1), Access::Read)
            .expect("adjacent");
        rmap.map(2, 0x10_000, range(0, 4), Access::Read)
            .expect("another space");
        assert_eq!(rmap.remove_space(1), 2);
        assert_eq!(rmap.len(), 1);
        assert_eq!(shown(&rmap, 0), [(2, 0x10_000)]);
    }

    #[test]
    fn bad_addresses_are_refused() {
        let mut rmap: Rmap<u8> = Rmap::new();
        let unaligned = AddressError::Unaligned { addr: 0x10_001 };
        assert_eq!(
            rmap.map(1, 0x10_001, range(0, 1), Access::Read),
            Err(MapError::Address(unaligned))
        );
        assert_eq!(
            rmap.map(1, u64::MAX - P + 1, range(0, 2), Access::Read),
            Err(AddressError::Overflow.into())
        );
        assert_eq!(rmap.unmap(1, 0x10_001, 1), Err(unaligned));
        assert_eq!(rmap.unmap(1, 0, u64::MAX), Err(AddressError::Overflow));
        assert!(rmap.is_empty());
        assert!(!MapError::Overlap.to_string().is_empty());
        let error = MapError::Address(unaligned);
        let source = core::error::Error::source(&error).expect("a source");
        assert_eq!(source.to_string(), unaligned.to_string());
    }

    /// A reference model: the set of `(space, virtual page, file page, access)` of every mapped
    /// virtual page. Whatever sequence of maps and unmaps runs, the reverse map must find
    /// exactly the virtual pages the model has for each file page.
    #[test]
    fn matches_a_page_model() {
        const PAGES: u64 = 48;
        const VPAGES: u64 = 64;
        let mut rmap = Rmap::new();
        let mut model: BTreeSet<(u8, u64, u64, bool)> = BTreeSet::new();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |bound: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % bound
        };
        for _ in 0..5_000 {
            let space = next(3) as u8;
            match next(5) {
                0..=2 => {
                    let vpage = next(VPAGES);
                    let count = 1 + next(12) as u32;
                    let first = next(PAGES);
                    let writable = next(2) == 1;
                    let access = if writable {
                        Access::ReadWrite
                    } else {
                        Access::Read
                    };
                    let vpages = vpage..vpage + u64::from(count);
                    let free = !model
                        .iter()
                        .any(|&(s, v, _, _)| s == space && vpages.contains(&v));
                    let result = rmap.map(space, vpage * P, range(first, count), access);
                    assert_eq!(result.is_ok(), free);
                    if free {
                        for (i, v) in vpages.enumerate() {
                            model.insert((space, v, first + i as u64, writable));
                        }
                    }
                }
                3 => {
                    let vpage = next(VPAGES);
                    let count = next(16);
                    let gone: Vec<_> = model
                        .iter()
                        .copied()
                        .filter(|&(s, v, _, _)| s == space && (vpage..vpage + count).contains(&v))
                        .collect();
                    for entry in &gone {
                        model.remove(entry);
                    }
                    assert_eq!(rmap.unmap(space, vpage * P, count), Ok(gone.len() as u64));
                }
                _ => {
                    let before = rmap.mappings_of(space).count();
                    assert_eq!(rmap.remove_space(space), before);
                    model.retain(|&(s, _, _, _)| s != space);
                }
            }
            for page in 0..PAGES + 12 {
                let mut found: Vec<(u8, u64, u64, bool)> = rmap
                    .overlapping(range(page, 1))
                    .map(|m| (m.space, m.addr / P, page, m.access == Access::ReadWrite))
                    .collect();
                found.sort_unstable();
                // The model is a sorted set, so this is in the same order as `found`.
                let expected: Vec<_> = model.iter().copied().filter(|e| e.2 == page).collect();
                assert_eq!(found, expected, "file page {page}");
            }
        }
    }
}
