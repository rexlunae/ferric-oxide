//! Per-domain IOVA allocation (needs the `alloc` feature).
//!
//! Each DMA domain has its own IOVA space, `[4 KiB, limit)`, where `limit` is what the
//! domain's tables and the units it is attached to can translate. [`IovaAllocator`] hands out
//! ranges of that space first-fit, from a `BTreeMap` of free ranges.
//!
//! Ranges are rounded up to a power of two and aligned to their own size, up to 2 MiB:
//! a 12 KiB request takes a 16 KiB-aligned 16 KiB block, a 3 MiB request a 2 MiB-aligned 4 MiB
//! block. Naturally aligned blocks let the kernel map buffers with 2 MiB pages and invalidate
//! a whole range with one page-selective IOTLB invalidation (one address mask).
//!
//! Two kinds of IOVA are never handed out:
//!
//! - [`INTERRUPT_RANGE`], `0xFEE0_0000..=0xFEEF_FFFF`: DMA writes there are interrupt
//!   requests, which the unit never translates (VT-d specification section 3.14,
//!   "Handling Requests to Interrupt Address Range").
//! - The reserved ranges passed to [`IovaAllocator::new`]: the RMRRs that apply to the
//!   domain's devices, which are mapped one-to-one in the domain.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

/// The interrupt address range, `0xFEE0_0000..=0xFEEF_FFFF`.
pub const INTERRUPT_RANGE: Range<u64> = 0xfee0_0000..0xfef0_0000;

/// The largest alignment a range gets: one 2 MiB second-stage page.
pub const MAX_ALIGN: u64 = 2 << 20;

/// The lowest IOVA handed out. Page 0 stays unused so a zero DMA address always faults.
pub const FIRST_IOVA: u64 = 0x1000;

/// A block of IOVA space from [`IovaAllocator::alloc`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IovaRange {
    /// The first IOVA, aligned to `min(len, MAX_ALIGN)`.
    pub start: u64,
    /// The length: a power of two of at least 4 KiB, or a multiple of [`MAX_ALIGN`].
    pub len: u64,
}

impl IovaRange {
    /// One past the last IOVA.
    pub const fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// Why an IOVA operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IovaError {
    /// The length is zero or too large to round up.
    InvalidLength {
        /// The requested length.
        len: u64,
    },
    /// No free range is large enough.
    Exhausted {
        /// The rounded-up length that did not fit.
        len: u64,
    },
    /// The range given back was not one this allocator handed out (misaligned, of a size it
    /// never hands out, outside the space, reserved, or already free).
    NotAllocated {
        /// The range.
        range: IovaRange,
    },
}

impl fmt::Display for IovaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { len } => write!(f, "invalid IOVA length {len:#x}"),
            Self::Exhausted { len } => write!(f, "no free IOVA range of {len:#x} bytes"),
            Self::NotAllocated { range } => write!(
                f,
                "IOVA range {:#x}+{:#x} was not allocated",
                range.start, range.len
            ),
        }
    }
}

impl core::error::Error for IovaError {}

/// The block length for a request of `len` bytes: the next power of two of at least 4 KiB up
/// to [`MAX_ALIGN`], and the next multiple of `MAX_ALIGN` above it. `None` when `len` is zero
/// or the result would overflow.
pub fn block_len(len: u64) -> Option<u64> {
    match len {
        0 => None,
        1..=MAX_ALIGN => Some(len.next_power_of_two().max(0x1000)),
        _ => len.checked_next_multiple_of(MAX_ALIGN),
    }
}

/// First-fit allocator over one domain's IOVA space.
///
/// # Examples
///
/// ```
/// use ferric_vtd::iova::IovaAllocator;
///
/// // A 39-bit space with an RMRR at 0x7c00_0000.
/// let mut iova = IovaAllocator::new(1 << 39, Some(0x7c00_0000..0x7c80_0000));
/// let a = iova.alloc(0x3000)?;
/// assert_eq!((a.start, a.len), (0x4000, 0x4000));
/// let b = iova.alloc(0x20_0000)?;
/// assert_eq!(b.start, 0x20_0000);
/// iova.free(a)?;
/// assert_eq!(iova.alloc(0x1000)?.start, 0x1000);
/// # Ok::<(), ferric_vtd::iova::IovaError>(())
/// ```
#[derive(Clone, Debug)]
pub struct IovaAllocator {
    /// Free ranges, start to end (exclusive). Adjacent ranges are always merged, unless a
    /// reserved range lies between them.
    free: BTreeMap<u64, u64>,
    /// Ranges never handed out, rounded out to 4 KiB, sorted.
    reserved: Vec<Range<u64>>,
    limit: u64,
}

impl IovaAllocator {
    /// An allocator for `[FIRST_IOVA, limit)` (with `limit` rounded down to 4 KiB) less
    /// [`INTERRUPT_RANGE`] and `reserved`. Reserved ranges are rounded out to whole 4 KiB pages;
    /// empty ones are ignored.
    pub fn new(limit: u64, reserved: impl IntoIterator<Item = Range<u64>>) -> Self {
        let limit = limit & !0xfff;
        let mut holes: Vec<Range<u64>> = reserved
            .into_iter()
            .chain([INTERRUPT_RANGE])
            .filter(|r| r.start < r.end)
            .map(|r| (r.start & !0xfff)..r.end.checked_next_multiple_of(0x1000).unwrap_or(!0xfff))
            .collect();
        holes.sort_unstable_by_key(|r| r.start);
        let mut free = BTreeMap::new();
        let mut at = FIRST_IOVA;
        for hole in &holes {
            if hole.start > at {
                free.insert(at, hole.start.min(limit));
            }
            at = at.max(hole.end);
            if at >= limit {
                break;
            }
        }
        if at < limit {
            free.insert(at, limit);
        }
        free.retain(|start, end| start < end);
        Self {
            free,
            reserved: holes,
            limit,
        }
    }

    /// One past the highest IOVA this allocator can hand out.
    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// The total size of the free ranges.
    pub fn free_bytes(&self) -> u64 {
        self.free.iter().map(|(start, end)| end - start).sum()
    }

    /// Whether `iova` lies in a reserved range or the interrupt range.
    pub fn is_reserved(&self, iova: u64) -> bool {
        self.reserved.iter().any(|r| r.contains(&iova))
    }

    /// Allocates a naturally aligned block of at least `len` bytes (see [`block_len`]), the
    /// lowest one that fits.
    ///
    /// # Errors
    ///
    /// [`IovaError::InvalidLength`] for a zero or huge `len`, and [`IovaError::Exhausted`]
    /// when no free range holds the block.
    pub fn alloc(&mut self, len: u64) -> Result<IovaRange, IovaError> {
        let len = block_len(len).ok_or(IovaError::InvalidLength { len })?;
        let align = len.min(MAX_ALIGN);
        let found = self.free.iter().find_map(|(&start, &end)| {
            let at = start.checked_next_multiple_of(align)?;
            (at.checked_add(len)? <= end).then_some((start, end, at))
        });
        let (start, end, at) = found.ok_or(IovaError::Exhausted { len })?;
        self.free.remove(&start);
        if start < at {
            self.free.insert(start, at);
        }
        if at + len < end {
            self.free.insert(at + len, end);
        }
        Ok(IovaRange { start: at, len })
    }

    /// Gives back a block from [`IovaAllocator::alloc`], merging it with free neighbours.
    ///
    /// # Errors
    ///
    /// [`IovaError::NotAllocated`] when `range` cannot have come from this allocator or is
    /// already (partly) free. Nothing changes then.
    pub fn free(&mut self, range: IovaRange) -> Result<(), IovaError> {
        let err = Err(IovaError::NotAllocated { range });
        let IovaRange { start, len } = range;
        if block_len(len) != Some(len) || start % len.min(MAX_ALIGN) != 0 {
            return err;
        }
        let Some(end) = start.checked_add(len) else {
            return err;
        };
        if start < FIRST_IOVA || end > self.limit {
            return err;
        }
        if self.reserved.iter().any(|r| r.start < end && start < r.end) {
            return err;
        }
        let prev = self.free.range(..=start).next_back().map(|(&s, &e)| (s, e));
        let next = self.free.range(start..).next().map(|(&s, &e)| (s, e));
        if prev.is_some_and(|(_, e)| e > start) || next.is_some_and(|(s, _)| s < end) {
            return err;
        }
        let (mut new_start, mut new_end) = (start, end);
        if let Some((s, e)) = prev.filter(|&(_, e)| e == start) {
            self.free.remove(&s);
            new_start = s;
            debug_assert_eq!(e, start);
        }
        if let Some((s, e)) = next.filter(|&(s, _)| s == end) {
            self.free.remove(&s);
            new_end = e;
        }
        self.free.insert(new_start, new_end);
        Ok(())
    }
}

/// Domain IDs for one set of units: `[first, limit)`, where `limit` is the smallest CAP.ND
/// count among them. Freed IDs are reused, most recently freed first.
///
/// The caller must invalidate a domain's context-cache and IOTLB entries on every unit before
/// freeing its ID, since a reused ID would otherwise hit stale entries.
#[derive(Clone, Debug)]
pub struct DomainIds {
    next: u32,
    limit: u32,
    free: Vec<u16>,
}

impl DomainIds {
    /// IDs from `first` up to (not including) `limit`, which is at most 65536.
    pub fn new(first: u16, limit: u32) -> Self {
        Self {
            next: u32::from(first),
            limit: limit.min(1 << 16),
            free: Vec::new(),
        }
    }

    /// A free ID, or `None` when all are in use.
    pub fn alloc(&mut self) -> Option<u16> {
        if let Some(id) = self.free.pop() {
            return Some(id);
        }
        let id = u16::try_from(self.next)
            .ok()
            .filter(|_| self.next < self.limit)?;
        self.next += 1;
        Some(id)
    }

    /// Gives back an ID from [`DomainIds::alloc`]. Returns false, and changes nothing, when
    /// the ID was not in use.
    pub fn free(&mut self, id: u16) -> bool {
        if u32::from(id) >= self.next || self.free.contains(&id) {
            return false;
        }
        self.free.push(id);
        true
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn block_lengths() {
        assert_eq!(block_len(0), None);
        assert_eq!(block_len(1), Some(0x1000));
        assert_eq!(block_len(0x1001), Some(0x2000));
        assert_eq!(block_len(0x3000), Some(0x4000));
        assert_eq!(block_len(MAX_ALIGN), Some(MAX_ALIGN));
        assert_eq!(block_len(MAX_ALIGN + 1), Some(2 * MAX_ALIGN));
        assert_eq!(block_len(5 * MAX_ALIGN), Some(5 * MAX_ALIGN));
        assert_eq!(block_len(u64::MAX), None);
    }

    #[test]
    fn natural_alignment_and_first_fit() {
        let mut a = IovaAllocator::new(1 << 39, []);
        let r4k = a.alloc(0x1000).unwrap();
        assert_eq!(
            r4k,
            IovaRange {
                start: 0x1000,
                len: 0x1000
            }
        );
        let r64k = a.alloc(0x1_0000).unwrap();
        assert_eq!(r64k.start, 0x1_0000);
        // The 8 KiB gap at 0x2000 is used before anything higher.
        let r8k = a.alloc(0x2000).unwrap();
        assert_eq!(r8k.start, 0x2000);
        let r2m = a.alloc(MAX_ALIGN).unwrap();
        assert_eq!(r2m.start, MAX_ALIGN);
        // Above 2 MiB, alignment stays at 2 MiB.
        let r6m = a.alloc(5 << 20).unwrap();
        assert_eq!((r6m.start, r6m.len), (2 * MAX_ALIGN, 3 * MAX_ALIGN));
        for r in [r4k, r64k, r8k, r2m, r6m] {
            assert_eq!(r.start % r.len.min(MAX_ALIGN), 0);
        }
        // Freeing everything merges back to one range.
        let total = a.free_bytes();
        for r in [r8k, r2m, r4k, r6m, r64k] {
            a.free(r).unwrap();
        }
        assert_eq!(
            a.free_bytes(),
            total + 0x1000 + 0x1_0000 + 0x2000 + MAX_ALIGN + (6 << 20)
        );
        assert_eq!(a.free.len(), 2, "below and above the interrupt range");
    }

    #[test]
    fn never_hands_out_interrupt_or_reserved_ranges() {
        // A 4 GiB space with RMRRs at 3 GiB (unaligned) and one crossing the interrupt range.
        let rmrr = (3 << 30) + 0x800..(3 << 30) + 0x2_0001;
        let mut a = IovaAllocator::new(1 << 32, [rmrr, 0xfed0_0000..0xfee0_1000, 0x10..0x10]);
        assert!(a.is_reserved(3 << 30));
        assert!(a.is_reserved((3 << 30) + 0x2_0fff));
        assert!(!a.is_reserved((3 << 30) + 0x2_1000));
        assert!(a.is_reserved(0xfee0_0000) && a.is_reserved(0xfeef_ffff));
        assert!(!a.is_reserved(0xfef0_0000));
        let mut got = std::vec::Vec::new();
        while let Ok(r) = a.alloc(MAX_ALIGN) {
            got.push(r);
        }
        for r in &got {
            for iova in [r.start, r.end() - 1] {
                assert!(!a.is_reserved(iova), "{r:?}");
            }
            assert!(r.end() <= 1 << 32);
            assert!(r.start >= FIRST_IOVA);
        }
        // 2 MiB blocks: [2 MiB, 3 GiB), then the RMRR's block is lost, as are the blocks
        // that hold 0xfed0_0000..0xfef0_0000.
        assert_eq!(got.len(), 2048 - 1 - 1 - 1 - 1);
        // Smaller blocks still fill the gaps.
        let r = a.alloc(0x1000).unwrap();
        assert_eq!(r.start, FIRST_IOVA);
        let near = a.alloc(0x2_0000).unwrap();
        assert!(!a.is_reserved(near.start));
        assert_eq!(a.alloc(4 << 30), Err(IovaError::Exhausted { len: 4 << 30 }));
        assert_eq!(a.alloc(0), Err(IovaError::InvalidLength { len: 0 }));
    }

    #[test]
    fn free_rejects_bad_ranges() {
        let mut a = IovaAllocator::new(1 << 30, Some(0x10_0000..0x20_0000));
        let r = a.alloc(0x4000).unwrap();
        let bad = |start, len| {
            Err(IovaError::NotAllocated {
                range: IovaRange { start, len },
            })
        };
        // Never allocated (free already), double free, misaligned, odd sizes, reserved, out of range.
        assert_eq!(
            a.free(IovaRange {
                start: 0x8000,
                len: 0x4000
            }),
            bad(0x8000, 0x4000)
        );
        a.free(r).unwrap();
        assert_eq!(a.free(r), bad(r.start, r.len));
        assert_eq!(
            a.free(IovaRange {
                start: 0x6000,
                len: 0x4000
            }),
            bad(0x6000, 0x4000)
        );
        assert_eq!(
            a.free(IovaRange {
                start: 0x3000,
                len: 0x3000
            }),
            bad(0x3000, 0x3000)
        );
        assert_eq!(
            a.free(IovaRange {
                start: 0x10_0000,
                len: 0x1000
            }),
            bad(0x10_0000, 0x1000)
        );
        assert_eq!(
            a.free(IovaRange {
                start: 0,
                len: 0x1000
            }),
            bad(0, 0x1000)
        );
        assert_eq!(
            a.free(IovaRange {
                start: 1 << 30,
                len: 0x1000
            }),
            bad(1 << 30, 0x1000)
        );
        assert_eq!(a.limit(), 1 << 30);
    }

    #[test]
    fn small_limits() {
        // A space that ends inside the interrupt range, and one with no room at all.
        let mut a = IovaAllocator::new(0xfee8_0000, []);
        assert_eq!(a.free_bytes(), 0xfee0_0000 - FIRST_IOVA);
        while a.alloc(MAX_ALIGN).is_ok() {}
        assert!(a.alloc(0x1000).is_ok());
        let mut none = IovaAllocator::new(0x1000, []);
        assert_eq!(none.free_bytes(), 0);
        assert_eq!(none.alloc(1), Err(IovaError::Exhausted { len: 0x1000 }));
    }

    #[test]
    fn domain_ids() {
        let mut ids = DomainIds::new(2, 5);
        assert_eq!(ids.alloc(), Some(2));
        assert_eq!(ids.alloc(), Some(3));
        assert_eq!(ids.alloc(), Some(4));
        assert_eq!(ids.alloc(), None);
        assert!(ids.free(3));
        assert!(!ids.free(3));
        assert!(!ids.free(9));
        assert_eq!(ids.alloc(), Some(3));
        let mut all = DomainIds::new(65535, 1 << 20);
        assert_eq!(all.alloc(), Some(65535));
        assert_eq!(all.alloc(), None);
    }
}
