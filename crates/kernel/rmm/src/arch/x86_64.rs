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
        // The shadow's PCID is the live kernel PCID with bit 11 set (see `user_pcid_of`).
        if USER_PCID_ACTIVE.load(Ordering::Relaxed) {
            let pcid = user_pcid_of(Self::cr3_raw());
            // SAFETY: `set_user_pcid` requires INVPCID and CR4.PCIDE on every CPU that gets here.
            // Callers invalidate addresses they mapped, which page tables only hold canonical.
            unsafe { invpcid(Invpcid::Address(pcid, address)) };
        }
    }

    #[inline(always)]
    fn invalidate_all() {
        // Ferric Oxide: reload CR3 as it is, PCID included, rather than through `set_table`,
        // which loads PCID 0. Without NOFLUSH (bit 63, which reads never return) this flushes
        // the current PCID's non-global entries.
        let cr3 = Self::cr3_raw();
        // SAFETY: CR3 already holds this value, so the same table and PCID stay loaded.
        unsafe { asm!("mov cr3, {0}", in(reg) cr3, options(nostack, preserves_flags)) };
        if USER_PCID_ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: `set_user_pcid` requires INVPCID and CR4.PCIDE on every CPU that gets here.
            unsafe { invpcid(Invpcid::SingleContext(user_pcid_of(cr3))) };
        }
    }

    /// The current top-level table: CR3 without its PCID bits (0 to 11, or PWT and PCD while
    /// CR4.PCIDE is clear) and without the control bits above the physical address.
    #[inline(always)]
    fn table(_table_kind: TableKind) -> PhysicalAddress {
        cr3_table(Self::cr3_raw())
    }

    /// Loads `address` into CR3 under PCID 0 and flushes that PCID, plus, with KPTI and PCID,
    /// the user PCID [`USER_PCID`].
    #[inline(always)]
    unsafe fn set_table(_table_kind: TableKind, address: PhysicalAddress) {
        unsafe { asm!("mov cr3, {0}", in(reg) address.data()) };
        // Ferric Oxide (KPTI with PCID): the CR3 write flushed the kernel's PCID only; the user
        // PCID still holds the previous table's shadow translations.
        if USER_PCID_ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: `set_user_pcid` requires INVPCID and CR4.PCIDE on every CPU that gets here.
            unsafe { invpcid(Invpcid::SingleContext(Pcid::USER)) };
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
pub const USER_PCID: usize = Pcid::USER.get() as usize;

/// Ferric Oxide: set once every CPU runs, or will run before it next invalidates a TLB entry, with
/// CR4.PCIDE and KPTI shadows tagged [`USER_PCID`]. Every invalidation then also reaches that PCID.
static USER_PCID_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The bits of CR3 that hold the top-level table's physical address: 12 to 51, the most a
/// physical address can have. Below them are the PCID (bits 0 to 11, or PWT and PCD while
/// CR4.PCIDE is clear); above them the LAM bits (61, 62) and, on writes only, NOFLUSH (63).
pub const CR3_ADDR_MASK: usize = 0x000F_FFFF_FFFF_F000;

/// The bits of CR3 that hold the PCID while CR4.PCIDE is set.
pub const CR3_PCID_MASK: usize = 0xFFF;

/// Ferric Oxide: the table that a raw CR3 value names, without its PCID and other control bits.
#[inline(always)]
pub const fn cr3_table(cr3: usize) -> PhysicalAddress {
    PhysicalAddress::new(cr3 & CR3_ADDR_MASK)
}

/// Ferric Oxide: the PCID of a raw CR3 value. Meaningful only while CR4.PCIDE is set.
#[inline(always)]
pub const fn cr3_pcid(cr3: usize) -> Pcid {
    Pcid((cr3 & CR3_PCID_MASK) as u16)
}

/// Ferric Oxide (KPTI with PCID): the PCID of the shadow table that pairs with the kernel table
/// loaded in `cr3`. A shadow's PCID is its kernel table's with bit 11 set, the bit the entry and
/// exit code toggle, so this is right for the fixed pair (0 and [`USER_PCID`]) and for any
/// per-address-space pair below `0x800` alike.
#[inline(always)]
pub const fn user_pcid_of(cr3: usize) -> Pcid {
    Pcid((cr3 & (CR3_PCID_MASK & !USER_PCID)) as u16 | Pcid::USER.0)
}

/// Ferric Oxide: a process-context identifier, the 12-bit tag CR3 gives TLB entries while
/// CR4.PCIDE is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pcid(u16);

impl Pcid {
    /// PCID 0: the kernel's tables, and the only PCID while CR4.PCIDE is clear.
    pub const KERNEL: Self = Self(0);
    /// `0x800`: KPTI's shadow tables (see [`USER_PCID`]).
    pub const USER: Self = Self(0x800);
    /// The largest PCID, `0xFFF`.
    pub const MAX: Self = Self(CR3_PCID_MASK as u16);

    /// Returns the PCID `value`, or `None` if it does not fit in 12 bits.
    #[inline(always)]
    pub const fn new(value: u16) -> Option<Self> {
        if value as usize <= CR3_PCID_MASK {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The PCID as a number, below `0x1000`.
    #[inline(always)]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Ferric Oxide: one `INVPCID` request (Intel SDM Vol. 3A §4.10.4.1 and the `INVPCID` entry in
/// Vol. 2A; AMD APM Vol. 3). Each variant is one of the instruction's four types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invpcid {
    /// Type 0: the translation of one linear address under one PCID, global entries excepted,
    /// and the paging-structure caches for that PCID.
    Address(Pcid, VirtualAddress),
    /// Type 1: every non-global translation under one PCID, and its paging-structure caches.
    SingleContext(Pcid),
    /// Type 2: every translation under every PCID, global entries included.
    AllIncludingGlobal,
    /// Type 3: every non-global translation under every PCID.
    AllExceptGlobal,
}

impl Invpcid {
    /// The instruction's type operand and its 16-byte descriptor (PCID, then linear address).
    #[inline(always)]
    fn encode(self) -> (usize, [usize; 2]) {
        match self {
            Self::Address(pcid, address) => (0, [pcid.0 as usize, address.data()]),
            Self::SingleContext(pcid) => (1, [pcid.0 as usize, 0]),
            Self::AllIncludingGlobal => (2, [0; 2]),
            Self::AllExceptGlobal => (3, [0; 2]),
        }
    }
}

/// Ferric Oxide: runs `INVPCID` for `request`.
///
/// # Safety
///
/// The instruction must not fault:
///
/// - the CPU supports `INVPCID` (CPUID.(EAX=7,ECX=0):EBX\[10\]), or it raises #UD;
/// - for [`Invpcid::Address`] and [`Invpcid::SingleContext`] with a PCID other than 0, CR4.PCIDE
///   is set, or it raises #GP;
/// - for [`Invpcid::Address`], the address is canonical, or it raises #GP.
///
/// It runs at CPL 0, as all of the kernel does. Dropping cached translations can never make
/// memory unsafe by itself; these are the only requirements.
#[inline(always)]
pub unsafe fn invpcid(request: Invpcid) {
    let (kind, descriptor) = request.encode();
    // SAFETY: the caller guarantees the instruction exists and its operands are valid; the
    // descriptor is a live 16-byte value on the stack, and `Pcid` keeps bits 12 to 63 of its
    // first word clear, as the instruction requires.
    unsafe {
        asm!(
            "invpcid {0}, [{1}]",
            in(reg) kind,
            in(reg) &descriptor,
            options(nostack, preserves_flags, readonly),
        )
    };
}

/// CR4.PGE: global pages.
const CR4_PGE: usize = 1 << 7;

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

    /// Ferric Oxide: CR3 as it is, PCID bits included. [`Arch::table`] masks them; this is for
    /// code that saves and restores CR3 or needs the current PCID.
    #[inline(always)]
    pub fn cr3_raw() -> usize {
        let cr3: usize;
        // SAFETY: reading CR3 has no side effects; the kernel runs at CPL 0.
        unsafe { asm!("mov {0}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
        cr3
    }

    /// Ferric Oxide: drops the kernel-half page at `address` from this CPU's TLB under *every*
    /// PCID that may cache it, global entries included.
    ///
    /// [`Arch::invalidate`] only reaches the address space that is loaded. A kernel mapping is
    /// shared by every address space, so a change to one (patched text, static keys) must reach
    /// all of them, or a stale translation (a writable alias of text, say) survives under
    /// another PCID.
    ///
    /// With the PCIDs the kernel uses today, 0 for kernel tables and [`USER_PCID`] for every
    /// KPTI shadow, this is `invlpg` (the current PCID, and global entries under all PCIDs) plus
    /// `INVPCID` type 0 on [`USER_PCID`] while [`set_user_pcid`](Self::set_user_pcid) is on, so it
    /// costs the same as [`Arch::invalidate`]. Per-address-space PCIDs must widen it (to
    /// [`Invpcid::AllIncludingGlobal`], or type 0 on every PCID in use) when they arrive.
    ///
    /// A non-canonical `address` can have no translation, so it is ignored (`INVPCID` would
    /// fault on it).
    #[inline(always)]
    pub fn invalidate_kernel_page(address: VirtualAddress) {
        if !Self::virt_is_valid(address) {
            return;
        }
        // SAFETY: `invlpg` never faults; it removes the translation from the current PCID and
        // any global translation for the address from every PCID.
        unsafe { asm!("invlpg [{0}]", in(reg) address.data(), options(nostack, preserves_flags)) };
        if USER_PCID_ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: `set_user_pcid` requires INVPCID and CR4.PCIDE on every CPU that gets here,
            // and `address` is canonical (checked above).
            unsafe { invpcid(Invpcid::Address(Pcid::USER, address)) };
        }
    }

    /// Ferric Oxide: flushes this CPU's whole TLB: every translation under every PCID, global
    /// entries included, and every paging-structure cache.
    ///
    /// A CR3 reload, which is what [`Arch::invalidate_all`] does, keeps global entries and other
    /// PCIDs' entries, so it is not enough after a change to kernel-half mappings that may be
    /// global (KPTI off) or cached under several PCIDs (KPTI with PCID).
    ///
    /// While [`set_user_pcid`](Self::set_user_pcid) is on this is `INVPCID` type 2
    /// ([`Invpcid::AllIncludingGlobal`]). Otherwise, the CPU may lack `INVPCID`, so it changes
    /// CR4.PGE and changes it back, with interrupts off in between: any change to CR4.PGE
    /// invalidates every TLB entry for every PCID, global ones included (Intel SDM Vol. 3A
    /// §4.10.4.1). That works with PGE either on or off; with it off (KPTI), global pages are
    /// on for the few instructions between the two writes, in kernel mode, and the second write
    /// drops whatever the first allowed.
    #[inline]
    pub fn invalidate_kernel_all() {
        if USER_PCID_ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: `set_user_pcid` requires INVPCID on every CPU that gets here; type 2 has no
            // other requirement.
            unsafe { invpcid(Invpcid::AllIncludingGlobal) };
            return;
        }
        // SAFETY: CR4 is written back to the value just read, after one write that differs only
        // in PGE, which affects nothing but TLB caching. RFLAGS (and so IF) is restored, so
        // interrupts are off only around the two writes and nothing on this CPU can change CR4
        // between the read and the writes. The raw value is used rather than `x86`'s `Cr4`, so
        // bits that crate does not know (CET) survive.
        unsafe {
            asm!(
                "pushfq",
                "cli",
                "mov {cr4}, cr4",
                "mov {toggled}, {cr4}",
                "xor {toggled}, {pge}",
                "mov cr4, {toggled}",
                "mov cr4, {cr4}",
                "popfq",
                cr4 = out(reg) _,
                toggled = out(reg) _,
                pge = in(reg) CR4_PGE,
            )
        };
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
    use super::{
        cr3_pcid, cr3_table, user_pcid_of, Invpcid, Pcid, VirtualAddress, X8664Arch, USER_PCID,
    };
    use crate::{Arch, PhysicalAddress};

    #[test]
    fn cr3_table_drops_pcid_and_control_bits() {
        let table = 0x0000_0012_3456_7000;
        assert_eq!(cr3_table(table), PhysicalAddress::new(table));
        // PCID 0x805 (or PWT and PCD without CR4.PCIDE).
        assert_eq!(cr3_table(table | 0x805), PhysicalAddress::new(table));
        assert_eq!(cr3_table(table | 0x18), PhysicalAddress::new(table));
        // LAM (61, 62) and NOFLUSH (63).
        assert_eq!(
            cr3_table(table | 0xE000_0000_0000_0FFF),
            PhysicalAddress::new(table)
        );
        // The KPTI shadow bit is part of the address: the shadow is the next page.
        assert_eq!(
            cr3_table(table | 0x1000 | 0x800),
            PhysicalAddress::new(table | 0x1000)
        );
    }

    #[test]
    fn pcids() {
        assert_eq!(cr3_pcid(0x1234_5000), Pcid::KERNEL);
        assert_eq!(cr3_pcid(0x1234_5803).get(), 0x803);
        assert_eq!(Pcid::new(0xFFF), Some(Pcid::MAX));
        assert_eq!(Pcid::new(0x1000), None);
        assert_eq!(USER_PCID, 0x800);
        // The fixed pair: kernel PCID 0 pairs with the user PCID 0x800.
        assert_eq!(user_pcid_of(0x1234_5000), Pcid::USER);
        // A per-address-space pair: kernel PCID i pairs with 0x800 | i, also from the shadow.
        assert_eq!(user_pcid_of(0x1234_5003).get(), 0x803);
        assert_eq!(user_pcid_of(0x1234_5803).get(), 0x803);
    }

    #[test]
    fn invpcid_descriptors() {
        let pcid = Pcid::new(0x803).unwrap();
        let address = VirtualAddress::new(0xFFFF_8000_0000_1000);
        assert_eq!(
            Invpcid::Address(pcid, address).encode(),
            (0, [0x803, 0xFFFF_8000_0000_1000])
        );
        assert_eq!(Invpcid::SingleContext(pcid).encode(), (1, [0x803, 0]));
        assert_eq!(Invpcid::AllIncludingGlobal.encode(), (2, [0, 0]));
        assert_eq!(Invpcid::AllExceptGlobal.encode(), (3, [0, 0]));
    }

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
