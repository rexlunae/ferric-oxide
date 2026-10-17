//! The `iommu` scheme's name, paths and call encoding.
//!
//! Every call is a `SYS_CALL`, without payload except [`Request::AllocIrte`]. Its first metadata
//! word is the [`Verb`], and the words after it carry the arguments; [`Request::encode`] and
//! [`Request::decode`] are the one place that layout is written down. The kernel reads at most
//! three metadata words.
//!
//! | Request | File descriptors | Metadata |
//! | --- | --- | --- |
//! | [`Request::Attach`] | attach capability, domain | verb, [`PciDevice::encode`] |
//! | [`Request::Detach`] | attach capability, domain | verb, [`PciDevice::encode`] |
//! | [`Request::Bridge`] | attach capability | verb, bridge address, bus range |
//! | [`Request::Bind`] | domain | verb |
//! | [`Request::Join`] | domain | verb |
//! | [`Request::AllocIrte`] | attach capability | verb, [`PciDevice::encode`] with [`SourceCheck::encode`] in bits 32 to 55, CPU and vector |
//! | [`Request::FreeIrte`] | attach capability | verb, entry ID |
//!
//! [`Request::AllocIrte`] also takes a payload of [`MsiMessage::BYTES`] bytes, which the kernel
//! fills with the message the device must send.

use core::fmt;

/// The scheme's number among the kernel's global schemes (`GlobalSchemes` in the kernel's
/// `redox_syscall`), which bootstrap uses to find it among the kernel scheme capabilities it
/// is started with.
pub const SCHEME_ID: u8 = 12;

/// The scheme's name in the namespace: clients open `/scheme/kernel.iommu`.
pub const SCHEME_NAME: &str = "kernel.iommu";

/// The path that, opened from the attach capability, creates a new domain.
pub const DOMAIN_PATH: &str = "domain";

/// A PCI function's address: segment, bus, device and function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PciDevice {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

impl PciDevice {
    /// The function at `segment`, `bus`, `device` and `function`, or `None` when the device
    /// number is 32 or more or the function number 8 or more.
    pub const fn new(segment: u16, bus: u8, device: u8, function: u8) -> Option<Self> {
        if device >= 32 || function >= 8 {
            return None;
        }
        Some(Self {
            segment,
            bus,
            device,
            function,
        })
    }

    /// The PCI segment (domain) number.
    pub const fn segment(self) -> u16 {
        self.segment
    }

    /// The bus number.
    pub const fn bus(self) -> u8 {
        self.bus
    }

    /// The device number, below 32.
    pub const fn device(self) -> u8 {
        self.device
    }

    /// The function number, below 8.
    pub const fn function(self) -> u8 {
        self.function
    }

    /// The 16-bit requester ID (bus, device, function) that tags the function's DMA.
    pub const fn requester_id(self) -> u16 {
        ((self.bus as u16) << 8) | ((self.device as u16) << 3) | self.function as u16
    }

    /// The function with requester ID `rid` in `segment`.
    pub const fn from_requester_id(segment: u16, rid: u16) -> Self {
        Self {
            segment,
            bus: (rid >> 8) as u8,
            device: ((rid >> 3) & 0x1f) as u8,
            function: (rid & 0x7) as u8,
        }
    }

    /// One metadata word: the segment in bits 16 to 31, the requester ID below it.
    pub const fn encode(self) -> u64 {
        ((self.segment as u64) << 16) | self.requester_id() as u64
    }

    /// The function [`PciDevice::encode`] wrote into `word`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Reserved`] when a bit above bit 31 is set.
    pub const fn decode(word: u64) -> Result<Self, DecodeError> {
        if word >> 32 != 0 {
            return Err(DecodeError::Reserved);
        }
        Ok(Self::from_requester_id((word >> 16) as u16, word as u16))
    }
}

impl fmt::Display for PciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{}",
            self.segment, self.bus, self.device, self.function
        )
    }
}

/// A PCI-to-PCI bridge and the buses behind it, as its configuration space reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bridge {
    /// The bridge's own address.
    pub address: PciDevice,
    /// The secondary bus number: the bus directly behind the bridge.
    pub secondary: u8,
    /// The subordinate bus number: the highest bus behind the bridge.
    pub subordinate: u8,
}

impl Bridge {
    /// Whether `bus` is behind the bridge.
    pub const fn covers_bus(&self, bus: u8) -> bool {
        self.secondary <= bus && bus <= self.subordinate
    }
}

/// Which requester IDs may raise an interrupt through an interrupt remapping entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceCheck {
    /// Only the function's own requester ID.
    Exact,
    /// Any requester ID on buses `first` to `last`: for a function behind a PCI Express to PCI
    /// bridge, whose requests can carry the bridge's IDs (see `alias`).
    Bus {
        /// The lowest bus number.
        first: u8,
        /// The highest bus number.
        last: u8,
    },
}

impl SourceCheck {
    /// 24 bits: 0 for [`SourceCheck::Exact`]; 1, with the first bus in bits 8 to 15 and the
    /// last in bits 16 to 23, for [`SourceCheck::Bus`].
    pub const fn encode(self) -> u64 {
        match self {
            Self::Exact => 0,
            Self::Bus { first, last } => 1 | ((first as u64) << 8) | ((last as u64) << 16),
        }
    }

    /// The check [`SourceCheck::encode`] wrote into `word`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Reserved`] for an unknown kind or a reserved bit, and
    /// [`DecodeError::BusRange`] when the last bus is below the first.
    pub const fn decode(word: u64) -> Result<Self, DecodeError> {
        match word & 0xff {
            0 if word == 0 => Ok(Self::Exact),
            1 if word >> 24 == 0 => {
                let (first, last) = ((word >> 8) as u8, (word >> 16) as u8);
                if last < first {
                    return Err(DecodeError::BusRange);
                }
                Ok(Self::Bus { first, last })
            }
            _ => Err(DecodeError::Reserved),
        }
    }
}

/// What an interrupt remapping entry delivers, and for which function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiRoute {
    /// The function that raises the interrupt.
    pub device: PciDevice,
    /// Which requester IDs the entry accepts.
    pub source: SourceCheck,
    /// The logical CPU (as the `irq` scheme numbers them, `irq:cpu-<n>`) to deliver to.
    pub cpu: u32,
    /// The interrupt vector on that CPU.
    pub vector: u8,
}

/// An MSI address and data pair, as [`Request::AllocIrte`] returns it in its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiMessage {
    /// The message address.
    pub address: u64,
    /// The message data.
    pub data: u32,
}

impl MsiMessage {
    /// The payload size: the address then the data, little-endian.
    pub const BYTES: usize = 12;

    /// The payload bytes.
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut bytes = [0; Self::BYTES];
        let (address, data) = bytes.split_at_mut(8);
        address.copy_from_slice(&self.address.to_le_bytes());
        data.copy_from_slice(&self.data.to_le_bytes());
        bytes
    }

    /// The message in a payload, or `None` when `bytes` is not [`MsiMessage::BYTES`] long.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::BYTES] = bytes.try_into().ok()?;
        let (address, data) = bytes.split_at(8);
        Some(Self {
            address: u64::from_le_bytes(address.try_into().ok()?),
            data: u32::from_le_bytes(data.try_into().ok()?),
        })
    }
}

/// The first metadata word of a call: what it asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
#[non_exhaustive]
pub enum Verb {
    /// Move a device into a domain.
    Attach = 1,
    /// Take a device out of a domain and block it.
    Detach = 2,
    /// Make a domain the calling process's own.
    Bind = 3,
    /// Report a bridge's bus range, so device scopes that name bridges can be resolved.
    Bridge = 4,
    /// Move a domain's devices into the calling process's bound domain.
    Join = 5,
    /// Set up an interrupt remapping entry for a function's MSI or MSI-X vector.
    AllocIrte = 6,
    /// Free an interrupt remapping entry.
    FreeIrte = 7,
}

impl Verb {
    /// The verb with number `raw`.
    pub const fn from_raw(raw: u64) -> Option<Self> {
        Some(match raw {
            1 => Self::Attach,
            2 => Self::Detach,
            3 => Self::Bind,
            4 => Self::Bridge,
            5 => Self::Join,
            6 => Self::AllocIrte,
            7 => Self::FreeIrte,
            _ => return None,
        })
    }
}

/// A call to the `iommu` scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// Move a device from the identity domain (or the blocked state) into the domain. The
    /// device's reserved memory regions (RMRRs) are mapped one-to-one in the domain first.
    Attach(PciDevice),
    /// Take a device out of the domain; its DMA faults until it is attached again.
    Detach(PciDevice),
    /// Make the domain the calling process's own. A process binds at most one domain. From
    /// then on `memory:translation` answers the process with IOVAs in the domain, mapping each
    /// buffer on its first translation and unmapping it when the process unmaps the memory.
    Bind,
    /// Report a bridge.
    Bridge(Bridge),
    /// Move every device attached to the domain (and the RMRRs they need) into the domain the
    /// calling process has bound, leaving this one empty. A driver that drives several
    /// functions gets a domain from pcid for each and joins the later ones into the first, so
    /// it keeps one domain and one IOVA space. Holding the domain's descriptor is the
    /// authority: pcid gave it only to the driver of those functions.
    Join,
    /// Set up an interrupt remapping entry that delivers the route's vector to its CPU, for
    /// requests from the route's function only, and write the remappable-format MSI address
    /// and data that name it into the payload ([`MsiMessage`]). Returns an ID for the entry
    /// (a 32-bit number with no meaning to user space), which [`Request::FreeIrte`] takes.
    /// pcid makes this call for a driver that asks it for an MSI message, so a driver never
    /// chooses a compatibility-format address itself.
    AllocIrte(MsiRoute),
    /// Free the interrupt remapping entry with this ID: requests that name it fault.
    FreeIrte(u32),
}

impl Request {
    /// The most metadata words a request takes.
    pub const MAX_WORDS: usize = 3;

    /// The metadata words for the request, and how many of them are used.
    pub const fn encode(&self) -> ([u64; Self::MAX_WORDS], usize) {
        match *self {
            Self::Attach(device) => ([Verb::Attach as u64, device.encode(), 0], 2),
            Self::Detach(device) => ([Verb::Detach as u64, device.encode(), 0], 2),
            Self::Bind => ([Verb::Bind as u64, 0, 0], 1),
            Self::Join => ([Verb::Join as u64, 0, 0], 1),
            Self::Bridge(bridge) => (
                [
                    Verb::Bridge as u64,
                    bridge.address.encode(),
                    bridge.secondary as u64 | ((bridge.subordinate as u64) << 8),
                ],
                3,
            ),
            Self::AllocIrte(route) => (
                [
                    Verb::AllocIrte as u64,
                    route.device.encode() | (route.source.encode() << 32),
                    route.cpu as u64 | ((route.vector as u64) << 32),
                ],
                3,
            ),
            Self::FreeIrte(index) => ([Verb::FreeIrte as u64, index as u64, 0], 2),
        }
    }

    /// The request in the metadata words `words`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// number of words does not match the verb, [`DecodeError::Reserved`] when a reserved bit
    /// is set, and [`DecodeError::BusRange`] when a bridge's subordinate bus is below its
    /// secondary bus.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&verb, args) = words.split_first().ok_or(DecodeError::Verb)?;
        match (Verb::from_raw(verb).ok_or(DecodeError::Verb)?, args) {
            (Verb::Attach, &[device]) => Ok(Self::Attach(PciDevice::decode(device)?)),
            (Verb::Detach, &[device]) => Ok(Self::Detach(PciDevice::decode(device)?)),
            (Verb::Bind, &[]) => Ok(Self::Bind),
            (Verb::Join, &[]) => Ok(Self::Join),
            (Verb::Bridge, &[address, buses]) => {
                if buses >> 16 != 0 {
                    return Err(DecodeError::Reserved);
                }
                let bridge = Bridge {
                    address: PciDevice::decode(address)?,
                    secondary: buses as u8,
                    subordinate: (buses >> 8) as u8,
                };
                if bridge.subordinate < bridge.secondary {
                    return Err(DecodeError::BusRange);
                }
                Ok(Self::Bridge(bridge))
            }
            (Verb::AllocIrte, &[device, target]) => {
                if target >> 40 != 0 {
                    return Err(DecodeError::Reserved);
                }
                Ok(Self::AllocIrte(MsiRoute {
                    device: PciDevice::decode(device & 0xffff_ffff)?,
                    source: SourceCheck::decode(device >> 32)?,
                    cpu: target as u32,
                    vector: (target >> 32) as u8,
                }))
            }
            (Verb::FreeIrte, &[index]) => u32::try_from(index)
                .map(Self::FreeIrte)
                .map_err(|_| DecodeError::Reserved),
            _ => Err(DecodeError::Length),
        }
    }
}

/// Why call metadata is not a valid [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    Verb,
    /// The number of words does not match the verb.
    Length,
    /// A reserved bit is set.
    Reserved,
    /// A bridge's subordinate bus is below its secondary bus.
    BusRange,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Verb => "missing or unknown iommu verb",
            Self::Length => "wrong number of metadata words for the iommu verb",
            Self::Reserved => "reserved bits set in iommu call metadata",
            Self::BusRange => "bridge subordinate bus below its secondary bus",
        })
    }
}

impl core::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(segment: u16, bus: u8, device: u8, function: u8) -> PciDevice {
        PciDevice::new(segment, bus, device, function).unwrap()
    }

    #[test]
    fn device_numbers_are_checked() {
        assert!(PciDevice::new(0, 0, 31, 7).is_some());
        assert!(PciDevice::new(0, 0, 32, 0).is_none());
        assert!(PciDevice::new(0, 0, 0, 8).is_none());
    }

    #[test]
    fn device_encoding_round_trips() {
        let d = dev(0x1234, 0xab, 0x1f, 6);
        assert_eq!(d.requester_id(), 0xabfe);
        assert_eq!(d.encode(), 0x1234_abfe);
        assert_eq!(PciDevice::decode(d.encode()), Ok(d));
        assert_eq!(PciDevice::decode(1 << 32), Err(DecodeError::Reserved));
        assert_eq!(PciDevice::from_requester_id(0x1234, 0xabfe), d);
    }

    #[test]
    fn device_display() {
        extern crate std;
        use std::string::ToString;
        assert_eq!(dev(0, 0x1f, 3, 1).to_string(), "0000:1f:03.1");
    }

    #[test]
    fn requests_round_trip() {
        let bridge = Bridge {
            address: dev(0, 0, 0x1c, 0),
            secondary: 2,
            subordinate: 5,
        };
        for request in [
            Request::Attach(dev(0, 3, 0, 0)),
            Request::Detach(dev(1, 0x80, 2, 1)),
            Request::Bind,
            Request::Bridge(bridge),
            Request::Join,
            Request::AllocIrte(MsiRoute {
                device: dev(0, 0, 4, 0),
                source: SourceCheck::Exact,
                cpu: 3,
                vector: 0x31,
            }),
            Request::AllocIrte(MsiRoute {
                device: dev(1, 5, 2, 0),
                source: SourceCheck::Bus { first: 4, last: 5 },
                cpu: u32::MAX,
                vector: 0xfe,
            }),
            Request::FreeIrte(0x0001_0fff),
        ] {
            let (words, len) = request.encode();
            assert_eq!(Request::decode(&words[..len]), Ok(request));
        }
    }

    #[test]
    fn bad_requests() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[0]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[99, 0]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[1]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[3, 0]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[5, 0]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[1, 1 << 40]), Err(DecodeError::Reserved));
        assert_eq!(
            Request::decode(&[4, 0, 1 << 16]),
            Err(DecodeError::Reserved)
        );
        assert_eq!(Request::decode(&[4, 0, 0x0203]), Err(DecodeError::BusRange));
        assert_eq!(Request::decode(&[6, 0]), Err(DecodeError::Length));
        assert_eq!(
            Request::decode(&[6, 0, 1 << 40]),
            Err(DecodeError::Reserved)
        );
        assert_eq!(
            Request::decode(&[6, 2 << 32, 0]),
            Err(DecodeError::Reserved)
        );
        assert_eq!(
            Request::decode(&[6, ((1 << 24) | 1) << 32, 0]),
            Err(DecodeError::Reserved)
        );
        assert_eq!(
            Request::decode(&[6, (1 | (5 << 8) | (4 << 16)) << 32, 0]),
            Err(DecodeError::BusRange)
        );
        assert_eq!(Request::decode(&[7, 1 << 32]), Err(DecodeError::Reserved));
    }

    #[test]
    fn msi_message_payload() {
        let msg = MsiMessage {
            address: 0xfee0_0074,
            data: 0x1234_5678,
        };
        let bytes = msg.to_bytes();
        assert_eq!(
            bytes,
            [0x74, 0, 0xe0, 0xfe, 0, 0, 0, 0, 0x78, 0x56, 0x34, 0x12]
        );
        assert_eq!(MsiMessage::from_bytes(&bytes), Some(msg));
        assert_eq!(MsiMessage::from_bytes(&bytes[..11]), None);
    }

    #[test]
    fn bridge_bus_range() {
        let bridge = Bridge {
            address: dev(0, 0, 1, 0),
            secondary: 2,
            subordinate: 4,
        };
        assert!(!bridge.covers_bus(1));
        assert!(bridge.covers_bus(2));
        assert!(bridge.covers_bus(4));
        assert!(!bridge.covers_bus(5));
    }
}
