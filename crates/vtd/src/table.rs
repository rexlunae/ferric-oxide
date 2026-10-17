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

/// Why [`unmap`] could not remove a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnmapError {
    /// Nothing is mapped at the IOVA.
    NotMapped {
        /// The IOVA.
        iova: u64,
    },
    /// The IOVA lies inside a large page but is not its first byte; large pages are only
    /// removed whole.
    InsideLargePage {
        /// The IOVA.
        iova: u64,
        /// The size of the page that covers it.
        size: PageSize,
    },
    /// The IOVA is beyond the tables' address width.
    OutOfRange {
        /// The IOVA.
        iova: u64,
    },
    /// A range to unmap ends inside a large page.
    PartialPage {
        /// The first IOVA of that page.
        iova: u64,
        /// Its size.
        size: PageSize,
    },
}

impl fmt::Display for UnmapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMapped { iova } => write!(f, "nothing is mapped at IOVA {iova:#x}"),
            Self::InsideLargePage { iova, size } => {
                write!(
                    f,
                    "IOVA {iova:#x} is inside a {:#x}-byte page",
                    size.bytes()
                )
            }
            Self::OutOfRange { iova } => write!(f, "IOVA {iova:#x} is out of range"),
            Self::PartialPage { iova, size } => write!(
                f,
                "the range ends inside the {:#x}-byte page at IOVA {iova:#x}",
                size.bytes()
            ),
        }
    }
}

impl core::error::Error for UnmapError {}

/// The table pages an [`unmap`] emptied and unlinked from their parents, lowest level first.
/// The root table is never among them.
///
/// The unit may still hold the unlinked entries in its paging-structure caches, so the caller
/// frees these pages only after an IOTLB invalidation that covers them (one with
/// [`Request::IotlbPage::leaf_only`](crate::invalidation::Request::IotlbPage) clear, or a
/// domain-selective one) has completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EmptiedTables {
    pages: [u64; 4],
    len: usize,
}

impl EmptiedTables {
    /// The physical addresses of the emptied pages.
    pub fn as_slice(&self) -> &[u64] {
        &self.pages[..self.len]
    }

    /// Whether no table page was emptied.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, phys: u64) {
        // Only tables below the root are pushed, and there are at most four levels below a
        // five-level root.
        self.pages[self.len] = phys;
        self.len += 1;
    }
}

/// One mapping that [`unmap`] removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unmapped {
    /// The physical address the page mapped.
    pub phys: u64,
    /// The page's size.
    pub size: PageSize,
    /// The access it granted.
    pub access: Access,
    /// The table pages left empty, for the caller to free after invalidating.
    pub emptied: EmptiedTables,
}

/// Removes the page mapped at `iova` (the first byte of a 4 KiB, 2 MiB or 1 GiB page) from
/// the tables rooted at `root`. Every table that is left with no present entry is unlinked
/// from its parent and reported in [`Unmapped::emptied`], so the caller can free it once the
/// IOTLB has been invalidated.
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
    if iova >= agaw.limit() {
        return Err(UnmapError::OutOfRange { iova });
    }
    // The tables walked through, from the root down: (table, index of the entry used).
    let mut path = [(0u64, 0usize); 5];
    let mut depth = 0;
    let mut table = root;
    let mut level = agaw.levels();
    let (leaf, size) = loop {
        let i = index(iova, level);
        path[depth] = (table, i);
        depth += 1;
        let entry = mem.table(table)[i];
        if entry & (SL_R | SL_W) == 0 {
            return Err(UnmapError::NotMapped { iova });
        }
        let size = match level {
            1 => Some(PageSize::Size4K),
            2 if entry & SL_PS != 0 => Some(PageSize::Size2M),
            3 if entry & SL_PS != 0 => Some(PageSize::Size1G),
            _ => None,
        };
        if let Some(size) = size {
            break (entry, size);
        }
        table = entry & ADDR_MASK;
        level -= 1;
    };
    if iova & (size.bytes() - 1) != 0 {
        return Err(UnmapError::InsideLargePage { iova, size });
    }
    let access = match leaf & (SL_R | SL_W) {
        SL_R => Access::Read,
        SL_W => Access::Write,
        _ => Access::ReadWrite,
    };
    let mut emptied = EmptiedTables::default();
    // Clear the leaf, then walk back up while the table just changed is empty, unlinking it
    // from its parent. The root (path[0]) is never unlinked.
    for d in (0..depth).rev() {
        let (table, i) = path[d];
        mem.table(table)[i] = 0;
        if d == 0 || mem.table(table).iter().any(|&e| e & (SL_R | SL_W) != 0) {
            break;
        }
        // The next pass clears the parent's entry that pointed to this table.
        emptied.push(table);
    }
    Ok(Unmapped {
        phys: leaf & ADDR_MASK,
        size,
        access,
        emptied,
    })
}

/// Which leaf sizes [`map_range`] may use, from the unit's CAP.SLLPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LargePages {
    /// 4 KiB pages only.
    None,
    /// 4 KiB and 2 MiB pages.
    Up2M,
    /// 4 KiB, 2 MiB and 1 GiB pages.
    Up1G,
}

impl LargePages {
    /// What a unit with these capabilities supports.
    pub const fn from_capability(cap: crate::regs::Capability) -> Self {
        match (cap.large_pages_2m(), cap.large_pages_1g()) {
            (true, true) => Self::Up1G,
            (true, false) => Self::Up2M,
            _ => Self::None,
        }
    }

    /// The largest page of at most `len` bytes that `iova` and `phys` are both aligned to.
    fn page_for(self, iova: u64, phys: u64, len: u64, agaw: Agaw) -> PageSize {
        let fits = |size: PageSize| {
            let mask = size.bytes() - 1;
            iova & mask == 0
                && phys & mask == 0
                && len >= size.bytes()
                && size.level() <= agaw.levels()
        };
        if self >= Self::Up1G && fits(PageSize::Size1G) {
            PageSize::Size1G
        } else if self >= Self::Up2M && fits(PageSize::Size2M) {
            PageSize::Size2M
        } else {
            PageSize::Size4K
        }
    }
}

/// A range operation that stopped part-way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeError<E> {
    /// Bytes from the start of the range that were processed before the failure. For
    /// [`map_range`] those stay mapped; for [`unmap_range`] they are unmapped.
    pub done: u64,
    /// What failed.
    pub error: E,
}

impl<E: fmt::Display> fmt::Display for RangeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "after {:#x} bytes: {}", self.done, self.error)
    }
}

impl<E: core::error::Error + 'static> core::error::Error for RangeError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(&self.error)
    }
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
    if (iova | phys | len) & 0xfff != 0 {
        return Err(RangeError {
            done: 0,
            error: MapError::Misaligned { iova, phys },
        });
    }
    let mut done = 0;
    while done < len {
        let (va, pa) = (iova.wrapping_add(done), phys.wrapping_add(done));
        let size = large.page_for(va, pa, len - done, agaw);
        map(mem, root, agaw, va, pa, size, access).map_err(|error| RangeError { done, error })?;
        done += size.bytes();
    }
    Ok(())
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
    mut emptied: impl FnMut(u64),
) -> Result<(), RangeError<UnmapError>> {
    let mut done = 0;
    while done < len {
        let va = iova.wrapping_add(done);
        let fail = |error| RangeError { done, error };
        let size =
            leaf_size(mem, root, agaw, va).ok_or(fail(UnmapError::NotMapped { iova: va }))?;
        if size.bytes() > len - done {
            return Err(fail(UnmapError::PartialPage { iova: va, size }));
        }
        let gone = unmap(mem, root, agaw, va).map_err(fail)?;
        gone.emptied
            .as_slice()
            .iter()
            .for_each(|&page| emptied(page));
        done += gone.size.bytes();
    }
    Ok(())
}

/// Where the leaf that maps `iova` sits: its table, its index in that table and its size.
fn leaf<M: TableMemory>(
    mem: &mut M,
    root: u64,
    agaw: Agaw,
    iova: u64,
) -> Option<(u64, usize, PageSize)> {
    if iova >= agaw.limit() {
        return None;
    }
    let mut table = root;
    let mut level = agaw.levels();
    loop {
        let i = index(iova, level);
        let entry = mem.table(table)[i];
        if entry & (SL_R | SL_W) == 0 {
            return None;
        }
        match level {
            1 => return Some((table, i, PageSize::Size4K)),
            2 if entry & SL_PS != 0 => return Some((table, i, PageSize::Size2M)),
            3 if entry & SL_PS != 0 => return Some((table, i, PageSize::Size1G)),
            _ => {}
        }
        table = entry & ADDR_MASK;
        level -= 1;
    }
}

/// The size of the leaf that maps `iova`, if any.
fn leaf_size<M: TableMemory>(mem: &mut M, root: u64, agaw: Agaw, iova: u64) -> Option<PageSize> {
    leaf(mem, root, agaw, iova).map(|(_, _, size)| size)
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
    let mut done = 0;
    while done < len {
        let va = iova.wrapping_add(done);
        let fail = |error| RangeError { done, error };
        if va >= agaw.limit() {
            return Err(fail(UnmapError::OutOfRange { iova: va }));
        }
        let (table, i, size) =
            leaf(mem, root, agaw, va).ok_or(fail(UnmapError::NotMapped { iova: va }))?;
        if va & (size.bytes() - 1) != 0 {
            return Err(fail(UnmapError::InsideLargePage { iova: va, size }));
        }
        if size.bytes() > len - done {
            return Err(fail(UnmapError::PartialPage { iova: va, size }));
        }
        let entry = &mut mem.table(table)[i];
        *entry = (*entry & !(SL_R | SL_W)) | access.bits();
        done += size.bytes();
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
            (0xa000, PageSize::Size4K, Access::Read)
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
        assert_eq!(
            LargePages::from_capability(crate::regs::tests::QEMU_CAP),
            LargePages::Up1G
        );
        assert_eq!(
            LargePages::from_capability(crate::regs::Capability(1 << 34)),
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
