//! Ferric Oxide: RAM kept out of the frame allocator for the kernel's life.
//!
//! The crash dump (`kernel.trace_crash`) and the persistent log (`kernel.log_persist`) each
//! keep one at a fixed physical address, so the next boot finds what this one left there
//! after a warm reboot, where firmware leaves RAM as it was. The host-visible trace rings
//! (`kernel.trace_host`) keep one wherever there is room, because the kernel tells the host
//! where it is.
//!
//! `Region::reserve` (with `kernel.trace_crash` or `kernel.log_persist`), during memory
//! setup, takes a fixed region out of the free memory, provided it is wholly free RAM in the
//! boot memory map, and has it mapped like the boot-time identity-mapped areas.
//! `Region::from_boot_allocator` (with `kernel.trace_host`) takes one from the boot-time
//! allocator, before the frame allocator gets the rest. Once the physical map exists,
//! [`crate::reserved::Region::words`] gives its memory.

use core::{fmt, sync::atomic::AtomicU64};

use rmm::{Arch, PhysicalAddress};

use crate::memory::{RmmA, PAGE_SIZE};
#[cfg(any(feature = "trace_crash", feature = "log_persist"))]
use crate::startup::memory::{register_memory_region, BootloaderMemoryKind};

/// A reserved region; only `Region::reserve` and `Region::from_boot_allocator` make one.
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
    #[cfg(any(feature = "trace_crash", feature = "log_persist"))]
    NotFree,
    /// The boot-time allocator has no free run that large.
    #[cfg(feature = "trace_host")]
    NoMemory,
}

impl fmt::Display for ReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReserveError::Unaligned => f.write_str("not page aligned"),
            #[cfg(any(feature = "trace_crash", feature = "log_persist"))]
            ReserveError::NotFree => f.write_str("not free RAM"),
            #[cfg(feature = "trace_host")]
            ReserveError::NoMemory => f.write_str("no free run of RAM that large"),
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
    #[cfg(any(feature = "trace_crash", feature = "log_persist"))]
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

    /// Takes `bytes` from the boot-time allocator, zeroed, wherever it has a free run that
    /// large. Called during memory setup, before the frame allocator takes over the memory
    /// the boot-time allocator has not handed out: what it hands out stays used for good,
    /// and the physical map already covers it. Free areas the allocator passes over to find
    /// the run stay unused too.
    ///
    /// # Errors
    ///
    /// [`ReserveError::Unaligned`] when `bytes` is not a whole number of pages, and
    /// [`ReserveError::NoMemory`] when no free run is that large; nothing is taken then.
    #[cfg(feature = "trace_host")]
    pub fn from_boot_allocator(
        allocator: &mut rmm::BumpAllocator<RmmA>,
        bytes: usize,
    ) -> Result<Self, ReserveError> {
        use rmm::FrameAllocator;

        let pages = bytes
            .checked_div(PAGE_SIZE)
            .filter(|&pages| pages > 0 && bytes.is_multiple_of(PAGE_SIZE))
            .ok_or(ReserveError::Unaligned)?;
        let base = allocator
            .allocate(rmm::FrameCount::new(pages))
            .ok_or(ReserveError::NoMemory)?;
        Ok(Self {
            base: base.data(),
            words: bytes.div_ceil(size_of::<AtomicU64>()),
        })
    }

    /// The region's physical address.
    pub fn base(&self) -> usize {
        self.base
    }

    /// The region's size in bytes.
    #[cfg(any(feature = "trace_crash", feature = "log_persist"))]
    pub fn bytes(&self) -> usize {
        self.words * size_of::<AtomicU64>()
    }

    /// The region's memory, as words. Called once the physical map exists (from `kmain`).
    pub fn words(&self) -> &'static [AtomicU64] {
        let virt = RmmA::phys_to_virt(PhysicalAddress::new(self.base)).data();
        // SAFETY: `reserve` took these bytes at `base` out of the free memory and had them mapped
        // in the physical map, or `from_boot_allocator` took them from the boot-time allocator,
        // which never hands them out again and whose memory the physical map covers. Either way
        // they stay mapped for the kernel's life, and nothing but this region's owner uses them.
        // They are page aligned, so aligned for `AtomicU64`, and any bits are a valid
        // `AtomicU64`. Every access through the slice is atomic, so a write on one CPU while
        // another reads is no data race.
        unsafe { core::slice::from_raw_parts(virt as *const AtomicU64, self.words) }
    }
}
