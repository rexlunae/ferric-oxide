//! The `iommu` scheme's name, paths and call encoding.
//!
//! Every call is a `SYS_CALL` without payload. Its first metadata word is the [`Verb`], and
//! the words after it carry the arguments; [`Request::encode`] and [`Request::decode`] are
//! the one place that layout is written down.
//!
//! | Request | File descriptors | Metadata |
//! | --- | --- | --- |
//! | [`Request::Attach`] | attach capability, domain | verb, [`PciDevice::encode`] |
//! | [`Request::Detach`] | attach capability, domain | verb, [`PciDevice::encode`] |
//! | [`Request::Bridge`] | attach capability | verb, bridge address, bus range |
//! | [`Request::Bind`] | domain | verb |
//! | [`Request::Join`] | domain | verb |

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
