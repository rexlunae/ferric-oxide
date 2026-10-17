//! Translation structures (chapter 9) in legacy mode.
//!
//! A unit's root table pointer (RTADDR) names a 4 KiB root table of 256 [`RootEntry`] values,
//! one per bus. Each present root entry points to a 4 KiB context table of 256
//! [`ContextEntry`] values, one per device and function. A context entry gives the device's
//! domain ID, its translation type and the root of its second-stage page tables.
//!
//! Second-stage tables have the same shape as x86-64 page tables: 512 8-byte entries per
//! 4 KiB table, 9 bits of IOVA per level, and 3, 4 or 5 levels depending on the adjusted guest
//! address width ([`Agaw`]). Entries are built with [`sl_table`] and [`sl_page`]; [`map`]
//! installs a mapping in tables held by a [`TableMemory`], and [`translate`] walks them.
//!
//! Scalable mode (PASID tables, first-stage tables) is not used.

use core::fmt;

/// Entries in one 4 KiB table.
pub const ENTRIES: usize = 512;

/// One 4 KiB table of root, context (two words per entry) or page-table entries.
pub type Table = [u64; ENTRIES];

/// The low bit of root, context and page-table entries that marks them present (for page
/// tables: readable).
const PRESENT: u64 = 1;

/// Bits 63:12 of a table entry: a 4 KiB-aligned physical address.
pub const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// A legacy-mode root entry (section 9.1): bits 63:12 of the low word point to the bus's
/// context table, bit 0 marks it present. The high word is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootEntry(pub [u64; 2]);

impl RootEntry {
    /// A present entry pointing to the context table at `context_table`.
    pub const fn new(context_table: u64) -> Self {
        Self([(context_table & ADDR_MASK) | PRESENT, 0])
    }
}

/// The translation type field of a legacy context entry (section 9.3, bits 3:2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TranslationType {
    /// 00: untranslated requests go through the second-stage tables; translated and
    /// translation requests (ATS) are blocked.
    Untranslated = 0,
    /// 01: as `Untranslated`, and ATS requests are allowed.
    All = 1,
    /// 10: untranslated requests pass through unchanged (needs ECAP.PT).
    PassThrough = 2,
}

/// A legacy-mode context entry (section 9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextEntry(pub [u64; 2]);

impl ContextEntry {
    /// A present entry for domain `did`, translating through the second-stage tables at
    /// `table` with address width `agaw`. For [`TranslationType::PassThrough`] the table
    /// pointer is ignored by hardware but the address width must still be one the unit
    /// supports.
    pub const fn new(table: u64, agaw: Agaw, did: u16, tt: TranslationType) -> Self {
        let lo = (table & ADDR_MASK) | ((tt as u64) << 2) | PRESENT;
        let hi = agaw.aw_field() | ((did as u64) << 8);
        Self([lo, hi])
    }

    /// The domain ID, bits 87:72.
    pub const fn domain_id(self) -> u16 {
        (self.0[1] >> 8) as u16
    }
}

/// An adjusted guest address width: how many IOVA bits the second-stage tables translate,
/// which fixes the number of table levels (section 3.6, CAP.SAGAW).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Agaw {
    /// 39 bits, 3-level tables.
    Bits39,
    /// 48 bits, 4-level tables.
    Bits48,
    /// 57 bits, 5-level tables.
    Bits57,
}

impl Agaw {
    /// Every width, smallest first.
    pub const ALL: [Agaw; 3] = [Agaw::Bits39, Agaw::Bits48, Agaw::Bits57];

    /// The address width in bits.
    pub const fn bits(self) -> u8 {
        match self {
            Self::Bits39 => 39,
            Self::Bits48 => 48,
            Self::Bits57 => 57,
        }
    }

    /// The number of table levels.
    pub const fn levels(self) -> u8 {
        match self {
            Self::Bits39 => 3,
            Self::Bits48 => 4,
            Self::Bits57 => 5,
        }
    }

    /// The context entry's AW field, which is also the bit position in CAP.SAGAW.
    pub const fn aw_field(self) -> u64 {
        match self {
            Self::Bits39 => 1,
            Self::Bits48 => 2,
            Self::Bits57 => 3,
        }
    }

    /// Whether a unit with this SAGAW field supports the width.
    pub const fn supported_by(self, sagaw: u8) -> bool {
        sagaw & (1 << self.aw_field()) != 0
    }

    /// The smallest width in `sagaw` that covers `needed_bits` of IOVA.
    pub fn smallest_covering(sagaw: u8, needed_bits: u8) -> Option<Agaw> {
        Self::ALL
            .into_iter()
            .find(|a| a.supported_by(sagaw) && a.bits() >= needed_bits)
    }

    /// One past the largest IOVA the tables can translate.
    pub const fn limit(self) -> u64 {
        1 << self.bits()
    }
}

/// The size of a second-stage leaf mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageSize {
    /// 4 KiB, a level-1 entry.
    Size4K,
    /// 2 MiB, a level-2 entry with PS set (needs CAP.SLLPS bit 0).
    Size2M,
    /// 1 GiB, a level-3 entry with PS set (needs CAP.SLLPS bit 1).
    Size1G,
}

impl PageSize {
    /// Size in bytes.
    pub const fn bytes(self) -> u64 {
        1 << self.shift()
    }
    const fn shift(self) -> u32 {
        match self {
            Self::Size4K => 12,
            Self::Size2M => 21,
            Self::Size1G => 30,
        }
    }
    const fn level(self) -> u8 {
        match self {
            Self::Size4K => 1,
            Self::Size2M => 2,
            Self::Size1G => 3,
        }
    }
}

/// Access a second-stage leaf entry grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// DMA reads only.
    Read,
    /// DMA writes only.
    Write,
    /// Reads and writes.
    ReadWrite,
}

impl Access {
    const fn bits(self) -> u64 {
        match self {
            Self::Read => SL_R,
            Self::Write => SL_W,
            Self::ReadWrite => SL_R | SL_W,
        }
    }
}

/// Second-stage entry bit 0: read allowed (and, for non-leaf entries, present).
pub const SL_R: u64 = 1 << 0;
/// Second-stage entry bit 1: write allowed.
pub const SL_W: u64 = 1 << 1;
/// Second-stage entry bit 7: this level-2 or level-3 entry maps a large page.
pub const SL_PS: u64 = 1 << 7;

/// A non-leaf second-stage entry (section 9.8) pointing to the next-level table. Read and
/// write are both set so the leaf alone decides access.
pub const fn sl_table(next: u64) -> u64 {
    (next & ADDR_MASK) | SL_R | SL_W
}

/// A second-stage leaf entry mapping `phys` (aligned to `size`) with `access`. Memory type,
/// snoop and transient-mapping bits stay clear: they are reserved unless the unit reports
/// nested translation or snoop control, which this crate does not use.
pub const fn sl_page(phys: u64, size: PageSize, access: Access) -> u64 {
    let ps = match size {
        PageSize::Size4K => 0,
        _ => SL_PS,
    };
    (phys & ADDR_MASK) | ps | access.bits()
}

/// Memory for translation tables: the kernel hands out zeroed physical pages and access to
/// them; tests use a vector.
pub trait TableMemory {
    /// Why a table page could not be allocated.
    type Error;

    /// Allocates a zeroed, 4 KiB-aligned table page and returns its physical address.
    ///
    /// # Errors
    ///
    /// When no memory is available.
    fn alloc_table(&mut self) -> Result<u64, Self::Error>;

    /// The table at physical address `phys`, which [`TableMemory::alloc_table`] returned.
    fn table(&mut self, phys: u64) -> &mut Table;
}

/// Why [`map`] could not install a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MapError<E> {
    /// A table page could not be allocated.
    Alloc(E),
    /// The IOVA or physical address is not aligned to the page size.
    Misaligned {
        /// The IOVA.
        iova: u64,
        /// The physical address.
        phys: u64,
    },
    /// The IOVA is beyond the tables' address width, or the page size needs more levels than
    /// the tables have.
    OutOfRange {
        /// The IOVA.
        iova: u64,
    },
    /// Something is already mapped at the IOVA.
    AlreadyMapped {
        /// The IOVA.
        iova: u64,
    },
}

impl<E: fmt::Display> fmt::Display for MapError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alloc(e) => write!(f, "cannot allocate a translation table: {e}"),
            Self::Misaligned { iova, phys } => {
                write!(f, "IOVA {iova:#x} or address {phys:#x} is misaligned")
            }
            Self::OutOfRange { iova } => write!(f, "IOVA {iova:#x} is out of range"),
            Self::AlreadyMapped { iova } => write!(f, "IOVA {iova:#x} is already mapped"),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for MapError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Alloc(e) => Some(e),
            _ => None,
        }
    }
}

/// The index into a level-`level` table for `iova` (level 1 holds 4 KiB leaves).
const fn index(iova: u64, level: u8) -> usize {
    ((iova >> (12 + 9 * (level as u32 - 1))) & 0x1ff) as usize
}

/// Maps one page of `size` at `iova` to `phys` in the tables rooted at `root`, allocating
/// intermediate tables as needed.
///
/// # Errors
///
/// [`MapError::Misaligned`] when either address is not aligned to `size`,
/// [`MapError::OutOfRange`] when `iova` is beyond `agaw`, [`MapError::AlreadyMapped`] when the
/// leaf (or a large page above it) is already present, and [`MapError::Alloc`] when a table
/// cannot be allocated.
///
/// # Examples
///
/// ```
/// use ferric_vtd::table::{map, translate, Access, Agaw, MapError, PageSize, Table, TableMemory};
///
/// struct Vec4k(Vec<Box<Table>>);
/// impl TableMemory for Vec4k {
///     type Error = core::convert::Infallible;
///     fn alloc_table(&mut self) -> Result<u64, Self::Error> {
///         self.0.push(Box::new([0; 512]));
///         Ok(self.0.len() as u64 * 4096)
///     }
///     fn table(&mut self, phys: u64) -> &mut Table {
///         &mut self.0[phys as usize / 4096 - 1]
///     }
/// }
///
/// let mut mem = Vec4k(Vec::new());
/// let root = mem.alloc_table().map_err(MapError::Alloc)?;
/// map(&mut mem, root, Agaw::Bits39, 0x20_0000, 0x4020_0000, PageSize::Size2M, Access::ReadWrite)?;
/// assert_eq!(translate(&mut mem, root, Agaw::Bits39, 0x20_1234), Some((0x4020_1234, Access::ReadWrite)));
/// # Ok::<(), ferric_vtd::table::MapError<core::convert::Infallible>>(())
/// ```
pub fn map<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
    phys: u64,
    size: PageSize,
    access: Access,
) -> Result<(), MapError<M::Error>> {
    let mask = size.bytes() - 1;
    if iova & mask != 0 || phys & mask != 0 {
        return Err(MapError::Misaligned { iova, phys });
    }
    if iova >= agaw.limit() || size.level() > agaw.levels() {
        return Err(MapError::OutOfRange { iova });
    }
    let mut table = root;
    let mut level = agaw.levels();
    while level > size.level() {
        let i = index(iova, level);
        let entry = mem.table(table)[i];
        table = if entry & (SL_R | SL_W) == 0 {
            let next = mem.alloc_table().map_err(MapError::Alloc)?;
            mem.table(table)[i] = sl_table(next);
            next
        } else if entry & SL_PS != 0 {
            return Err(MapError::AlreadyMapped { iova });
        } else {
            entry & ADDR_MASK
        };
        level -= 1;
    }
    let slot = &mut mem.table(table)[index(iova, level)];
    if *slot & (SL_R | SL_W) != 0 {
        return Err(MapError::AlreadyMapped { iova });
    }
    *slot = sl_page(phys, size, access);
    Ok(())
}

/// Maps `[start, end)` one-to-one (IOVA equal to physical address) with read-write pages of
/// `size`, after rounding `start` down and `end` up to `size`.
///
/// # Errors
///
/// As [`map`], for the first page that fails.
pub fn map_identity<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    start: u64,
    end: u64,
    size: PageSize,
) -> Result<(), MapError<M::Error>> {
    let step = size.bytes();
    let mut addr = start & !(step - 1);
    while addr < end {
        map(mem, root, agaw, addr, addr, size, Access::ReadWrite)?;
        addr = match addr.checked_add(step) {
            Some(next) => next,
            None => break,
        };
    }
    Ok(())
}

/// Walks the tables rooted at `root` for `iova`, returning the physical address and access
/// a device would get, or `None` when nothing is mapped.
pub fn translate<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
) -> Option<(u64, Access)> {
    if iova >= agaw.limit() {
        return None;
    }
    let mut table = root;
    let mut level = agaw.levels();
    loop {
        let entry = mem.table(table)[index(iova, level)];
        let access = match entry & (SL_R | SL_W) {
            0 => return None,
            SL_R => Access::Read,
            SL_W => Access::Write,
            _ => Access::ReadWrite,
        };
        if level == 1 || entry & SL_PS != 0 {
            let offset = iova & ((1 << (12 + 9 * (u32::from(level) - 1))) - 1);
            return Some(((entry & ADDR_MASK) | offset, access));
        }
        table = entry & ADDR_MASK;
        level -= 1;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    extern crate std;
    use std::boxed::Box;
    use std::vec::Vec;

    use super::*;

    /// Table pages in a vector; physical address `n * 4096` is page `n - 1`. Fails after
    /// `limit` pages.
    pub(crate) struct VecMemory {
        pub(crate) pages: Vec<Box<Table>>,
        pub(crate) limit: usize,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct OutOfPages;

    impl VecMemory {
        pub(crate) fn new() -> Self {
            Self {
                pages: Vec::new(),
                limit: usize::MAX,
            }
        }
    }

    impl TableMemory for VecMemory {
        type Error = OutOfPages;
        fn alloc_table(&mut self) -> Result<u64, OutOfPages> {
            if self.pages.len() >= self.limit {
                return Err(OutOfPages);
            }
            self.pages.push(Box::new([0; ENTRIES]));
            Ok(self.pages.len() as u64 * 4096)
        }
        fn table(&mut self, phys: u64) -> &mut Table {
            &mut self.pages[phys as usize / 4096 - 1]
        }
    }

    #[test]
    fn entry_encodings() {
        assert_eq!(RootEntry::new(0x1234_5000).0, [0x1234_5001, 0]);
        let ce = ContextEntry::new(
            0x8_0000_0000,
            Agaw::Bits39,
            1,
            TranslationType::Untranslated,
        );
        assert_eq!(ce.0, [0x8_0000_0001, 0x101]);
        assert_eq!(ce.domain_id(), 1);
        let pt = ContextEntry::new(0, Agaw::Bits48, 0xabcd, TranslationType::PassThrough);
        assert_eq!(pt.0, [0b1001, 0x00ab_cd02]);
        assert_eq!(sl_table(0x5000), 0x5003);
        assert_eq!(
            sl_page(0x4000_0000, PageSize::Size1G, Access::Read),
            0x4000_0081
        );
        assert_eq!(
            sl_page(0x20_0000, PageSize::Size2M, Access::ReadWrite),
            0x20_0083
        );
        assert_eq!(sl_page(0x3000, PageSize::Size4K, Access::Write), 0x3002);
    }

    #[test]
    fn agaw_choice() {
        // QEMU 8.2: 39 bits only.
        assert_eq!(Agaw::smallest_covering(0b10, 32), Some(Agaw::Bits39));
        assert_eq!(Agaw::smallest_covering(0b10, 40), None);
        assert_eq!(Agaw::smallest_covering(0b110, 40), Some(Agaw::Bits48));
        assert_eq!(Agaw::smallest_covering(0b1100, 12), Some(Agaw::Bits48));
        assert_eq!(Agaw::smallest_covering(0, 12), None);
        assert_eq!(Agaw::Bits57.levels(), 5);
    }

    #[test]
    fn identity_map_with_2m_pages() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let top = 4 << 30;
        map_identity(&mut mem, root, Agaw::Bits39, 0, top, PageSize::Size2M).unwrap();
        // Root (level 3), and four level-2 tables of 512 2 MiB pages each.
        assert_eq!(mem.pages.len(), 5);
        for addr in [0, 0x1000, 0x7fff_f123, 0xfee0_0000, top - 1] {
            assert_eq!(
                translate(&mut mem, root, Agaw::Bits39, addr),
                Some((addr, Access::ReadWrite))
            );
        }
        assert_eq!(translate(&mut mem, root, Agaw::Bits39, top), None);
        assert_eq!(translate(&mut mem, root, Agaw::Bits39, 1 << 39), None);
    }

    #[test]
    fn identity_map_with_4k_pages_and_four_levels() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        // Rounded out to whole pages.
        map_identity(
            &mut mem,
            root,
            Agaw::Bits48,
            0x7c80_0800,
            0x7c80_2001,
            PageSize::Size4K,
        )
        .unwrap();
        assert_eq!(mem.pages.len(), 4);
        for addr in [0x7c80_0000, 0x7c80_2fff] {
            assert_eq!(
                translate(&mut mem, root, Agaw::Bits48, addr),
                Some((addr, Access::ReadWrite))
            );
        }
        assert_eq!(translate(&mut mem, root, Agaw::Bits48, 0x7c80_3000), None);
    }

    #[test]
    fn map_errors() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits39;
        assert_eq!(
            map(
                &mut mem,
                root,
                a,
                0x1000,
                0x1000,
                PageSize::Size2M,
                Access::Read
            ),
            Err(MapError::Misaligned {
                iova: 0x1000,
                phys: 0x1000
            })
        );
        assert_eq!(
            map(
                &mut mem,
                root,
                a,
                1 << 39,
                0,
                PageSize::Size4K,
                Access::Read
            ),
            Err(MapError::OutOfRange { iova: 1 << 39 })
        );
        map(
            &mut mem,
            root,
            a,
            0x20_0000,
            0x20_0000,
            PageSize::Size2M,
            Access::Read,
        )
        .unwrap();
        assert_eq!(
            map(
                &mut mem,
                root,
                a,
                0x20_0000,
                0,
                PageSize::Size2M,
                Access::Read
            ),
            Err(MapError::AlreadyMapped { iova: 0x20_0000 })
        );
        // A 4 KiB page under an existing 2 MiB page.
        assert_eq!(
            map(
                &mut mem,
                root,
                a,
                0x20_1000,
                0,
                PageSize::Size4K,
                Access::Read
            ),
            Err(MapError::AlreadyMapped { iova: 0x20_1000 })
        );
        mem.limit = mem.pages.len();
        assert_eq!(
            map(
                &mut mem,
                root,
                a,
                1 << 30,
                0,
                PageSize::Size4K,
                Access::Read
            ),
            Err(MapError::Alloc(OutOfPages))
        );
        assert_eq!(
            translate(&mut mem, root, a, 0x20_0000),
            Some((0x20_0000, Access::Read))
        );
    }
}
