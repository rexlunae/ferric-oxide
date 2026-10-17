//! Ferric Oxide: kernel image KASLR.
//!
//! With the `kaslr` feature, the bootloader maps the kernel at a random
//! page-aligned offset inside the 1 GiB window it reserves for it (starting at
//! the link address, -2 GiB), instead of at the start of that window. The
//! kernel is not position independent, so before it runs, the bootloader
//! replays the relocations the kernel was linked with (`--emit-relocs`) for
//! that offset with `ferric-elfreloc`. A kernel without relocations, or a CPU
//! without RDRAND, boots at the link address as upstream does.

use core::sync::atomic::{AtomicUsize, Ordering};

/// How far above its link address the kernel is mapped. The x86_64 page table
/// setup reads it; it stays 0 unless `randomize` moved the kernel. A slide is
/// under 1 GiB, so it fits a `usize` on the 32-bit BIOS build too, which has
/// no 64-bit atomics.
static SLIDE: AtomicUsize = AtomicUsize::new(0);

/// How far above its link address the kernel is mapped.
pub fn slide() -> u64 {
    SLIDE.load(Ordering::Relaxed) as u64
}

/// Picks a random slide for a 64-bit kernel image, relocates the image for it
/// and returns it, or returns 0 and leaves the image alone when it cannot.
#[cfg(feature = "kaslr")]
pub fn randomize(kernel: &mut [u8], kernel_64bit: bool) -> u64 {
    /// The window the x86_64 page tables reserve for the kernel: one page
    /// directory.
    const WINDOW: u64 = 1 << 30;
    const PAGE_SIZE: u64 = 4096;

    if !kernel_64bit {
        return 0;
    }
    let size = (kernel.len() as u64).next_multiple_of(PAGE_SIZE);
    let Some(room) = WINDOW.checked_sub(size).map(|room| room / PAGE_SIZE) else {
        return 0;
    };
    let Some(random) = ferric_hwrand::u64() else {
        log::warn!("KASLR: no RDRAND; kernel image not moved");
        return 0;
    };
    // Never pick slide 0: a randomized boot must actually move the image, so
    // the kernel (and CI) can tell it was randomized. 0 stays the fallback.
    if room == 0 {
        return 0;
    }
    let slide = (random % room + 1) * PAGE_SIZE;
    match ferric_elfreloc::relocate(kernel, slide) {
        Ok(_) => {
            log::info!(
                "KASLR: kernel image moved ({} bits)",
                u64::BITS - 1 - room.leading_zeros()
            );
            SLIDE.store(slide as usize, Ordering::Relaxed);
            slide
        }
        Err(err) => {
            log::warn!("KASLR: kernel image not moved: {err}");
            0
        }
    }
}
