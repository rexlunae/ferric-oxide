//! Host page tables (section 2.2.3).
//!
//! A device table entry with Mode 3, 4 or 5 names a root table of that many levels. Every
//! table is 4 KiB of 512 entries, 9 bits of IOVA per level, as on VT-d, so the walk is
//! `ferric_iommu::table`'s, with [`HostTables`] as the entry encoding:
//!
//! - bit 0, PR: present;
//! - bits 11:9, Next Level: in a table entry (PDE), the level of the table it points to, one
//!   below its own (this crate never skips levels); in a leaf (PTE), 0, which makes it a page
//!   of the default size for its level: 4 KiB at level 1, 2 MiB at level 2, 1 GiB at level 3
//!   (the "7h" encoding for other sizes is not used);
//! - bits 51:12: the next table or the page;
//! - bits 61 and 62, IR and IW: reads and writes allowed. They are checked at every level, so
//!   table entries set both and the leaf decides.
//!
//! The accessed and dirty bits (5, 6), FC (60) and U (59) stay clear.
//!
//! The functions here fix the encoding; the [`Levels`] they take come from the domain's paging
//! mode ([`crate::plan`]).

use ferric_iommu::table as radix;
pub use ferric_iommu::table::{
    ADDR_MASK, Access, EntryFormat, LargePages, Levels, MapError, PageSize, RangeError, Table,
    TableMemory, UnmapError, Unmapped,
};

/// Bit 0, PR: present.
pub const PR: u64 = 1 << 0;
/// Bits 11:9: the next level.
pub const NEXT_LEVEL_MASK: u64 = 7 << 9;
/// Bit 61, IR: reads allowed.
pub const IR: u64 = 1 << 61;
/// Bit 62, IW: writes allowed.
pub const IW: u64 = 1 << 62;

/// The IR and IW bits for `access`.
const fn access_bits(access: Access) -> u64 {
    match access {
        Access::Read => IR,
        Access::Write => IW,
        Access::ReadWrite => IR | IW,
    }
}

/// A table entry (PDE) in a level-`level` table pointing to the level-`level - 1` table at
/// `next`, granting reads and writes.
pub const fn pde(next: u64, level: u8) -> u64 {
    (next & ADDR_MASK) | (((level.saturating_sub(1)) as u64) << 9) | IR | IW | PR
}

/// A leaf (PTE) mapping `phys` with `access`, for a table of level `size.level()`: Next Level
/// 0, so the page has that level's default size.
pub const fn pte(phys: u64, access: Access) -> u64 {
    (phys & ADDR_MASK) | access_bits(access) | PR
}

/// The AMD-Vi host page-table entry encoding.
#[derive(Clone, Copy, Debug)]
pub struct HostTables;

impl EntryFormat for HostTables {
    fn is_present(entry: u64) -> bool {
        entry & PR != 0
    }

    fn table_entry(next: u64, level: u8) -> u64 {
        pde(next, level)
    }

    fn leaf_entry(phys: u64, _size: PageSize, access: Access) -> u64 {
        pte(phys, access)
    }

    fn is_leaf(entry: u64, level: u8) -> bool {
        // Next Level 0 is a page of the level's default size; 7 a page whose size the address
        // encodes, which is never built here and is treated as a leaf so no walk follows it.
        level == 1 || matches!(entry & NEXT_LEVEL_MASK, 0 | NEXT_LEVEL_MASK)
    }

    fn access(entry: u64) -> Option<Access> {
        Access::from_bits(entry & IR != 0, entry & IW != 0)
    }

    fn with_access(entry: u64, access: Access) -> u64 {
        (entry & !(IR | IW)) | access_bits(access)
    }
}

/// Maps `len` bytes from `iova` to `phys` with `access` in the tables rooted at `root`, using
/// the largest pages `large` allows wherever both addresses are aligned to them.
///
/// # Errors
///
/// As `ferric_iommu::table::map_range`: misaligned addresses or length, an IOVA beyond
/// `levels`, something already mapped, or no memory for a table. The pages before the failure
/// stay mapped; [`RangeError::done`] says how many bytes that is.
///
/// # Examples
///
/// ```
/// use ferric_amdvi::table::{map_range, translate, Access, LargePages, Levels, Table, TableMemory};
///
/// struct Pages(Vec<Box<Table>>);
/// impl TableMemory for Pages {
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
/// let mut mem = Pages(Vec::new());
/// let root = mem.alloc_table().unwrap();
/// map_range(&mut mem, root, Levels::Four, 0x10000, 0x200_0000, 0x2000, Access::ReadWrite, LargePages::Up2M)
///     .unwrap();
/// assert_eq!(translate(&mut mem, root, Levels::Four, 0x11008), Some((0x200_1008, Access::ReadWrite)));
/// ```
#[allow(clippy::too_many_arguments)]
pub fn map_range<M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
    phys: u64,
    len: u64,
    access: Access,
    large: LargePages,
) -> Result<(), RangeError<MapError<M::Error>>> {
    radix::map_range::<HostTables, M>(mem, root, levels, iova, phys, len, access, large)
}

/// Unmaps every page in `len` bytes from `iova`, calling `emptied` with each table page left
/// empty, which the caller frees only after an INVALIDATE_IOMMU_PAGES with PDE set has
/// completed.
///
/// # Errors
///
/// As `ferric_iommu::table::unmap_range`: an address nothing maps, or a range that ends inside
/// a large page. The pages before it are unmapped; [`RangeError::done`] says how many bytes
/// that is.
pub fn unmap_range<M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
    len: u64,
    emptied: impl FnMut(u64),
) -> Result<(), RangeError<UnmapError>> {
    radix::unmap_range::<HostTables, M>(mem, root, levels, iova, len, emptied)
}

/// Changes the access of every page mapped in `len` bytes from `iova` to `access`.
///
/// # Errors
///
/// As `ferric_iommu::table::protect_range`. The pages before the failure are changed;
/// [`RangeError::done`] says how many bytes that is.
pub fn protect_range<M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
    len: u64,
    access: Access,
) -> Result<(), RangeError<UnmapError>> {
    radix::protect_range::<HostTables, M>(mem, root, levels, iova, len, access)
}

/// Walks the tables rooted at `root` for `iova`, returning the physical address and access a
/// device would get, or `None` when nothing is mapped.
pub fn translate<M: TableMemory>(
    mem: &mut M,
    root: u64,
    levels: Levels,
    iova: u64,
) -> Option<(u64, Access)> {
    radix::translate::<HostTables, M>(mem, root, levels, iova)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::boxed::Box;
    use std::vec::Vec;

    use super::*;

    struct VecMemory(Vec<Box<Table>>);

    impl TableMemory for VecMemory {
        type Error = core::convert::Infallible;
        fn alloc_table(&mut self) -> Result<u64, Self::Error> {
            self.0.push(Box::new([0; 512]));
            Ok(self.0.len() as u64 * 4096)
        }
        fn table(&mut self, phys: u64) -> &mut Table {
            &mut self.0[phys as usize / 4096 - 1]
        }
    }

    #[test]
    fn entry_encodings() {
        // A level-4 PDE names a level-3 table.
        assert_eq!(pde(0x2000, 4), 0x6000_0000_0000_2601);
        assert_eq!(pde(0x3000, 2), 0x6000_0000_0000_3201);
        assert_eq!(pte(0x2000_0000, Access::ReadWrite), 0x6000_0000_2000_0001);
        assert_eq!(pte(0x2001_0000, Access::Read), 0x2000_0000_2001_0001);
        assert_eq!(pte(0x5000, Access::Write), 0x4000_0000_0000_5001);
        assert!(HostTables::is_leaf(pte(0, Access::Read), 2));
        assert!(!HostTables::is_leaf(pde(0x1000, 2), 2));
        assert!(HostTables::is_leaf(0x0e01, 3));
        assert_eq!(HostTables::access(PR), None);
    }

    /// The tables built for the probe that checked QEMU's walker: IOVA 0x10000 to 0x2000000
    /// read-write, 0x11000 read-only, and a 2 MiB page at 0x200000 (NL 0 at level 2).
    #[test]
    fn builds_the_tables_qemu_walks() {
        let mut mem = VecMemory(Vec::new());
        let root = mem.alloc_table().unwrap();
        let l = Levels::Four;
        let large = LargePages::Up2M;
        map_range(
            &mut mem,
            root,
            l,
            0x10000,
            0x200_0000,
            0x1000,
            Access::ReadWrite,
            large,
        )
        .unwrap();
        map_range(
            &mut mem,
            root,
            l,
            0x11000,
            0x200_1000,
            0x1000,
            Access::Read,
            large,
        )
        .unwrap();
        map_range(
            &mut mem,
            root,
            l,
            0x20_0000,
            0x220_0000,
            0x20_0000,
            Access::ReadWrite,
            large,
        )
        .unwrap();
        assert_eq!(mem.0.len(), 4);
        let (l3, l2, l1) = (2 * 4096, 3 * 4096, 4 * 4096);
        assert_eq!(mem.table(root)[0], pde(l3, 4));
        assert_eq!(mem.table(l3)[0], pde(l2, 3));
        assert_eq!(mem.table(l2)[0], pde(l1, 2));
        assert_eq!(mem.table(l2)[1], 0x6000_0000_0220_0001);
        assert_eq!(mem.table(l1)[0x10], 0x6000_0000_0200_0001);
        assert_eq!(mem.table(l1)[0x11], 0x2000_0000_0200_1001);
        assert_eq!(
            translate(&mut mem, root, l, 0x21_2345),
            Some((0x221_2345, Access::ReadWrite))
        );
        protect_range(&mut mem, root, l, 0x10000, 0x1000, Access::Read).unwrap();
        assert_eq!(
            translate(&mut mem, root, l, 0x10000),
            Some((0x200_0000, Access::Read))
        );
        let mut emptied = Vec::new();
        unmap_range(&mut mem, root, l, 0x10000, 0x2000, |p| emptied.push(p)).unwrap();
        // The level-2 table still holds the 2 MiB page.
        assert_eq!(emptied, [l1]);
        assert_eq!(translate(&mut mem, root, l, 0x11000), None);
    }
}
