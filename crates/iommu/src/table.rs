//! Radix I/O page tables, shared by the VT-d and AMD-Vi encodings.
//!
//! Intel VT-d second-stage tables (VT-d specification, section 9.8) and AMD-Vi host page
//! tables (AMD I/O Virtualization Technology (IOMMU) Specification, #48882, section 2.2.3)
//! have the same shape as x86-64 page tables: 4 KiB tables of 512 eight-byte entries, 9 bits
//! of IOVA per level, 4 KiB leaves at level 1 and optional 2 MiB and 1 GiB leaves at levels 2
//! and 3. Only the entry encodings differ: where the present, read and write bits are, how a
//! leaf above level 1 is marked, and what a table entry carries besides the next table's
//! address. This module walks and edits the tables; an [`EntryFormat`] supplies the encoding.
//!
//! - [`map`], [`map_range`] and [`map_identity`] install mappings, allocating tables from a
//!   [`TableMemory`];
//! - [`unmap`] and [`unmap_range`] remove them and report the table pages left empty, which
//!   the caller frees only after the IOTLB invalidation that covers them has completed;
//! - [`protect_range`] changes the access of mapped pages;
//! - [`translate`] walks the tables the way a device's request would.
//!
//! Neither format's level skipping (AMD-Vi) nor anything above 5 levels is used.
//!
//! # Examples
//!
//! ```
//! use ferric_iommu::table::{
//!     map, translate, Access, EntryFormat, Levels, MapError, PageSize, Table, TableMemory,
//! };
//!
//! // A toy format: bit 0 present, bit 1 writable, bit 7 a large leaf.
//! struct Toy;
//! impl EntryFormat for Toy {
//!     fn is_present(entry: u64) -> bool { entry & 1 != 0 }
//!     fn table_entry(next: u64, _level: u8) -> u64 { next | 3 }
//!     fn leaf_entry(phys: u64, size: PageSize, access: Access) -> u64 {
//!         let large = if size == PageSize::Size4K { 0 } else { 0x80 };
//!         phys | large | 1 | if access == Access::Read { 0 } else { 2 }
//!     }
//!     fn is_leaf(entry: u64, level: u8) -> bool { level == 1 || entry & 0x80 != 0 }
//!     fn access(entry: u64) -> Option<Access> {
//!         Some(if entry & 2 != 0 { Access::ReadWrite } else { Access::Read })
//!     }
//!     fn with_access(entry: u64, access: Access) -> u64 {
//!         (entry & !2) | if access == Access::Read { 0 } else { 2 }
//!     }
//! }
//!
//! struct Pages(Vec<Box<Table>>);
//! impl TableMemory for Pages {
//!     type Error = core::convert::Infallible;
//!     fn alloc_table(&mut self) -> Result<u64, Self::Error> {
//!         self.0.push(Box::new([0; 512]));
//!         Ok(self.0.len() as u64 * 4096)
//!     }
//!     fn table(&mut self, phys: u64) -> &mut Table {
//!         &mut self.0[phys as usize / 4096 - 1]
//!     }
//! }
//!
//! let mut mem = Pages(Vec::new());
//! let root = mem.alloc_table().map_err(MapError::Alloc)?;
//! map::<Toy, _>(&mut mem, root, Levels::Four, 0x20_0000, 0x4020_0000, PageSize::Size2M, Access::Read)?;
//! assert_eq!(
//!     translate::<Toy, _>(&mut mem, root, Levels::Four, 0x20_1234),
//!     Some((0x4020_1234, Access::Read))
//! );
//! # Ok::<(), MapError<core::convert::Infallible>>(())
//! ```

use core::fmt;

/// Entries in one 4 KiB table.
pub const ENTRIES: usize = 512;

/// One 4 KiB table of entries.
pub type Table = [u64; ENTRIES];

/// Bits 51:12 of a table entry: a 4 KiB-aligned physical address.
pub const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// How many levels the tables have, which fixes how many IOVA bits they translate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Levels {
    /// 3 levels, 39 bits of IOVA.
    Three,
    /// 4 levels, 48 bits of IOVA.
    Four,
    /// 5 levels, 57 bits of IOVA.
    Five,
}

impl Levels {
    /// The number of levels.
    pub const fn count(self) -> u8 {
        match self {
            Self::Three => 3,
            Self::Four => 4,
            Self::Five => 5,
        }
    }

    /// The number of IOVA bits the tables translate.
    pub const fn bits(self) -> u8 {
        12 + 9 * self.count()
    }

    /// One past the largest IOVA the tables can translate.
    pub const fn limit(self) -> u64 {
        1 << self.bits()
    }
}

/// The size of a leaf mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PageSize {
    /// 4 KiB, a level-1 entry.
    Size4K,
    /// 2 MiB, a leaf at level 2.
    Size2M,
    /// 1 GiB, a leaf at level 3.
    Size1G,
}

impl PageSize {
    /// Size in bytes.
    pub const fn bytes(self) -> u64 {
        1 << self.shift()
    }

    /// log2 of the size in bytes.
    pub const fn shift(self) -> u32 {
        match self {
            Self::Size4K => 12,
            Self::Size2M => 21,
            Self::Size1G => 30,
        }
    }

    /// The level of the table that holds a leaf of this size.
    pub const fn level(self) -> u8 {
        match self {
            Self::Size4K => 1,
            Self::Size2M => 2,
            Self::Size1G => 3,
        }
    }

    /// The leaf size of a leaf at `level`, if a leaf can sit there.
    pub const fn at_level(level: u8) -> Option<Self> {
        match level {
            1 => Some(Self::Size4K),
            2 => Some(Self::Size2M),
            3 => Some(Self::Size1G),
            _ => None,
        }
    }
}

/// The access a leaf grants to device requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// DMA reads only.
    Read,
    /// DMA writes only.
    Write,
    /// Reads and writes.
    ReadWrite,
}

impl Access {
    /// Whether reads are allowed.
    pub const fn read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    /// Whether writes are allowed.
    pub const fn write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    /// The access with these permissions, or `None` for neither.
    pub const fn from_bits(read: bool, write: bool) -> Option<Self> {
        match (read, write) {
            (true, true) => Some(Self::ReadWrite),
            (true, false) => Some(Self::Read),
            (false, true) => Some(Self::Write),
            (false, false) => None,
        }
    }
}

/// Which leaf sizes [`map_range`] may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LargePages {
    /// 4 KiB pages only.
    None,
    /// 4 KiB and 2 MiB pages.
    Up2M,
    /// 4 KiB, 2 MiB and 1 GiB pages.
    Up1G,
}

impl LargePages {
    /// The largest page of at most `len` bytes that `iova` and `phys` are both aligned to.
    fn page_for(self, iova: u64, phys: u64, len: u64, levels: Levels) -> PageSize {
        let fits = |size: PageSize| {
            let mask = size.bytes() - 1;
            iova & mask == 0
                && phys & mask == 0
                && len >= size.bytes()
                && size.level() <= levels.count()
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

/// How one IOMMU encodes table and leaf entries.
pub trait EntryFormat {
    /// Whether `entry` is present (maps something or points to a table).
    fn is_present(entry: u64) -> bool;

    /// A present entry in a level-`level` table pointing to the next-level table at `next`.
    /// It grants all access, so the leaf alone decides.
    fn table_entry(next: u64, level: u8) -> u64;

    /// A present leaf mapping `phys` (aligned to `size`) with `access`, for a table of level
    /// `size.level()`.
    fn leaf_entry(phys: u64, size: PageSize, access: Access) -> u64;

    /// Whether the present `entry` in a level-`level` table is a leaf. Level-1 entries always
    /// are.
    fn is_leaf(entry: u64, level: u8) -> bool;

    /// The access a present leaf grants, or `None` when it grants none.
    fn access(entry: u64) -> Option<Access>;

    /// The present leaf `entry` with its access changed to `access`.
    fn with_access(entry: u64, access: Access) -> u64;
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

/// Why [`unmap`] or [`protect_range`] could not change a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnmapError {
    /// Nothing is mapped at the IOVA.
    NotMapped {
        /// The IOVA.
        iova: u64,
    },
    /// The IOVA lies inside a large page but is not its first byte; large pages are only
    /// changed whole.
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
    /// A range ends inside a large page.
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
/// The IOMMU may still hold the unlinked entries in its caches, so the caller frees these
/// pages only after an IOTLB invalidation that covers them, including the cached non-leaf
/// entries, has completed.
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
    /// The access it granted, or `None` when it granted none.
    pub access: Option<Access>,
    /// The table pages left empty, for the caller to free after invalidating.
    pub emptied: EmptiedTables,
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

/// The index into a level-`level` table for `iova` (level 1 holds 4 KiB leaves).
const fn index(iova: u64, level: u8) -> usize {
    ((iova >> (12 + 9 * (level as u32 - 1))) & 0x1ff) as usize
}

/// The size of the leaf `entry` in a level-`level` table maps, if it is a leaf.
fn leaf_size_of<F: EntryFormat>(entry: u64, level: u8) -> Option<PageSize> {
    if F::is_leaf(entry, level) {
        // A format whose leaves sit above level 3 is not used; such an entry reads as a
        // 1 GiB leaf so a walk never follows it as a table.
        Some(PageSize::at_level(level).unwrap_or(PageSize::Size1G))
    } else {
        None
    }
}

/// Maps one page of `size` at `iova` to `phys` in the tables rooted at `root`, allocating
/// intermediate tables as needed.
///
/// # Errors
///
/// [`MapError::Misaligned`] when either address is not aligned to `size`,
/// [`MapError::OutOfRange`] when `iova` is beyond `levels`, [`MapError::AlreadyMapped`] when
/// the leaf (or a large page above it) is already present, and [`MapError::Alloc`] when a
/// table cannot be allocated.
pub fn map<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
    phys: u64,
    size: PageSize,
    access: Access,
) -> Result<(), MapError<M::Error>> {
    let mask = size.bytes() - 1;
    if iova & mask != 0 || phys & mask != 0 {
        return Err(MapError::Misaligned { iova, phys });
    }
    if iova >= levels.limit() || size.level() > levels.count() {
        return Err(MapError::OutOfRange { iova });
    }
    let mut table = root;
    let mut level = levels.count();
    while level > size.level() {
        let i = index(iova, level);
        let entry = mem.table(table)[i];
        table = if !F::is_present(entry) {
            let next = mem.alloc_table().map_err(MapError::Alloc)?;
            mem.table(table)[i] = F::table_entry(next, level);
            next
        } else if F::is_leaf(entry, level) {
            return Err(MapError::AlreadyMapped { iova });
        } else {
            entry & ADDR_MASK
        };
        level -= 1;
    }
    let slot = &mut mem.table(table)[index(iova, level)];
    if F::is_present(*slot) {
        return Err(MapError::AlreadyMapped { iova });
    }
    *slot = F::leaf_entry(phys, size, access);
    Ok(())
}

/// Maps `[start, end)` one-to-one (IOVA equal to physical address) with read-write pages of
/// `size`, after rounding `start` down and `end` up to `size`.
///
/// # Errors
///
/// As [`map`], for the first page that fails.
pub fn map_identity<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    start: u64,
    end: u64,
    size: PageSize,
) -> Result<(), MapError<M::Error>> {
    let step = size.bytes();
    let mut addr = start & !(step - 1);
    while addr < end {
        map::<F, M>(mem, root, levels, addr, addr, size, Access::ReadWrite)?;
        addr = match addr.checked_add(step) {
            Some(next) => next,
            None => break,
        };
    }
    Ok(())
}

/// Removes the page mapped at `iova` (the first byte of a 4 KiB, 2 MiB or 1 GiB page) from
/// the tables rooted at `root`. Every table that is left with no present entry is unlinked
/// from its parent and reported in [`Unmapped::emptied`], so the caller can free it once the
/// IOTLB has been invalidated.
///
/// # Errors
///
/// [`UnmapError::OutOfRange`] when `iova` is beyond `levels`, [`UnmapError::NotMapped`] when
/// no page covers it, and [`UnmapError::InsideLargePage`] when it falls inside a large page
/// without being that page's first byte. Nothing is changed on error.
pub fn unmap<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
) -> Result<Unmapped, UnmapError> {
    if iova >= levels.limit() {
        return Err(UnmapError::OutOfRange { iova });
    }
    // The tables walked through, from the root down: (table, index of the entry used).
    let mut path = [(0u64, 0usize); 5];
    let mut depth = 0;
    let mut table = root;
    let mut level = levels.count();
    let (leaf, size) = loop {
        let i = index(iova, level);
        path[depth] = (table, i);
        depth += 1;
        let entry = mem.table(table)[i];
        if !F::is_present(entry) {
            return Err(UnmapError::NotMapped { iova });
        }
        if let Some(size) = leaf_size_of::<F>(entry, level) {
            break (entry, size);
        }
        table = entry & ADDR_MASK;
        level -= 1;
    };
    if iova & (size.bytes() - 1) != 0 {
        return Err(UnmapError::InsideLargePage { iova, size });
    }
    let mut emptied = EmptiedTables::default();
    // Clear the leaf, then walk back up while the table just changed is empty, unlinking it
    // from its parent. The root (path[0]) is never unlinked.
    for d in (0..depth).rev() {
        let (table, i) = path[d];
        mem.table(table)[i] = 0;
        if d == 0 || mem.table(table).iter().any(|&e| F::is_present(e)) {
            break;
        }
        // The next pass clears the parent's entry that pointed to this table.
        emptied.push(table);
    }
    Ok(Unmapped {
        phys: leaf & ADDR_MASK,
        size,
        access: F::access(leaf),
        emptied,
    })
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
pub fn map_range<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
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
        let size = large.page_for(va, pa, len - done, levels);
        map::<F, M>(mem, root, levels, va, pa, size, access)
            .map_err(|error| RangeError { done, error })?;
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
pub fn unmap_range<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
    len: u64,
    mut emptied: impl FnMut(u64),
) -> Result<(), RangeError<UnmapError>> {
    let mut done = 0;
    while done < len {
        let va = iova.wrapping_add(done);
        let fail = |error| RangeError { done, error };
        let size = leaf::<F, M>(mem, root, levels, va)
            .map(|(_, _, size)| size)
            .ok_or(fail(UnmapError::NotMapped { iova: va }))?;
        if size.bytes() > len - done {
            return Err(fail(UnmapError::PartialPage { iova: va, size }));
        }
        let gone = unmap::<F, M>(mem, root, levels, va).map_err(fail)?;
        gone.emptied
            .as_slice()
            .iter()
            .for_each(|&page| emptied(page));
        done += gone.size.bytes();
    }
    Ok(())
}

/// Where the leaf that maps `iova` sits: its table, its index in that table and its size.
fn leaf<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
) -> Option<(u64, usize, PageSize)> {
    if iova >= levels.limit() {
        return None;
    }
    let mut table = root;
    let mut level = levels.count();
    loop {
        let i = index(iova, level);
        let entry = mem.table(table)[i];
        if !F::is_present(entry) {
            return None;
        }
        if let Some(size) = leaf_size_of::<F>(entry, level) {
            return Some((table, i, size));
        }
        if level == 1 {
            return None;
        }
        table = entry & ADDR_MASK;
        level -= 1;
    }
}

/// The size of the leaf that maps `iova` in the tables rooted at `root`, or `None` when
/// nothing does.
pub fn leaf_size<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
) -> Option<PageSize> {
    leaf::<F, M>(mem, root, levels, iova).map(|(_, _, size)| size)
}

/// Changes the access of every page mapped in `len` bytes from `iova` to `access`, keeping
/// the addresses they map. The change takes effect for an IOMMU only after an IOTLB
/// invalidation that covers the range, since it may cache the old entries.
///
/// # Errors
///
/// [`UnmapError::OutOfRange`] or [`UnmapError::NotMapped`] for the first address in the range
/// that no page maps, [`UnmapError::InsideLargePage`] when the range starts inside a large
/// page, and [`UnmapError::PartialPage`] when it ends inside one. The pages before it are
/// changed; [`RangeError::done`] says how many bytes that is.
pub fn protect_range<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
    len: u64,
    access: Access,
) -> Result<(), RangeError<UnmapError>> {
    let mut done = 0;
    while done < len {
        let va = iova.wrapping_add(done);
        let fail = |error| RangeError { done, error };
        if va >= levels.limit() {
            return Err(fail(UnmapError::OutOfRange { iova: va }));
        }
        let (table, i, size) =
            leaf::<F, M>(mem, root, levels, va).ok_or(fail(UnmapError::NotMapped { iova: va }))?;
        if va & (size.bytes() - 1) != 0 {
            return Err(fail(UnmapError::InsideLargePage { iova: va, size }));
        }
        if size.bytes() > len - done {
            return Err(fail(UnmapError::PartialPage { iova: va, size }));
        }
        let entry = &mut mem.table(table)[i];
        *entry = F::with_access(*entry, access);
        done += size.bytes();
    }
    Ok(())
}

/// Walks the tables rooted at `root` for `iova`, returning the physical address and access a
/// device would get, or `None` when nothing is mapped or the leaf grants no access.
pub fn translate<F: EntryFormat, M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
) -> Option<(u64, Access)> {
    let (table, i, size) = leaf::<F, M>(mem, root, levels, iova)?;
    let entry = mem.table(table)[i];
    let access = F::access(entry)?;
    Some(((entry & ADDR_MASK) | (iova & (size.bytes() - 1)), access))
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

    /// Bit 0 read (and present), bit 1 write, bit 7 large leaf: VT-d-like.
    struct Rw;
    impl EntryFormat for Rw {
        fn is_present(entry: u64) -> bool {
            entry & 3 != 0
        }
        fn table_entry(next: u64, _level: u8) -> u64 {
            next | 3
        }
        fn leaf_entry(phys: u64, size: PageSize, access: Access) -> u64 {
            let large = if size == PageSize::Size4K { 0 } else { 0x80 };
            Self::with_access(phys | large, access)
        }
        fn is_leaf(entry: u64, level: u8) -> bool {
            level == 1 || entry & 0x80 != 0
        }
        fn access(entry: u64) -> Option<Access> {
            Access::from_bits(entry & 1 != 0, entry & 2 != 0)
        }
        fn with_access(entry: u64, access: Access) -> u64 {
            (entry & !3) | u64::from(access.read()) | (u64::from(access.write()) << 1)
        }
    }

    #[test]
    fn levels_and_sizes() {
        assert_eq!(Levels::Three.limit(), 1 << 39);
        assert_eq!(Levels::Five.bits(), 57);
        assert_eq!(PageSize::at_level(2), Some(PageSize::Size2M));
        assert_eq!(PageSize::at_level(4), None);
        assert_eq!(Access::from_bits(false, false), None);
        assert!(Access::Write.write() && !Access::Write.read());
    }

    #[test]
    fn map_walk_unmap() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let l = Levels::Four;
        map_range::<Rw, _>(
            &mut mem,
            root,
            l,
            0x1f_f000,
            0x8000_0000 - 0x1000,
            0x20_2000,
            Access::Write,
            LargePages::Up2M,
        )
        .unwrap();
        // A 4 KiB page, then 2 MiB and 4 KiB once the addresses align.
        assert_eq!(
            translate::<Rw, _>(&mut mem, root, l, 0x20_0123),
            Some((0x8000_0123, Access::Write))
        );
        assert_eq!(
            translate::<Rw, _>(&mut mem, root, l, 0x40_0000),
            Some((0x8020_0000, Access::Write))
        );
        assert_eq!(translate::<Rw, _>(&mut mem, root, l, 0x40_1000), None);
        protect_range::<Rw, _>(&mut mem, root, l, 0x20_0000, 0x20_0000, Access::Read).unwrap();
        assert_eq!(
            translate::<Rw, _>(&mut mem, root, l, 0x20_0000),
            Some((0x8000_0000, Access::Read))
        );
        let mut freed = Vec::new();
        unmap_range::<Rw, _>(&mut mem, root, l, 0x1f_f000, 0x20_2000, |p| freed.push(p)).unwrap();
        // Level 1 tables (two), level 2 and level 3 emptied; the root stays.
        assert_eq!(freed.len(), 4);
        assert!(mem.table(root).iter().all(|&e| e == 0));
    }

    #[test]
    fn errors() {
        let mut mem = VecMemory::new();
        let root = mem.alloc_table().unwrap();
        let l = Levels::Three;
        assert_eq!(
            map::<Rw, _>(
                &mut mem,
                root,
                l,
                1 << 39,
                0,
                PageSize::Size4K,
                Access::Read
            ),
            Err(MapError::OutOfRange { iova: 1 << 39 })
        );
        assert_eq!(
            map::<Rw, _>(
                &mut mem,
                root,
                l,
                0x1000,
                0x1800,
                PageSize::Size4K,
                Access::Read
            ),
            Err(MapError::Misaligned {
                iova: 0x1000,
                phys: 0x1800
            })
        );
        map::<Rw, _>(&mut mem, root, l, 0, 0, PageSize::Size2M, Access::Read).unwrap();
        assert_eq!(
            map::<Rw, _>(
                &mut mem,
                root,
                l,
                0x1000,
                0x1000,
                PageSize::Size4K,
                Access::Read
            ),
            Err(MapError::AlreadyMapped { iova: 0x1000 })
        );
        assert_eq!(
            unmap::<Rw, _>(&mut mem, root, l, 0x1000),
            Err(UnmapError::InsideLargePage {
                iova: 0x1000,
                size: PageSize::Size2M
            })
        );
        assert_eq!(
            unmap_range::<Rw, _>(&mut mem, root, l, 0, 0x1000, |_| {}),
            Err(RangeError {
                done: 0,
                error: UnmapError::PartialPage {
                    iova: 0,
                    size: PageSize::Size2M
                }
            })
        );
        mem.limit = mem.pages.len();
        assert_eq!(
            map::<Rw, _>(
                &mut mem,
                root,
                l,
                1 << 30,
                0,
                PageSize::Size4K,
                Access::Read
            ),
            Err(MapError::Alloc(OutOfPages))
        );
    }
}
