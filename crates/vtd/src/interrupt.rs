//! Interrupt remapping (chapter 5, section 9.10 and register 10.4.31).
//!
//! With interrupt remapping on, a unit no longer delivers an interrupt request (a DMA write
//! to `0xFEEx_xxxx`) as the address and data say. A request in *remappable format* carries a
//! 16-bit handle instead of a destination and vector. The unit looks the handle up in the
//! interrupt remapping table (IRT), checks that the requester is the device the entry names,
//! and delivers the vector and destination the entry holds. A request in the old
//! *compatibility format* is passed through only while GCMD.CFI is set. The kernel leaves CFI
//! clear, so a device cannot reach any vector or CPU that nobody gave it, whatever its driver
//! writes into its MSI registers.
//!
//! - [`Irte`] encodes and decodes one 128-bit table entry (section 9.10, "Interrupt Remapping
//!   Table Entry (IRTE) for Remapped Interrupts"), including the source validation fields
//!   ([`SourceValidation`]) that bind the entry to a requester ID.
//! - [`TableSize`] and [`irta`] give the interrupt remapping table address register value
//!   (IRTA, 10.4.31), with EIME for 32-bit x2APIC destinations.
//! - [`remappable_msi`] gives the MSI address and data that name an entry (section 5.1.2.2,
//!   "Interrupt Requests in Remappable Format"), and [`MsiFormat::of`] decodes an MSI address.
//! - [`ioapic_rte`] encodes an I/O APIC redirection table entry in remappable format (section
//!   5.1.5.1, "I/OxAPIC Programming").
//! - [`plan`] decides whether the kernel can turn interrupt remapping on for a unit, and
//!   whether destinations use the extended (x2APIC) format.
//! - `IrteAllocator` (with the `alloc` feature) hands out table indices.
//!
//! Interrupt remapping faults are recorded in the same fault recording registers as DMA
//! faults ([`crate::fault`]); their reasons are codes 20h to 26h (appendix A).

use core::fmt;

use crate::dmar::DmarFlags;
use crate::regs::{ExtendedCapability, global};

/// The number of entries the kernel gives each unit's table: 4096 entries of 16 bytes, which
/// is 64 KiB.
pub const DEFAULT_ENTRIES: u32 = 4096;

/// The size of an interrupt remapping table: a power of two from 2 to 65536 entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableSize(u32);

impl TableSize {
    /// [`DEFAULT_ENTRIES`] entries.
    pub const DEFAULT: Self = Self(DEFAULT_ENTRIES);

    /// A table of `entries` entries, or `None` unless it is a power of two from 2 to 65536.
    pub const fn new(entries: u32) -> Option<Self> {
        if entries.is_power_of_two() && entries >= 2 && entries <= 65536 {
            Some(Self(entries))
        } else {
            None
        }
    }

    /// The number of entries.
    pub const fn entries(self) -> u32 {
        self.0
    }

    /// The table's size in bytes (16 per entry).
    pub const fn bytes(self) -> usize {
        self.0 as usize * 16
    }

    /// IRTA.S (bits 3:0): the table holds `2^(S + 1)` entries.
    pub const fn size_field(self) -> u64 {
        (self.0.trailing_zeros() - 1) as u64
    }
}

/// IRTA.EIME, bit 11: extended interrupt mode, 32-bit x2APIC destination IDs.
pub const IRTA_EIME: u64 = 1 << 11;

/// The IRTA register value for a table at the 4 KiB-aligned physical address `base` with
/// `size` entries; `extended` sets EIME.
///
/// # Examples
///
/// ```
/// use ferric_vtd::interrupt::{irta, TableSize};
/// assert_eq!(irta(0x1234_0000, TableSize::DEFAULT, true), 0x1234_080b);
/// ```
pub const fn irta(base: u64, size: TableSize, extended: bool) -> u64 {
    let eime = if extended { IRTA_EIME } else { 0 };
    (base & !0xfff) | eime | size.size_field()
}

/// Where an interrupt goes: a local APIC ID in the format the table uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    /// An 8-bit xAPIC ID (IRTA.EIME clear), held in bits 47:40 of the entry.
    XApic(u8),
    /// A 32-bit x2APIC ID (IRTA.EIME set), held in bits 63:32.
    X2Apic(u32),
}

impl Destination {
    /// The destination for APIC ID `id`: 32 bits wide when the table uses extended mode, else
    /// 8 bits, or `None` when `id` does not fit in 8 bits.
    pub const fn for_apic(id: u32, extended: bool) -> Option<Self> {
        if extended {
            Some(Self::X2Apic(id))
        } else if id <= 0xff {
            Some(Self::XApic(id as u8))
        } else {
            None
        }
    }

    /// The APIC ID.
    pub const fn apic_id(self) -> u32 {
        match self {
            Self::XApic(id) => id as u32,
            Self::X2Apic(id) => id,
        }
    }

    /// Bits 63:32 of the entry's low word.
    const fn field(self) -> u64 {
        match self {
            Self::XApic(id) => (id as u64) << 8,
            Self::X2Apic(id) => id as u64,
        }
    }
}

/// How the interrupt is signalled (IRTE.TM, bit 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerMode {
    /// Edge-triggered: MSI, MSI-X and edge I/O APIC pins.
    Edge,
    /// Level-triggered I/O APIC pins.
    Level,
}

/// How the destination APIC handles the interrupt (IRTE.DLM, bits 7:5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryMode {
    /// Deliver the vector to the destination (000b).
    Fixed,
    /// Deliver to the lowest-priority processor of a logical destination (001b).
    LowestPriority,
}

impl DeliveryMode {
    const fn bits(self) -> u64 {
        match self {
            Self::Fixed => 0,
            Self::LowestPriority => 1,
        }
    }
}

/// Which requests may use an entry (IRTE.SVT, SQ and SID; section 9.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceValidation {
    /// Any requester (SVT = 00b). The kernel does not use it.
    None,
    /// Only requests whose requester ID matches `source_id` (SVT = 01b). With
    /// `ignore_function_bits` of 1, 2 or 3 (SQ), that many low bits of the function number are
    /// not compared.
    RequesterId {
        /// The requester ID ([`crate::Bdf::source_id`]).
        source_id: u16,
        /// SQ, 0 to 3.
        ignore_function_bits: u8,
    },
    /// Only requests whose bus number is from `first` to `last` (SVT = 10b): devices behind a
    /// PCI Express to PCI bridge, whose requests may carry the bridge's IDs instead of their
    /// own.
    Bus {
        /// The lowest bus number.
        first: u8,
        /// The highest bus number.
        last: u8,
    },
}

impl SourceValidation {
    /// Bits 83:64 of the entry (the high word's bits 19:0).
    const fn field(self) -> u64 {
        match self {
            Self::None => 0,
            Self::RequesterId {
                source_id,
                ignore_function_bits,
            } => (1 << 18) | (((ignore_function_bits & 3) as u64) << 16) | source_id as u64,
            Self::Bus { first, last } => (2 << 18) | ((first as u64) << 8) | last as u64,
        }
    }

    /// Whether a request with requester ID `requester` passes the check, as the unit decides.
    pub const fn admits(self, requester: u16) -> bool {
        match self {
            Self::None => true,
            Self::RequesterId {
                source_id,
                ignore_function_bits,
            } => {
                let ignored = (1u16 << (ignore_function_bits & 3)) - 1;
                source_id & !ignored == requester & !ignored
            }
            Self::Bus { first, last } => {
                let bus = (requester >> 8) as u8;
                first <= bus && bus <= last
            }
        }
    }
}

/// One remapped-interrupt table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Irte {
    /// The vector delivered, 16 to 255.
    pub vector: u8,
    /// The destination APIC.
    pub destination: Destination,
    /// DM, bit 2: the destination is a logical APIC ID.
    pub logical: bool,
    /// RH, bit 3: redirection hint (lowest-priority among the logical destination).
    pub redirection_hint: bool,
    /// TM, bit 4.
    pub trigger: TriggerMode,
    /// DLM, bits 7:5.
    pub delivery: DeliveryMode,
    /// Which requesters may use the entry.
    pub source: SourceValidation,
    /// FPD, bit 1: do not record faults for requests that hit this entry.
    pub fault_processing_disable: bool,
}

/// Why a table entry does not decode as an [`Irte`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IrteError {
    /// IM (bit 15) is set: the entry is for posted interrupts.
    Posted,
    /// A reserved bit is set, or a field holds a reserved encoding.
    Reserved,
}

impl fmt::Display for IrteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Posted => "the entry is a posted-interrupt entry",
            Self::Reserved => "the entry has a reserved bit or encoding",
        })
    }
}

impl core::error::Error for IrteError {}

/// Bit 0 of the low word: present.
const IRTE_P: u64 = 1;
/// Bit 15 of the low word: IM, posted format.
const IRTE_IM: u64 = 1 << 15;
/// The low word's reserved bits in remapped format: 14:12 and 31:24. Bits 11:8 (AVAIL) are
/// for software and ignored.
const IRTE_LO_RESERVED: u64 = 0x0000_0000_ff00_7000;
/// The high word's reserved bits: everything above SVT.
const IRTE_HI_RESERVED: u64 = !0x000f_ffff;

impl Irte {
    /// A not-present entry: requests that name it fault (reason 22h).
    pub const NOT_PRESENT: [u64; 2] = [0, 0];

    /// An edge-triggered, fixed, physical-destination entry for an MSI or an edge I/O APIC
    /// pin, which only `source` may use.
    pub const fn fixed(vector: u8, destination: Destination, source: SourceValidation) -> Self {
        Self {
            vector,
            destination,
            logical: false,
            redirection_hint: false,
            trigger: TriggerMode::Edge,
            delivery: DeliveryMode::Fixed,
            source,
            fault_processing_disable: false,
        }
    }

    /// The entry's two 64-bit words, low first, with P set.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_vtd::interrupt::{Destination, Irte, SourceValidation};
    ///
    /// // Vector 0x30 to x2APIC 1, only for requester 00:04.0 (source-id 0x0020).
    /// let source = SourceValidation::RequesterId { source_id: 0x0020, ignore_function_bits: 0 };
    /// let irte = Irte::fixed(0x30, Destination::X2Apic(1), source);
    /// assert_eq!(irte.encode(), [0x0000_0001_0030_0001, 0x0004_0020]);
    /// ```
    pub const fn encode(&self) -> [u64; 2] {
        let lo = IRTE_P
            | ((self.fault_processing_disable as u64) << 1)
            | ((self.logical as u64) << 2)
            | ((self.redirection_hint as u64) << 3)
            | ((matches!(self.trigger, TriggerMode::Level) as u64) << 4)
            | (self.delivery.bits() << 5)
            | ((self.vector as u64) << 16)
            | (self.destination.field() << 32);
        [lo, self.source.field()]
    }

    /// Decodes an entry read back from a table whose destinations are extended (`extended`,
    /// IRTA.EIME) or not; `Ok(None)` for a not-present entry.
    ///
    /// # Errors
    ///
    /// [`IrteError::Posted`] for a posted-interrupt entry, and [`IrteError::Reserved`] when a
    /// reserved bit is set or the delivery mode, source validation type or (in xAPIC mode)
    /// destination uses a reserved encoding.
    pub const fn decode(words: [u64; 2], extended: bool) -> Result<Option<Self>, IrteError> {
        let [lo, hi] = words;
        if lo & IRTE_P == 0 {
            return Ok(None);
        }
        if lo & IRTE_IM != 0 {
            return Err(IrteError::Posted);
        }
        if lo & IRTE_LO_RESERVED != 0 || hi & IRTE_HI_RESERVED != 0 {
            return Err(IrteError::Reserved);
        }
        let delivery = match (lo >> 5) & 7 {
            0 => DeliveryMode::Fixed,
            1 => DeliveryMode::LowestPriority,
            _ => return Err(IrteError::Reserved),
        };
        let dst = lo >> 32;
        let destination = if extended {
            Destination::X2Apic(dst as u32)
        } else if dst & !0xff00 == 0 {
            Destination::XApic((dst >> 8) as u8)
        } else {
            return Err(IrteError::Reserved);
        };
        let source = match (hi >> 18) & 3 {
            0 => SourceValidation::None,
            1 => SourceValidation::RequesterId {
                source_id: hi as u16,
                ignore_function_bits: ((hi >> 16) & 3) as u8,
            },
            2 => SourceValidation::Bus {
                first: (hi >> 8) as u8,
                last: hi as u8,
            },
            _ => return Err(IrteError::Reserved),
        };
        let trigger = if lo & (1 << 4) != 0 {
            TriggerMode::Level
        } else {
            TriggerMode::Edge
        };
        Ok(Some(Self {
            vector: (lo >> 16) as u8,
            destination,
            logical: lo & (1 << 2) != 0,
            redirection_hint: lo & (1 << 3) != 0,
            trigger,
            delivery,
            source,
            fault_processing_disable: lo & (1 << 1) != 0,
        }))
    }
}

/// An MSI address and data pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiMessage {
    /// The message address.
    pub address: u64,
    /// The message data.
    pub data: u32,
}

/// The base of the interrupt address range, `0xFEE0_0000` (bits 31:20 = FEEh).
pub const INTERRUPT_ADDRESS_BASE: u64 = 0xfee0_0000;

/// MSI address bit 4: the request is in remappable format.
const MSI_REMAPPABLE: u64 = 1 << 4;
/// MSI address bit 3: SHV, the data's low 16 bits are a subhandle added to the handle.
const MSI_SHV: u64 = 1 << 3;

/// The MSI address and data that name table entry `handle`: the handle's bits 14:0 in address
/// bits 19:5, bit 15 in address bit 2, the format bit 4 set and SHV clear, so the data is
/// not used (a device with one MSI vector, or MSI-X, which has an address per vector).
///
/// # Examples
///
/// ```
/// use ferric_vtd::interrupt::{remappable_msi, MsiFormat};
///
/// let msg = remappable_msi(0x8003);
/// assert_eq!(msg.address, 0xfee0_0074);
/// assert_eq!(msg.data, 0);
/// assert_eq!(
///     MsiFormat::of(msg.address),
///     MsiFormat::Remappable { handle: 0x8003, subhandle_valid: false },
/// );
/// ```
pub const fn remappable_msi(handle: u16) -> MsiMessage {
    let low = (handle & 0x7fff) as u64;
    let high = (handle >> 15) as u64;
    MsiMessage {
        address: INTERRUPT_ADDRESS_BASE | (low << 5) | MSI_REMAPPABLE | (high << 2),
        data: 0,
    }
}

/// What an MSI address asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsiFormat {
    /// Compatibility format: the destination APIC ID is in address bits 19:12 and the vector in
    /// the data. Blocked while interrupt remapping is on with CFI clear.
    Compatibility {
        /// The 8-bit destination ID.
        destination: u8,
    },
    /// Remappable format, naming a table entry.
    Remappable {
        /// The table index (before any subhandle is added).
        handle: u16,
        /// SHV: the data's low 16 bits are added to the handle.
        subhandle_valid: bool,
    },
    /// Not in the interrupt address range: a DMA write, not an interrupt.
    NotInterrupt,
}

impl MsiFormat {
    /// Decodes `address`.
    pub const fn of(address: u64) -> Self {
        if address >> 20 != INTERRUPT_ADDRESS_BASE >> 20 {
            return Self::NotInterrupt;
        }
        if address & MSI_REMAPPABLE == 0 {
            return Self::Compatibility {
                destination: (address >> 12) as u8,
            };
        }
        let handle = (((address >> 5) & 0x7fff) | (((address >> 2) & 1) << 15)) as u16;
        Self::Remappable {
            handle,
            subhandle_valid: address & MSI_SHV != 0,
        }
    }
}

/// An I/O APIC redirection table entry in remappable format (section 5.1.5.1): the handle's
/// bits 14:0 in bits 63:49, the format bit 48 set, bit 15 of the handle in bit 11, and
/// `vector` and `trigger`, which must match the entry the handle names, in bits 7:0 and 15.
/// `active_low` is the pin's polarity (bit 13) and `masked` its mask (bit 16). The delivery
/// mode bits 10:8 stay 000b.
///
/// # Examples
///
/// ```
/// use ferric_vtd::interrupt::{ioapic_rte, TriggerMode};
/// assert_eq!(
///     ioapic_rte(0x8001, 0x45, TriggerMode::Edge, false, true),
///     0x0003_0000_0001_0845,
/// );
/// ```
pub const fn ioapic_rte(
    handle: u16,
    vector: u8,
    trigger: TriggerMode,
    active_low: bool,
    masked: bool,
) -> u64 {
    (((handle & 0x7fff) as u64) << 49)
        | (1 << 48)
        | ((masked as u64) << 16)
        | ((matches!(trigger, TriggerMode::Level) as u64) << 15)
        | ((active_low as u64) << 13)
        | (((handle >> 15) as u64) << 11)
        | vector as u64
}

/// Bit 16 of an I/O APIC redirection table entry: the pin is masked.
pub const RTE_MASKED: u64 = 1 << 16;

/// Bit 48 of an I/O APIC redirection table entry: the entry is in remappable format.
pub const RTE_REMAPPABLE: u64 = 1 << 48;

/// How the kernel will turn on interrupt remapping for a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptPlan {
    /// Use 32-bit (x2APIC) destination IDs: IRTA.EIME.
    pub extended: bool,
}

/// Why a unit's interrupt remapping stays off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InterruptSkip {
    /// The unit has no interrupt remapping (ECAP.IR clear).
    NotSupported,
    /// The DMAR table does not report interrupt remapping (INTR_REMAP flag clear).
    NotReported,
    /// The unit is not using queued invalidation, which interrupt remapping needs for its
    /// interrupt entry cache invalidations.
    NoQueue,
    /// Firmware left interrupt remapping on; taking over a live table is not supported.
    AlreadyEnabled,
}

impl fmt::Display for InterruptSkip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotSupported => "the unit has no interrupt remapping (ECAP.IR clear)",
            Self::NotReported => "the DMAR table does not report interrupt remapping",
            Self::NoQueue => "the unit does not use queued invalidation",
            Self::AlreadyEnabled => "firmware left interrupt remapping on",
        })
    }
}

/// Decides whether the kernel turns on interrupt remapping for a unit with extended
/// capabilities `ecap` and global status `gsts`, given the DMAR table's `flags`, whether the
/// unit uses queued invalidation (`queued`) and whether the local APICs run in x2APIC mode.
///
/// Extended (32-bit) destinations are used when the APICs are in x2APIC mode and the unit has
/// ECAP.EIM; otherwise destinations are 8-bit xAPIC IDs, which covers APIC IDs up to 255.
///
/// # Errors
///
/// The [`InterruptSkip`] reason to log.
///
/// # Examples
///
/// ```
/// use ferric_vtd::dmar::DmarFlags;
/// use ferric_vtd::interrupt::{plan, InterruptPlan, InterruptSkip};
/// use ferric_vtd::regs::ExtendedCapability;
///
/// // QEMU's intel-iommu with intremap=on and eim=on: IR (bit 3) and EIM (bit 4).
/// let ecap = ExtendedCapability(0xf5a);
/// assert_eq!(plan(ecap, DmarFlags(1), 0, true, true), Ok(InterruptPlan { extended: true }));
/// assert_eq!(plan(ecap, DmarFlags(0), 0, true, true), Err(InterruptSkip::NotReported));
/// ```
pub const fn plan(
    ecap: ExtendedCapability,
    flags: DmarFlags,
    gsts: u32,
    queued: bool,
    x2apic: bool,
) -> Result<InterruptPlan, InterruptSkip> {
    if !ecap.interrupt_remapping() {
        return Err(InterruptSkip::NotSupported);
    }
    if !flags.interrupt_remapping() {
        return Err(InterruptSkip::NotReported);
    }
    if gsts & global::IRE != 0 {
        return Err(InterruptSkip::AlreadyEnabled);
    }
    if !queued {
        return Err(InterruptSkip::NoQueue);
    }
    Ok(InterruptPlan {
        extended: x2apic && ecap.extended_interrupt_mode(),
    })
}

#[cfg(feature = "alloc")]
pub use self::allocator::{IrteAllocator, IrteFreeError};

#[cfg(feature = "alloc")]
mod allocator {
    use alloc::vec::Vec;
    use core::fmt;

    use super::TableSize;

    /// Hands out the indices of an interrupt remapping table, lowest free first.
    #[derive(Clone, Debug)]
    pub struct IrteAllocator {
        /// One bit per entry, set while it is in use.
        used: Vec<u64>,
        entries: u32,
        in_use: u32,
    }

    /// [`IrteAllocator::free`] was given an index that is not in use.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct IrteFreeError {
        /// The index.
        pub index: u16,
    }

    impl fmt::Display for IrteFreeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "interrupt remapping entry {} is not in use", self.index)
        }
    }

    impl core::error::Error for IrteFreeError {}

    impl IrteAllocator {
        /// An allocator for a table of `size` entries, all free.
        pub fn new(size: TableSize) -> Self {
            let words = size.entries().div_ceil(64) as usize;
            Self {
                used: alloc::vec![0; words],
                entries: size.entries(),
                in_use: 0,
            }
        }

        /// The lowest free index, now in use, or `None` when the table is full.
        ///
        /// # Examples
        ///
        /// ```
        /// use ferric_vtd::interrupt::{IrteAllocator, TableSize};
        ///
        /// let mut irtes = IrteAllocator::new(TableSize::new(2).unwrap());
        /// assert_eq!((irtes.alloc(), irtes.alloc(), irtes.alloc()), (Some(0), Some(1), None));
        /// irtes.free(0).unwrap();
        /// assert_eq!(irtes.alloc(), Some(0));
        /// ```
        pub fn alloc(&mut self) -> Option<u16> {
            let (word_index, word) = self
                .used
                .iter_mut()
                .enumerate()
                .find(|(_, w)| **w != u64::MAX)?;
            let bit = word.trailing_ones();
            let index = u32::try_from(word_index)
                .ok()?
                .checked_mul(64)?
                .checked_add(bit)?;
            if index >= self.entries {
                return None;
            }
            *word |= 1 << bit;
            self.in_use = self.in_use.saturating_add(1);
            u16::try_from(index).ok()
        }

        /// Gives `index` back.
        ///
        /// # Errors
        ///
        /// [`IrteFreeError`] when `index` is not in use.
        pub fn free(&mut self, index: u16) -> Result<(), IrteFreeError> {
            let bit = 1u64 << (index % 64);
            match self.used.get_mut(usize::from(index / 64)) {
                Some(word) if *word & bit != 0 => {
                    *word &= !bit;
                    self.in_use = self.in_use.saturating_sub(1);
                    Ok(())
                }
                _ => Err(IrteFreeError { index }),
            }
        }

        /// Whether `index` is in use.
        pub fn is_used(&self, index: u16) -> bool {
            self.used
                .get(usize::from(index / 64))
                .is_some_and(|w| w & (1 << (index % 64)) != 0)
        }

        /// How many entries are in use.
        pub fn in_use(&self) -> u32 {
            self.in_use
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXACT: SourceValidation = SourceValidation::RequesterId {
        source_id: 0x0080,
        ignore_function_bits: 0,
    };

    #[test]
    fn table_size_and_irta() {
        assert_eq!(TableSize::DEFAULT.size_field(), 11);
        assert_eq!(TableSize::DEFAULT.bytes(), 64 * 1024);
        assert_eq!(TableSize::new(2).map(TableSize::size_field), Some(0));
        assert_eq!(TableSize::new(65536).map(TableSize::size_field), Some(15));
        assert_eq!(TableSize::new(1), None);
        assert_eq!(TableSize::new(3), None);
        assert_eq!(TableSize::new(131072), None);
        assert_eq!(irta(0xabc_d000, TableSize::DEFAULT, false), 0xabc_d00b);
        // The base's low 12 bits are not part of the address.
        assert_eq!(irta(0xabc_dfff, TableSize::DEFAULT, false), 0xabc_d00b);
    }

    #[test]
    fn xapic_entry_layout() {
        // Vector 0x45 to xAPIC 3: the 8-bit ID sits in bits 47:40.
        let irte = Irte::fixed(0x45, Destination::XApic(3), EXACT);
        assert_eq!(irte.encode(), [0x0000_0300_0045_0001, 0x0004_0080]);
        assert_eq!(Irte::decode(irte.encode(), false), Ok(Some(irte)));
    }

    #[test]
    fn level_lowest_priority_logical_entry() {
        let irte = Irte {
            vector: 0xfe,
            destination: Destination::X2Apic(0x1234_5678),
            logical: true,
            redirection_hint: true,
            trigger: TriggerMode::Level,
            delivery: DeliveryMode::LowestPriority,
            source: SourceValidation::Bus { first: 2, last: 5 },
            fault_processing_disable: true,
        };
        // P, FPD, DM, RH, TM and DLM = 001b: 0x3f.
        assert_eq!(irte.encode(), [0x1234_5678_00fe_003f, 0x0008_0205]);
        assert_eq!(Irte::decode(irte.encode(), true), Ok(Some(irte)));
    }

    #[test]
    fn decode_rejects_bad_entries() {
        assert_eq!(Irte::decode(Irte::NOT_PRESENT, true), Ok(None));
        assert_eq!(
            Irte::decode([1 | (1 << 15), 0], true),
            Err(IrteError::Posted)
        );
        assert_eq!(
            Irte::decode([1 | (1 << 12), 0], true),
            Err(IrteError::Reserved)
        );
        assert_eq!(
            Irte::decode([1 | (1 << 24), 0], true),
            Err(IrteError::Reserved)
        );
        assert_eq!(Irte::decode([1, 1 << 20], true), Err(IrteError::Reserved));
        // SVT = 11b is reserved, and so is DLM = 010b (SMI) here.
        assert_eq!(Irte::decode([1, 3 << 18], true), Err(IrteError::Reserved));
        assert_eq!(
            Irte::decode([1 | (2 << 5), 0], true),
            Err(IrteError::Reserved)
        );
        // In xAPIC mode only bits 47:40 of the destination may be set.
        assert_eq!(
            Irte::decode([1 | (1 << 32), 0], false),
            Err(IrteError::Reserved)
        );
        // AVAIL (bits 11:8) belongs to software.
        let avail = Irte::decode([1 | (0xf << 8), 0], true);
        assert!(matches!(avail, Ok(Some(_))));
    }

    #[test]
    fn destinations() {
        assert_eq!(Destination::for_apic(7, false), Some(Destination::XApic(7)));
        assert_eq!(Destination::for_apic(256, false), None);
        assert_eq!(
            Destination::for_apic(256, true),
            Some(Destination::X2Apic(256))
        );
        assert_eq!(Destination::XApic(9).apic_id(), 9);
    }

    #[test]
    fn source_validation() {
        assert!(EXACT.admits(0x0080));
        assert!(!EXACT.admits(0x0081));
        assert!(!EXACT.admits(0x0020));
        let any_function = SourceValidation::RequesterId {
            source_id: 0x0080,
            ignore_function_bits: 3,
        };
        assert!(any_function.admits(0x0087));
        assert!(!any_function.admits(0x0088));
        let buses = SourceValidation::Bus { first: 4, last: 5 };
        assert!(buses.admits(0x0400) && buses.admits(0x05ff));
        assert!(!buses.admits(0x0300) && !buses.admits(0x0600));
        assert!(SourceValidation::None.admits(0xffff));
    }

    #[test]
    fn msi_formats() {
        for handle in [0, 1, 0x7fff, 0x8000, 0xffff, 4095] {
            let msg = remappable_msi(handle);
            assert_eq!(msg.address >> 20, 0xfee);
            assert_eq!(msg.address & 0b11, 0, "MSI addresses are dword aligned");
            assert_eq!(
                MsiFormat::of(msg.address),
                MsiFormat::Remappable {
                    handle,
                    subhandle_valid: false
                }
            );
        }
        assert_eq!(
            MsiFormat::of(0xfee0_3000),
            MsiFormat::Compatibility { destination: 3 }
        );
        assert!(matches!(
            MsiFormat::of(0xfee0_0018),
            MsiFormat::Remappable {
                handle: 0,
                subhandle_valid: true
            }
        ));
        assert_eq!(MsiFormat::of(0x1000), MsiFormat::NotInterrupt);
        assert_eq!(MsiFormat::of(0x1_fee0_0010), MsiFormat::NotInterrupt);
    }

    #[test]
    fn ioapic_entries() {
        let rte = ioapic_rte(5, 0x30, TriggerMode::Level, true, false);
        assert_eq!(
            rte,
            (5 << 49) | RTE_REMAPPABLE | (1 << 15) | (1 << 13) | 0x30
        );
        assert_eq!(rte & RTE_MASKED, 0);
        // Delivery mode (10:8) and the compatibility destination mode bit stay clear for a
        // handle below 0x8000.
        assert_eq!(rte & 0x0f00, 0);
    }

    #[test]
    fn plans() {
        let ir = ExtendedCapability(1 << 3);
        let ir_eim = ExtendedCapability((1 << 3) | (1 << 4));
        let on = DmarFlags(1);
        assert_eq!(
            plan(ExtendedCapability(0), on, 0, true, false),
            Err(InterruptSkip::NotSupported)
        );
        assert_eq!(
            plan(ir, DmarFlags(0), 0, true, false),
            Err(InterruptSkip::NotReported)
        );
        assert_eq!(
            plan(ir, on, global::IRE, true, false),
            Err(InterruptSkip::AlreadyEnabled)
        );
        assert_eq!(plan(ir, on, 0, false, false), Err(InterruptSkip::NoQueue));
        assert_eq!(
            plan(ir, on, 0, true, true),
            Ok(InterruptPlan { extended: false })
        );
        assert_eq!(
            plan(ir_eim, on, 0, true, false),
            Ok(InterruptPlan { extended: false })
        );
        assert_eq!(
            plan(ir_eim, on, 0, true, true),
            Ok(InterruptPlan { extended: true })
        );
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn allocator_hands_out_lowest_free() {
        let mut irtes = IrteAllocator::new(TableSize::new(128).unwrap());
        for expected in 0..128 {
            assert_eq!(irtes.alloc(), Some(expected));
        }
        assert_eq!(irtes.alloc(), None);
        assert_eq!(irtes.in_use(), 128);
        irtes.free(70).unwrap();
        irtes.free(3).unwrap();
        assert!(!irtes.is_used(3) && irtes.is_used(4));
        assert_eq!(irtes.free(3), Err(IrteFreeError { index: 3 }));
        assert_eq!(irtes.free(500), Err(IrteFreeError { index: 500 }));
        assert_eq!(irtes.alloc(), Some(3));
        assert_eq!(irtes.alloc(), Some(70));
        assert_eq!(irtes.in_use(), 128);
    }
}
