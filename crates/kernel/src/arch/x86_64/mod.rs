pub use crate::arch::x86_shared::*;

pub mod alternative;

/// Boot-time check for the configured x86-64 microarchitecture level
pub mod cpu_level;

pub mod macros;

/// Constants like memory locations
pub mod consts;

/// Ferric Oxide: randomized kernel memory layout
#[cfg(feature = "kaslr")]
pub mod kaslr;

/// Interrupt instructions
pub mod interrupt;

/// Ferric Oxide: static keys for tracepoints (`kernel.trace`)
#[cfg(feature = "trace")]
pub mod static_key;

/// Miscellaneous processor features
pub mod misc;

/// Ferric Oxide: kernel page-table isolation
pub mod kpti;

/// Ferric Oxide: kernel indirect branch tracking (CET IBT)
pub mod ibt;

/// Ferric Oxide: supervisor shadow-stack allocation and enforcement (CET SS)
pub mod shstk;

/// Ferric Oxide: Intel VT-d DMA remapping
#[cfg(iommu_enabled)]
pub mod iommu;

// TODO: Maybe support rewriting relocations (using LD's --emit-relocs) when working with entire
// functions?
#[unsafe(naked)]
pub unsafe extern "C" fn arch_copy_to_user(dst: usize, src: usize, len: usize) -> u8 {
    // TODO: spectre_v1

    core::arch::naked_asm!(
        ".global __usercopy_start
        __usercopy_start:",
        crate::arch::macros::alternative!(
            feature: "smap",
            then: ["
            xor eax, eax
            mov rcx, rdx
            stac
            rep movsb
            clac
            ret
        "],
            default: ["
            xor eax, eax
            mov rcx, rdx
            rep movsb
            ret
        "]
        ),
        ".global __usercopy_end
        __usercopy_end:"
    );
}
pub use arch_copy_to_user as arch_copy_from_user;

pub use alternative::kfx_size;
