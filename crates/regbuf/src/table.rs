//! The kernel's bookkeeping for one region: its length, the grants made from it, and the
//! mappings each grant has made.
//!
//! The table knows nothing about address spaces: a mapping is whatever `M` the kernel records
//! (the address space and the span it mapped), and the table hands mappings back when they must
//! be taken down: on [`GrantTable::revoke`], [`GrantTable::remove`] (the grant's descriptor was
//! closed) and [`GrantTable::revoke_all`] (the region's was). It never hands back the same
//! mapping twice, and once a grant is revoked it refuses new mappings, so taking down what it
//! returned leaves nothing of the grant mapped.
//!
//! Grants are byte-granular. [`GrantTable::check_map`] plans a mapping of a grant's window
//! ([`MapPlan`]): which region pages it maps, and which of them the grant covers only in part
//! ([`PartialPage`]), which the kernel maps as bounce pages instead of the region's own.

use alloc::{collections::BTreeMap, vec::Vec};
use core::fmt;

use crate::abi::{GrantId, GrantInfo, GrantSpec, PAGE_SIZE};

/// Why a table operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableError {
    /// The region already has memory registered.
    AlreadyRegistered,
    /// The region has no memory registered yet.
    NotRegistered,
    /// The range does not lie inside the region or the grant's window.
    OutOfRange,
    /// The offset or length of a registration or mapping is not page-aligned, or the length
    /// is zero.
    Unaligned,
    /// No grant with this ID was ever made from the region.
    Unknown(GrantId),
    /// The grant has been revoked.
    Revoked(GrantId),
    /// A write mapping of a read-only grant.
    Access(GrantId),
    /// The grant has no such mapping.
    NoMapping(GrantId),
    /// Every grant ID has been used.
    IdsExhausted,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRegistered => f.write_str("region already registered"),
            Self::NotRegistered => f.write_str("region has no memory registered"),
            Self::OutOfRange => f.write_str("range lies outside the region or grant"),
            Self::Unaligned => f.write_str("range is empty or not page-aligned"),
            Self::Unknown(id) => write!(f, "{id} was never made from this region"),
            Self::Revoked(id) => write!(f, "{id} has been revoked"),
            Self::Access(id) => write!(f, "{id} is read-only"),
            Self::NoMapping(id) => write!(f, "{id} has no such mapping"),
            Self::IdsExhausted => f.write_str("grant IDs exhausted"),
        }
    }
}

impl core::error::Error for TableError {}

/// A run of pages of the region, by index from its first page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRange {
    /// The first page.
    pub first: usize,
    /// How many pages.
    pub count: usize,
}

/// A page of a mapping that the grant covers only in part. The kernel maps a bounce page in
/// its place holding just the granted bytes, and copies those back for a writable mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartialPage {
    /// The page's index in the region.
    pub region_page: usize,
    /// The page's index in the mapping.
    pub mapping_page: usize,
    /// The first granted byte within the page.
    pub start: usize,
    /// The byte after the last granted one within the page; `start < end <= PAGE_SIZE`.
    pub end: usize,
}

/// What one mapping of a grant is: a run of region pages, at most two of which (the window's
/// first and last) the grant covers only in part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapPlan {
    /// The region pages the mapping spans, in order.
    pub pages: PageRange,
    partial: [Option<PartialPage>; 2],
}

impl MapPlan {
    /// The pages of the mapping the grant covers only in part, in order.
    pub fn partial_pages(&self) -> impl Iterator<Item = PartialPage> + '_ {
        self.partial.iter().flatten().copied()
    }

    /// The partial page at index `mapping_page` of the mapping, if that page is one.
    pub fn partial_at(&self, mapping_page: usize) -> Option<PartialPage> {
        self.partial_pages()
            .find(|p| p.mapping_page == mapping_page)
    }
}

/// The pages of `spec`'s window it covers only in part: the region page and the granted bytes
/// within it, for the first and the last window page (one entry when they are the same page).
fn partial_window_pages(spec: &GrantSpec) -> [Option<(u64, u64, u64)>; 2] {
    let first = spec.window_offset() / PAGE_SIZE;
    let last = (spec.window_offset() + spec.window_len()) / PAGE_SIZE - 1;
    let lead = spec.lead();
    let tail = spec.end() % PAGE_SIZE;
    let first_end = if first == last && tail != 0 {
        tail
    } else {
        PAGE_SIZE
    };
    let head = (lead != 0 || first_end != PAGE_SIZE).then_some((first, lead, first_end));
    let back = (last != first && tail != 0).then_some((last, 0, tail));
    [head, back]
}

/// One grant.
#[derive(Debug)]
struct Entry<M> {
    spec: GrantSpec,
    revoked: bool,
    mappings: Vec<M>,
}

/// The grants of one region, with the mappings each has made.
#[derive(Debug)]
pub struct GrantTable<M> {
    region_len: u64,
    next_id: u64,
    grants: BTreeMap<GrantId, Entry<M>>,
}

impl<M> Default for GrantTable<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> GrantTable<M> {
    /// The table of a region with nothing registered.
    pub const fn new() -> Self {
        Self {
            region_len: 0,
            next_id: 1,
            grants: BTreeMap::new(),
        }
    }

    /// Records that `len` bytes are registered.
    ///
    /// # Errors
    ///
    /// [`TableError::AlreadyRegistered`] when memory is registered already and
    /// [`TableError::Unaligned`] when `len` is zero or not a multiple of [`PAGE_SIZE`].
    pub fn register(&mut self, len: u64) -> Result<(), TableError> {
        if self.region_len != 0 {
            return Err(TableError::AlreadyRegistered);
        }
        if len == 0 || !len.is_multiple_of(PAGE_SIZE) {
            return Err(TableError::Unaligned);
        }
        self.region_len = len;
        Ok(())
    }

    /// The registered length in bytes, zero before [`GrantTable::register`].
    pub const fn region_len(&self) -> u64 {
        self.region_len
    }

    /// How many grants are open (revoked ones included, until their descriptors close).
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// Whether no grant is open.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Makes a grant and returns its ID.
    ///
    /// # Errors
    ///
    /// [`TableError::NotRegistered`] before registration, [`TableError::OutOfRange`] when the
    /// range does not lie inside the region and [`TableError::IdsExhausted`] after `u64::MAX`
    /// grants.
    pub fn create(&mut self, spec: GrantSpec) -> Result<GrantId, TableError> {
        if self.region_len == 0 {
            return Err(TableError::NotRegistered);
        }
        if !spec.fits(self.region_len) {
            return Err(TableError::OutOfRange);
        }
        let id = GrantId::new(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(TableError::IdsExhausted)?;
        self.grants.insert(
            id,
            Entry {
                spec,
                revoked: false,
                mappings: Vec::new(),
            },
        );
        Ok(id)
    }

    /// Whether `id` was ever made from this region.
    fn issued(&self, id: GrantId) -> bool {
        id.get() != 0 && id.get() < self.next_id
    }

    fn entry(&self, id: GrantId) -> Result<&Entry<M>, TableError> {
        self.grants.get(&id).ok_or(TableError::Unknown(id))
    }

    fn entry_mut(&mut self, id: GrantId) -> Result<&mut Entry<M>, TableError> {
        self.grants.get_mut(&id).ok_or(TableError::Unknown(id))
    }

    /// What [`crate::abi::Request::Info`] reports about grant `id`.
    ///
    /// # Errors
    ///
    /// [`TableError::Unknown`] when the grant is not open.
    pub fn info(&self, id: GrantId) -> Result<GrantInfo, TableError> {
        let entry = self.entry(id)?;
        Ok(GrantInfo {
            id,
            spec: entry.spec,
            revoked: entry.revoked,
        })
    }

    /// How many mappings grant `id` has, or zero when it is not open.
    pub fn mapping_count(&self, id: GrantId) -> usize {
        self.grants.get(&id).map_or(0, |e| e.mappings.len())
    }

    /// Checks that `len` bytes from `offset` of grant `id`'s window (offsets relative to the
    /// window's first page, [`GrantSpec::window_offset`]) may be mapped, writable when `write`
    /// is set, and returns the plan of the mapping.
    ///
    /// # Errors
    ///
    /// [`TableError::Unknown`] when the grant is not open, [`TableError::Revoked`] once it is
    /// revoked, [`TableError::Access`] for a write mapping of a read-only grant,
    /// [`TableError::Unaligned`] for an empty or unaligned range and
    /// [`TableError::OutOfRange`] for one that leaves the window.
    pub fn check_map(
        &self,
        id: GrantId,
        offset: u64,
        len: u64,
        write: bool,
    ) -> Result<MapPlan, TableError> {
        let entry = self.entry(id)?;
        if entry.revoked {
            return Err(TableError::Revoked(id));
        }
        if write && !entry.spec.access().allows_write() {
            return Err(TableError::Access(id));
        }
        if len == 0 || !offset.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(TableError::Unaligned);
        }
        let spec = entry.spec;
        let end = offset.checked_add(len).ok_or(TableError::OutOfRange)?;
        if end > spec.window_len() {
            return Err(TableError::OutOfRange);
        }
        // Inside the window, which GrantSpec::new checked ends by u64::MAX.
        let start = spec.window_offset() + offset;
        let index = |bytes: u64| usize::try_from(bytes).map_err(|_| TableError::OutOfRange);
        let pages = PageRange {
            first: index(start / PAGE_SIZE)?,
            count: index(len / PAGE_SIZE)?,
        };
        let mut partial = [None; 2];
        for (slot, window_page) in partial.iter_mut().zip(partial_window_pages(&spec)) {
            let Some((page, from, to)) = window_page else {
                continue;
            };
            let page = index(page)?;
            if page < pages.first || page - pages.first >= pages.count {
                continue;
            }
            *slot = Some(PartialPage {
                region_page: page,
                mapping_page: page - pages.first,
                start: index(from)?,
                end: index(to)?,
            });
        }
        Ok(MapPlan { pages, partial })
    }

    /// Records a mapping grant `id` has made.
    ///
    /// # Errors
    ///
    /// [`TableError::Unknown`] when the grant is not open and [`TableError::Revoked`] once it is
    /// revoked; the mapping is handed back then, so the caller takes it down.
    pub fn add_mapping(&mut self, id: GrantId, mapping: M) -> Result<(), (TableError, M)> {
        let entry = match self.entry_mut(id) {
            Ok(entry) => entry,
            Err(err) => return Err((err, mapping)),
        };
        if entry.revoked {
            return Err((TableError::Revoked(id), mapping));
        }
        entry.mappings.push(mapping);
        Ok(())
    }

    /// Removes and returns the first mapping of grant `id` that `matches`.
    ///
    /// # Errors
    ///
    /// [`TableError::Unknown`] when the grant is not open and [`TableError::NoMapping`] when no
    /// mapping matches.
    pub fn take_mapping(
        &mut self,
        id: GrantId,
        matches: impl FnMut(&M) -> bool,
    ) -> Result<M, TableError> {
        let entry = self.entry_mut(id)?;
        let index = entry
            .mappings
            .iter()
            .position(matches)
            .ok_or(TableError::NoMapping(id))?;
        Ok(entry.mappings.swap_remove(index))
    }

    /// Revokes grant `id` and returns its mappings, which the caller must take down. A grant
    /// whose descriptor has closed is gone already, so revoking it returns nothing; so does
    /// revoking a revoked grant.
    ///
    /// # Errors
    ///
    /// [`TableError::Unknown`] when no grant with this ID was ever made from the region.
    pub fn revoke(&mut self, id: GrantId) -> Result<Vec<M>, TableError> {
        let issued = self.issued(id);
        match self.grants.get_mut(&id) {
            Some(entry) => {
                entry.revoked = true;
                Ok(core::mem::take(&mut entry.mappings))
            }
            None if issued => Ok(Vec::new()),
            None => Err(TableError::Unknown(id)),
        }
    }

    /// Forgets grant `id`, whose descriptor has closed, and returns its mappings, which the
    /// caller must take down. Nothing when it is not open.
    pub fn remove(&mut self, id: GrantId) -> Vec<M> {
        self.grants
            .remove(&id)
            .map(|e| e.mappings)
            .unwrap_or_default()
    }

    /// Revokes every grant (the region's descriptor has closed) and returns all their
    /// mappings, which the caller must take down. Grants stay in the table, revoked, until
    /// their own descriptors close.
    pub fn revoke_all(&mut self) -> Vec<M> {
        let mut all = Vec::new();
        for entry in self.grants.values_mut() {
            entry.revoked = true;
            all.append(&mut entry.mappings);
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::Access;

    const P: u64 = PAGE_SIZE;

    fn spec(access: Access, offset: u64, len: u64) -> GrantSpec {
        GrantSpec::new(access, offset, len).expect("valid test range")
    }

    fn table(pages: u64) -> GrantTable<u32> {
        let mut t = GrantTable::new();
        t.register(pages * P).expect("first registration");
        t
    }

    #[test]
    fn register_once() {
        let mut t = GrantTable::<u32>::new();
        assert_eq!(
            t.create(spec(Access::Read, 0, P)),
            Err(TableError::NotRegistered)
        );
        assert_eq!(t.register(0), Err(TableError::Unaligned));
        assert_eq!(t.register(P + 1), Err(TableError::Unaligned));
        assert_eq!(t.register(4 * P), Ok(()));
        assert_eq!(t.register(4 * P), Err(TableError::AlreadyRegistered));
        assert_eq!(t.region_len(), 4 * P);
    }

    #[test]
    fn create_checks_range_and_numbers_ids() {
        let mut t = table(4);
        assert_eq!(
            t.create(spec(Access::Read, 2 * P, 3 * P)),
            Err(TableError::OutOfRange)
        );
        let a = t.create(spec(Access::Read, 0, 4 * P)).expect("fits");
        let b = t.create(spec(Access::ReadWrite, 3 * P, P)).expect("fits");
        assert_eq!((a.get(), b.get()), (1, 2));
        assert_eq!(t.len(), 2);
        let info = t.info(b).expect("open");
        assert_eq!(info.spec, spec(Access::ReadWrite, 3 * P, P));
        assert!(!info.revoked);
    }

    #[test]
    fn check_map_enforces_access_and_bounds() {
        let mut t = table(8);
        let ro = t.create(spec(Access::Read, 2 * P, 4 * P)).expect("fits");
        let rw = t.create(spec(Access::ReadWrite, 0, 8 * P)).expect("fits");
        assert_eq!(
            t.check_map(ro, P, 2 * P, false).map(|p| p.pages),
            Ok(PageRange { first: 3, count: 2 })
        );
        assert_eq!(t.check_map(ro, 0, P, true), Err(TableError::Access(ro)));
        let whole = t.check_map(rw, 0, 8 * P, true).expect("whole grant");
        assert_eq!(whole.pages, PageRange { first: 0, count: 8 });
        assert_eq!(whole.partial_pages().count(), 0);
        assert_eq!(
            t.check_map(ro, 3 * P, 2 * P, false),
            Err(TableError::OutOfRange)
        );
        assert_eq!(
            t.check_map(ro, u64::MAX - P + 1, 2 * P, false),
            Err(TableError::OutOfRange)
        );
        assert_eq!(t.check_map(ro, 1, P, false), Err(TableError::Unaligned));
        assert_eq!(t.check_map(ro, 0, 0, false), Err(TableError::Unaligned));
        let missing = GrantId::new(99);
        assert_eq!(
            t.check_map(missing, 0, P, false),
            Err(TableError::Unknown(missing))
        );
    }

    #[test]
    fn byte_granular_plans_bounce_the_partial_pages() {
        let mut t = table(8);
        // From 0x100 into page 2 to 0x80 into page 5: pages 2 and 5 are partial.
        let g = t
            .create(spec(Access::ReadWrite, 2 * P + 0x100, 3 * P - 0x80))
            .expect("fits");
        let plan = t.check_map(g, 0, 4 * P, true).expect("whole window");
        assert_eq!(plan.pages, PageRange { first: 2, count: 4 });
        let partial: Vec<_> = plan.partial_pages().collect();
        assert_eq!(
            partial,
            [
                PartialPage {
                    region_page: 2,
                    mapping_page: 0,
                    start: 0x100,
                    end: P as usize,
                },
                PartialPage {
                    region_page: 5,
                    mapping_page: 3,
                    start: 0,
                    end: 0x80,
                },
            ]
        );
        assert_eq!(plan.partial_at(3), Some(partial[1]));
        assert_eq!(plan.partial_at(1), None);
        // A mapping of the middle of the window has no bounce pages.
        let middle = t.check_map(g, P, 2 * P, false).expect("middle");
        assert_eq!(middle.pages, PageRange { first: 3, count: 2 });
        assert_eq!(middle.partial_pages().count(), 0);
        // One that starts at the last page sees only that partial page, at index 0.
        let last = t.check_map(g, 3 * P, P, false).expect("last page");
        assert_eq!(
            last.partial_at(0).map(|p| (p.region_page, p.end)),
            Some((5, 0x80))
        );
        // The window is four pages; nothing beyond it maps.
        assert_eq!(t.check_map(g, 0, 5 * P, false), Err(TableError::OutOfRange));
    }

    #[test]
    fn byte_granular_inside_one_page() {
        let mut t = table(2);
        let g = t.create(spec(Access::Read, P + 0x10, 0x20)).expect("fits");
        let plan = t.check_map(g, 0, P, false).expect("window");
        assert_eq!(plan.pages, PageRange { first: 1, count: 1 });
        assert_eq!(
            plan.partial_pages().collect::<Vec<_>>(),
            [PartialPage {
                region_page: 1,
                mapping_page: 0,
                start: 0x10,
                end: 0x30,
            }]
        );
        // Starts on a page boundary but ends inside the page.
        let h = t.create(spec(Access::Read, 0, 0x10)).expect("fits");
        let plan = t.check_map(h, 0, P, false).expect("window");
        assert_eq!(
            plan.partial_at(0).map(|p| (p.start, p.end)),
            Some((0, 0x10))
        );
        // Ends on a page boundary but starts inside the page.
        let k = t.create(spec(Access::Read, 0xf00, 0x100)).expect("fits");
        let plan = t.check_map(k, 0, P, false).expect("window");
        assert_eq!(
            plan.partial_at(0).map(|p| (p.start, p.end)),
            Some((0xf00, P as usize))
        );
        // Outside the region.
        assert_eq!(
            t.create(spec(Access::Read, 2 * P - 1, 2)),
            Err(TableError::OutOfRange)
        );
    }

    #[test]
    fn revoke_returns_mappings_once_and_blocks_new_ones() {
        let mut t = table(2);
        let g = t.create(spec(Access::ReadWrite, 0, 2 * P)).expect("fits");
        t.add_mapping(g, 10).expect("live");
        t.add_mapping(g, 11).expect("live");
        assert_eq!(t.mapping_count(g), 2);
        let mut gone = t.revoke(g).expect("issued");
        gone.sort_unstable();
        assert_eq!(gone, [10, 11]);
        assert_eq!(t.revoke(g), Ok(Vec::new()));
        assert_eq!(t.check_map(g, 0, P, false), Err(TableError::Revoked(g)));
        assert_eq!(t.add_mapping(g, 12), Err((TableError::Revoked(g), 12)));
        assert!(t.info(g).expect("still open").revoked);
        assert_eq!(t.mapping_count(g), 0);
    }

    #[test]
    fn revoke_of_closed_and_unknown_grants() {
        let mut t = table(1);
        let g = t.create(spec(Access::Read, 0, P)).expect("fits");
        t.add_mapping(g, 1).expect("live");
        assert_eq!(t.remove(g), [1]);
        assert!(t.is_empty());
        assert_eq!(t.revoke(g), Ok(Vec::new()));
        assert_eq!(t.remove(g), Vec::<u32>::new());
        let never = GrantId::new(2);
        assert_eq!(t.revoke(never), Err(TableError::Unknown(never)));
        assert_eq!(
            t.revoke(GrantId::new(0)),
            Err(TableError::Unknown(GrantId::new(0)))
        );
    }

    #[test]
    fn take_mapping_finds_one() {
        let mut t = table(1);
        let g = t.create(spec(Access::Read, 0, P)).expect("fits");
        t.add_mapping(g, 5).expect("live");
        t.add_mapping(g, 6).expect("live");
        assert_eq!(t.take_mapping(g, |&m| m == 6), Ok(6));
        assert_eq!(
            t.take_mapping(g, |&m| m == 6),
            Err(TableError::NoMapping(g))
        );
        assert_eq!(t.mapping_count(g), 1);
    }

    #[test]
    fn revoke_all_covers_every_grant() {
        let mut t = table(4);
        let a = t.create(spec(Access::Read, 0, P)).expect("fits");
        let b = t.create(spec(Access::ReadWrite, P, P)).expect("fits");
        t.add_mapping(a, 1).expect("live");
        t.add_mapping(b, 2).expect("live");
        t.add_mapping(b, 3).expect("live");
        let mut all = t.revoke_all();
        all.sort_unstable();
        assert_eq!(all, [1, 2, 3]);
        assert_eq!(t.check_map(a, 0, P, false), Err(TableError::Revoked(a)));
        assert_eq!(t.check_map(b, 0, P, false), Err(TableError::Revoked(b)));
        assert!(t.revoke_all().is_empty());
        assert_eq!(t.len(), 2);
    }
}
