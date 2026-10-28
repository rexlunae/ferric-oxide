use core::{
    arch::asm,
    num::NonZeroU8,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{Arch, PhysicalAddress, TableKind, VirtualAddress};

#[derive(Clone, Copy, Debug)]
pub struct X8664Arch;

impl Arch for X8664Arch {
    const KERNEL_SEPARATE_TABLE: bool = false;

    const PAGE_SHIFT: usize = 12; // 4096 bytes
    const PAGE_ENTRY_SHIFT: usize = 9; // 512 entries, 8 bytes each
    const PAGE_LEVELS: usize = 4; // PML4, PDP, PD, PT

    const ENTRY_ADDRESS_WIDTH: usize = 40;
    const ENTRY_FLAG_DEFAULT_PAGE: usize = Self::ENTRY_FLAG_PRESENT;
    const ENTRY_FLAG_DEFAULT_TABLE: usize = Self::ENTRY_FLAG_PRESENT | Self::ENTRY_FLAG_READWRITE;
    const ENTRY_FLAG_PRESENT: usize = 1 << 0;
    const ENTRY_FLAG_READONLY: usize = 0;
    const ENTRY_FLAG_READWRITE: usize = 1 << 1;
    const ENTRY_FLAG_PAGE_USER: usize = 1 << 2;
    // Not used: const ENTRY_FLAG_HUGE: usize = 1 << 7;
    const ENTRY_FLAG_GLOBAL: usize = 1 << 8;
    const ENTRY_FLAG_NO_GLOBAL: usize = 0;
    const ENTRY_FLAG_NO_EXEC: usize = 1 << 63;
    const ENTRY_FLAG_EXEC: usize = 0;
    const ENTRY_FLAG_DEVICE_MEMORY: usize = PAT_UC_;
    const ENTRY_FLAG_UNCACHEABLE: usize = PAT_UC_;
    const ENTRY_FLAG_WRITE_COMBINING: usize = PAT_WC;

    fn entry_flag_large(level: NonZeroU8) -> Option<usize> {
        match level.get() {
            1 => Some(1 << 7),
            // TODO: should make it possible to be conditional on CPU features in this struct
            //2 => Some(1 << 7),

            // PDPs, PML4s (and PML5s) can only be mapped to paging structures
            _ => None,
        }
    }

    const PHYS_OFFSET: usize = Self::PAGE_NEGATIVE_MASK + (Self::PAGE_ADDRESS_SIZE >> 1) as usize; // PML4 slot 256 and onwards

    #[inline(always)]
    fn phys_offset() -> usize {
        PHYS_OFFSET_CURRENT.load(Ordering::Relaxed)
    }

    #[inline(always)]
    fn invalidate(address: VirtualAddress) {
        unsafe { asm!("invlpg [{0}]", in(reg) address.data()) };
        // Ferric Oxide (KPTI with PCID): `invlpg` only reaches the current PCID, the kernel's.
        if USER_PCID_ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: `set_user_pcid` requires CR4.PCIDE on every CPU that gets here.
            unsafe { invpcid(INVPCID_ADDRESS, USER_PCID, address.data()) };
        }
    }

    #[inline(always)]
    fn invalidate_all() {
        unsafe { Self::set_table(TableKind::User, Self::table(TableKind::User)) };
    }

    /// Uses `INVPCID` type 2 when [`X8664Arch::set_invpcid_available`] says it exists, and
    /// otherwise toggles CR4.PGE.
    #[inline(always)]
    fn invalidate_all_global() {
        if INVPCID_AVAILABLE.load(Ordering::Relaxed) {
            // SAFETY: `set_invpcid_available` guarantees the instruction; type 2 ignores the
            // descriptor's PCID and is valid whether or not CR4.PCIDE is set.
            unsafe { invpcid(INVPCID_ALL_INCLUDING_GLOBAL, 0, 0) };
        } else if !super::x86_shared::toggle_global_pages() {
            Self::invalidate_all();
        }
    }

    #[inline(always)]
    fn table(_table_kind: TableKind) -> PhysicalAddress {
        let address: usize;
        unsafe { asm!("mov {0}, cr3", out(reg) address) };
        PhysicalAddress::new(address)
    }

    #[inline(always)]
    unsafe fn set_table(_table_kind: TableKind, address: PhysicalAddress) {
        unsafe { asm!("mov cr3, {0}", in(reg) address.data()) };
        // Ferric Oxide (KPTI with PCID): the CR3 write flushed the kernel's PCID only; the user
        // PCID still holds the previous table's shadow translations.
        if USER_PCID_ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: `set_user_pcid` requires CR4.PCIDE on every CPU that gets here.
            unsafe { invpcid(INVPCID_SINGLE_CONTEXT, USER_PCID, 0) };
        }
    }

    fn virt_is_valid(address: VirtualAddress) -> bool {
        // On x86_64, an address is valid if and only if it is canonical. It may still point to
        // unmapped memory, but will always be valid once translated via the page table has
        // suceeded.
        let masked = address.data() & 0xFFFF_8000_0000_0000;
        // TODO: 5-level paging
        masked == 0xFFFF_8000_0000_0000 || masked == 0
    }
}

/// Ferric Oxide: the current base of the physical memory map, which the kernel may move once at
/// boot to randomize it (KASLR). Starts at [`Arch::PHYS_OFFSET`], where the bootloader puts it.
static PHYS_OFFSET_CURRENT: AtomicUsize = AtomicUsize::new(X8664Arch::PHYS_OFFSET);

impl X8664Arch {
    /// Moves the physical memory map to `offset`.
    ///
    /// # Safety
    ///
    /// The active page table must map all physical memory that will be reached through
    /// [`Arch::phys_to_virt`] at `offset`, and no CPU may be using addresses derived from the old
    /// offset that are not also still mapped.
    pub unsafe fn set_phys_offset(offset: usize) {
        PHYS_OFFSET_CURRENT.store(offset, Ordering::Relaxed);
    }
}

/// Ferric Oxide (KPTI with PCID): the PCID of every KPTI shadow table, the one loaded while user
/// space runs. Kernel tables keep PCID 0, so CR3 reads and writes in the kernel are unchanged.
pub const USER_PCID: usize = 0x800;

/// Ferric Oxide: set once every CPU runs, or will run before it next invalidates a TLB entry, with
/// CR4.PCIDE and KPTI shadows tagged [`USER_PCID`]. Every invalidation then also reaches that PCID.
static USER_PCID_ACTIVE: AtomicBool = AtomicBool::new(false);

/// `INVPCID` type 0: one linear address in one PCID.
const INVPCID_ADDRESS: usize = 0;
/// `INVPCID` type 1: every non-global translation in one PCID.
const INVPCID_SINGLE_CONTEXT: usize = 1;
/// `INVPCID` type 2: every translation in every PCID, global ones included.
const INVPCID_ALL_INCLUDING_GLOBAL: usize = 2;

/// Ferric Oxide: whether every CPU supports `INVPCID`, for [`Arch::invalidate_all_global`].
static INVPCID_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Invalidates TLB and paging-structure cache entries for `pcid` with `INVPCID`.
///
/// # Safety
///
/// The CPU must support `INVPCID` and, for types 0 and 1 with a `pcid` other than 0, have
/// CR4.PCIDE set.
#[inline(always)]
unsafe fn invpcid(kind: usize, pcid: usize, address: usize) {
    let descriptor: [usize; 2] = [pcid, address];
    // SAFETY: the caller guarantees the instruction exists and PCIDs are on; the descriptor is a
    // live 16-byte value on the stack.
    unsafe {
        asm!(
            "invpcid {0}, [{1}]",
            in(reg) kind,
            in(reg) &descriptor,
            options(nostack),
        )
    };
}

impl X8664Arch {
    /// Ferric Oxide (KPTI with PCID): makes every TLB invalidation also invalidate
    /// [`USER_PCID`], with `INVPCID`, or stops doing so.
    ///
    /// # Safety
    ///
    /// When `active`, the CPU must support `INVPCID` and every CPU that invalidates TLB entries
    /// from now on must have CR4.PCIDE set before it does.
    pub unsafe fn set_user_pcid(active: bool) {
        USER_PCID_ACTIVE.store(active, Ordering::Relaxed);
    }

    /// Ferric Oxide: lets [`Arch::invalidate_all_global`] use `INVPCID` instead of toggling
    /// CR4.PGE.
    ///
    /// # Safety
    ///
    /// When `available`, every CPU that calls `invalidate_all_global` from now on must support
    /// `INVPCID`, or must itself call this with `false` before its first call. A CPU's own store
    /// is visible to its later loads, so withdrawing needs no ordering with other CPUs.
    pub unsafe fn set_invpcid_available(available: bool) {
        INVPCID_AVAILABLE.store(available, Ordering::Relaxed);
    }
}

pub use super::x86_shared::*;

const _: () = {
    assert!(X8664Arch::PAGE_SIZE == 4096);
    assert!(X8664Arch::PAGE_OFFSET_MASK == 0xFFF);
    assert!(X8664Arch::PAGE_ADDRESS_SHIFT == 48);
    assert!(X8664Arch::PAGE_ADDRESS_SIZE == 0x0001_0000_0000_0000);
    assert!(X8664Arch::PAGE_ADDRESS_MASK == 0x0000_FFFF_FFFF_F000);
    assert!(X8664Arch::PAGE_ENTRY_SIZE == 8);
    assert!(X8664Arch::PAGE_ENTRIES == 512);
    assert!(X8664Arch::PAGE_ENTRY_MASK == 0x1FF);
    assert!(X8664Arch::PAGE_NEGATIVE_MASK == 0xFFFF_0000_0000_0000);

    assert!(X8664Arch::ENTRY_ADDRESS_SIZE == 0x0000_0100_0000_0000);
    assert!(X8664Arch::ENTRY_ADDRESS_MASK == 0x0000_00FF_FFFF_FFFF);
    assert!(X8664Arch::ENTRY_FLAGS_MASK == 0xFFF0_0000_0000_0FFF);

    assert!(X8664Arch::PHYS_OFFSET == 0xFFFF_8000_0000_0000);
};

#[cfg(test)]
mod tests {
    use super::{VirtualAddress, X8664Arch};
    use crate::Arch;

    #[test]
    fn is_canonical() {
        fn yes(address: usize) {
            assert!(X8664Arch::virt_is_valid(VirtualAddress::new(address)));
        }
        fn no(address: usize) {
            assert!(!X8664Arch::virt_is_valid(VirtualAddress::new(address)));
        }

        yes(0xFFFF_8000_1337_1337);
        yes(0xFFFF_FFFF_FFFF_FFFF);
        yes(0x0000_0000_0000_0042);
        yes(0x0000_7FFF_FFFF_FFFF);
        no(0x1337_0000_0000_0000);
        no(0x1337_8000_0000_0000);
        no(0x0000_8000_0000_0000);
    }
}
