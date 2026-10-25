//! Bookkeeping for the DMA mappings `memory:translation` makes in a bound domain (needs the
//! `alloc` feature).
//!
//! When a driver process has bound a DMA domain, the kernel answers a translation request by
//! mapping the page's frame (or the whole physically contiguous allocation it belongs to) at a
//! fresh IOVA in the domain, and returning that IOVA. [`DmaMappings`] remembers those mappings
//! so that:
//!
//! - translating a frame that is already mapped returns the IOVA it already has
//!   ([`DmaMappings::iova_of`]), however many times and through however many virtual mappings
//!   of it the process asks;
//! - each mapping counts the virtual ranges of the process that reached it (its *users*,
//!   [`DmaMappings::add_user`]), so that when the process unmaps memory
//!   ([`DmaMappings::release`]) the mappings no longer reachable from any of its virtual
//!   memory are handed back for the kernel to unmap, invalidate and unpin;
//! - when the process takes write access away from memory (`mprotect`), the writable
//!   mappings that memory reaches are found ([`DmaMappings::writable_reached_by`]) for the
//!   kernel to make read-only for the device too;
//! - closing the domain hands back everything ([`DmaMappings::drain`]);
//! - an installation that failed without confirming its invalidation may still be reachable by
//!   a device, and no mapping record names it, so [`Retained`] keeps its resources owned until a
//!   teardown has blocked the domain's devices.
//!
//! The type only does the bookkeeping. The kernel owns the IOVA allocator, the translation
//! tables, the invalidation and the frame pinning, and keeps what it needs to undo them in the
//! mapping's `data`.
//!
//! # Examples
//!
//! ```
//! use ferric_iommu::dma::{DmaMappings, Mapping, Span};
//!
//! let mut maps = DmaMappings::new();
//! // A two-page buffer at physical 0x8000_0000, mapped at IOVA 0x4000 and reached from the
//! // driver's virtual range 0x7000_0000..0x7000_2000.
//! let user = Span::new(0x7000_0000, 0x2000);
//! maps.insert(Mapping::new(0x8000_0000, 0x2000, 0x4000, true, user, "block"))?;
//! assert_eq!(maps.iova_of(0x8000_1234), Some(0x5234));
//!
//! // Unmapping the buffer's virtual range releases the mapping.
//! let released = maps.release(user);
//! assert_eq!(released.len(), 1);
//! assert_eq!(maps.iova_of(0x8000_0000), None);
//! # Ok::<(), ferric_iommu::dma::Overlap>(())
//! ```

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;

/// Completion state of a strict DMA unmap.
///
/// A failed operation may have cleared page-table entries without completing invalidation.
/// Repeating the clear cannot prove that cached translations are gone, so failure remains
/// terminal until the owning domain is successfully blocked and its mappings are discarded.
#[derive(Debug, Default)]
pub struct UnmapState {
    status: UnmapStatus,
}

#[derive(Debug, Default)]
enum UnmapStatus {
    #[default]
    Mapped,
    Failed,
    Unmapped,
}

/// A strict unmap failed to establish that DMA can no longer reach the mapping.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnmapFailure<E> {
    /// The backend failed while clearing entries or invalidating cached translations.
    Backend(E),
    /// An earlier attempt failed; only confirmed domain blocking can retire this mapping.
    PreviouslyFailed,
}

impl<E: fmt::Display> fmt::Display for UnmapFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(f, "strict DMA unmap failed: {error}"),
            Self::PreviouslyFailed => f.write_str("strict DMA unmap previously failed"),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for UnmapFailure<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::PreviouslyFailed => None,
        }
    }
}

impl UnmapState {
    /// A mapping whose installation or rollback already failed to confirm invalidation.
    ///
    /// Retain its resources and grant record until successful domain blocking. Clearing
    /// entries again cannot establish that translations cached before the failure are gone.
    pub const fn failed() -> Self {
        Self {
            status: UnmapStatus::Failed,
        }
    }

    /// Runs `unmap` once, recording whether its strict invalidation completed.
    ///
    /// Keep the mapping's IOVA and frame pins owned until this returns `Ok`, or until
    /// successful domain blocking makes the entire mapping unreachable. Completed unmaps
    /// are idempotent; failed unmaps keep failing without rerunning a partially completed
    /// backend operation. The caller must serialize this state with its mapping table.
    ///
    /// # Errors
    ///
    /// [`UnmapFailure::Backend`] on the first backend failure, and
    /// [`UnmapFailure::PreviouslyFailed`] on every later attempt after that failure.
    ///
    /// # Panics
    ///
    /// Propagates a panic from `unmap`; callers must use a non-panicking backend so its
    /// invalidation outcome is recorded.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_iommu::dma::{UnmapFailure, UnmapState};
    /// let mut state = UnmapState::default();
    /// assert_eq!(state.attempt(|| Err("invalidation timeout")),
    ///     Err(UnmapFailure::Backend("invalidation timeout")));
    /// assert_eq!(state.attempt(|| Ok::<(), &str>(())),
    ///     Err(UnmapFailure::PreviouslyFailed));
    /// ```
    pub fn attempt<E>(
        &mut self,
        unmap: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), UnmapFailure<E>> {
        match self.status {
            UnmapStatus::Failed => Err(UnmapFailure::PreviouslyFailed),
            UnmapStatus::Unmapped => Ok(()),
            UnmapStatus::Mapped => match unmap() {
                Ok(()) => {
                    self.status = UnmapStatus::Unmapped;
                    Ok(())
                }
                Err(error) => {
                    self.status = UnmapStatus::Failed;
                    Err(UnmapFailure::Backend(error))
                }
            },
        }
    }
}

/// Completes a DMA mapping's teardown before removing its owned resources from `mappings`.
///
/// Returns `None` for a missing IOVA. On success the caller receives the entry and may free
/// its IOVA and frame pins. On failure the entry remains in place, including any failure
/// state recorded by `unmap`. Hold the owning domain's lock throughout this operation.
///
/// # Errors
///
/// Returns the error from `unmap` without removing or dropping the mapping entry.
///
/// # Panics
///
/// Propagates a panic from `unmap`, leaving the entry in `mappings`.
pub fn complete_unmap<T, E>(
    mappings: &mut BTreeMap<u64, T>,
    iova: u64,
    unmap: impl FnOnce(&mut T) -> Result<(), E>,
) -> Result<Option<T>, E> {
    let Some(mapping) = mappings.get_mut(&iova) else {
        return Ok(None);
    };
    unmap(mapping)?;
    Ok(mappings.remove(&iova))
}

/// What a teardown did to the devices that could reach a domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Teardown {
    /// Every device was blocked, so nothing the domain translated is reachable any more.
    Blocked,
    /// At least one device could not be blocked, so what the domain translated may still be
    /// reachable.
    Unblocked,
}

/// Installations the hardware did not confirm, kept owned until the domain is torn down.
///
/// A mapping whose installation failed without completing its invalidation may still be
/// reachable by a device, so its IOVA block and its frame pins have to stay owned. No mapping
/// record names it, so no unmap or close request can find it: only a teardown that blocked the
/// domain's devices can retire it.
///
/// [`Retained::release`] hands every entry back exactly once, and only when that teardown
/// blocked the devices. Otherwise the entries stay owned and are dropped at the end, which
/// leaks what they hold by design, the safe outcome while a device may still reach it.
#[derive(Debug, Default)]
pub struct Retained<T> {
    entries: Vec<T>,
}

impl<T> Retained<T> {
    /// Nothing retained.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Keeps `value` owned until a teardown releases it.
    pub fn insert(&mut self, value: T) {
        self.entries.push(value);
    }

    /// How many installations are retained.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is retained.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Hands every retained entry to `release`, in the order they were inserted, when
    /// `teardown` blocked the devices; otherwise keeps them all.
    ///
    /// Releasing drains the entries, so a second call after a blocking teardown hands nothing
    /// over.
    ///
    /// # Panics
    ///
    /// A panic in `release` propagates, and the entries after the one being released are not
    /// processed: they were already taken out of the list and are dropped instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_iommu::dma::{Retained, Teardown};
    ///
    /// let mut retained = Retained::new();
    /// retained.insert("block at 0x4000");
    /// // A teardown that could not block the devices keeps it owned.
    /// retained.release(Teardown::Unblocked, |_| unreachable!("nothing may be released"));
    /// assert_eq!(retained.len(), 1);
    /// // One that blocked them releases it, once.
    /// retained.release(Teardown::Blocked, |entry| assert_eq!(entry, "block at 0x4000"));
    /// assert!(retained.is_empty());
    /// retained.release(Teardown::Blocked, |_| unreachable!("released once already"));
    /// ```
    pub fn release(&mut self, teardown: Teardown, mut release: impl FnMut(T)) {
        if teardown != Teardown::Blocked {
            return;
        }
        for entry in core::mem::take(&mut self.entries) {
            release(entry);
        }
    }
}

/// A range of addresses: `len` bytes from `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    /// The first address.
    pub start: u64,
    /// The length in bytes.
    pub len: u64,
}

impl Span {
    /// `len` bytes from `start`.
    pub const fn new(start: u64, len: u64) -> Self {
        Self { start, len }
    }

    /// One past the last address, saturating at `u64::MAX`.
    pub const fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// Whether `addr` is in the span.
    pub const fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end()
    }

    /// Whether the two spans share an address. Empty spans share none.
    pub const fn intersects(&self, other: &Span) -> bool {
        self.start < other.end() && other.start < self.end()
    }
}

/// One frame range mapped in a domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping<T> {
    phys: Span,
    iova: u64,
    writable: bool,
    users: Vec<Span>,
    /// What the kernel needs to undo the mapping: its IOVA block, the frames it pinned.
    pub data: T,
}

impl<T> Mapping<T> {
    /// `phys` mapped at `iova` (writable by the device or read-only), first reached through
    /// the virtual range `user`, with the kernel's `data`.
    pub fn new(phys: u64, len: u64, iova: u64, writable: bool, user: Span, data: T) -> Self {
        Self {
            phys: Span::new(phys, len),
            iova,
            writable,
            users: alloc::vec![user],
            data,
        }
    }

    /// The physical range that is mapped.
    pub fn phys(&self) -> Span {
        self.phys
    }

    /// The IOVA of the range's first byte.
    pub fn iova(&self) -> u64 {
        self.iova
    }

    /// Whether the device may write the range.
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Records that the device may now write the range too.
    pub fn set_writable(&mut self) {
        self.writable = true;
    }

    /// Records that the device may now only read the range.
    pub fn set_read_only(&mut self) {
        self.writable = false;
    }

    /// The virtual ranges of the process that reach the mapping. While there is at least one,
    /// the mapping stays.
    pub fn users(&self) -> &[Span] {
        &self.users
    }

    /// The IOVA of physical address `phys`, if it is in the range.
    pub fn iova_of(&self, phys: u64) -> Option<u64> {
        self.phys
            .contains(phys)
            .then(|| self.iova.wrapping_add(phys.wrapping_sub(self.phys.start)))
    }
}

/// [`DmaMappings::insert`] was given a range that overlaps one already mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// The physical range already mapped.
    pub existing: Span,
}

impl fmt::Display for Overlap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "physical range {:#x}+{:#x} is already mapped",
            self.existing.start, self.existing.len
        )
    }
}

impl core::error::Error for Overlap {}

/// The DMA mappings of one domain, by physical address. Physical ranges never overlap.
#[derive(Clone, Debug)]
pub struct DmaMappings<T> {
    by_phys: BTreeMap<u64, Mapping<T>>,
}

impl<T> Default for DmaMappings<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> DmaMappings<T> {
    /// No mappings.
    pub const fn new() -> Self {
        Self {
            by_phys: BTreeMap::new(),
        }
    }

    /// How many mappings there are.
    pub fn len(&self) -> usize {
        self.by_phys.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.by_phys.is_empty()
    }

    /// The mapping whose physical range holds `phys`.
    pub fn get(&self, phys: u64) -> Option<&Mapping<T>> {
        self.by_phys
            .range(..=phys)
            .next_back()
            .map(|(_, m)| m)
            .filter(|m| m.phys.contains(phys))
    }

    /// The mapping whose physical range holds `phys`, to change.
    pub fn get_mut(&mut self, phys: u64) -> Option<&mut Mapping<T>> {
        self.by_phys
            .range_mut(..=phys)
            .next_back()
            .map(|(_, m)| m)
            .filter(|m| m.phys.contains(phys))
    }

    /// The IOVA physical address `phys` is mapped at, if it is.
    pub fn iova_of(&self, phys: u64) -> Option<u64> {
        self.get(phys).and_then(|m| m.iova_of(phys))
    }

    /// The first mapping that shares an address with `phys`.
    pub fn overlapping(&self, phys: Span) -> Option<&Mapping<T>> {
        // A range that starts below `phys` but reaches into it is the last one before its end.
        self.by_phys
            .range(..phys.end())
            .next_back()
            .map(|(_, m)| m)
            .filter(|m| m.phys.intersects(&phys))
            .or_else(|| {
                self.by_phys
                    .range(phys.start..phys.end())
                    .map(|(_, m)| m)
                    .find(|m| m.phys.intersects(&phys))
            })
    }

    /// Adds a mapping.
    ///
    /// # Errors
    ///
    /// [`Overlap`] when part of its physical range is already mapped; nothing changes.
    pub fn insert(&mut self, mapping: Mapping<T>) -> Result<(), Overlap> {
        if let Some(existing) = self.overlapping(mapping.phys) {
            return Err(Overlap {
                existing: existing.phys,
            });
        }
        self.by_phys.insert(mapping.phys.start, mapping);
        Ok(())
    }

    /// Removes and returns the mapping whose physical range holds `phys`, whatever its users.
    pub fn remove(&mut self, phys: u64) -> Option<Mapping<T>> {
        let start = self.get(phys)?.phys.start;
        self.by_phys.remove(&start)
    }

    /// Records that the virtual range `user` reaches the mapping holding `phys`. Returns
    /// `false` when no mapping holds it. Recording the same range twice keeps one entry, so
    /// translating every page of a buffer does not make it harder to release.
    pub fn add_user(&mut self, phys: u64, user: Span) -> bool {
        match self.get_mut(phys) {
            Some(mapping) => {
                if !mapping.users.contains(&user) {
                    mapping.users.push(user);
                }
                true
            }
            None => false,
        }
    }

    /// The virtual range `unmapped` of the process is gone: forgets every user that shares an
    /// address with it, and removes and returns the mappings left with no user, for the
    /// kernel to tear down.
    ///
    /// A user that is only partly unmapped is forgotten whole, so unmapping part of a buffer
    /// releases its mapping: the rest must be translated again before the device uses it.
    pub fn release(&mut self, unmapped: Span) -> Vec<Mapping<T>> {
        let mut gone = Vec::new();
        for (&start, mapping) in self.by_phys.iter_mut() {
            let before = mapping.users.len();
            mapping.users.retain(|u| !u.intersects(&unmapped));
            if mapping.users.is_empty() && before > 0 {
                gone.push(start);
            }
        }
        gone.into_iter()
            .filter_map(|start| self.by_phys.remove(&start))
            .collect()
    }

    /// The physical start of every writable mapping that a virtual range sharing an address
    /// with `span` reaches, lowest first.
    ///
    /// A mapping is one IOVA block with one access, so it is listed even when only part of it
    /// is reached, and even when another virtual range that reaches it keeps write access:
    /// the kernel narrows the whole mapping, and a later translation of a page that is still
    /// writable widens it again.
    pub fn writable_reached_by(&self, span: Span) -> Vec<u64> {
        self.by_phys
            .iter()
            .filter(|(_, m)| m.writable && m.users.iter().any(|u| u.intersects(&span)))
            .map(|(&start, _)| start)
            .collect()
    }

    /// Removes and returns every mapping.
    pub fn drain(&mut self) -> impl Iterator<Item = Mapping<T>> + use<T> {
        core::mem::take(&mut self.by_phys).into_values()
    }
}

/// Lifecycle of links between DMA domains and registered-buffer grants.
pub mod links;

/// Grant installation, rollback and publication of possibly reachable mappings.
pub mod grant;

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn concurrent_unmap_and_revoke_keep_a_failed_mapping_recorded() {
        use core::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Barrier, Mutex};
        use std::thread;

        let mappings = Mutex::new(BTreeMap::from([(0x1000, UnmapState::default())]));
        let invalidations = AtomicUsize::new(0);
        let start = Barrier::new(3);
        thread::scope(|scope| {
            let attempt = || {
                start.wait();
                let mut mappings = mappings.lock().unwrap();
                complete_unmap(&mut mappings, 0x1000, |state| {
                    state.attempt(|| {
                        invalidations.fetch_add(1, Ordering::Relaxed);
                        Err("IOTLB timeout")
                    })
                })
            };
            let unmap = scope.spawn(attempt);
            let revoke = scope.spawn(attempt);
            start.wait();
            assert!(unmap.join().unwrap().is_err());
            assert!(revoke.join().unwrap().is_err());
        });
        assert_eq!(invalidations.load(Ordering::Relaxed), 1);
        assert_eq!(mappings.lock().unwrap().len(), 1);
    }

    #[test]
    fn failed_invalidation_never_becomes_success_on_retry() {
        let mut state = UnmapState::default();
        let mut entries_present = true;
        let mut invalidations = 0;
        assert_eq!(
            state.attempt(|| {
                entries_present = false;
                invalidations += 1;
                Err("IOTLB timeout")
            }),
            Err(UnmapFailure::Backend("IOTLB timeout"))
        );
        assert!(!entries_present);
        for _ in 0..3 {
            assert_eq!(
                state.attempt(|| {
                    invalidations += 1;
                    Ok::<(), &str>(())
                }),
                Err(UnmapFailure::PreviouslyFailed)
            );
        }
        assert_eq!(invalidations, 1);
    }

    #[test]
    fn failed_teardown_does_not_allow_iova_reuse() {
        use crate::iova::IovaAllocator;
        let mut allocator = IovaAllocator::new(0x10_000, []);
        let block = allocator.alloc(0x1000).unwrap();
        let mut held = BTreeMap::from([(block.start, (block, UnmapState::default()))]);
        assert!(
            complete_unmap(&mut held, block.start, |(_, state)| {
                state.attempt(|| Err("timeout"))
            })
            .is_err()
        );
        // The kernel keeps this entry and its pins on error rather than returning its block.
        assert_ne!(allocator.alloc(0x1000).unwrap().start, block.start);
        let retry = complete_unmap(&mut held, block.start, |(_, state)| {
            state.attempt(|| Ok::<(), &str>(()))
        });
        assert!(matches!(retry, Err(UnmapFailure::PreviouslyFailed)));
        assert_eq!(held.len(), 1);
        // Only confirmed domain blocking permits the owner to discard the failed entry.
        let (blocked, _) = held.remove(&block.start).unwrap();
        allocator.free(blocked).unwrap();
        assert_eq!(allocator.alloc(0x1000).unwrap().start, block.start);
    }

    #[test]
    fn completed_unmap_does_not_invalidate_a_reused_iova() {
        let mut state = UnmapState::default();
        state.attempt(|| Ok::<(), &str>(())).unwrap();
        assert_eq!(state.attempt(|| Err("must not run twice")), Ok(()));
    }

    #[test]
    fn completed_teardown_returns_resources_once() {
        let mut held = BTreeMap::from([(0x1000, UnmapState::default())]);
        let taken = complete_unmap(&mut held, 0x1000, |state| {
            state.attempt(|| Ok::<(), &str>(()))
        })
        .unwrap();
        assert!(taken.is_some());
        assert!(held.is_empty());
        assert!(
            complete_unmap(&mut held, 0x1000, |_| Err("must not run"))
                .unwrap()
                .is_none()
        );
    }

    fn map(phys: u64, len: u64, iova: u64, user: Span) -> Mapping<u32> {
        Mapping::new(phys, len, iova, true, user, 0)
    }

    #[test]
    fn span_rules() {
        let s = Span::new(0x1000, 0x2000);
        assert!(s.contains(0x1000));
        assert!(s.contains(0x2fff));
        assert!(!s.contains(0x3000));
        assert!(s.intersects(&Span::new(0x2fff, 1)));
        assert!(!s.intersects(&Span::new(0x3000, 0x1000)));
        assert!(!s.intersects(&Span::new(0x1000, 0)));
        assert_eq!(Span::new(u64::MAX, 2).end(), u64::MAX);
    }

    #[test]
    fn lookups_follow_the_physical_range() {
        let mut maps = DmaMappings::new();
        maps.insert(map(0x10_0000, 0x3000, 0x8000, Span::new(0x7000, 0x3000)))
            .unwrap();
        maps.insert(map(0x20_0000, 0x1000, 0x1000, Span::new(0xa000, 0x1000)))
            .unwrap();
        assert_eq!(maps.len(), 2);
        assert_eq!(maps.iova_of(0x10_0000), Some(0x8000));
        assert_eq!(maps.iova_of(0x10_2abc), Some(0xaabc));
        assert_eq!(maps.iova_of(0x10_3000), None);
        assert_eq!(maps.iova_of(0x0f_ffff), None);
        assert_eq!(maps.iova_of(0x20_0010), Some(0x1010));
    }

    #[test]
    fn overlaps_are_refused() {
        let mut maps = DmaMappings::new();
        maps.insert(map(0x10_0000, 0x3000, 0x8000, Span::new(0, 0x3000)))
            .unwrap();
        let existing = Span::new(0x10_0000, 0x3000);
        // Inside, straddling either end, and covering it whole.
        for (phys, len) in [
            (0x10_1000, 0x1000),
            (0x0f_f000, 0x2000),
            (0x10_2000, 0x2000),
            (0x0f_0000, 0x10_0000),
        ] {
            assert_eq!(
                maps.insert(map(phys, len, 0x1_0000, Span::new(0, 0x1000))),
                Err(Overlap { existing })
            );
        }
        // Adjacent ranges are fine.
        maps.insert(map(0x10_3000, 0x1000, 0x1000, Span::new(0x9000, 0x1000)))
            .unwrap();
        maps.insert(map(0x0f_f000, 0x1000, 0x2000, Span::new(0xa000, 0x1000)))
            .unwrap();
        assert_eq!(maps.len(), 3);
    }

    #[test]
    fn users_are_counted_once_and_released_together() {
        let mut maps = DmaMappings::new();
        let first = Span::new(0x7000_0000, 0x2000);
        let second = Span::new(0x7100_0000, 0x2000);
        maps.insert(map(0x10_0000, 0x2000, 0x4000, first)).unwrap();
        assert!(maps.add_user(0x10_1000, first));
        assert!(maps.add_user(0x10_0000, second));
        assert!(!maps.add_user(0x20_0000, second));
        assert_eq!(maps.get(0x10_0000).unwrap().users(), &[first, second]);

        // Another virtual mapping of the same frames keeps the IOVA alive.
        assert!(maps.release(first).is_empty());
        assert_eq!(maps.iova_of(0x10_0000), Some(0x4000));
        // Unmapping unrelated memory changes nothing.
        assert!(maps.release(Span::new(0x1000, 0x1000)).is_empty());
        // A partial unmap of the last user releases the whole mapping.
        let gone = maps.release(Span::new(0x7100_1000, 0x1000));
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].iova(), 0x4000);
        assert!(maps.is_empty());
    }

    #[test]
    fn one_unmap_releases_several_mappings() {
        let mut maps = DmaMappings::new();
        maps.insert(map(0x10_0000, 0x1000, 0x1000, Span::new(0x7000, 0x1000)))
            .unwrap();
        maps.insert(map(0x30_0000, 0x1000, 0x2000, Span::new(0x8000, 0x1000)))
            .unwrap();
        maps.insert(map(0x50_0000, 0x1000, 0x3000, Span::new(0x9000, 0x1000)))
            .unwrap();
        let mut gone = maps
            .release(Span::new(0x7000, 0x2000))
            .into_iter()
            .map(|m| m.iova())
            .collect::<Vec<_>>();
        gone.sort_unstable();
        assert_eq!(gone, [0x1000, 0x2000]);
        assert_eq!(maps.len(), 1);
    }

    #[test]
    fn write_removal_finds_the_writable_mappings_reached() {
        let mut maps = DmaMappings::new();
        // Writable, reached from two virtual ranges.
        maps.insert(map(
            0x10_0000,
            0x2000,
            0x1000,
            Span::new(0x7000_0000, 0x2000),
        ))
        .unwrap();
        assert!(maps.add_user(0x10_0000, Span::new(0x7100_0000, 0x2000)));
        // Read-only already.
        maps.insert(Mapping::new(
            0x20_0000,
            0x1000,
            0x3000,
            false,
            Span::new(0x7000_2000, 0x1000),
            0,
        ))
        .unwrap();
        // Writable, but out of reach of the ranges below.
        maps.insert(map(
            0x30_0000,
            0x1000,
            0x4000,
            Span::new(0x7200_0000, 0x1000),
        ))
        .unwrap();

        // Part of the first user, and all of the read-only mapping's.
        assert_eq!(
            maps.writable_reached_by(Span::new(0x7000_1000, 0x2000)),
            [0x10_0000]
        );
        // Through the second user only.
        assert_eq!(
            maps.writable_reached_by(Span::new(0x7100_0000, 0x1000)),
            [0x10_0000]
        );
        assert!(
            maps.writable_reached_by(Span::new(0x7300_0000, 0x1000))
                .is_empty()
        );
        assert_eq!(
            maps.writable_reached_by(Span::new(0x7000_0000, 0x0300_0000)),
            [0x10_0000, 0x30_0000]
        );

        // Once read-only, it is not listed again, and nothing is released.
        maps.get_mut(0x10_0000).unwrap().set_read_only();
        assert!(!maps.get(0x10_0000).unwrap().writable());
        assert!(
            maps.writable_reached_by(Span::new(0x7000_0000, 0x2000))
                .is_empty()
        );
        assert_eq!(maps.len(), 3);
    }

    #[test]
    fn writable_upgrade_and_drain() {
        let mut maps = DmaMappings::new();
        maps.insert(Mapping::new(
            0x1000,
            0x1000,
            0x1000,
            false,
            Span::new(0, 0x1000),
            7,
        ))
        .unwrap();
        assert!(!maps.get(0x1000).unwrap().writable());
        maps.get_mut(0x1fff).unwrap().set_writable();
        assert!(maps.get(0x1000).unwrap().writable());
        assert!(maps.remove(0x3000).is_none());
        maps.insert(Mapping::new(
            0x3000,
            0x2000,
            0x4000,
            true,
            Span::new(0, 0x1000),
            8,
        ))
        .unwrap();
        assert_eq!(maps.remove(0x4abc).map(|m| m.data), Some(8));
        let all = maps.drain().collect::<Vec<_>>();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].data, 7);
        assert!(maps.is_empty());
    }

    #[test]
    fn retained_installations_wait_for_a_clean_teardown() {
        let mut retained = Retained::new();
        assert!(retained.is_empty());
        retained.insert(0x4000);
        retained.insert(0x8000);
        let mut released = Vec::new();
        // A teardown that could not block the devices releases nothing.
        retained.release(Teardown::Unblocked, |block| released.push(block));
        assert!(released.is_empty());
        assert_eq!(retained.len(), 2);
        // One that blocked them releases every installation once, in order.
        retained.release(Teardown::Blocked, |block| released.push(block));
        assert_eq!(released, [0x4000, 0x8000]);
        assert!(retained.is_empty());
        retained.release(Teardown::Blocked, |block| released.push(block));
        assert_eq!(released.len(), 2);
    }
}
