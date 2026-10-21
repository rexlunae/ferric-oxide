//! EPT paging-structure entries and the EPT pointer (SDM Vol. 3C, "The Extended Page Table
//! Mechanism").
//!
//! An EPT entry has read (bit 0), write (bit 1) and execute (bit 2) permissions, a memory type
//! in bits 5:3 and "ignore PAT" in bit 6 for leaves, a page-size bit 7 for 2 MiB and 1 GiB
//! leaves, accessed and dirty flags in bits 8 and 9, and the physical address of the next table
//! or the page in bits 51:12. The encoders here refuse the combinations the SDM calls EPT
//! misconfigurations (writable but not readable, execute-only without the capability, reserved
//! memory types) so the kernel cannot build one.

use thiserror::Error;

/// The page size.
pub const PAGE_SIZE: u64 = 4096;

/// The highest physical-address bit an EPT entry can hold, plus one.
pub const MAX_PHYS_BITS: u32 = 52;

const ADDR_MASK: u64 = ((1 << MAX_PHYS_BITS) - 1) & !(PAGE_SIZE - 1);
const READ: u64 = 1 << 0;
const WRITE: u64 = 1 << 1;
const EXECUTE: u64 = 1 << 2;
const IGNORE_PAT: u64 = 1 << 6;
const LARGE: u64 = 1 << 7;
const ACCESSED: u64 = 1 << 8;
const DIRTY: u64 = 1 << 9;

/// A memory type for EPT leaves and the EPT pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryType {
    /// Uncacheable (0).
    Uncacheable,
    /// Write-combining (1).
    WriteCombining,
    /// Write-through (4).
    WriteThrough,
    /// Write-protected (5).
    WriteProtected,
    /// Write-back (6).
    WriteBack,
}

impl MemoryType {
    const fn raw(self) -> u64 {
        match self {
            Self::Uncacheable => 0,
            Self::WriteCombining => 1,
            Self::WriteThrough => 4,
            Self::WriteProtected => 5,
            Self::WriteBack => 6,
        }
    }

    const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            0 => Some(Self::Uncacheable),
            1 => Some(Self::WriteCombining),
            4 => Some(Self::WriteThrough),
            5 => Some(Self::WriteProtected),
            6 => Some(Self::WriteBack),
            _ => None,
        }
    }
}

/// The level of an EPT paging structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// A page table: its entries map 4 KiB pages.
    Pt,
    /// A page directory: entries point to page tables or map 2 MiB pages.
    Pd,
    /// A page-directory-pointer table: entries point to page directories or map 1 GiB pages.
    Pdpt,
    /// The PML4 table: entries point to PDPTs.
    Pml4,
}

impl Level {
    /// The size a leaf at this level maps, or `None` for the PML4, which has no leaves.
    pub const fn leaf_size(self) -> Option<u64> {
        match self {
            Self::Pt => Some(PAGE_SIZE),
            Self::Pd => Some(PAGE_SIZE << 9),
            Self::Pdpt => Some(PAGE_SIZE << 18),
            Self::Pml4 => None,
        }
    }

    /// The bit position of this level's index in a guest-physical address.
    pub const fn shift(self) -> u32 {
        match self {
            Self::Pt => 12,
            Self::Pd => 21,
            Self::Pdpt => 30,
            Self::Pml4 => 39,
        }
    }

    /// This level's index (0..512) of `gpa`.
    pub const fn index(self, gpa: u64) -> usize {
        ((gpa >> self.shift()) & 0x1ff) as usize
    }
}

/// What a guest may do with a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Perms {
    /// Data reads.
    pub read: bool,
    /// Data writes.
    pub write: bool,
    /// Instruction fetches.
    pub execute: bool,
}

impl Perms {
    /// Read, write and execute.
    pub const ALL: Self = Self {
        read: true,
        write: true,
        execute: true,
    };

    const fn bits(self) -> u64 {
        (self.read as u64) | ((self.write as u64) << 1) | ((self.execute as u64) << 2)
    }
}

/// An EPT entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry(u64);

impl Entry {
    /// An entry that maps nothing.
    pub const EMPTY: Self = Self(0);

    /// The raw value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The entry `raw`, as read from a table.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// An entry pointing to the next-level table at physical address `table`. It grants all
    /// permissions, so the leaves alone decide.
    ///
    /// # Errors
    ///
    /// [`EptError::Unaligned`] or [`EptError::AddressTooWide`] for a bad address.
    pub const fn table(table: u64) -> Result<Self, EptError> {
        if let Err(error) = check_addr(table, PAGE_SIZE) {
            return Err(error);
        }
        Ok(Self(table | READ | WRITE | EXECUTE))
    }

    /// A leaf at `level` mapping the page at physical address `addr`.
    ///
    /// # Errors
    ///
    /// - [`EptError::NoLeafAtLevel`] at the PML4.
    /// - [`EptError::Unaligned`] or [`EptError::AddressTooWide`] for a bad address.
    /// - [`EptError::NoAccess`] when `perms` allows nothing (use [`Entry::EMPTY`]).
    /// - [`EptError::WriteWithoutRead`]: a misconfiguration.
    /// - [`EptError::ExecuteOnly`] when `perms` is execute-only and `execute_only_supported`
    ///   is false.
    pub const fn leaf(
        level: Level,
        addr: u64,
        perms: Perms,
        memory_type: MemoryType,
        ignore_pat: bool,
        execute_only_supported: bool,
    ) -> Result<Self, EptError> {
        let Some(size) = level.leaf_size() else {
            return Err(EptError::NoLeafAtLevel);
        };
        if let Err(error) = check_addr(addr, size) {
            return Err(error);
        }
        if !perms.read && !perms.write && !perms.execute {
            return Err(EptError::NoAccess);
        }
        if perms.write && !perms.read {
            return Err(EptError::WriteWithoutRead);
        }
        if perms.execute && !perms.read && !execute_only_supported {
            return Err(EptError::ExecuteOnly);
        }
        let large = if matches!(level, Level::Pt) { 0 } else { LARGE };
        let pat = if ignore_pat { IGNORE_PAT } else { 0 };
        Ok(Self(
            addr | perms.bits() | (memory_type.raw() << 3) | pat | large,
        ))
    }

    /// Whether the entry maps anything (any permission bit is set).
    pub const fn present(self) -> bool {
        self.0 & (READ | WRITE | EXECUTE) != 0
    }

    /// Whether this entry, at `level`, is a leaf rather than a pointer to a table.
    pub const fn is_leaf(self, level: Level) -> bool {
        match level {
            Level::Pt => true,
            Level::Pml4 => false,
            Level::Pd | Level::Pdpt => self.0 & LARGE != 0,
        }
    }

    /// The physical address of the next table or the page.
    pub const fn addr(self) -> u64 {
        self.0 & ADDR_MASK
    }

    /// The permissions.
    pub const fn perms(self) -> Perms {
        Perms {
            read: self.0 & READ != 0,
            write: self.0 & WRITE != 0,
            execute: self.0 & EXECUTE != 0,
        }
    }

    /// The memory type of a leaf, or `None` for a reserved value.
    pub const fn memory_type(self) -> Option<MemoryType> {
        MemoryType::from_raw((self.0 >> 3) & 7)
    }

    /// Whether the accessed flag is set (with EPT A/D flags enabled).
    pub const fn accessed(self) -> bool {
        self.0 & ACCESSED != 0
    }

    /// Whether the dirty flag is set (leaves, with EPT A/D flags enabled).
    pub const fn dirty(self) -> bool {
        self.0 & DIRTY != 0
    }

    /// The entry with the accessed and dirty flags cleared.
    pub const fn clean(self) -> Self {
        Self(self.0 & !(ACCESSED | DIRTY))
    }
}

/// The EPT pointer: the PML4 table's address, the walk length and options.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Eptp(u64);

impl Eptp {
    /// An EPT pointer for a 4-level structure at physical address `pml4`.
    ///
    /// # Errors
    ///
    /// [`EptError::Unaligned`] or [`EptError::AddressTooWide`] for a bad address, and
    /// [`EptError::PointerMemoryType`] for a memory type other than uncacheable or
    /// write-back.
    pub const fn new(
        pml4: u64,
        memory_type: MemoryType,
        accessed_dirty: bool,
    ) -> Result<Self, EptError> {
        if let Err(error) = check_addr(pml4, PAGE_SIZE) {
            return Err(error);
        }
        if !matches!(memory_type, MemoryType::Uncacheable | MemoryType::WriteBack) {
            return Err(EptError::PointerMemoryType);
        }
        // Bits 5:3 hold the page-walk length minus one: 3 for four levels.
        Ok(Self(
            pml4 | memory_type.raw() | (3 << 3) | ((accessed_dirty as u64) << 6),
        ))
    }

    /// The raw value for the EPT_POINTER field.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The PML4 table's address.
    pub const fn pml4(self) -> u64 {
        self.0 & ADDR_MASK
    }
}

const fn check_addr(addr: u64, align: u64) -> Result<(), EptError> {
    if addr & (align - 1) != 0 {
        return Err(EptError::Unaligned { addr, align });
    }
    if addr & !ADDR_MASK != 0 {
        return Err(EptError::AddressTooWide { addr });
    }
    Ok(())
}

/// An EPT entry or pointer that would be invalid or a misconfiguration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum EptError {
    /// The address is not aligned to the page or table size.
    #[error("address {addr:#x} is not aligned to {align:#x}")]
    Unaligned {
        /// The address.
        addr: u64,
        /// The alignment it needs.
        align: u64,
    },
    /// The address has bits above bit 51.
    #[error("address {addr:#x} does not fit an EPT entry")]
    AddressTooWide {
        /// The address.
        addr: u64,
    },
    /// The PML4 cannot hold a leaf.
    #[error("EPT PML4 entries cannot map pages")]
    NoLeafAtLevel,
    /// A leaf with no permission maps nothing; use an empty entry.
    #[error("an EPT leaf needs at least one permission")]
    NoAccess,
    /// Writable but not readable is an EPT misconfiguration.
    #[error("writable but not readable EPT entries are misconfigurations")]
    WriteWithoutRead,
    /// Execute-only needs the execute-only capability.
    #[error("the CPU does not support execute-only EPT entries")]
    ExecuteOnly,
    /// The EPT pointer's memory type must be uncacheable or write-back.
    #[error("the EPT pointer's memory type must be uncacheable or write-back")]
    PointerMemoryType,
}

#[cfg(test)]
mod tests {
    use super::*;

    const RW: Perms = Perms {
        read: true,
        write: true,
        execute: false,
    };

    #[test]
    fn leaves_and_tables_encode() {
        let page = Entry::leaf(
            Level::Pt,
            0x1234_5000,
            RW,
            MemoryType::WriteBack,
            false,
            false,
        )
        .expect("valid");
        assert_eq!(page.raw(), 0x1234_5000 | 0b11 | (6 << 3));
        assert!(page.present() && page.is_leaf(Level::Pt));
        assert_eq!(page.memory_type(), Some(MemoryType::WriteBack));
        assert_eq!(page.perms(), RW);
        let big = Entry::leaf(
            Level::Pd,
            0x4020_0000,
            Perms::ALL,
            MemoryType::WriteBack,
            true,
            false,
        )
        .expect("valid");
        assert!(big.is_leaf(Level::Pd));
        assert_eq!(big.raw() & (LARGE | IGNORE_PAT), LARGE | IGNORE_PAT);
        let table = Entry::table(0x9000).expect("valid");
        assert_eq!(table.raw(), 0x9007);
        assert!(!table.is_leaf(Level::Pd));
        assert_eq!(table.addr(), 0x9000);
        assert!(!Entry::EMPTY.present());
        let touched = Entry::from_raw(page.raw() | ACCESSED | DIRTY);
        assert!(touched.accessed() && touched.dirty());
        assert_eq!(touched.clean(), page);
    }

    #[test]
    fn misconfigurations_are_refused() {
        let wb = MemoryType::WriteBack;
        let write_only = Perms {
            read: false,
            write: true,
            execute: false,
        };
        let execute_only = Perms {
            read: false,
            write: false,
            execute: true,
        };
        let none = Perms {
            read: false,
            write: false,
            execute: false,
        };
        assert_eq!(
            Entry::leaf(Level::Pt, 0x1000, write_only, wb, false, true),
            Err(EptError::WriteWithoutRead)
        );
        assert_eq!(
            Entry::leaf(Level::Pt, 0x1000, execute_only, wb, false, false),
            Err(EptError::ExecuteOnly)
        );
        assert!(Entry::leaf(Level::Pt, 0x1000, execute_only, wb, false, true).is_ok());
        assert_eq!(
            Entry::leaf(Level::Pt, 0x1000, none, wb, false, true),
            Err(EptError::NoAccess)
        );
        assert_eq!(
            Entry::leaf(Level::Pml4, 0, RW, wb, false, false),
            Err(EptError::NoLeafAtLevel)
        );
        assert_eq!(
            Entry::leaf(Level::Pdpt, 0x20_0000, RW, wb, false, false),
            Err(EptError::Unaligned {
                addr: 0x20_0000,
                align: 1 << 30
            })
        );
        assert_eq!(
            Entry::table(1 << 52),
            Err(EptError::AddressTooWide { addr: 1 << 52 })
        );
        // A reserved memory type read back from a table decodes as none.
        assert_eq!(Entry::from_raw(2 << 3).memory_type(), None);
    }

    #[test]
    fn eptp_and_indices() {
        let eptp = Eptp::new(0x10_0000, MemoryType::WriteBack, true).expect("valid");
        assert_eq!(eptp.raw(), 0x10_0000 | 6 | (3 << 3) | (1 << 6));
        assert_eq!(eptp.pml4(), 0x10_0000);
        assert_eq!(
            Eptp::new(0x1000, MemoryType::WriteThrough, false),
            Err(EptError::PointerMemoryType)
        );
        let gpa = (5 << 39) | (4 << 30) | (3 << 21) | (2 << 12) | 0x123;
        assert_eq!(
            [Level::Pml4, Level::Pdpt, Level::Pd, Level::Pt].map(|level| level.index(gpa)),
            [5, 4, 3, 2]
        );
    }
}
