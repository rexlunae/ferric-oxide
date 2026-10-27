//! AArch64 MMIO accessors.
//!
//! A hypervisor that traps a device access (KVM, Apple's Hypervisor.framework under QEMU) emulates
//! it from the exception syndrome, which only describes single loads and stores with a plain base
//! register (`ESR_EL2.ISV`). `read_volatile`/`write_volatile` leave the addressing mode to LLVM,
//! which may emit pre/post-index writeback, `ldp`/`stp` or SIMD accesses that carry no syndrome,
//! and QEMU's HVF backend aborts on those. Each access here is one `ldr`/`str` of the register's
//! width with a `[xN]` operand, as Linux does in its arm64 `__raw_read*`/`__raw_write*`.

use core::arch::asm;

/// An integer that one AArch64 load or store with a syndrome-carrying encoding can transfer.
pub trait MmioWord: Copy {
    /// Reads `Self` from `ptr` with a single `ldr` of `Self`'s width.
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for a read of `Self` and aligned to `size_of::<Self>()`.
    unsafe fn mmio_read(ptr: *const Self) -> Self;

    /// Writes `value` to `ptr` with a single `str` of `Self`'s width.
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for a write of `Self` and aligned to `size_of::<Self>()`.
    unsafe fn mmio_write(ptr: *mut Self, value: Self);
}

macro_rules! mmio_word {
    ($ty:ty, $load:literal, $store:literal) => {
        impl MmioWord for $ty {
            #[inline(always)]
            unsafe fn mmio_read(ptr: *const Self) -> Self {
                let value: $ty;
                // SAFETY: the caller guarantees `ptr` is valid and aligned for this read.
                unsafe {
                    asm!($load, out = out(reg) value, ptr = in(reg) ptr,
                        options(nostack, preserves_flags, readonly));
                }
                value
            }

            #[inline(always)]
            unsafe fn mmio_write(ptr: *mut Self, value: Self) {
                // SAFETY: the caller guarantees `ptr` is valid and aligned for this write.
                unsafe {
                    asm!($store, value = in(reg) value, ptr = in(reg) ptr,
                        options(nostack, preserves_flags));
                }
            }
        }
    };
}

mmio_word!(u8, "ldrb {out:w}, [{ptr}]", "strb {value:w}, [{ptr}]");
mmio_word!(u16, "ldrh {out:w}, [{ptr}]", "strh {value:w}, [{ptr}]");
mmio_word!(u32, "ldr {out:w}, [{ptr}]", "str {value:w}, [{ptr}]");
mmio_word!(u64, "ldr {out:x}, [{ptr}]", "str {value:x}, [{ptr}]");
