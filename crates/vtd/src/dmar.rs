//! The ACPI DMA Remapping Reporting table (DMAR).
//!
//! Firmware describes the VT-d hardware in one ACPI table, signature `DMAR` (section 8.1). After
//! the standard 36-byte ACPI header and a 12-byte DMAR header comes a list of remapping
//! structures, each starting with a 16-bit type and a 16-bit length (section 8.2):
//!
//! | Type | Structure | Section |
//! | --- | --- | --- |
//! | 0 | DMA Remapping Hardware Unit Definition (DRHD) | 8.3 |
//! | 1 | Reserved Memory Region Reporting (RMRR) | 8.4 |
//! | 2 | Root Port ATS Capability Reporting (ATSR) | 8.5 |
//! | 3 | Remapping Hardware Static Affinity (RHSA) | 8.6 |
//! | 4 | ACPI Name-space Device Declaration (ANDD) | 8.7 |
//! | 5 | SoC Integrated Address Translation Cache (SATC) | 8.8 |
//!
//! DRHD, RMRR, ATSR and SATC end in a list of device scopes (section 8.3.1), each naming a PCI
//! endpoint, a bridge and everything below it, an I/O APIC, an HPET or an ACPI namespace device.
//!
//! [`Dmar::parse`] checks the header and checksum; [`Dmar::structures`] walks the list without
//! copying, checking each length before using it. Unknown structure types are returned as
//! [`Structure::Unknown`] so newer firmware does not stop the walk.

use core::fmt;

use crate::Bdf;

/// The table's ACPI signature.
pub const SIGNATURE: [u8; 4] = *b"DMAR";

/// Bytes before the first remapping structure: the ACPI header (36) plus host address width,
/// flags and 10 reserved bytes.
pub const HEADER_LEN: usize = 48;

/// Why a DMAR table or one of its structures could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DmarError {
    /// The buffer is shorter than the DMAR header.
    TooShort {
        /// Bytes available.
        len: usize,
    },
    /// The signature is not `DMAR`.
    BadSignature([u8; 4]),
    /// The header's length field is below the header size or beyond the buffer.
    BadLength {
        /// The table length the header claims.
        claimed: u32,
        /// Bytes available.
        available: usize,
    },
    /// The bytes of the table do not sum to zero.
    BadChecksum {
        /// The sum of all bytes, modulo 256.
        sum: u8,
    },
    /// A remapping structure's header or length runs past the table, or its length is below
    /// the minimum for its type.
    BadStructure {
        /// Offset of the structure from the start of the table.
        offset: usize,
        /// Its type field.
        kind: u16,
        /// Its length field.
        length: u16,
    },
    /// A device scope's length is malformed or runs past its structure.
    BadScope {
        /// Offset of the scope from the start of its structure's scope list.
        offset: usize,
        /// Its length field.
        length: u8,
    },
}

impl fmt::Display for DmarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len } => write!(f, "DMAR table too short ({len} bytes)"),
            Self::BadSignature(sig) => write!(f, "signature {sig:02x?} is not DMAR"),
            Self::BadLength { claimed, available } => write!(
                f,
                "DMAR length {claimed} is invalid ({available} bytes available)"
            ),
            Self::BadChecksum { sum } => write!(f, "DMAR checksum is off by {sum:#04x}"),
            Self::BadStructure {
                offset,
                kind,
                length,
            } => write!(
                f,
                "remapping structure type {kind} at offset {offset:#x} has invalid length {length}"
            ),
            Self::BadScope { offset, length } => {
                write!(
                    f,
                    "device scope at offset {offset} has invalid length {length}"
                )
            }
        }
    }
}

impl core::error::Error for DmarError {}

/// Little-endian field readers that return `None` instead of panicking when out of range.
fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}
fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s: [u8; 4] = b.get(off..off.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(s))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    let s: [u8; 8] = b.get(off..off.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(s))
}

/// DMAR header flags (section 8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmarFlags(pub u8);

impl DmarFlags {
    /// Bit 0, INTR_REMAP: the platform supports interrupt remapping.
    pub const fn interrupt_remapping(self) -> bool {
        self.0 & 1 != 0
    }
    /// Bit 1, X2APIC_OPT_OUT: firmware asks the OS not to enable x2APIC mode with interrupt
    /// remapping.
    pub const fn x2apic_opt_out(self) -> bool {
        self.0 & 2 != 0
    }
    /// Bit 2, DMA_CTRL_PLATFORM_OPT_IN: firmware asks the OS to keep DMA remapping on, because
    /// it protected memory during boot (for example against hot-plugged Thunderbolt devices).
    pub const fn dma_control_opt_in(self) -> bool {
        self.0 & 4 != 0
    }
}

/// A validated DMAR table borrowed from firmware memory.
#[derive(Clone, Copy, Debug)]
pub struct Dmar<'a> {
    bytes: &'a [u8],
}

impl<'a> Dmar<'a> {
    /// Checks the header, length and checksum of a DMAR table.
    ///
    /// `bytes` may extend past the table; only the length in its header is used.
    ///
    /// # Errors
    ///
    /// [`DmarError::TooShort`], [`DmarError::BadSignature`], [`DmarError::BadLength`] or
    /// [`DmarError::BadChecksum`] when the header is not a valid DMAR header.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, DmarError> {
        if bytes.len() < HEADER_LEN {
            return Err(DmarError::TooShort { len: bytes.len() });
        }
        let sig = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if sig != SIGNATURE {
            return Err(DmarError::BadSignature(sig));
        }
        let claimed = u32_at(bytes, 4).ok_or(DmarError::TooShort { len: bytes.len() })?;
        let bytes = usize::try_from(claimed)
            .ok()
            .filter(|&len| len >= HEADER_LEN)
            .and_then(|len| bytes.get(..len))
            .ok_or(DmarError::BadLength {
                claimed,
                available: bytes.len(),
            })?;
        let sum = bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        if sum != 0 {
            return Err(DmarError::BadChecksum { sum });
        }
        Ok(Self { bytes })
    }

    /// The raw table, header included.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The maximum DMA physical address width the platform supports, in bits (the HAW field
    /// plus one).
    pub fn host_address_width(&self) -> u8 {
        self.bytes[36].saturating_add(1)
    }

    /// The header flags.
    pub fn flags(&self) -> DmarFlags {
        DmarFlags(self.bytes[37])
    }

    /// Iterates over the remapping structures in table order. The iterator stops after the
    /// first malformed structure, which it returns as an error.
    pub fn structures(&self) -> Structures<'a> {
        Structures {
            table: self.bytes,
            offset: HEADER_LEN,
        }
    }
}

/// Iterator over a DMAR table's remapping structures; see [`Dmar::structures`].
#[derive(Clone, Debug)]
pub struct Structures<'a> {
    table: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for Structures<'a> {
    type Item = Result<Structure<'a>, DmarError>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.offset;
        let rest = self.table.get(offset..).filter(|r| !r.is_empty())?;
        let (kind, length) = match (u16_at(rest, 0), u16_at(rest, 2)) {
            (Some(k), Some(l)) => (k, l),
            _ => {
                self.offset = self.table.len();
                return Some(Err(DmarError::BadStructure {
                    offset,
                    kind: u16_at(rest, 0).unwrap_or(0),
                    length: 0,
                }));
            }
        };
        let bad = DmarError::BadStructure {
            offset,
            kind,
            length,
        };
        let body = match rest.get(..usize::from(length)) {
            Some(body) if length >= 4 => body,
            _ => {
                self.offset = self.table.len();
                return Some(Err(bad));
            }
        };
        self.offset = offset + usize::from(length);
        let parsed = Structure::parse(kind, body).ok_or(bad);
        if parsed.is_err() {
            self.offset = self.table.len();
        }
        Some(parsed)
    }
}

/// One remapping structure from a DMAR table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Structure<'a> {
    /// A remapping hardware unit.
    Drhd(Drhd<'a>),
    /// A reserved memory region some devices keep using after boot.
    Rmrr(Rmrr<'a>),
    /// Root ports whose devices may use Address Translation Services.
    Atsr(Atsr<'a>),
    /// The NUMA node of a remapping unit.
    Rhsa(Rhsa),
    /// An ACPI namespace device that device scopes refer to by number.
    Andd(Andd<'a>),
    /// SoC-integrated devices with an address translation cache.
    Satc(Satc<'a>),
    /// A structure type this crate does not know.
    Unknown {
        /// Its type field.
        kind: u16,
        /// The whole structure, header included.
        bytes: &'a [u8],
    },
}

impl<'a> Structure<'a> {
    /// Parses a structure whose length has been checked to cover `body`. `None` when `body` is
    /// shorter than the fixed part of the type.
    fn parse(kind: u16, body: &'a [u8]) -> Option<Self> {
        Some(match kind {
            0 => Self::Drhd(Drhd {
                flags: u8_at(body, 4)?,
                size: u8_at(body, 5)?,
                segment: u16_at(body, 6)?,
                register_base: u64_at(body, 8)?,
                scopes: DeviceScopes::new(body.get(16..)?),
            }),
            1 => Self::Rmrr(Rmrr {
                segment: u16_at(body, 6)?,
                base: u64_at(body, 8)?,
                limit: u64_at(body, 16)?,
                scopes: DeviceScopes::new(body.get(24..)?),
            }),
            2 => Self::Atsr(Atsr {
                flags: u8_at(body, 4)?,
                segment: u16_at(body, 6)?,
                scopes: DeviceScopes::new(body.get(8..)?),
            }),
            3 => Self::Rhsa(Rhsa {
                register_base: u64_at(body, 8)?,
                proximity_domain: u32_at(body, 16)?,
            }),
            4 => Self::Andd(Andd {
                device_number: u8_at(body, 7)?,
                name: body.get(8..)?,
            }),
            5 => Self::Satc(Satc {
                flags: u8_at(body, 4)?,
                segment: u16_at(body, 6)?,
                scopes: DeviceScopes::new(body.get(8..)?),
            }),
            _ => Self::Unknown { kind, bytes: body },
        })
    }
}

/// DMA Remapping Hardware Unit Definition (section 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drhd<'a> {
    /// Flags; see [`Drhd::include_pci_all`].
    pub flags: u8,
    /// Register set size field (revision 3 and later; zero before): bits 3:0 give the size as
    /// a power of two of 4 KiB pages.
    pub size: u8,
    /// PCI segment the unit serves.
    pub segment: u16,
    /// Physical address of the unit's 4 KiB-aligned register set.
    pub register_base: u64,
    /// The devices the unit serves (or, with INCLUDE_PCI_ALL, I/O APICs and HPETs).
    pub scopes: DeviceScopes<'a>,
}

impl Drhd<'_> {
    /// Flag bit 0, INCLUDE_PCI_ALL: the unit serves every PCI device in its segment that no
    /// other unit lists.
    pub const fn include_pci_all(&self) -> bool {
        self.flags & 1 != 0
    }

    /// The size of the register set in 4 KiB pages, as the size field reports it (at least 1).
    pub const fn register_pages(&self) -> u32 {
        1 << (self.size & 0xf)
    }
}

/// Reserved Memory Region Reporting structure (section 8.4): memory that the listed devices
/// may DMA into at any time (USB legacy emulation buffers, integrated graphics stolen memory),
/// so it must stay identity-mapped for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rmrr<'a> {
    /// PCI segment of the devices.
    pub segment: u16,
    /// First byte of the region (4 KiB-aligned).
    pub base: u64,
    /// Last byte of the region (inclusive; `limit + 1` is 4 KiB-aligned).
    pub limit: u64,
    /// The devices that use the region.
    pub scopes: DeviceScopes<'a>,
}

impl Rmrr<'_> {
    /// One past the last byte of the region, or `None` if the region is empty or ends at the
    /// top of the 64-bit space.
    pub fn end(&self) -> Option<u64> {
        self.limit.checked_add(1).filter(|&end| end > self.base)
    }
}

/// Root Port ATS Capability Reporting structure (section 8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Atsr<'a> {
    /// Flags; see [`Atsr::all_ports`].
    pub flags: u8,
    /// PCI segment.
    pub segment: u16,
    /// The root ports that support ATS, unless [`Atsr::all_ports`] is set.
    pub scopes: DeviceScopes<'a>,
}

impl Atsr<'_> {
    /// Flag bit 0, ALL_PORTS: every root port in the segment supports ATS.
    pub const fn all_ports(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// Remapping Hardware Static Affinity structure (section 8.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rhsa {
    /// Register base of the unit this describes.
    pub register_base: u64,
    /// Its ACPI proximity domain (NUMA node).
    pub proximity_domain: u32,
}

/// ACPI Name-space Device Declaration structure (section 8.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Andd<'a> {
    /// The number device scopes of type [`ScopeKind::AcpiNamespaceDevice`] use as their
    /// enumeration ID.
    pub device_number: u8,
    /// The ACPI object name, NUL-terminated.
    pub name: &'a [u8],
}

/// SoC Integrated Address Translation Cache reporting structure (section 8.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Satc<'a> {
    /// Flags; bit 0 (ATC_REQUIRED) means the devices need their ATC enabled to work.
    pub flags: u8,
    /// PCI segment.
    pub segment: u16,
    /// The devices.
    pub scopes: DeviceScopes<'a>,
}

/// The kind of device a [`DeviceScope`] names (section 8.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeKind {
    /// 1: one PCI endpoint function.
    PciEndpoint,
    /// 2: a PCI-PCI bridge and every device below it.
    PciSubHierarchy,
    /// 3: an I/O APIC; the enumeration ID is its APIC ID.
    IoApic,
    /// 4: an MSI-capable HPET; the enumeration ID is its HPET number.
    Hpet,
    /// 5: an ACPI namespace device declared by an [`Andd`].
    AcpiNamespaceDevice,
    /// Another value.
    Unknown(u8),
}

impl From<u8> for ScopeKind {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::PciEndpoint,
            2 => Self::PciSubHierarchy,
            3 => Self::IoApic,
            4 => Self::Hpet,
            5 => Self::AcpiNamespaceDevice,
            v => Self::Unknown(v),
        }
    }
}

/// The device scope list at the end of a structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceScopes<'a> {
    bytes: &'a [u8],
}

impl<'a> DeviceScopes<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Iterates over the scopes. The iterator stops after the first malformed scope, which it
    /// returns as an error.
    pub fn iter(&self) -> DeviceScopeIter<'a> {
        DeviceScopeIter {
            bytes: self.bytes,
            offset: 0,
        }
    }
}

impl<'a> IntoIterator for &DeviceScopes<'a> {
    type Item = Result<DeviceScope<'a>, DmarError>;
    type IntoIter = DeviceScopeIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over a [`DeviceScopes`] list.
#[derive(Clone, Debug)]
pub struct DeviceScopeIter<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for DeviceScopeIter<'a> {
    type Item = Result<DeviceScope<'a>, DmarError>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.offset;
        let rest = self.bytes.get(offset..).filter(|r| !r.is_empty())?;
        let length = u8_at(rest, 1).unwrap_or(0);
        // Six header bytes, then at least one two-byte (device, function) path entry.
        let scope = rest
            .get(..usize::from(length))
            .filter(|s| s.len() >= 8 && s.len() % 2 == 0);
        let Some(scope) = scope else {
            self.offset = self.bytes.len();
            return Some(Err(DmarError::BadScope { offset, length }));
        };
        self.offset = offset + scope.len();
        Some(Ok(DeviceScope {
            kind: ScopeKind::from(scope[0]),
            flags: scope[2],
            enumeration_id: scope[4],
            start_bus: scope[5],
            path: &scope[6..],
        }))
    }
}

/// One device scope entry (section 8.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceScope<'a> {
    /// What the scope names.
    pub kind: ScopeKind,
    /// Flags (revision 4: bit 0 marks a device that requires the ATS capability). Zero in
    /// older tables.
    pub flags: u8,
    /// APIC ID, HPET number or ACPI device number, depending on [`DeviceScope::kind`].
    pub enumeration_id: u8,
    /// The bus number the path starts on.
    pub start_bus: u8,
    path: &'a [u8],
}

impl DeviceScope<'_> {
    /// The (device, function) hops from [`DeviceScope::start_bus`]: every hop but the last is a
    /// bridge, and the last is the device itself.
    pub fn path(&self) -> impl Iterator<Item = (u8, u8)> + '_ {
        self.path.chunks_exact(2).map(|hop| (hop[0], hop[1]))
    }

    /// The device's BDF when the path has a single hop, so no bridge has to be read.
    pub fn direct_bdf(&self) -> Option<Bdf> {
        match self.path {
            [device, function] => Some(Bdf::new(self.start_bus, *device, *function)),
            _ => None,
        }
    }

    /// Follows the path through bridges to the device's BDF. `secondary_bus` reads the
    /// secondary bus number register (configuration offset 0x19) of a bridge; it returns
    /// `None` when it cannot, which ends the walk.
    pub fn resolve(&self, mut secondary_bus: impl FnMut(Bdf) -> Option<u8>) -> Option<Bdf> {
        let mut bus = self.start_bus;
        let mut hops = self.path().peekable();
        while let Some((device, function)) = hops.next() {
            let bdf = Bdf::new(bus, device, function);
            if hops.peek().is_none() {
                return Some(bdf);
            }
            bus = secondary_bus(bdf)?;
        }
        None
    }

    /// Whether this scope names `bdf`, once the bridges on its path are resolved through
    /// `bridge_buses`, which gives a bridge's secondary and subordinate bus numbers (from its
    /// configuration space; `None` when unknown). An endpoint scope names the device its path
    /// leads to; a sub-hierarchy scope names that bridge and every device on the buses behind
    /// it (section 8.3.1). Scopes of other kinds name no PCI device.
    ///
    /// Returns `None` when the answer needs a bridge that `bridge_buses` does not know.
    pub fn covers(&self, bdf: Bdf, bridge_buses: impl Fn(Bdf) -> Option<BusRange>) -> Option<bool> {
        if !matches!(
            self.kind,
            ScopeKind::PciEndpoint | ScopeKind::PciSubHierarchy
        ) {
            return Some(false);
        }
        let target = self.resolve(|bridge| bridge_buses(bridge).map(|r| r.secondary))?;
        if target == bdf {
            return Some(true);
        }
        match self.kind {
            ScopeKind::PciSubHierarchy => bridge_buses(target).map(|r| r.contains(bdf.bus)),
            _ => Some(false),
        }
    }
}

/// The buses behind a PCI-to-PCI bridge: its secondary bus number up to its subordinate bus
/// number (configuration offsets 0x19 and 0x1A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusRange {
    /// The bus directly behind the bridge.
    pub secondary: u8,
    /// The highest bus behind the bridge.
    pub subordinate: u8,
}

impl BusRange {
    /// Whether `bus` is behind the bridge.
    pub const fn contains(&self, bus: u8) -> bool {
        self.secondary <= bus && bus <= self.subordinate
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::*;

    const QEMU_OFF: &[u8] = include_bytes!("../tests/fixtures/qemu-8.2-q35-intremap-off.dmar");
    const QEMU_IR: &[u8] = include_bytes!("../tests/fixtures/qemu-8.2-q35-intremap-on-iotlb.dmar");

    fn all<'a>(dmar: &Dmar<'a>) -> Vec<Structure<'a>> {
        dmar.structures().collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn qemu_intremap_off() {
        let dmar = Dmar::parse(QEMU_OFF).unwrap();
        assert_eq!(dmar.host_address_width(), 39);
        assert!(!dmar.flags().interrupt_remapping());
        let s = all(&dmar);
        assert_eq!(s.len(), 1);
        let Structure::Drhd(drhd) = s[0] else {
            panic!("{:?}", s[0])
        };
        assert_eq!(drhd.register_base, 0xfed9_0000);
        assert_eq!(drhd.segment, 0);
        assert!(!drhd.include_pci_all());
        assert_eq!(drhd.register_pages(), 1);
        let scopes: Vec<_> = drhd.scopes.iter().map(Result::unwrap).collect();
        assert_eq!(scopes[0].kind, ScopeKind::IoApic);
        assert_eq!(scopes[0].start_bus, 0xff);
        let bdfs: Vec<_> = scopes[1..]
            .iter()
            .map(|s| {
                assert_eq!(s.kind, ScopeKind::PciEndpoint);
                s.direct_bdf().unwrap()
            })
            .collect();
        // Host bridge, the NVMe disk, the e1000 NIC and the ICH9 LPC, SATA and SMBus functions.
        assert_eq!(
            bdfs,
            [
                Bdf::new(0, 0, 0),
                Bdf::new(0, 1, 0),
                Bdf::new(0, 2, 0),
                Bdf::new(0, 0x1f, 0),
                Bdf::new(0, 0x1f, 2),
                Bdf::new(0, 0x1f, 3),
            ]
        );
    }

    #[test]
    fn qemu_intremap_on_with_atsr() {
        let dmar = Dmar::parse(QEMU_IR).unwrap();
        assert_eq!(dmar.host_address_width(), 48);
        assert!(dmar.flags().interrupt_remapping());
        let s = all(&dmar);
        assert_eq!(s.len(), 2);
        assert!(matches!(s[0], Structure::Drhd(d) if d.register_base == 0xfed9_0000));
        let Structure::Atsr(atsr) = s[1] else {
            panic!("{:?}", s[1])
        };
        assert!(atsr.all_ports());
        assert_eq!(atsr.scopes.iter().count(), 0);
    }

    /// Appends a remapping structure, filling in its length.
    fn push_struct(t: &mut Vec<u8>, kind: u16, fixed: &[u8], scopes: &[&[u8]]) {
        let len = 4 + fixed.len() + scopes.iter().map(|s| s.len()).sum::<usize>();
        t.extend(kind.to_le_bytes());
        t.extend(u16::try_from(len).unwrap().to_le_bytes());
        t.extend(fixed);
        for s in scopes {
            t.extend(*s);
        }
    }

    fn scope(kind: u8, id: u8, bus: u8, path: &[(u8, u8)]) -> Vec<u8> {
        let mut s = vec![kind, (6 + 2 * path.len()) as u8, 0, 0, id, bus];
        for (d, f) in path {
            s.extend([*d, *f]);
        }
        s
    }

    /// A client-platform-like table built from the specification's layouts: an integrated
    /// graphics unit, a catch-all unit, two RMRRs, an ATSR, an RHSA, an ANDD and a structure
    /// type from the future.
    fn synthetic() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend(SIGNATURE);
        t.extend([0; 4]); // length, patched below
        t.extend([1, 0]); // revision, checksum (patched)
        t.extend(*b"FERRIC");
        t.extend(*b"TESTDMAR");
        t.extend([0; 12]); // OEM revision, creator ID, creator revision
        t.push(45); // host address width 46 bits
        t.push(0b101); // INTR_REMAP | DMA_CTRL_PLATFORM_OPT_IN
        t.extend([0; 10]);

        // DRHD fixed part: flags, size, segment, register base.
        let gfx = scope(1, 0, 0, &[(2, 0)]);
        let mut fixed = vec![0u8, 0];
        fixed.extend(0u16.to_le_bytes());
        fixed.extend(0xfed9_0000u64.to_le_bytes());
        push_struct(&mut t, 0, &fixed, &[&gfx]);

        let ioapic = scope(3, 2, 0xf0, &[(0x1f, 0)]);
        let hpet = scope(4, 0, 0, &[(0x1f, 0)]);
        let mut fixed = vec![1u8, 1]; // INCLUDE_PCI_ALL, two register pages
        fixed.extend(0u16.to_le_bytes());
        fixed.extend(0xfed9_1000u64.to_le_bytes());
        push_struct(&mut t, 0, &fixed, &[&ioapic, &hpet]);

        let usb = scope(1, 0, 0, &[(0x14, 0)]);
        let mut fixed = vec![0u8, 0];
        fixed.extend(0u16.to_le_bytes());
        fixed.extend(0x7c80_0000u64.to_le_bytes());
        fixed.extend(0x7c81_ffffu64.to_le_bytes());
        push_struct(&mut t, 1, &fixed, &[&usb]);

        // A device behind a root port at 00:1c.0, then a switch port.
        let deep = scope(1, 0, 0, &[(0x1c, 0), (0, 0), (0, 1)]);
        let mut fixed = vec![0u8, 0];
        fixed.extend(0u16.to_le_bytes());
        fixed.extend(0x8000_0000u64.to_le_bytes());
        fixed.extend(0x87ff_ffffu64.to_le_bytes());
        push_struct(&mut t, 1, &fixed, &[&gfx, &deep]);

        let port = scope(2, 0, 0, &[(0x1c, 0)]);
        push_struct(&mut t, 2, &[0, 0, 0, 0], &[&port]);

        let mut fixed = vec![0u8; 4];
        fixed.extend(0xfed9_1000u64.to_le_bytes());
        fixed.extend(1u32.to_le_bytes());
        push_struct(&mut t, 3, &fixed, &[]);

        let mut fixed = vec![0u8, 0, 0, 7];
        fixed.extend(*b"\\_SB.PCI0.I2C0\0");
        push_struct(&mut t, 4, &fixed, &[]);

        push_struct(&mut t, 0x7f, &[1, 2, 3, 4], &[]);

        let len = u32::try_from(t.len()).unwrap();
        t[4..8].copy_from_slice(&len.to_le_bytes());
        let sum = t.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        t[9] = 0u8.wrapping_sub(sum);
        t
    }

    #[test]
    fn synthetic_table() {
        let bytes = synthetic();
        let dmar = Dmar::parse(&bytes).unwrap();
        assert_eq!(dmar.host_address_width(), 46);
        assert!(dmar.flags().interrupt_remapping());
        assert!(!dmar.flags().x2apic_opt_out());
        assert!(dmar.flags().dma_control_opt_in());
        let s = all(&dmar);
        assert_eq!(s.len(), 8);

        let Structure::Drhd(gfx) = s[0] else { panic!() };
        assert!(!gfx.include_pci_all());
        assert_eq!(gfx.register_base, 0xfed9_0000);
        let Structure::Drhd(all_pci) = s[1] else {
            panic!()
        };
        assert!(all_pci.include_pci_all());
        assert_eq!(all_pci.register_pages(), 2);
        let kinds: Vec<_> = all_pci.scopes.iter().map(|s| s.unwrap().kind).collect();
        assert_eq!(kinds, [ScopeKind::IoApic, ScopeKind::Hpet]);

        let Structure::Rmrr(usb) = s[2] else { panic!() };
        assert_eq!((usb.base, usb.end()), (0x7c80_0000, Some(0x7c82_0000)));
        let Structure::Rmrr(two) = s[3] else { panic!() };
        let scopes: Vec<_> = two.scopes.iter().map(Result::unwrap).collect();
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[1].direct_bdf(), None);
        let resolved = scopes[1].resolve(|bridge| match bridge {
            b if b == Bdf::new(0, 0x1c, 0) => Some(3),
            b if b == Bdf::new(3, 0, 0) => Some(4),
            _ => None,
        });
        assert_eq!(resolved, Some(Bdf::new(4, 0, 1)));
        assert_eq!(scopes[1].resolve(|_| None), None);

        let Structure::Atsr(atsr) = s[4] else {
            panic!()
        };
        assert!(!atsr.all_ports());
        assert_eq!(
            atsr.scopes.iter().next().unwrap().unwrap().kind,
            ScopeKind::PciSubHierarchy
        );
        assert_eq!(
            s[5],
            Structure::Rhsa(Rhsa {
                register_base: 0xfed9_1000,
                proximity_domain: 1
            })
        );
        let Structure::Andd(andd) = s[6] else {
            panic!()
        };
        assert_eq!(andd.device_number, 7);
        assert!(andd.name.starts_with(b"\\_SB.PCI0.I2C0"));
        assert!(matches!(s[7], Structure::Unknown { kind: 0x7f, bytes } if bytes.len() == 8));
    }

    #[test]
    fn header_errors() {
        assert_eq!(
            Dmar::parse(&QEMU_OFF[..40]).unwrap_err(),
            DmarError::TooShort { len: 40 }
        );
        let mut b = QEMU_OFF.to_vec();
        b[0] = b'X';
        assert!(matches!(Dmar::parse(&b), Err(DmarError::BadSignature(_))));
        assert_eq!(
            Dmar::parse(&QEMU_OFF[..100]).unwrap_err(),
            DmarError::BadLength {
                claimed: 120,
                available: 100
            }
        );
        let mut b = QEMU_OFF.to_vec();
        b[60] ^= 1;
        assert!(matches!(
            Dmar::parse(&b),
            Err(DmarError::BadChecksum { .. })
        ));
        // Trailing bytes after the table are ignored.
        let mut b = QEMU_OFF.to_vec();
        b.extend([0xaa; 16]);
        assert_eq!(Dmar::parse(&b).unwrap().as_bytes().len(), 120);
    }

    /// Fixes the checksum after a test edits a table.
    fn reseal(b: &mut [u8]) {
        b[9] = 0;
        let sum = b.iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[9] = 0u8.wrapping_sub(sum);
    }

    #[test]
    fn structure_errors() {
        // A DRHD length running past the table.
        let mut b = QEMU_OFF.to_vec();
        b[50] = 0xff;
        reseal(&mut b);
        let dmar = Dmar::parse(&b).unwrap();
        let mut it = dmar.structures();
        assert!(matches!(
            it.next(),
            Some(Err(DmarError::BadStructure {
                offset: 48,
                kind: 0,
                length: 0xff
            }))
        ));
        assert!(it.next().is_none());

        // A DRHD too short for its fixed fields.
        let mut b = QEMU_OFF.to_vec();
        b[50] = 8;
        reseal(&mut b);
        let dmar = Dmar::parse(&b).unwrap();
        assert!(matches!(
            dmar.structures().next(),
            Some(Err(DmarError::BadStructure { .. }))
        ));

        // A zero-length structure must not loop forever.
        let mut b = QEMU_OFF.to_vec();
        b[50] = 0;
        reseal(&mut b);
        let dmar = Dmar::parse(&b).unwrap();
        assert_eq!(dmar.structures().count(), 1);
    }

    #[test]
    fn scope_errors() {
        // First scope of the QEMU DRHD claims 7 bytes (odd) and then 2 (too short).
        for bad in [7u8, 2, 0xf0] {
            let mut b = QEMU_OFF.to_vec();
            b[65] = bad;
            reseal(&mut b);
            let dmar = Dmar::parse(&b).unwrap();
            let Some(Ok(Structure::Drhd(drhd))) = dmar.structures().next() else {
                panic!()
            };
            let mut it = drhd.scopes.iter();
            assert_eq!(
                it.next(),
                Some(Err(DmarError::BadScope {
                    offset: 0,
                    length: bad
                }))
            );
            assert!(it.next().is_none());
        }
    }

    #[test]
    fn scope_coverage_through_bridges() {
        // Root port 00:1c.0 has buses 2-4 behind it; a switch port 02:00.0 has bus 3.
        let buses = |b: Bdf| match (b.bus, b.device, b.function) {
            (0, 0x1c, 0) => Some(BusRange {
                secondary: 2,
                subordinate: 4,
            }),
            (2, 0, 0) => Some(BusRange {
                secondary: 3,
                subordinate: 3,
            }),
            _ => None,
        };
        fn parse(bytes: &[u8]) -> DeviceScope<'_> {
            DeviceScopes::new(bytes).iter().next().unwrap().unwrap()
        }

        let deep = scope(1, 0, 0, &[(0x1c, 0), (0, 0), (0, 1)]);
        let deep = parse(&deep);
        assert_eq!(deep.covers(Bdf::new(3, 0, 1), buses), Some(true));
        assert_eq!(deep.covers(Bdf::new(3, 0, 0), buses), Some(false));
        assert_eq!(deep.covers(Bdf::new(3, 0, 1), |_| None), None);

        let port = scope(2, 0, 0, &[(0x1c, 0)]);
        let port = parse(&port);
        assert_eq!(port.covers(Bdf::new(0, 0x1c, 0), |_| None), Some(true));
        assert_eq!(port.covers(Bdf::new(4, 7, 0), buses), Some(true));
        assert_eq!(port.covers(Bdf::new(5, 0, 0), buses), Some(false));
        assert_eq!(port.covers(Bdf::new(4, 7, 0), |_| None), None);

        let direct = scope(1, 0, 0, &[(2, 0)]);
        let direct = parse(&direct);
        assert_eq!(direct.covers(Bdf::new(0, 2, 0), |_| None), Some(true));
        assert_eq!(direct.covers(Bdf::new(0, 3, 0), |_| None), Some(false));

        let ioapic = scope(3, 2, 0xf0, &[(0x1f, 0)]);
        assert_eq!(
            parse(&ioapic).covers(Bdf::new(0xf0, 0x1f, 0), buses),
            Some(false)
        );
    }
}
