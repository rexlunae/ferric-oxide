//! Volatile access to memory the CPU shares with an IOMMU: a unit's registers, and the tables,
//! rings and status words the IOMMU reads or writes behind the compiler's back.
//!
//! Both backends (VT-d and AMD-Vi) go through [`Volatile`], so the module holds the only
//! volatile loads and stores of the IOMMU code. A [`Volatile`] is built only from a span that
//! stays mapped for the life of the kernel, and every access is checked against that span and
//! the word's alignment, so the accessors are safe.

use core::{
    mem::{align_of, size_of},
    ptr::{read_volatile, write_volatile},
};

use crate::memory::{PhysicalAddress, RmmA, RmmArch};

mod sealed {
    /// Keeps [`super::Word`] to the integer types below.
    pub trait Sealed {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// A register or memory word width: `u32` or `u64`. Every bit pattern is a valid value of
/// both, which is what makes reading one from device-written memory sound.
pub(super) trait Word: Copy + Default + sealed::Sealed {}
impl Word for u32 {}
impl Word for u64 {}

/// `len` bytes of shared memory at kernel virtual address `virt`, mapped for the life of the
/// kernel.
#[derive(Clone, Copy, Debug)]
pub(super) struct Volatile {
    virt: usize,
    len: usize,
}

impl Volatile {
    /// `len` bytes of an IOMMU's register set at `phys`, mapped as device memory.
    pub(super) fn registers(phys: PhysicalAddress, len: usize) -> Self {
        Self {
            virt: super::map_registers(phys, len),
            len,
        }
    }

    /// Widens a register span to `len` bytes, after the pages past the first were mapped
    /// right after it with [`super::map_registers`].
    pub(super) fn with_len(self, len: usize) -> Self {
        Self { len, ..self }
    }

    /// `len` bytes of frames at `phys` that the IOMMU code allocated and never frees, reached
    /// through the physical memory map.
    pub(super) fn frames(phys: u64, len: usize) -> Self {
        Self {
            virt: RmmA::phys_to_virt(PhysicalAddress::new(phys as usize)).data(),
            len,
        }
    }

    /// The address of the `T` at `offset`, if it lies inside the span and is aligned.
    fn at<T: Word>(self, offset: usize) -> Option<usize> {
        let end = offset.checked_add(size_of::<T>())?;
        let addr = self.virt.checked_add(offset)?;
        (end <= self.len && addr.is_multiple_of(align_of::<T>())).then_some(addr)
    }

    /// Reads the word at `offset`, or 0 when it lies outside the span or is misaligned.
    pub(super) fn read<T: Word>(self, offset: usize) -> T {
        self.at::<T>(offset).map_or_else(T::default, |addr| {
            // SAFETY: `at` checked that `addr` is an aligned `T` inside the span, which stays
            // mapped for the life of the kernel (see the constructors); `T` accepts any bits.
            unsafe { read_volatile(addr as *const T) }
        })
    }

    /// Writes the word at `offset`; nothing when it lies outside the span or is misaligned.
    pub(super) fn write<T: Word>(self, offset: usize, value: T) {
        if let Some(addr) = self.at::<T>(offset) {
            // SAFETY: as in `read`. No Rust reference to shared IOMMU memory is live while it
            // is accessed through here, and callers serialize writes with the unit's lock.
            unsafe { write_volatile(addr as *mut T, value) }
        }
    }
}
