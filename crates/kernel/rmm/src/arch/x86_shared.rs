#![expect(clippy::identity_op)]

// Page attribute table is indexed by PAT(7) PCD(4) PWT(3)
pub(crate) const _PAT_WB: usize = (0b0 << 7) + (0b00 << 3);
pub(crate) const _PAT_WT: usize = (0b0 << 7) + (0b01 << 3);
pub(crate) const PAT_UC_: usize = (0b0 << 7) + (0b10 << 3); // UC-
pub(crate) const _PAT_UC: usize = (0b0 << 7) + (0b11 << 3); // UC
pub(crate) const PAT_WC: usize = (0b1 << 7) + (0b00 << 3);

/// Setup page attribute table
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
pub unsafe fn init_pat() {
    unsafe {
        let uncacheable = 0; // UC
        let write_combining = 1; // WC
        let write_through = 4; // WT
        let _write_protected = 5; // WP
        let write_back = 6; // WB
        let uncached = 7; // UC- (overridable by WC MTRR)

        let pat0 = write_back;
        let pat1 = write_through;
        let pat2 = uncached;
        let pat3 = uncacheable;

        let pat4 = write_combining;
        let pat5 = pat1;
        let pat6 = pat2;
        let pat7 = pat3;

        let msr = 631; // IA32_PAT
        let low = u32::from_be_bytes([pat3, pat2, pat1, pat0]);
        let high = u32::from_be_bytes([pat7, pat6, pat5, pat4]);
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high);
    }
}

/// CR4.PGE: global pages enabled.
const CR4_PGE: usize = 1 << 7;

/// Ferric Oxide: flushes every TLB and paging-structure-cache entry, global ones and those of
/// every PCID included, by clearing and restoring CR4.PGE (Intel SDM vol. 3A, 4.10.4.1). Returns
/// `false`, doing nothing, when PGE is off: then no entry is global.
///
/// Built on every 64-bit target like the rest of `x86_64.rs`, but only emitted where used.
#[inline(always)]
pub(crate) fn toggle_global_pages() -> bool {
    use core::arch::asm;

    let cr4: usize;
    // SAFETY: reading CR4 has no side effects.
    unsafe { asm!("mov {0}, cr4", out(reg) cr4, options(nostack, preserves_flags)) };
    if cr4 & CR4_PGE == 0 {
        return false;
    }
    // SAFETY: only PGE changes, and only until the second write; every other bit (CET, SMEP,
    // PCIDE and the rest) is written back as read, so nothing but the TLB is affected. The raw
    // `mov` keeps bits that `x86::controlregs::cr4` would drop, such as CR4.CET.
    unsafe {
        asm!(
            "mov cr4, {0}",
            "mov cr4, {1}",
            in(reg) cr4 & !CR4_PGE,
            in(reg) cr4,
            options(nostack, preserves_flags),
        )
    };
    true
}
