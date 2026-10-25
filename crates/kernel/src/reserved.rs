//! Ferric Oxide: RAM kept out of the frame allocator at a fixed physical address, so the next
//! boot finds what this one left there after a warm reboot, where firmware leaves RAM as it
//! was. The crash dump (`kernel.trace_crash`) and the persistent log (`kernel.log_persist`)
//! each keep one.
//!
//! [`crate::reserved::Region::reserve`], during memory setup, takes the region out of the free memory, provided
//! it is wholly free RAM in the boot memory map, and has it mapped like the boot-time
//! identity-mapped areas. Once the physical map exists, [`crate::reserved::Region::words`] gives its memory.

use core::{fmt, sync::atomic::AtomicU64};

use rmm::{Arch, PhysicalAddress};

use crate::{
    memory::{RmmA, PAGE_SIZE},
    startup::memory::{register_memory_region, BootloaderMemoryKind},
};

/// A reserved region; only [`crate::reserved::Region::reserve`] makes one.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    base: usize,
    words: usize,
}

/// Why a region was not reserved.
#[derive(Clone, Copy, Debug)]
pub enum ReserveError {
    /// The base or the size is not a whole number of pages.
    Unaligned,
    /// Some of the region is not free RAM in the boot memory map, or another area overlaps it.
    NotFree,
}

impl fmt::Display for ReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReserveError::Unaligned => f.write_str("not page aligned"),
            ReserveError::NotFree => f.write_str("not free RAM"),
        }
    }
}

impl Region {
    /// Takes `bytes` at physical address `base` out of the free memory, if all of it is free
    /// RAM in the boot memory map that no other area overlaps, as `is_free(start, end)` says
    /// ([`ferric_trace::crash::region_is_free`]). Called during memory setup, after the
    /// bootloader's areas are registered and before the free areas are built from them.
    ///
    /// # Errors
    ///
    /// [`ReserveError`] when the region is not page aligned or not free; nothing is reserved.
    pub fn reserve(
        base: usize,
        bytes: usize,
        is_free: impl Fn(usize, usize) -> bool,
    ) -> Result<Self, ReserveError> {
        if !base.is_multiple_of(PAGE_SIZE) || !bytes.is_multiple_of(PAGE_SIZE) || bytes == 0 {
            return Err(ReserveError::Unaligned);
        }
        if !is_free(base, base.saturating_add(bytes)) {
            return Err(ReserveError::NotFree);
        }
        // Identity-mapped areas are kept out of the frame allocator and mapped in the physical
        // map.
        register_memory_region(base, bytes, BootloaderMemoryKind::IdentityMap);
        Ok(Self {
            base,
            words: bytes / size_of::<AtomicU64>(),
        })
    }

    /// The region's physical address.
    pub fn base(&self) -> usize {
        self.base
    }

    /// The region's size in bytes.
    pub fn bytes(&self) -> usize {
        self.words * size_of::<AtomicU64>()
    }

    /// The region's memory, as words. Called once the physical map exists (from `kmain`).
    pub fn words(&self) -> &'static [AtomicU64] {
        let virt = RmmA::phys_to_virt(PhysicalAddress::new(self.base)).data();
        // SAFETY: `reserve` took these bytes at `base` out of the free memory and had them mapped
        // in the physical map, where they stay for the kernel's life, and nothing but this
        // region's owner uses them. They are page aligned, so aligned for `AtomicU64`, and any
        // bits are a valid `AtomicU64`. Every access through the slice is atomic, so a write on
        // one CPU while another reads is no data race.
        unsafe { core::slice::from_raw_parts(virt as *const AtomicU64, self.words) }
    }
}
