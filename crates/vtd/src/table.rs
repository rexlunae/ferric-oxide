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
//! The walk itself is the radix-table code in `ferric_iommu::table`, which AMD-Vi shares;
//! [`SecondStage`] is the VT-d entry encoding it is given, and the functions here fix it and
//! take the address width as an [`Agaw`].
//!
//! Scalable mode (PASID tables, first-stage tables) is not used.

use ferric_iommu::table::{self as radix, EntryFormat, Levels};
pub use ferric_iommu::table::{
    ADDR_MASK, Access, ENTRIES, EmptiedTables, LargePages, MapError, PageSize, RangeError, Table,
    TableMemory, UnmapError, Unmapped,
};

/// The low bit of root, context and page-table entries that marks them present (for page
/// tables: readable).
const PRESENT: u64 = 1;

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

    /// A not-present entry: requests from the device are blocked and fault (section 7.1.3,
    /// fault reason 0x2). Used for devices taken out of a domain.
    pub const BLOCKED: Self = Self([0, 0]);

    /// The domain ID, bits 87:72.
    pub const fn domain_id(self) -> u16 {
        (self.0[1] >> 8) as u16
    }

    /// The present bit, bit 0.
    pub const fn is_present(self) -> bool {
        self.0[0] & PRESENT != 0
    }

    /// The physical address of the second-stage page-table root, bits 63:12.
    pub const fn table(self) -> u64 {
        self.0[0] & ADDR_MASK
    }

    /// The translation type, bits 3:2, or `None` for the reserved value 11.
    pub const fn translation_type(self) -> Option<TranslationType> {
        match (self.0[0] >> 2) & 3 {
            0 => Some(TranslationType::Untranslated),
            1 => Some(TranslationType::All),
            2 => Some(TranslationType::PassThrough),
            _ => None,
        }
    }

    /// The address width, from the AW field (bits 66:64), or `None` for a value no width uses.
    pub const fn agaw(self) -> Option<Agaw> {
        match self.0[1] & 7 {
            1 => Some(Agaw::Bits39),
            2 => Some(Agaw::Bits48),
            3 => Some(Agaw::Bits57),
            _ => None,
        }
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

/// Second-stage entry bit 0: read allowed (and, for non-leaf entries, present).
pub const SL_R: u64 = 1 << 0;
/// Second-stage entry bit 1: write allowed.
pub const SL_W: u64 = 1 << 1;
/// Second-stage entry bit 7: this level-2 or level-3 entry maps a large page.
pub const SL_PS: u64 = 1 << 7;

/// The R and W bits for `access`.
const fn access_bits(access: Access) -> u64 {
    match access {
        Access::Read => SL_R,
        Access::Write => SL_W,
        Access::ReadWrite => SL_R | SL_W,
    }
}

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
    (phys & ADDR_MASK) | ps | access_bits(access)
}

/// The leaf sizes a unit with these capabilities supports (CAP.SLLPS).
pub const fn large_pages(cap: crate::regs::Capability) -> LargePages {
    match (cap.large_pages_2m(), cap.large_pages_1g()) {
        (true, true) => LargePages::Up1G,
        (true, false) => LargePages::Up2M,
        _ => LargePages::None,
    }
}

/// The second-stage entry encoding (section 9.8): an entry is present when it allows a read
/// or a write, and a level-2 or level-3 entry with PS set is a leaf.
#[derive(Clone, Copy, Debug)]
pub struct SecondStage;

impl EntryFormat for SecondStage {
    fn is_present(entry: u64) -> bool {
        entry & (SL_R | SL_W) != 0
    }

    fn table_entry(next: u64, _level: u8) -> u64 {
        sl_table(next)
    }

    fn leaf_entry(phys: u64, size: PageSize, access: Access) -> u64 {
        sl_page(phys, size, access)
    }

    fn is_leaf(entry: u64, level: u8) -> bool {
        level == 1 || (matches!(level, 2 | 3) && entry & SL_PS != 0)
    }

    fn access(entry: u64) -> Option<Access> {
        Access::from_bits(entry & SL_R != 0, entry & SL_W != 0)
    }

    fn with_access(entry: u64, access: Access) -> u64 {
        (entry & !(SL_R | SL_W)) | access_bits(access)
    }
}

impl Agaw {
    /// The table depth for this width.
    pub const fn table_levels(self) -> Levels {
        match self {
            Self::Bits39 => Levels::Three,
            Self::Bits48 => Levels::Four,
            Self::Bits57 => Levels::Five,
        }
    }
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
    radix::map::<SecondStage, M>(mem, root, agaw.table_levels(), iova, phys, size, access)
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
    radix::map_identity::<SecondStage, M>(mem, root, agaw.table_levels(), start, end, size)
}

/// Removes the page mapped at `iova` (the first byte of a 4 KiB, 2 MiB or 1 GiB page) from
/// the tables rooted at `root`. Every table that is left with no present entry is unlinked
/// from its parent and reported in [`Unmapped::emptied`], so the caller can free it once the
/// IOTLB has been invalidated (with an invalidation that covers the paging-structure caches:
/// [`Request::IotlbPage::leaf_only`](crate::invalidation::Request::IotlbPage) clear, or a
/// domain-selective one).
///
/// # Errors
///
/// [`UnmapError::OutOfRange`] when `iova` is beyond `agaw`, [`UnmapError::NotMapped`] when no
/// page covers it, and [`UnmapError::InsideLargePage`] when it falls inside a large page
/// without being that page's first byte. Nothing is changed on error.
///
/// # Examples
///
/// ```
/// use ferric_vtd::table::{map, translate, unmap, Access, Agaw, MapError, PageSize, Table, TableMemory};
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
/// map(&mut mem, root, Agaw::Bits39, 0x5000, 0x9000, PageSize::Size4K, Access::Read)?;
/// let gone = unmap(&mut mem, root, Agaw::Bits39, 0x5000).expect("mapped above");
/// assert_eq!((gone.phys, gone.size), (0x9000, PageSize::Size4K));
/// // The level-2 and level-1 tables are empty now; only the root is left in use.
/// assert_eq!(gone.emptied.as_slice(), [3 * 4096, 2 * 4096]);
/// assert_eq!(translate(&mut mem, root, Agaw::Bits39, 0x5000), None);
/// # Ok::<(), ferric_vtd::table::MapError<core::convert::Infallible>>(())
/// ```
pub fn unmap<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
) -> Result<Unmapped, UnmapError> {
    radix::unmap::<SecondStage, M>(mem, root, agaw.table_levels(), iova)
}

/// Maps `len` bytes from `iova` to `phys` with `access`, using the largest pages `large`
/// allows wherever both addresses are aligned to them.
///
/// # Errors
///
/// [`MapError::Misaligned`] when `iova`, `phys` or `len` is not a multiple of 4 KiB, and
/// otherwise as [`map`] for the first page that fails. The pages before it stay mapped;
/// [`RangeError::done`] says how many bytes that is, so the caller can unmap them.
#[allow(clippy::too_many_arguments)]
pub fn map_range<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
    phys: u64,
    len: u64,
    access: Access,
    large: LargePages,
) -> Result<(), RangeError<MapError<M::Error>>> {
    radix::map_range::<SecondStage, M>(
        mem,
        root,
        agaw.table_levels(),
        iova,
        phys,
        len,
        access,
        large,
    )
}

/// Unmaps every page in `len` bytes from `iova`, calling `emptied` with each table page that
/// is left empty (see [`unmap`]; free them only after invalidating the IOTLB).
///
/// # Errors
///
/// As [`unmap`] for the first page that fails, and [`UnmapError::PartialPage`] when the range
/// ends inside a large page, which is then left mapped. The pages before it are unmapped;
/// [`RangeError::done`] says how many bytes that is.
pub fn unmap_range<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
    len: u64,
    emptied: impl FnMut(u64),
) -> Result<(), RangeError<UnmapError>> {
    radix::unmap_range::<SecondStage, M>(mem, root, agaw.table_levels(), iova, len, emptied)
}

/// Changes the access of every page mapped in `len` bytes from `iova` to `access`, keeping
/// the addresses they map. Narrowing access (read-write to read-only, say) takes effect for a
/// unit only after an IOTLB invalidation that covers the range, since the unit may cache the
/// old entries; so does widening it, since a cached read-only entry makes a write fault.
///
/// # Errors
///
/// [`UnmapError::OutOfRange`] or [`UnmapError::NotMapped`] for the first address in the range
/// that no page maps, [`UnmapError::InsideLargePage`] when the range starts inside a large
/// page, and [`UnmapError::PartialPage`] when it ends inside one. The pages before it are
/// changed; [`RangeError::done`] says how many bytes that is.
///
/// # Examples
///
/// ```
/// use ferric_vtd::table::{map, protect_range, translate, Access, Agaw, MapError, PageSize, Table, TableMemory};
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
/// map(&mut mem, root, Agaw::Bits39, 0x5000, 0x9000, PageSize::Size4K, Access::ReadWrite)?;
/// protect_range(&mut mem, root, Agaw::Bits39, 0x5000, 0x1000, Access::Read).expect("mapped above");
/// assert_eq!(translate(&mut mem, root, Agaw::Bits39, 0x5000), Some((0x9000, Access::Read)));
/// # Ok::<(), ferric_vtd::table::MapError<core::convert::Infallible>>(())
/// ```
pub fn protect_range<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
    len: u64,
    access: Access,
) -> Result<(), RangeError<UnmapError>> {
    radix::protect_range::<SecondStage, M>(mem, root, agaw.table_levels(), iova, len, access)
}

/// Walks the tables rooted at `root` for `iova`, returning the physical address and access
/// a device would get, or `None` when nothing is mapped.
pub fn translate<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
) -> Option<(u64, Access)> {
    radix::translate::<SecondStage, M>(mem, root, agaw.table_levels(), iova)
}

#[cfg(test)]
pub(crate) mod tests {
    extern crate std;
    use std::boxed::Box;
    use std::vec::Vec;

    use super::*;

    fn leaf_size(mem: &mut VecMemory, root: u64, agaw: Agaw, iova: u64) -> Option<PageSize> {
        radix::leaf_size::<SecondStage, _>(mem, root, agaw.table_levels(), iova)
    }

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

    /// The number of present entries in a table page.
    fn present(mem: &mut VecMemory, table: u64) -> usize {
        mem.table(table)
            .iter()
            .filter(|&&e| e & (SL_R | SL_W) != 0)
            .count()
    }

    #[test]
    fn context_entry_decoding() {
        let ce = ContextEntry::new(0x7_7000, Agaw::Bits48, 42, TranslationType::Untranslated);
        assert!(ce.is_present());
        assert_eq!(ce.table(), 0x7_7000);
        assert_eq!(ce.agaw(), Some(Agaw::Bits48));
        assert_eq!(ce.translation_type(), Some(TranslationType::Untranslated));
        assert_eq!(ce.domain_id(), 42);
        assert!(!ContextEntry::BLOCKED.is_present());
        assert_eq!(ContextEntry::BLOCKED.agaw(), None);
        assert_eq!(ContextEntry([0xc | 1, 0]).translation_type(), None);
        let pt = ContextEntry::new(0, Agaw::Bits39, 1, TranslationType::PassThrough);
        assert_eq!(pt.translation_type(), Some(TranslationType::PassThrough));
    }

    #[test]
    fn unmap_frees_emptied_tables_bottom_up() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits48;
        map(
            &mut mem,
            root,
            a,
            0x1000,
            0xa000,
            PageSize::Size4K,
            Access::Read,
        )
        .unwrap();
        map(
            &mut mem,
            root,
            a,
            0x2000,
            0xb000,
            PageSize::Size4K,
            Access::Write,
        )
        .unwrap();
        // Root, then one table at each of levels 3, 2 and 1.
        assert_eq!(mem.pages.len(), 4);
        let first = unmap(&mut mem, root, a, 0x1000).unwrap();
        assert_eq!(
            (first.phys, first.size, first.access),
            (0xa000, PageSize::Size4K, Some(Access::Read))
        );
        // The level-1 table still holds 0x2000.
        assert!(first.emptied.is_empty());
        assert_eq!(
            translate(&mut mem, root, a, 0x2000),
            Some((0xb000, Access::Write))
        );
        let second = unmap(&mut mem, root, a, 0x2000).unwrap();
        // Level 1, 2 and 3 tables, lowest first.
        assert_eq!(second.emptied.as_slice(), [4 * 4096, 3 * 4096, 2 * 4096]);
        assert_eq!(present(&mut mem, root), 0);
        assert_eq!(
            unmap(&mut mem, root, a, 0x2000),
            Err(UnmapError::NotMapped { iova: 0x2000 })
        );
    }

    #[test]
    fn unmap_stops_at_shared_tables() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits39;
        // Two level-1 tables under one level-2 table.
        map(
            &mut mem,
            root,
            a,
            0x0000_1000,
            0x1000,
            PageSize::Size4K,
            Access::ReadWrite,
        )
        .unwrap();
        map(
            &mut mem,
            root,
            a,
            0x0020_0000,
            0x2000,
            PageSize::Size4K,
            Access::ReadWrite,
        )
        .unwrap();
        let gone = unmap(&mut mem, root, a, 0x0020_0000).unwrap();
        // Only the second level-1 table is empty; the level-2 table still points at the first.
        assert_eq!(gone.emptied.as_slice(), [4 * 4096]);
        let l2 = mem.table(root)[0] & ADDR_MASK;
        assert_eq!(present(&mut mem, l2), 1);
        assert_eq!(
            translate(&mut mem, root, a, 0x1000),
            Some((0x1000, Access::ReadWrite))
        );
    }

    #[test]
    fn unmap_large_pages() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits39;
        map(
            &mut mem,
            root,
            a,
            0x4000_0000,
            0x8000_0000,
            PageSize::Size1G,
            Access::Read,
        )
        .unwrap();
        map(
            &mut mem,
            root,
            a,
            0x20_0000,
            0x60_0000,
            PageSize::Size2M,
            Access::ReadWrite,
        )
        .unwrap();
        assert_eq!(
            unmap(&mut mem, root, a, 0x20_1000),
            Err(UnmapError::InsideLargePage {
                iova: 0x20_1000,
                size: PageSize::Size2M
            })
        );
        assert_eq!(
            unmap(&mut mem, root, a, 1 << 39),
            Err(UnmapError::OutOfRange { iova: 1 << 39 })
        );
        let big = unmap(&mut mem, root, a, 0x4000_0000).unwrap();
        assert_eq!((big.phys, big.size), (0x8000_0000, PageSize::Size1G));
        assert!(big.emptied.is_empty());
        let two = unmap(&mut mem, root, a, 0x20_0000).unwrap();
        assert_eq!((two.phys, two.size), (0x60_0000, PageSize::Size2M));
        assert_eq!(two.emptied.as_slice(), [2 * 4096]);
        assert_eq!(present(&mut mem, root), 0);
    }

    #[test]
    fn map_range_picks_large_pages() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits39;
        // 4 KiB short of 2 MiB-aligned at the start, so: one 4 KiB page, one 2 MiB page, one
        // 4 KiB page.
        let (iova, phys, len) = (0x1f_f000, 0x7f_f000, 0x20_2000);
        map_range(
            &mut mem,
            root,
            a,
            iova,
            phys,
            len,
            Access::ReadWrite,
            LargePages::Up2M,
        )
        .unwrap();
        assert_eq!(
            leaf_size(&mut mem, root, a, 0x1f_f000),
            Some(PageSize::Size4K)
        );
        assert_eq!(
            leaf_size(&mut mem, root, a, 0x20_0000),
            Some(PageSize::Size2M)
        );
        assert_eq!(
            leaf_size(&mut mem, root, a, 0x40_0000),
            Some(PageSize::Size4K)
        );
        assert_eq!(leaf_size(&mut mem, root, a, 0x40_1000), None);
        for off in [0, 0x1000, 0x10_0123, len - 1] {
            assert_eq!(
                translate(&mut mem, root, a, iova + off),
                Some((phys + off, Access::ReadWrite))
            );
        }
        // Without large pages, the same range takes 4 KiB pages only.
        let root2 = mem.alloc_table().unwrap();
        map_range(
            &mut mem,
            root2,
            a,
            iova,
            phys,
            len,
            Access::Read,
            LargePages::None,
        )
        .unwrap();
        assert_eq!(
            leaf_size(&mut mem, root2, a, 0x20_0000),
            Some(PageSize::Size4K)
        );

        let mut freed = std::vec::Vec::new();
        unmap_range(&mut mem, root, a, iova, len, |p| freed.push(p)).unwrap();
        assert_eq!(present(&mut mem, root), 0);
        // The level-2 table and the two level-1 tables.
        freed.sort_unstable();
        assert_eq!(freed.len(), 3);
    }

    #[test]
    fn map_range_one_gig_and_errors() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits48;
        map_range(
            &mut mem,
            root,
            a,
            1 << 30,
            1 << 31,
            1 << 30,
            Access::Read,
            LargePages::Up1G,
        )
        .unwrap();
        assert_eq!(
            leaf_size(&mut mem, root, a, 1 << 30),
            Some(PageSize::Size1G)
        );
        assert_eq!(
            map_range(
                &mut mem,
                root,
                a,
                0x1800,
                0,
                0x1000,
                Access::Read,
                LargePages::None
            ),
            Err(RangeError {
                done: 0,
                error: MapError::Misaligned {
                    iova: 0x1800,
                    phys: 0
                }
            })
        );
        // The second page collides with the 1 GiB page.
        assert_eq!(
            map_range(
                &mut mem,
                root,
                a,
                (1 << 30) - 0x1000,
                0,
                0x2000,
                Access::Read,
                LargePages::None
            ),
            Err(RangeError {
                done: 0x1000,
                error: MapError::AlreadyMapped { iova: 1 << 30 }
            })
        );
        // Ending inside the 1 GiB page leaves it mapped.
        assert_eq!(
            unmap_range(&mut mem, root, a, (1 << 30) - 0x1000, 0x2000, |_| {}),
            Err(RangeError {
                done: 0x1000,
                error: UnmapError::PartialPage {
                    iova: 1 << 30,
                    size: PageSize::Size1G
                }
            })
        );
        assert!(translate(&mut mem, root, a, 1 << 30).is_some());
        assert_eq!(
            unmap_range(&mut mem, root, a, 0x5000, 0x1000, |_| {}),
            Err(RangeError {
                done: 0,
                error: UnmapError::NotMapped { iova: 0x5000 }
            })
        );
        assert_eq!(large_pages(crate::regs::tests::QEMU_CAP), LargePages::Up1G);
        assert_eq!(
            large_pages(crate::regs::Capability(1 << 34)),
            LargePages::Up2M
        );
    }

    #[test]
    fn protect_range_changes_access_only() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits39;
        // One 4 KiB page, one 2 MiB page, one 4 KiB page, as in `map_range_picks_large_pages`.
        let (iova, phys, len) = (0x1f_f000, 0x7f_f000, 0x20_2000);
        map_range(
            &mut mem,
            root,
            a,
            iova,
            phys,
            len,
            Access::ReadWrite,
            LargePages::Up2M,
        )
        .unwrap();
        let tables = mem.pages.len();
        protect_range(&mut mem, root, a, iova, len, Access::Read).unwrap();
        for off in [0, 0x1000, 0x10_0123, len - 1] {
            assert_eq!(
                translate(&mut mem, root, a, iova + off),
                Some((phys + off, Access::Read))
            );
        }
        // Only leaves change: no table is allocated or freed, sizes stay.
        assert_eq!(mem.pages.len(), tables);
        assert_eq!(
            leaf_size(&mut mem, root, a, 0x20_0000),
            Some(PageSize::Size2M)
        );
        // Widening back, one 4 KiB page at a time.
        protect_range(&mut mem, root, a, 0x40_0000, 0x1000, Access::ReadWrite).unwrap();
        assert_eq!(
            translate(&mut mem, root, a, 0x40_0000),
            Some((phys + 0x20_1000, Access::ReadWrite))
        );
        assert_eq!(
            translate(&mut mem, root, a, 0x3f_f000),
            Some((phys + 0x20_0000, Access::Read))
        );
    }

    #[test]
    fn protect_range_errors() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let a = Agaw::Bits39;
        map_range(
            &mut mem,
            root,
            a,
            0x20_0000,
            0x40_0000,
            0x20_0000,
            Access::ReadWrite,
            LargePages::Up2M,
        )
        .unwrap();
        // Starting inside the 2 MiB page, ending inside it, and running past it.
        assert_eq!(
            protect_range(&mut mem, root, a, 0x20_1000, 0x1000, Access::Read),
            Err(RangeError {
                done: 0,
                error: UnmapError::InsideLargePage {
                    iova: 0x20_1000,
                    size: PageSize::Size2M
                }
            })
        );
        assert_eq!(
            protect_range(&mut mem, root, a, 0x20_0000, 0x1000, Access::Read),
            Err(RangeError {
                done: 0,
                error: UnmapError::PartialPage {
                    iova: 0x20_0000,
                    size: PageSize::Size2M
                }
            })
        );
        assert_eq!(
            protect_range(&mut mem, root, a, 0x20_0000, 0x20_1000, Access::Read),
            Err(RangeError {
                done: 0x20_0000,
                error: UnmapError::NotMapped { iova: 0x40_0000 }
            })
        );
        // The page before the failure was changed.
        assert_eq!(
            translate(&mut mem, root, a, 0x20_0000),
            Some((0x40_0000, Access::Read))
        );
        assert_eq!(
            protect_range(&mut mem, root, a, a.limit(), 0x1000, Access::Read),
            Err(RangeError {
                done: 0,
                error: UnmapError::OutOfRange { iova: a.limit() }
            })
        );
    }
}
