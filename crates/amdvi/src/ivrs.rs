//! The ACPI I/O Virtualization Reporting Structure, IVRS (chapter 5).
//!
//! After the standard 36-byte ACPI header, the IVRS holds the IVinfo word, eight reserved
//! bytes, and then a list of structures, each starting with a type byte and a 16-bit length:
//!
//! - **IVHD** (I/O virtualization hardware definition), types 10h, 11h and 40h: one IOMMU, its
//!   register base and PCI segment, and a list of *device entries* naming the device IDs whose
//!   requests it translates. Type 10h has a 24-byte header; 11h and 40h have a 40-byte one that
//!   also carries an image of the extended feature register, and 40h may name ACPI devices by
//!   their `_HID`. Firmware lists the same IOMMU once for each type it supports, and system
//!   software uses the highest type it understands and ignores the others for that IOMMU
//!   (section 5.2.2): [`Ivrs::units`] does that.
//! - **IVMD** (I/O virtualization memory definition), types 20h (all devices), 21h (one device)
//!   and 22h (a device ID range): memory the devices use on the firmware's behalf, which must be
//!   identity-mapped (unity) or excluded from translation.
//!
//! Device entries (section 5.2.2.3) are 4 bytes for types 00h-3Fh, 8 bytes for 40h-7Fh,
//! 16 bytes for 80h-BFh and 32 bytes for C0h-FFh, except type F0h (an ACPI device), whose
//! length depends on its UID. A *start of range* entry (03h, 43h, 47h) applies to every device
//! ID up to the next *end of range* entry (04h). *Alias* entries (42h, 43h) say that requests
//! from those devices reach the IOMMU with another device ID, typically that of a PCI Express
//! to PCI bridge, so it is the alias's device table entry that translates them.
//!
//! [`Ivrs::parse`] checks the header and checksum; walking the structures and entries checks
//! every length before using it, and unknown structure and entry types are skipped by their
//! length so newer firmware does not stop the walk.

use core::fmt;
use core::iter::FusedIterator;

use crate::DeviceId;

/// The table signature.
pub const SIGNATURE: [u8; 4] = *b"IVRS";

/// Bytes before the first structure: the ACPI header, IVinfo and 8 reserved bytes.
const HEADER_LEN: usize = 48;

/// Why the table, or a structure in it, could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IvrsError {
    /// The buffer is shorter than the IVRS header.
    TooShort {
        /// Bytes available.
        len: usize,
    },
    /// The signature is not `IVRS`.
    BadSignature([u8; 4]),
    /// The header's length is shorter than the header or longer than the buffer.
    BadLength {
        /// The length the header claims.
        claimed: u32,
        /// Bytes available.
        available: usize,
    },
    /// The bytes do not sum to zero.
    BadChecksum {
        /// What they sum to.
        sum: u8,
    },
    /// A structure's length runs past the table or is too short for its type.
    BadStructure {
        /// The structure's offset in the table.
        offset: usize,
        /// Its type byte.
        kind: u8,
    },
}

impl fmt::Display for IvrsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len } => write!(f, "IVRS table is only {len} bytes"),
            Self::BadSignature(sig) => write!(f, "signature {sig:02x?} is not IVRS"),
            Self::BadLength { claimed, available } => write!(
                f,
                "IVRS length {claimed} does not fit the {available} bytes available"
            ),
            Self::BadChecksum { sum } => write!(f, "IVRS checksum is off by {sum:#04x}"),
            Self::BadStructure { offset, kind } => write!(
                f,
                "IVRS structure of type {kind:#04x} at offset {offset:#x} has a bad length"
            ),
        }
    }
}

impl core::error::Error for IvrsError {}

/// Reads a little-endian integer of `N` bytes at `offset`.
fn le<const N: usize>(bytes: &[u8], offset: usize) -> Option<u64> {
    let raw: [u8; N] = bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()?;
    Some(
        raw.iter()
            .rev()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b)),
    )
}

fn u8_at(bytes: &[u8], offset: usize) -> Option<u8> {
    bytes.get(offset).copied()
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    le::<2>(bytes, offset).map(|v| v as u16)
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    le::<4>(bytes, offset).map(|v| v as u32)
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    le::<8>(bytes, offset)
}

/// The IVinfo word (section 5.2.1): the platform's virtualization address sizes and whether
/// the IVHDs of types 11h and 40h carry an extended feature register image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IvInfo(pub u32);

impl IvInfo {
    /// Bit 0, EFRSup: the IVHDs' EFR image fields are valid.
    pub const fn efr_supported(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1, DMA remapping support: firmware asks the OS to keep DMA remapping on, as it did
    /// before boot (pre-boot DMA protection).
    pub const fn dma_remap_requested(self) -> bool {
        self.0 & 2 != 0
    }

    /// Bits 7:5, the guest virtual address size (as an encoded value).
    pub const fn gva_size(self) -> u8 {
        ((self.0 >> 5) & 7) as u8
    }

    /// Bits 14:8, the largest physical address the IOMMUs can produce, in bits.
    pub const fn pa_size(self) -> u8 {
        ((self.0 >> 8) & 0x7f) as u8
    }

    /// Bits 21:15, the largest virtual address the IOMMUs translate, in bits.
    pub const fn va_size(self) -> u8 {
        ((self.0 >> 15) & 0x7f) as u8
    }

    /// Bit 22, HtAtsResv: the HyperTransport address range is reserved for ATS responses.
    pub const fn ht_ats_reserved(self) -> bool {
        self.0 & (1 << 22) != 0
    }
}

/// A parsed IVRS table.
#[derive(Clone, Copy, Debug)]
pub struct Ivrs<'a> {
    bytes: &'a [u8],
}

impl<'a> Ivrs<'a> {
    /// Checks the header, length and checksum of an IVRS table.
    ///
    /// `bytes` may extend past the table; only the length in its header is used.
    ///
    /// # Errors
    ///
    /// [`IvrsError::TooShort`], [`IvrsError::BadSignature`], [`IvrsError::BadLength`] and
    /// [`IvrsError::BadChecksum`] for a table that cannot be trusted.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, IvrsError> {
        if bytes.len() < HEADER_LEN {
            return Err(IvrsError::TooShort { len: bytes.len() });
        }
        let sig = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if sig != SIGNATURE {
            return Err(IvrsError::BadSignature(sig));
        }
        let claimed = u32_at(bytes, 4).ok_or(IvrsError::TooShort { len: bytes.len() })?;
        let bytes = usize::try_from(claimed)
            .ok()
            .filter(|&len| len >= HEADER_LEN)
            .and_then(|len| bytes.get(..len))
            .ok_or(IvrsError::BadLength {
                claimed,
                available: bytes.len(),
            })?;
        let sum = bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        if sum != 0 {
            return Err(IvrsError::BadChecksum { sum });
        }
        Ok(Self { bytes })
    }

    /// The raw table, header included.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The IVinfo word.
    pub fn info(&self) -> IvInfo {
        IvInfo(u32_at(self.bytes, 36).unwrap_or(0))
    }

    /// Iterates over the structures in table order. The iterator stops after the first
    /// structure whose length is bad, which it returns as an error.
    pub fn structures(&self) -> Structures<'a> {
        Structures {
            bytes: self.bytes,
            offset: HEADER_LEN,
            failed: false,
        }
    }

    /// The IOMMUs: for each one (by segment and register base), the IVHD of the highest type
    /// this crate understands, in the order the first of its IVHDs appears. Structures after a
    /// bad one are not seen.
    pub fn units(&self) -> impl Iterator<Item = Ivhd<'a>> + 'a {
        let all = self.structures();
        let ivhds = move || {
            all.clone().filter_map(|s| match s {
                Ok(Structure::Ivhd(ivhd)) => Some(ivhd),
                _ => None,
            })
        };
        ivhds().enumerate().filter_map(move |(i, ivhd)| {
            let same = |other: &Ivhd<'_>| other.segment == ivhd.segment && other.base == ivhd.base;
            let first = ivhds().position(|o| same(&o)) == Some(i);
            first.then(|| {
                ivhds()
                    .filter(|o| same(o))
                    .fold(ivhd, |best, o| if o.kind > best.kind { o } else { best })
            })
        })
    }

    /// The memory definitions.
    pub fn memory(&self) -> impl Iterator<Item = Ivmd> + 'a {
        self.structures().filter_map(|s| match s {
            Ok(Structure::Ivmd(ivmd)) => Some(ivmd),
            _ => None,
        })
    }
}

/// One IVRS structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Structure<'a> {
    /// An IOMMU definition.
    Ivhd(Ivhd<'a>),
    /// A memory definition.
    Ivmd(Ivmd),
    /// A type this crate does not know.
    Unknown {
        /// The type byte.
        kind: u8,
    },
}

/// The structures of an IVRS table, from [`Ivrs::structures`].
#[derive(Clone, Debug)]
pub struct Structures<'a> {
    bytes: &'a [u8],
    offset: usize,
    failed: bool,
}

impl<'a> Iterator for Structures<'a> {
    type Item = Result<Structure<'a>, IvrsError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.offset >= self.bytes.len() {
            return None;
        }
        let offset = self.offset;
        let kind = u8_at(self.bytes, offset)?;
        let bad = IvrsError::BadStructure { offset, kind };
        let body = u16_at(self.bytes, offset + 2)
            .map(usize::from)
            .filter(|&len| len >= 4)
            .and_then(|len| self.bytes.get(offset..offset.checked_add(len)?));
        let parsed = body.and_then(|body| Structure::parse(kind, body).map(|s| (s, body.len())));
        match parsed {
            Some((structure, len)) => {
                self.offset = offset + len;
                Some(Ok(structure))
            }
            None => {
                self.failed = true;
                Some(Err(bad))
            }
        }
    }
}

impl FusedIterator for Structures<'_> {}

impl<'a> Structure<'a> {
    /// Decodes one structure, or `None` when it is too short for its type.
    fn parse(kind: u8, body: &'a [u8]) -> Option<Self> {
        Some(match kind {
            0x10 | 0x11 | 0x40 => Self::Ivhd(Ivhd::parse(kind, body)?),
            0x20..=0x22 => Self::Ivmd(Ivmd::parse(kind, body)?),
            _ => Self::Unknown { kind },
        })
    }
}

/// The type of an IVHD, which fixes its header layout and the device entries it may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IvhdType {
    /// Type 10h: the original 24-byte header.
    Type10,
    /// Type 11h: a 40-byte header with the EFR image.
    Type11,
    /// Type 40h: as 11h, and device entries may name ACPI devices (type F0h).
    Type40,
}

impl IvhdType {
    /// The type byte.
    pub const fn code(self) -> u8 {
        match self {
            Self::Type10 => 0x10,
            Self::Type11 => 0x11,
            Self::Type40 => 0x40,
        }
    }

    /// Bytes before the first device entry.
    const fn header_len(self) -> usize {
        match self {
            Self::Type10 => 24,
            Self::Type11 | Self::Type40 => 40,
        }
    }
}

/// The IVHD flags byte (section 5.2.2.1): what the firmware asks software to program into the
/// control register, for type 10h; types 11h and 40h report the same bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IvhdFlags(pub u8);

impl IvhdFlags {
    /// Bit 0, HtTunEn: HyperTransport tunnel translation.
    pub const fn ht_tunnel(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1, PassPW.
    pub const fn pass_pw(self) -> bool {
        self.0 & 2 != 0
    }

    /// Bit 2, ResPassPW.
    pub const fn res_pass_pw(self) -> bool {
        self.0 & 4 != 0
    }

    /// Bit 3, Isoc: isochronous memory accesses for the IOMMU's own reads.
    pub const fn isoc(self) -> bool {
        self.0 & 8 != 0
    }

    /// Bit 4, IotlbSup: the IOMMU supports remote IOTLBs (ATS).
    pub const fn iotlb(self) -> bool {
        self.0 & 0x10 != 0
    }

    /// Bit 5, Coherent: the IOMMU's device table reads should be coherent.
    pub const fn coherent(self) -> bool {
        self.0 & 0x20 != 0
    }

    /// Bit 6, PreFSup: the IOMMU supports the prefetch command.
    pub const fn prefetch(self) -> bool {
        self.0 & 0x40 != 0
    }

    /// Bit 7, PPRSup: the IOMMU supports peripheral page requests.
    pub const fn ppr(self) -> bool {
        self.0 & 0x80 != 0
    }
}

/// An IOMMU definition block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ivhd<'a> {
    /// The block's type.
    pub kind: IvhdType,
    /// The flags byte.
    pub flags: IvhdFlags,
    /// The IOMMU's own PCI function.
    pub device_id: DeviceId,
    /// The offset of the IOMMU's capability block in its configuration space.
    pub capability_offset: u16,
    /// The physical address of the IOMMU's MMIO registers.
    pub base: u64,
    /// The PCI segment the IOMMU and its devices are in.
    pub segment: u16,
    /// The IOMMU info word: bits 4:0 the MSI message number, bits 12:8 the HyperTransport
    /// unit ID.
    pub info: u16,
    /// Type 10h: the IOMMU feature reporting word; types 11h and 40h: the IOMMU attributes.
    pub attributes: u32,
    /// Types 11h and 40h: the image of the extended feature register.
    pub efr: Option<u64>,
    entries: &'a [u8],
}

impl<'a> Ivhd<'a> {
    fn parse(kind: u8, body: &'a [u8]) -> Option<Self> {
        let kind = match kind {
            0x10 => IvhdType::Type10,
            0x11 => IvhdType::Type11,
            _ => IvhdType::Type40,
        };
        let header = kind.header_len();
        Some(Self {
            kind,
            flags: IvhdFlags(u8_at(body, 1)?),
            device_id: DeviceId(u16_at(body, 4)?),
            capability_offset: u16_at(body, 6)?,
            base: u64_at(body, 8)?,
            segment: u16_at(body, 16)?,
            info: u16_at(body, 18)?,
            attributes: u32_at(body, 20)?,
            efr: match kind {
                IvhdType::Type10 => None,
                _ => Some(u64_at(body, 24)?),
            },
            entries: body.get(header..)?,
        })
    }

    /// The raw device entries.
    pub fn entries(&self) -> DeviceEntries<'a> {
        DeviceEntries {
            bytes: self.entries,
            offset: 0,
            failed: false,
        }
    }

    /// The device ID ranges the IOMMU translates, with the alias and settings of each: device
    /// entries with range starts and ends paired up. The iterator stops after the first entry
    /// that cannot be read or paired, which it returns as an error.
    pub fn coverage(&self) -> CoverageIter<'a> {
        CoverageIter {
            entries: self.entries(),
            pending: None,
            done: false,
        }
    }

    /// How the IOMMU sees requests from `device`: `None` when no device entry names it. The
    /// last entry that names it wins, so a select entry after an "all" entry overrides it.
    /// Entries after one that cannot be read are not seen.
    pub fn lookup(&self, device: DeviceId) -> Option<Coverage> {
        self.coverage()
            .map_while(Result::ok)
            .filter(|c| c.contains(device))
            .last()
    }

    /// The special devices (I/O APICs and HPETs) the IVHD names.
    pub fn special_devices(&self) -> impl Iterator<Item = SpecialDevice> + 'a {
        self.entries()
            .map_while(Result::ok)
            .filter_map(|entry| match entry {
                DeviceEntry::Special(special) => Some(special),
                _ => None,
            })
    }
}

/// The DTE setting byte of a device entry (section 5.2.2.3): bits software copies into the
/// device table entries of the devices it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DteSetting(pub u8);

impl DteSetting {
    /// Bit 0, INITPass: INIT interrupts pass through.
    pub const fn init_pass(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1, EIntPass: ExtInt interrupts pass through.
    pub const fn eint_pass(self) -> bool {
        self.0 & 2 != 0
    }

    /// Bit 2, NMIPass: NMIs pass through.
    pub const fn nmi_pass(self) -> bool {
        self.0 & 4 != 0
    }

    /// Bits 5:4, SysMgt: how system management messages are handled.
    pub const fn sys_mgt(self) -> u8 {
        (self.0 >> 4) & 3
    }

    /// Bit 6, Lint0Pass.
    pub const fn lint0_pass(self) -> bool {
        self.0 & 0x40 != 0
    }

    /// Bit 7, Lint1Pass.
    pub const fn lint1_pass(self) -> bool {
        self.0 & 0x80 != 0
    }
}

/// What kind of special device a type 48h entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialVariety {
    /// 01h: an I/O APIC; the handle is its APIC ID.
    IoApic,
    /// 02h: an HPET; the handle is its number.
    Hpet,
    /// Any other value.
    Other(u8),
}

/// A special device (type 48h): an I/O APIC or HPET and the device ID its interrupts carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpecialDevice {
    /// The I/O APIC ID or HPET number.
    pub handle: u8,
    /// The device ID its requests carry.
    pub device: DeviceId,
    /// The DTE setting for it.
    pub setting: DteSetting,
    /// What it is.
    pub variety: SpecialVariety,
}

/// An ACPI device (type F0h): a device named by its ACPI `_HID`, `_CID` and `_UID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiDevice<'a> {
    /// The device ID its requests carry.
    pub device: DeviceId,
    /// The DTE setting for it.
    pub setting: DteSetting,
    /// The `_HID`, as 8 ASCII bytes.
    pub hid: [u8; 8],
    /// The `_CID`, as 8 bytes (zero when absent).
    pub cid: [u8; 8],
    /// The UID format: 0 none, 1 an integer, 2 a string.
    pub uid_format: u8,
    /// The UID bytes.
    pub uid: &'a [u8],
}

/// One device entry of an IVHD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceEntry<'a> {
    /// Type 00h: padding.
    Pad,
    /// Type 01h: every device ID.
    All {
        /// Their DTE setting.
        setting: DteSetting,
    },
    /// Type 02h: one device.
    Select {
        /// The device.
        device: DeviceId,
        /// Its DTE setting.
        setting: DteSetting,
    },
    /// Type 03h: the first device of a range that the next [`DeviceEntry::RangeEnd`] closes.
    RangeStart {
        /// The first device.
        device: DeviceId,
        /// The range's DTE setting.
        setting: DteSetting,
    },
    /// Type 04h: the last device of a range.
    RangeEnd {
        /// The last device.
        device: DeviceId,
    },
    /// Type 42h: one device whose requests carry `alias`.
    AliasSelect {
        /// The device.
        device: DeviceId,
        /// Its DTE setting.
        setting: DteSetting,
        /// The device ID its requests carry.
        alias: DeviceId,
    },
    /// Type 43h: the first device of a range whose requests carry `alias`.
    AliasRangeStart {
        /// The first device.
        device: DeviceId,
        /// The range's DTE setting.
        setting: DteSetting,
        /// The device ID their requests carry.
        alias: DeviceId,
    },
    /// Type 46h: one device with an extended setting.
    ExtendedSelect {
        /// The device.
        device: DeviceId,
        /// Its DTE setting.
        setting: DteSetting,
        /// The extended setting; bit 31 is AtsDisabled.
        extended: u32,
    },
    /// Type 47h: the first device of a range with an extended setting.
    ExtendedRangeStart {
        /// The first device.
        device: DeviceId,
        /// The range's DTE setting.
        setting: DteSetting,
        /// The extended setting; bit 31 is AtsDisabled.
        extended: u32,
    },
    /// Type 48h: an I/O APIC or HPET.
    Special(SpecialDevice),
    /// Type F0h: an ACPI device.
    Acpi(AcpiDevice<'a>),
    /// A type this crate does not know, skipped by its length.
    Unknown {
        /// The type byte.
        kind: u8,
    },
}

/// Why a device entry, or the coverage built from the entries, could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryError {
    /// The entry runs past the end of the IVHD.
    Truncated {
        /// The entry's offset from the first device entry.
        offset: usize,
        /// Its type byte.
        kind: u8,
    },
    /// An end-of-range entry without a range start before it.
    EndWithoutStart {
        /// The device the end names.
        device: DeviceId,
    },
    /// A range start not closed by an end-of-range entry before the next start or the end of
    /// the list.
    StartWithoutEnd {
        /// The device the start names.
        device: DeviceId,
    },
    /// A range whose end is below its start.
    Backwards {
        /// The first device.
        first: DeviceId,
        /// The last device.
        last: DeviceId,
    },
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { offset, kind } => write!(
                f,
                "IVHD device entry of type {kind:#04x} at offset {offset:#x} is truncated"
            ),
            Self::EndWithoutStart { device } => {
                write!(f, "IVHD range end at {device} has no start")
            }
            Self::StartWithoutEnd { device } => {
                write!(f, "IVHD range start at {device} has no end")
            }
            Self::Backwards { first, last } => {
                write!(f, "IVHD range {first}-{last} runs backwards")
            }
        }
    }
}

impl core::error::Error for EntryError {}

/// The device entries of an IVHD, from [`Ivhd::entries`].
#[derive(Clone, Debug)]
pub struct DeviceEntries<'a> {
    bytes: &'a [u8],
    offset: usize,
    failed: bool,
}

/// The length of a device entry of type `kind`, given its bytes (for type F0h).
fn entry_len(kind: u8, entry: &[u8]) -> Option<usize> {
    match kind {
        0xf0 => Some(22 + usize::from(u8_at(entry, 21)?)),
        0x00..=0x3f => Some(4),
        0x40..=0x7f => Some(8),
        0x80..=0xbf => Some(16),
        _ => Some(32),
    }
}

impl<'a> DeviceEntry<'a> {
    /// Decodes one entry from exactly its bytes.
    fn parse(kind: u8, e: &'a [u8]) -> Option<Self> {
        let device = DeviceId(u16_at(e, 1)?);
        let setting = DteSetting(u8_at(e, 3)?);
        Some(match kind {
            0x00 => Self::Pad,
            0x01 => Self::All { setting },
            0x02 => Self::Select { device, setting },
            0x03 => Self::RangeStart { device, setting },
            0x04 => Self::RangeEnd { device },
            0x42 => Self::AliasSelect {
                device,
                setting,
                alias: DeviceId(u16_at(e, 5)?),
            },
            0x43 => Self::AliasRangeStart {
                device,
                setting,
                alias: DeviceId(u16_at(e, 5)?),
            },
            0x46 => Self::ExtendedSelect {
                device,
                setting,
                extended: u32_at(e, 4)?,
            },
            0x47 => Self::ExtendedRangeStart {
                device,
                setting,
                extended: u32_at(e, 4)?,
            },
            0x48 => Self::Special(SpecialDevice {
                handle: u8_at(e, 4)?,
                device: DeviceId(u16_at(e, 5)?),
                setting,
                variety: match u8_at(e, 7)? {
                    1 => SpecialVariety::IoApic,
                    2 => SpecialVariety::Hpet,
                    other => SpecialVariety::Other(other),
                },
            }),
            0xf0 => Self::Acpi(AcpiDevice {
                device,
                setting,
                hid: e.get(4..12)?.try_into().ok()?,
                cid: e.get(12..20)?.try_into().ok()?,
                uid_format: u8_at(e, 20)?,
                uid: e.get(22..)?,
            }),
            _ => Self::Unknown { kind },
        })
    }
}

impl<'a> Iterator for DeviceEntries<'a> {
    type Item = Result<DeviceEntry<'a>, EntryError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.offset >= self.bytes.len() {
            return None;
        }
        let offset = self.offset;
        let rest = self.bytes.get(offset..)?;
        let kind = *rest.first()?;
        let entry = entry_len(kind, rest)
            .and_then(|len| rest.get(..len))
            .and_then(|e| Some((DeviceEntry::parse(kind, e)?, e.len())));
        match entry {
            Some((entry, len)) => {
                self.offset = offset + len;
                Some(Ok(entry))
            }
            None => {
                self.failed = true;
                Some(Err(EntryError::Truncated { offset, kind }))
            }
        }
    }
}

impl FusedIterator for DeviceEntries<'_> {}

/// A range of device IDs an IOMMU translates, from [`Ivhd::coverage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coverage {
    /// The first device ID.
    pub first: DeviceId,
    /// The last device ID, inclusive.
    pub last: DeviceId,
    /// The device ID the IOMMU sees on their requests, when it is not their own.
    pub alias: Option<DeviceId>,
    /// Their DTE setting.
    pub setting: DteSetting,
    /// The extended setting's AtsDisabled bit (bit 31): ATS must stay off for them.
    pub ats_disabled: bool,
}

impl Coverage {
    /// Whether `device` is in the range.
    pub fn contains(&self, device: DeviceId) -> bool {
        (self.first..=self.last).contains(&device)
    }

    /// The device ID whose device table entry translates requests from `device`: the alias,
    /// or `device` itself.
    pub fn requester(&self, device: DeviceId) -> DeviceId {
        self.alias.unwrap_or(device)
    }

    fn one(device: DeviceId, setting: DteSetting) -> Self {
        Self {
            first: device,
            last: device,
            alias: None,
            setting,
            ats_disabled: false,
        }
    }
}

/// A range start waiting for its end.
#[derive(Clone, Copy, Debug)]
struct Pending {
    first: DeviceId,
    alias: Option<DeviceId>,
    setting: DteSetting,
    ats_disabled: bool,
}

/// The coverage of an IVHD, from [`Ivhd::coverage`].
#[derive(Clone, Debug)]
pub struct CoverageIter<'a> {
    entries: DeviceEntries<'a>,
    pending: Option<Pending>,
    done: bool,
}

/// Bit 31 of an extended setting: ATS is disabled for the devices.
const ATS_DISABLED: u32 = 1 << 31;

impl CoverageIter<'_> {
    /// Opens a range, failing when one is already open.
    fn start(&mut self, start: Pending) -> Result<(), EntryError> {
        match self.pending.replace(start) {
            Some(open) => Err(EntryError::StartWithoutEnd { device: open.first }),
            None => Ok(()),
        }
    }

    /// What the next entry adds: `Ok(None)` for entries that cover nothing by themselves.
    fn step(&mut self, entry: DeviceEntry<'_>) -> Result<Option<Coverage>, EntryError> {
        let pending = |first, alias, setting, ats_disabled| Pending {
            first,
            alias,
            setting,
            ats_disabled,
        };
        match entry {
            DeviceEntry::All { setting } => Ok(Some(Coverage {
                first: DeviceId(0),
                last: DeviceId(0xffff),
                ..Coverage::one(DeviceId(0), setting)
            })),
            DeviceEntry::Select { device, setting } => Ok(Some(Coverage::one(device, setting))),
            DeviceEntry::AliasSelect {
                device,
                setting,
                alias,
            } => Ok(Some(Coverage {
                alias: Some(alias),
                ..Coverage::one(device, setting)
            })),
            DeviceEntry::ExtendedSelect {
                device,
                setting,
                extended,
            } => Ok(Some(Coverage {
                ats_disabled: extended & ATS_DISABLED != 0,
                ..Coverage::one(device, setting)
            })),
            DeviceEntry::RangeStart { device, setting } => {
                self.start(pending(device, None, setting, false))?;
                Ok(None)
            }
            DeviceEntry::AliasRangeStart {
                device,
                setting,
                alias,
            } => {
                self.start(pending(device, Some(alias), setting, false))?;
                Ok(None)
            }
            DeviceEntry::ExtendedRangeStart {
                device,
                setting,
                extended,
            } => {
                let ats = extended & ATS_DISABLED != 0;
                self.start(pending(device, None, setting, ats))?;
                Ok(None)
            }
            DeviceEntry::RangeEnd { device } => {
                let open = self
                    .pending
                    .take()
                    .ok_or(EntryError::EndWithoutStart { device })?;
                if device < open.first {
                    return Err(EntryError::Backwards {
                        first: open.first,
                        last: device,
                    });
                }
                Ok(Some(Coverage {
                    first: open.first,
                    last: device,
                    alias: open.alias,
                    setting: open.setting,
                    ats_disabled: open.ats_disabled,
                }))
            }
            // Special and ACPI devices carry their own device IDs, which the IOMMU translates
            // like any other.
            DeviceEntry::Special(special) => {
                Ok(Some(Coverage::one(special.device, special.setting)))
            }
            DeviceEntry::Acpi(acpi) => Ok(Some(Coverage::one(acpi.device, acpi.setting))),
            DeviceEntry::Pad | DeviceEntry::Unknown { .. } => Ok(None),
        }
    }
}

impl Iterator for CoverageIter<'_> {
    type Item = Result<Coverage, EntryError>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            let Some(entry) = self.entries.next() else {
                self.done = true;
                return self
                    .pending
                    .take()
                    .map(|open| Err(EntryError::StartWithoutEnd { device: open.first }));
            };
            match entry.and_then(|e| self.step(e)) {
                Ok(Some(coverage)) => return Some(Ok(coverage)),
                Ok(None) => {}
                Err(err) => {
                    self.done = true;
                    return Some(Err(err));
                }
            }
        }
        None
    }
}

impl FusedIterator for CoverageIter<'_> {}

/// Which devices an IVMD applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IvmdDevices {
    /// Type 20h: every device.
    All,
    /// Type 21h: one device.
    One(DeviceId),
    /// Type 22h: a range of device IDs, inclusive.
    Range {
        /// The first device.
        first: DeviceId,
        /// The last device.
        last: DeviceId,
    },
}

/// The IVMD flags byte (section 5.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IvmdFlags(pub u8);

impl IvmdFlags {
    /// Bit 0, Unity: the range must be identity-mapped for the devices.
    pub const fn unity(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1, IR: the devices read the range.
    pub const fn read(self) -> bool {
        self.0 & 2 != 0
    }

    /// Bit 2, IW: the devices write the range.
    pub const fn write(self) -> bool {
        self.0 & 4 != 0
    }

    /// Bit 3, ExclusionRange: requests to the range are not translated at all.
    pub const fn exclusion(self) -> bool {
        self.0 & 8 != 0
    }
}

/// An I/O virtualization memory definition: memory some devices use on the firmware's behalf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ivmd {
    /// The devices it applies to.
    pub devices: IvmdDevices,
    /// The flags.
    pub flags: IvmdFlags,
    /// The first byte.
    pub start: u64,
    /// The length in bytes.
    pub length: u64,
}

impl Ivmd {
    fn parse(kind: u8, body: &[u8]) -> Option<Self> {
        let device = DeviceId(u16_at(body, 4)?);
        Some(Self {
            devices: match kind {
                0x20 => IvmdDevices::All,
                0x21 => IvmdDevices::One(device),
                _ => IvmdDevices::Range {
                    first: device,
                    last: DeviceId(u16_at(body, 6)?),
                },
            },
            flags: IvmdFlags(u8_at(body, 1)?),
            start: u64_at(body, 16)?,
            length: u64_at(body, 24)?,
        })
    }

    /// Whether the definition applies to `device`.
    pub fn applies_to(&self, device: DeviceId) -> bool {
        match self.devices {
            IvmdDevices::All => true,
            IvmdDevices::One(one) => one == device,
            IvmdDevices::Range { first, last } => (first..=last).contains(&device),
        }
    }

    /// The range as whole 4 KiB pages, `[start, end)`, or `None` when it is empty or wraps.
    pub fn pages(&self) -> Option<core::ops::Range<u64>> {
        let start = self.start & !0xfff;
        let end = self.start.checked_add(self.length)?.checked_add(0xfff)? & !0xfff;
        (end > start).then_some(start..end)
    }

    /// Whether the devices need the range mapped one-to-one: unity ranges, and exclusion
    /// ranges, which this crate's users map one-to-one rather than program the exclusion
    /// registers.
    pub const fn needs_identity_map(&self) -> bool {
        self.flags.unity() || self.flags.exclusion()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    const QEMU_82_OFF: &[u8] = include_bytes!("../tests/fixtures/qemu-8.2-q35-intremap-off.ivrs");
    const QEMU_82_ON: &[u8] = include_bytes!("../tests/fixtures/qemu-8.2-q35-intremap-on.ivrs");
    const QEMU_102: &[u8] = include_bytes!("../tests/fixtures/qemu-10.2-q35-dma-remap.ivrs");
    const QEMU_102_XT: &[u8] = include_bytes!("../tests/fixtures/qemu-10.2-q35-intremap-xt.ivrs");

    /// The device IDs QEMU lists with CI's devices: host bridge, the IOMMU, NVMe, e1000, edu,
    /// LPC, AHCI and SMBus.
    const QEMU_DEVICES: [u16; 8] = [0x00, 0x08, 0x20, 0x28, 0x80, 0xf8, 0xfa, 0xfb];

    /// An IVRS header followed by `structures`, with the checksum fixed.
    fn table(info: u32, structures: &[&[u8]]) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend(*b"IVRS");
        t.extend([0; 4]); // length (patched)
        t.extend([2, 0]); // revision, checksum (patched)
        t.extend(*b"FERRIC");
        t.extend(*b"TESTIVRS");
        t.extend([0; 12]);
        t.extend(info.to_le_bytes());
        t.extend([0; 8]);
        for s in structures {
            t.extend(*s);
        }
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        reseal(&mut t);
        t
    }

    /// Fixes the checksum after a test edits a table.
    fn reseal(b: &mut [u8]) {
        b[9] = 0;
        let sum = b.iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[9] = 0u8.wrapping_sub(sum);
    }

    /// A type 40h IVHD for the IOMMU at `base` with `entries`.
    fn ivhd40(base: u64, entries: &[u8]) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend([0x40, 0x20]);
        h.extend(((40 + entries.len()) as u16).to_le_bytes());
        h.extend(0x0002u16.to_le_bytes()); // IOMMU at 00:00.2
        h.extend(0x40u16.to_le_bytes());
        h.extend(base.to_le_bytes());
        h.extend(1u16.to_le_bytes()); // segment 1
        h.extend(0x1f00u16.to_le_bytes());
        h.extend(0u32.to_le_bytes());
        h.extend(0x0000_0040_0000_0044u64.to_le_bytes());
        h.extend([0; 8]);
        h.extend(entries);
        h
    }

    #[test]
    fn qemu_82_tables() {
        let ivrs = Ivrs::parse(QEMU_82_OFF).unwrap();
        assert_eq!(ivrs.info().pa_size(), 40);
        assert!(!ivrs.info().efr_supported());
        let units = ivrs.units().collect::<Vec<_>>();
        assert_eq!(units.len(), 1);
        let unit = units[0];
        assert_eq!(unit.kind, IvhdType::Type10);
        assert_eq!(unit.base, 0xfed8_0000);
        assert_eq!(unit.device_id, DeviceId(0x0008));
        assert_eq!(unit.capability_offset, 0x40);
        assert_eq!(unit.efr, None);
        assert!(unit.flags.ht_tunnel() && unit.flags.iotlb() && !unit.flags.coherent());
        let covered = unit
            .coverage()
            .map(|c| c.unwrap().first.0)
            .collect::<Vec<_>>();
        assert_eq!(covered, QEMU_DEVICES);
        assert!(unit.lookup(DeviceId::new(0, 0x10, 0)).is_some());
        assert_eq!(unit.lookup(DeviceId::new(0, 0x11, 0)), None);
        assert_eq!(unit.special_devices().count(), 0);

        let on = Ivrs::parse(QEMU_82_ON).unwrap();
        let unit = on.units().next().unwrap();
        let ioapics = unit.special_devices().collect::<Vec<_>>();
        assert_eq!(
            ioapics,
            [SpecialDevice {
                handle: 0,
                device: DeviceId::new(0, 0x14, 0),
                setting: DteSetting(0),
                variety: SpecialVariety::IoApic,
            }]
        );
        // The I/O APIC's ID is covered too.
        assert!(unit.lookup(DeviceId(0xa0)).is_some());
    }

    #[test]
    fn qemu_102_prefers_type_11h() {
        for bytes in [QEMU_102, QEMU_102_XT] {
            let ivrs = Ivrs::parse(bytes).unwrap();
            assert!(ivrs.info().efr_supported());
            let kinds = ivrs
                .structures()
                .map(|s| match s.unwrap() {
                    Structure::Ivhd(h) => h.kind,
                    other => panic!("unexpected {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(kinds, [IvhdType::Type10, IvhdType::Type11]);
            let units = ivrs.units().collect::<Vec<_>>();
            assert_eq!(units.len(), 1);
            assert_eq!(units[0].kind, IvhdType::Type11);
            let efr = units[0].efr.unwrap();
            assert_eq!(efr & !4, 0x29d3);
            let covered = units[0]
                .coverage()
                .map(|c| c.unwrap().first.0)
                .filter(|&id| id != 0xa0)
                .collect::<Vec<_>>();
            assert_eq!(covered, QEMU_DEVICES);
        }
    }

    #[test]
    fn ranges_aliases_and_acpi_devices() {
        let mut entries = Vec::new();
        entries.extend([0x01, 0, 0, 0x30]); // all, SysMgt 3
        entries.extend([0x03, 0x00, 0x01, 0x00]); // 01:00.0 ..
        entries.extend([0x04, 0xff, 0x01, 0x00]); // .. 01:1f.7
        entries.extend([0x43, 0x00, 0x03, 0x00, 0, 0x08, 0x02, 0]); // 03:00.0 .. alias 02:01.0
        entries.extend([0x04, 0xff, 0x03, 0x00]); // .. 03:1f.7
        entries.extend([0x42, 0x10, 0x04, 0x07, 0, 0x08, 0x02, 0]); // 04:02.0 alias 02:01.0
        entries.extend([0x46, 0x18, 0x04, 0x00]); // 04:03.0 extended
        entries.extend((1u32 << 31).to_le_bytes());
        entries.extend([0x00, 0, 0, 0]); // pad
        entries.extend([0x48, 0, 0, 0xd7, 0x21, 0xa0, 0x00, 2]); // HPET 0x21 at 00:14.0
        entries.extend([0xf0, 0xa5, 0x00, 0x00]); // ACPI device at 00:14.5
        entries.extend(*b"AMDI0020");
        entries.extend([0; 8]);
        entries.extend([2, 3]); // string UID of 3 bytes
        entries.extend(*b"ID0");
        entries.extend([0x9f, 0, 0, 0]); // an unknown type: 80h-BFh entries are 16 bytes
        entries.extend([0; 12]);
        let ivrs_bytes = table(1, &[&ivhd40(0xfd00_0000, &entries)]);
        let ivrs = Ivrs::parse(&ivrs_bytes).unwrap();
        let unit = ivrs.units().next().unwrap();
        assert_eq!(unit.kind, IvhdType::Type40);
        assert_eq!(unit.segment, 1);
        assert_eq!(unit.efr, Some(0x0000_0040_0000_0044));
        let raw = unit.entries().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(raw.len(), 11);
        assert!(matches!(
            raw[9],
            DeviceEntry::Acpi(AcpiDevice {
                hid: [b'A', b'M', b'D', b'I', b'0', b'0', b'2', b'0'],
                uid_format: 2,
                uid: b"ID0",
                ..
            })
        ));
        assert_eq!(raw[10], DeviceEntry::Unknown { kind: 0x9f });
        let coverage = unit.coverage().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(coverage.len(), 7);

        let bridge = DeviceId::new(2, 1, 0);
        let behind = unit.lookup(DeviceId::new(3, 5, 1)).unwrap();
        assert_eq!(behind.requester(DeviceId::new(3, 5, 1)), bridge);
        let one = unit.lookup(DeviceId::new(4, 2, 0)).unwrap();
        assert_eq!(one.alias, Some(bridge));
        assert_eq!(one.setting, DteSetting(7));
        assert!(one.setting.init_pass() && one.setting.nmi_pass());
        assert!(unit.lookup(DeviceId::new(4, 3, 0)).unwrap().ats_disabled);
        // Only the "all" entry names 05:00.0.
        let any = unit.lookup(DeviceId::new(5, 0, 0)).unwrap();
        assert_eq!(
            any.requester(DeviceId::new(5, 0, 0)),
            DeviceId::new(5, 0, 0)
        );
        assert_eq!(any.setting.sys_mgt(), 3);
        let hpet = unit.special_devices().next().unwrap();
        assert_eq!((hpet.handle, hpet.variety), (0x21, SpecialVariety::Hpet));
        assert!(hpet.setting.lint0_pass() && hpet.setting.lint1_pass());
    }

    #[test]
    fn entry_errors() {
        let cases: [(&[u8], EntryError); 4] = [
            (
                &[0x04, 0x10, 0, 0],
                EntryError::EndWithoutStart {
                    device: DeviceId(0x10),
                },
            ),
            (
                &[0x03, 0x10, 0, 0, 0x03, 0x20, 0, 0],
                EntryError::StartWithoutEnd {
                    device: DeviceId(0x10),
                },
            ),
            (
                &[0x03, 0x10, 0, 0],
                EntryError::StartWithoutEnd {
                    device: DeviceId(0x10),
                },
            ),
            (
                &[0x03, 0x10, 0, 0, 0x04, 0x08, 0, 0],
                EntryError::Backwards {
                    first: DeviceId(0x10),
                    last: DeviceId(0x08),
                },
            ),
        ];
        for (entries, err) in cases {
            let bytes = table(0, &[&ivhd40(0x1000, entries)]);
            let ivrs = Ivrs::parse(&bytes).unwrap();
            let unit = ivrs.units().next().unwrap();
            assert_eq!(unit.coverage().last(), Some(Err(err)), "{entries:x?}");
        }
        // An 8-byte entry with only 4 bytes left.
        let bytes = table(0, &[&ivhd40(0x1000, &[0x02, 0, 0, 0, 0x42, 0, 0, 0])]);
        let ivrs = Ivrs::parse(&bytes).unwrap();
        let unit = ivrs.units().next().unwrap();
        assert_eq!(
            unit.entries().nth(1),
            Some(Err(EntryError::Truncated {
                offset: 4,
                kind: 0x42
            }))
        );
        // The select entry before the bad one still counts.
        assert!(unit.lookup(DeviceId(0)).is_some());
    }

    #[test]
    fn memory_definitions() {
        let ivmd = |kind: u8, flags: u8, id: u16, aux: u16, start: u64, len: u64| {
            let mut m = Vec::from([kind, flags]);
            m.extend(32u16.to_le_bytes());
            m.extend(id.to_le_bytes());
            m.extend(aux.to_le_bytes());
            m.extend([0; 8]);
            m.extend(start.to_le_bytes());
            m.extend(len.to_le_bytes());
            m
        };
        let all = ivmd(0x20, 0x07, 0, 0, 0x7f00_0000, 0x10_0000);
        let one = ivmd(0x21, 0x08, 0x0010, 0, 0x7e00_0800, 0x1000);
        let range = ivmd(0x22, 0x06, 0x0100, 0x01ff, 0x7d00_0000, 0);
        let unknown = [0x30, 0, 4, 0];
        let bytes = table(0, &[&all, &one, &range, &unknown]);
        let ivrs = Ivrs::parse(&bytes).unwrap();
        assert_eq!(ivrs.units().count(), 0);
        let defs = ivrs.memory().collect::<Vec<_>>();
        assert_eq!(defs.len(), 3);
        assert!(defs[0].applies_to(DeviceId(0x1234)) && defs[0].flags.unity());
        assert!(defs[0].flags.read() && defs[0].flags.write());
        assert_eq!(defs[0].pages(), Some(0x7f00_0000..0x7f10_0000));
        assert!(defs[1].applies_to(DeviceId(0x10)) && !defs[1].applies_to(DeviceId(0x11)));
        assert!(defs[1].needs_identity_map() && defs[1].flags.exclusion());
        // Rounded out to whole pages.
        assert_eq!(defs[1].pages(), Some(0x7e00_0000..0x7e00_2000));
        assert!(defs[2].applies_to(DeviceId(0x1ff)) && !defs[2].applies_to(DeviceId(0x200)));
        assert_eq!(defs[2].pages(), None);
        assert!(!defs[2].needs_identity_map());
        assert_eq!(
            ivrs.structures().nth(3),
            Some(Ok(Structure::Unknown { kind: 0x30 }))
        );
    }

    #[test]
    fn header_errors() {
        assert_eq!(
            Ivrs::parse(&QEMU_82_OFF[..40]).unwrap_err(),
            IvrsError::TooShort { len: 40 }
        );
        let mut bad = QEMU_82_OFF.to_vec();
        bad[0] = b'X';
        assert!(matches!(Ivrs::parse(&bad), Err(IvrsError::BadSignature(_))));
        let mut bad = QEMU_82_OFF.to_vec();
        bad[60] ^= 1;
        assert!(matches!(
            Ivrs::parse(&bad),
            Err(IvrsError::BadChecksum { .. })
        ));
        let mut bad = QEMU_82_OFF.to_vec();
        bad[4] = 200;
        assert_eq!(
            Ivrs::parse(&bad).unwrap_err(),
            IvrsError::BadLength {
                claimed: 200,
                available: 104
            }
        );
        // An IVHD whose length runs past the table.
        let mut bad = QEMU_82_OFF.to_vec();
        bad[50] = 0x60;
        reseal(&mut bad);
        let ivrs = Ivrs::parse(&bad).unwrap();
        assert_eq!(
            ivrs.structures().next(),
            Some(Err(IvrsError::BadStructure {
                offset: 48,
                kind: 0x10
            }))
        );
        assert_eq!(ivrs.structures().count(), 1);
        assert_eq!(ivrs.units().count(), 0);
    }
}
