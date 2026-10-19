//! A boot-reserved frame pool excluded from the writable physical-memory map.
//!
//! The kernel reserves this memory before building its page tables and maps allocated frames
//! only through read-only shadow-stack entries (Intel SDM Volume 3A, sections 4.6 and 4.10).
//! Exhaustion fails allocation; frames never fall back to ordinary, physically mapped RAM.
//! This module owns slot bookkeeping, not physical memory or page tables.
//!
//! # Examples
//!
//! ```
//! use ferric_cet::shstk::pool::StackPool;
//! let mut pool = StackPool::new(0x1000).unwrap();
//! let frame = pool.allocate().unwrap();
//! assert_eq!(frame.physical_address(), 0x1000);
//! pool.release(frame).unwrap();
//! ```

use core::{fmt, ops::Range};

/// Physical size of one supervisor shadow stack (two 4 KiB pages).
pub const STACK_BYTES: u64 = 8192;
/// Maximum simultaneous shadow stacks, including per-CPU and boot stacks.
pub const STACK_COUNT: usize = 4096;
/// Bytes reserved at boot when shadow-stack support is compiled in (32 MiB).
pub const POOL_BYTES: u64 = STACK_BYTES * STACK_COUNT as u64;
const WORD_BITS: usize = u64::BITS as usize;

/// Why a pool reservation or release is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PoolError {
    /// The pool base is not aligned to a 4 KiB page.
    Unaligned,
    /// The physical range overflows a 64-bit address.
    Overflow,
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unaligned => "shadow-stack pool base is not page aligned",
            Self::Overflow => "shadow-stack pool exceeds the physical address range",
        })
    }
}

impl core::error::Error for PoolError {}

/// The reserved physical range, which must never enter the writable physical-memory map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolRegion {
    base: u64,
    end: u64,
}

impl PoolRegion {
    /// The complete reserved physical range.
    pub fn range(self) -> Range<u64> {
        self.base..self.end
    }

    /// Selects a window key whose entire physical key block is owned by this reservation.
    ///
    /// Such a key cannot belong to an ordinary kernel-stack allocation, so the kernel can use
    /// its window slot for an isolation probe. Returns `None` if the pool has no complete key
    /// block in the window, or if the window's layout overflows.
    pub fn isolated_key(self, window: super::SlotWindow) -> Option<u64> {
        let key_size = 1u64.checked_shl(window.key_shift)?;
        let key = self.base.checked_next_multiple_of(key_size)?;
        if key.checked_add(key_size)? > self.end {
            return None;
        }
        window.slot_top(key).ok()?;
        Some(key)
    }

    /// Portions of `area` outside the reservation, in address order (possibly empty).
    ///
    /// The kernel uses these exact ranges when choosing ordinary and huge physmap pages, so
    /// no mapping may straddle the reservation. An empty or reversed input produces no pages.
    pub fn exclude(self, area: Range<u64>) -> [Range<u64>; 2] {
        if area.is_empty() {
            return [area.start..area.start, area.start..area.start];
        }
        [
            area.start..area.end.min(self.base).max(area.start),
            area.start.max(self.end).min(area.end)..area.end,
        ]
    }
}

/// An exclusive allocation; it cannot be copied or reconstructed by callers.
#[derive(Debug)]
pub struct StackFrame {
    pool_base: u64,
    slot: usize,
}

impl StackFrame {
    /// First physical byte of this stack's two contiguous pages.
    pub fn physical_address(&self) -> u64 {
        self.pool_base + self.slot as u64 * STACK_BYTES
    }
}

/// A rejected release, preserving the caller's exclusive allocation token.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReleaseError {
    /// The token belongs to another reservation and remains allocated there.
    ForeignAllocation(StackFrame),
}

impl ReleaseError {
    /// Recovers the allocation so it can be returned to its original pool.
    pub fn into_frame(self) -> StackFrame {
        match self {
            Self::ForeignAllocation(frame) => frame,
        }
    }
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("shadow-stack allocation belongs to another pool")
    }
}

impl core::error::Error for ReleaseError {}

/// Fixed-size slot allocator for one exclusively owned boot reservation.
///
/// Callers must reserve the physical range separately and serialize access to this value.
/// Frames must be unmapped and unused before release. Moving an allocation into `release`
/// prevents a stale token from releasing a later allocation of the same slot.
pub struct StackPool {
    region: PoolRegion,
    occupied: [u64; STACK_COUNT / WORD_BITS],
}

impl StackPool {
    /// Creates bookkeeping for the physical reservation beginning at `base`.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::Unaligned`] for a non-page-aligned base or
    /// [`PoolError::Overflow`] if the complete reservation cannot be addressed.
    pub fn new(base: u64) -> Result<Self, PoolError> {
        if !base.is_multiple_of(4096) {
            return Err(PoolError::Unaligned);
        }
        let end = base.checked_add(POOL_BYTES).ok_or(PoolError::Overflow)?;
        Ok(Self {
            region: PoolRegion { base, end },
            occupied: [0; STACK_COUNT / WORD_BITS],
        })
    }

    /// Returns the immutable physical reservation.
    pub fn region(&self) -> PoolRegion {
        self.region
    }

    /// Allocates an exclusive stack, or returns `None` when the reserved pool is exhausted.
    pub fn allocate(&mut self) -> Option<StackFrame> {
        for (index, word) in self.occupied.iter_mut().enumerate() {
            if *word == u64::MAX {
                continue;
            }
            let bit = word.trailing_ones() as usize;
            *word |= 1 << bit;
            return Some(StackFrame {
                pool_base: self.region.base,
                slot: index * WORD_BITS + bit,
            });
        }
        None
    }

    /// Releases an allocation after its mappings and users have been retired.
    ///
    /// # Errors
    ///
    /// Returns [`ReleaseError::ForeignAllocation`] with the token if it came from another
    /// reservation; rejection does not lose ownership of that allocation.
    pub fn release(&mut self, frame: StackFrame) -> Result<(), ReleaseError> {
        if frame.pool_base != self.region.base {
            return Err(ReleaseError::ForeignAllocation(frame));
        }
        if let Some(word) = self.occupied.get_mut(frame.slot / WORD_BITS) {
            *word &= !(1 << (frame.slot % WORD_BITS));
            Ok(())
        } else {
            Err(ReleaseError::ForeignAllocation(frame))
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn isolation_probe_key_must_fit_both_pool_and_window() {
        let limit = 1u64 << 40;
        let window = super::super::SlotWindow {
            base: 0xFFFF_FF80_0000_0000,
            len: 1 << 38,
            key_shift: 16,
            slot_shift: 14,
        };
        let low = StackPool::new(0x1000).unwrap().region();
        assert_eq!(low.isolated_key(window), Some(0x1_0000));
        let last = StackPool::new(limit - 0x1_0000).unwrap().region();
        assert_eq!(last.isolated_key(window), Some(limit - 0x1_0000));
        for base in [limit - 4096, limit, limit + 0x1000_0000] {
            assert_eq!(
                StackPool::new(base).unwrap().region().isolated_key(window),
                None
            );
        }
        assert_eq!(
            low.isolated_key(super::super::SlotWindow {
                key_shift: 64,
                ..window
            }),
            None
        );
        assert_eq!(
            low.isolated_key(super::super::SlotWindow {
                base: u64::MAX - 4095,
                ..window
            }),
            None
        );
    }

    #[test]
    fn exhaustion_never_falls_back_and_release_reuses_only_its_slot() {
        let mut pool = StackPool::new(0x1000).unwrap();
        let mut frames: Vec<_> = (0..STACK_COUNT).map(|_| pool.allocate().unwrap()).collect();
        assert!(pool.allocate().is_none());
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(
                frame.physical_address(),
                0x1000 + index as u64 * STACK_BYTES
            );
            assert!(pool.region().range().contains(&frame.physical_address()));
        }
        let released = frames.swap_remove(65);
        let physical = released.physical_address();
        pool.release(released).unwrap();
        let replacement = pool.allocate().unwrap();
        assert_eq!(replacement.physical_address(), physical);
        assert!(pool.allocate().is_none());
        pool.release(replacement).unwrap();
        for frame in frames {
            pool.release(frame).unwrap();
        }
        assert_eq!(pool.allocate().unwrap().physical_address(), 0x1000);
    }

    #[test]
    fn rejects_invalid_ranges_and_foreign_frames() {
        assert!(matches!(StackPool::new(1), Err(PoolError::Unaligned)));
        assert!(matches!(
            StackPool::new(u64::MAX - 4095),
            Err(PoolError::Overflow)
        ));
        let mut first = StackPool::new(0).unwrap();
        let mut other = StackPool::new(POOL_BYTES).unwrap();
        let error = first.release(other.allocate().unwrap()).unwrap_err();
        other.release(error.into_frame()).unwrap();
        assert_eq!(other.allocate().unwrap().physical_address(), POOL_BYTES);
        assert_eq!(first.allocate().unwrap().physical_address(), 0);
    }

    #[test]
    fn no_physmap_range_can_cross_the_reservation() {
        // Deliberately unaligned to 2 MiB and 1 GiB: huge-page selection must stop at both ends.
        let region = StackPool::new((1 << 30) - 4096).unwrap().region();
        let reserved = region.range();
        for area in [
            0..reserved.start,
            reserved.end..reserved.end + 4096,
            reserved.clone(),
            0..reserved.end + (1 << 30),
            reserved.start + 4096..reserved.end - 4096,
            reserved.start - 4096..reserved.start + 4096,
            reserved.end - 4096..reserved.end + 4096,
        ] {
            let ranges = region.exclude(area.clone());
            for mapped in &ranges {
                assert!(
                    mapped.is_empty()
                        || mapped.end <= reserved.start
                        || mapped.start >= reserved.end
                );
                assert!(mapped.start >= area.start && mapped.end <= area.end);
            }
            let mapped_bytes: u64 = ranges.iter().map(|r| r.end - r.start).sum();
            let overlap = area
                .end
                .min(reserved.end)
                .saturating_sub(area.start.max(reserved.start));
            assert_eq!(mapped_bytes + overlap, area.end - area.start);
        }
    }
}
