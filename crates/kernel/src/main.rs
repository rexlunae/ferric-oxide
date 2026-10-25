//! Ferric Oxide microkernel.
//!
//! Architecture startup initializes memory, CPU state and scheduling before
//! handing control to user-space services through the Redox system-call ABI.

#![feature(int_roundings)]
#![cfg_attr(dtb, feature(iter_next_chunk))]
#![feature(sync_unsafe_cell)]
#![feature(btree_cursors)]
#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![allow(clippy::new_without_default)]

extern crate alloc;

/// Shared data structures
mod common;

mod macros;

/// Ferric Oxide: tracepoints (`trace_event!`) and their per-CPU rings (`kernel.trace`)
mod trace;

/// Architecture-dependent stuff
#[allow(dead_code)] // TODO
mod arch;
use crate::arch::{consts::*, ipi, stop, CurrentRmmArch};
/// Offset of physmap
#[cfg_attr(any(target_arch = "x86", target_arch = "x86_64"), expect(dead_code))]
const PHYS_OFFSET: usize = <arch::CurrentRmmArch as ::rmm::Arch>::PHYS_OFFSET;

/// Heap allocators
mod allocator;

/// ACPI table parsing
mod acpi;

mod dtb;

/// Logical CPU ID and bitset types
mod cpu_set;
use cpu_set::cpu_count;
#[cfg(any(target_arch = "aarch64", test))]
use cpu_set::publish_cpu;

/// Stats for the CPUs
mod cpu_stats;

/// Context management
mod context;

/// Debugger
#[cfg(feature = "debugger")]
mod debugger;

/// Architecture-independent devices
mod devices;

/// Event handling
mod event;

/// Logging
mod log;

/// Ferric Oxide: the persistent kernel log (`kernel.log_persist`)
#[cfg(feature = "log_persist")]
mod log_persist;

/// Ferric Oxide: RAM kept out of use at a fixed address across a warm reboot
#[cfg(any(feature = "trace_crash", feature = "log_persist"))]
mod reserved;

/// Memory management
mod memory;

/// NUMA support
mod numa;

/// Panic
mod panic;

mod percpu;

/// Process tracing
mod ptrace;

/// Performance profiling of the kernel
mod profiling;

/// Schemes, filesystem handlers
mod scheme;

/// Early init
mod startup;

/// Synchronization primitives
mod sync;

/// Syscall handlers
mod syscall;

/// Time
mod time;

#[cfg_attr(not(test), global_allocator)]
static ALLOCATOR: allocator::Allocator = allocator::Allocator;

/// Get the current CPU's scheduling ID
#[inline(always)]
fn cpu_id() -> crate::cpu_set::LogicalCpuId {
    crate::percpu::PercpuBlock::current().cpu_id
}

macro_rules! linker_offsets(
    ($($name:ident),*) => {
        $(
        #[doc = concat!("Returns the address of the linker symbol `", stringify!($name), "`.")]
        #[inline(always)]
        #[allow(non_snake_case)]
        pub fn $name() -> usize {
            #[cfg(not(test))]
            {
                unsafe extern "C" {
                    static $name: u8;
                }
                (&raw const $name) as usize
            }
            #[cfg(test)]
            unreachable!("linker symbols are unavailable in host tests")
        }
        )*
    }
);

/// Addresses supplied by the architecture's kernel linker script.
mod kernel_executable_offsets {
    linker_offsets!(
        KERNEL_OFFSET,
        __text_start,
        __text_end,
        __rodata_start,
        __rodata_end,
        __usercopy_start,
        __usercopy_end
    );

    #[cfg(target_arch = "x86_64")]
    linker_offsets!(__altrelocs_start, __altrelocs_end, KERNEL_LINK_BASE);

    // Ferric Oxide: the tracepoint sites (arch/x86_64/static_key.rs).
    #[cfg(all(target_arch = "x86_64", feature = "trace"))]
    linker_offsets!(__ferric_trace_keys_start, __ferric_trace_keys_end);
}

/// Ferric Oxide: how far above its link address the bootloader loaded the kernel (kernel image
/// KASLR), or 0 when it did not move it.
pub fn kernel_image_slide() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        // KERNEL_OFFSET moves with the image; KERNEL_LINK_BASE is an absolute symbol.
        kernel_executable_offsets::KERNEL_OFFSET() - kernel_executable_offsets::KERNEL_LINK_BASE()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}
