//! Ferric Oxide: randomized base for the linear map of physical memory (KASLR, part 1).
//!
//! The bootloader maps physical memory at the fixed address [`Arch::PHYS_OFFSET`] (PML4 slot
//! 256). An attacker who knows a physical address then also knows its kernel virtual address,
//! which helps exploits that need to point the kernel at their data. At boot the kernel builds its
//! own page table anyway (`startup::memory::map_memory`), so it places the map at a random
//! 1 GiB-aligned address between the bootloader's slot and the kernel heap instead.
//!
//! The bootloader's slot stays reserved: the boot stack is still mapped there, because the kernel
//! is running on it when it switches page tables.
//!
//! This follows the same idea as Linux's `CONFIG_RANDOMIZE_MEMORY`, written from its public
//! description (Documentation/arch/x86/x86_64/mm.rst), not from its source.

use rmm::Arch;

use crate::arch::{
    consts::{kernel_heap_offset, PML4_SIZE},
    CurrentRmmArch as X8664Arch,
};

/// The map starts on a 1 GiB boundary, so 2 MiB and 1 GiB pages still line up with physical
/// memory.
const ALIGN: usize = 1 << 30;

/// The map is sized for at least this much physical address space, so the kernel's own
/// `phys_to_virt` mappings of ACPI tables and MMIO above the end of RAM stay inside it.
const MIN_SPAN: usize = 1 << 40;

/// Picks where to put the physical memory map, given the highest physical address that has to be
/// reachable through it.
///
/// Returns `None` when the CPU has no hardware random number generator, or when the map would not
/// fit anywhere but its default place. The caller then keeps [`Arch::PHYS_OFFSET`].
pub fn choose_phys_offset(phys_end: usize) -> Option<usize> {
    let span = phys_end.max(MIN_SPAN).checked_next_multiple_of(ALIGN)?;
    // Leave the bootloader's slot alone; see the module documentation.
    let low = X8664Arch::PHYS_OFFSET.checked_add(PML4_SIZE)?;
    let high = kernel_heap_offset().checked_sub(span)?;
    if high <= low {
        return None;
    }
    let slots = slot_count(low, high);
    let random = usize::try_from(ferric_hwrand::u64()?).ok()?;
    low.checked_add(random.checked_rem(slots)?.checked_mul(ALIGN)?)
}

/// Number of `ALIGN`-sized steps from `low` to `high`, both ends included.
fn slot_count(low: usize, high: usize) -> usize {
    (high.saturating_sub(low) / ALIGN).saturating_add(1)
}

/// How many bits of randomness [`choose_phys_offset`] has to choose from for `phys_end`, for the
/// boot log.
pub fn entropy_bits(phys_end: usize) -> u32 {
    let low = X8664Arch::PHYS_OFFSET.checked_add(PML4_SIZE);
    let high = phys_end
        .max(MIN_SPAN)
        .checked_next_multiple_of(ALIGN)
        .and_then(|span| kernel_heap_offset().checked_sub(span));
    match (low, high) {
        (Some(low), Some(high)) if high > low => slot_count(low, high).ilog2(),
        _ => 0,
    }
}
