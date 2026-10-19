//! Steering filters: which received frames go to a queue socket instead of netstack.
//!
//! A [`FilterSpec`] is one of:
//!
//! - an exact match ([`MatchSpec`]) on any of EtherType, VLAN ([`VlanMatch`]: one VLAN ID, or
//!   no VLAN ID at all), IP protocol, destination and source IP address, and (for TCP and UDP)
//!   destination and source port;
//! - all frames of the port, which takes the NIC away from netstack;
//! - all frames sent to a dedicated secondary unicast MAC address.
//!
//! [`Filter::new`] checks that a spec can match anything and is not ambiguous; only a
//! [`Filter`] can be encoded, matched, or installed. The driver programs a filter into the
//! NIC's queue-assignment filters in zero-copy mode, and calls [`Filter::matches`] on each
//! received frame in copy mode.
//!
//! # Encoding
//!
//! A filter travels as the [`Filter::BYTES`]-byte payload of [`crate::call::Request::AddFilter`]:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0 | kind: 1 match, 2 all frames, 3 secondary MAC |
//! | 1 | reserved, 0 |
//! | 2..4 | the fields a match uses, a bit each (`u16`): EtherType, VLAN ID, protocol, destination address, destination port, source address, source port, untagged (at most one of VLAN ID and untagged) |
//! | 4..6 | EtherType (`u16`) |
//! | 6..8 | VLAN ID (`u16`), for the VLAN ID bit |
//! | 8 | IP protocol |
//! | 9 | destination address family: 0 none, 4 IPv4, 6 IPv6 |
//! | 10 | source address family |
//! | 11 | reserved, 0 |
//! | 12..14 | destination port (`u16`) |
//! | 14..16 | source port (`u16`) |
//! | 16..32 | destination address (IPv4 in the first 4 bytes) |
//! | 32..48 | source address |
//! | 48..54 | secondary MAC address |
//! | 54..64 | reserved, 0 |
//!
//! Integers are little-endian. Every field a filter does not use is zero, so each filter has
//! exactly one encoding and [`Filter::from_bytes`] refuses any other.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use thiserror::Error;

use crate::frame::{
    ETHERTYPE_IPV4, ETHERTYPE_IPV6, Headers, IP_PROTOCOL_TCP, IP_PROTOCOL_UDP, MacAddr,
};

/// A VLAN ID that names a VLAN: 1 to 4094 (IEEE 802.1Q reserves 0 for priority tags and
/// 4095).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VlanId(u16);

impl VlanId {
    /// VLAN `id`, or `None` when it is 0 or above 4094.
    pub const fn new(id: u16) -> Option<Self> {
        if id == 0 || id > 4094 {
            return None;
        }
        Some(Self(id))
    }

    /// The ID.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Which VLAN a [`MatchSpec`] selects.
///
/// IEEE 802.1Q-2018 (section 9.6) gives VLAN ID 0 to *priority-tagged* frames: they carry a
/// priority but no VLAN ID, and belong to the port's untagged traffic. So a priority tag never
/// names a VLAN, and frames that only have priority tags go with untagged frames
/// ([`Headers::vlan`] skips them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VlanMatch {
    /// Frames with no VLAN ID: untagged, or carrying only priority tags.
    Untagged,
    /// Frames whose outermost VLAN ID is this one.
    Id(VlanId),
}

impl VlanMatch {
    fn matches(self, vlan: Option<u16>) -> bool {
        match self {
            Self::Untagged => vlan.is_none(),
            Self::Id(id) => vlan == Some(id.get()),
        }
    }
}

/// An exact match: every field that is set must equal the frame's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MatchSpec {
    /// The payload EtherType, after any VLAN tags.
    pub ether_type: Option<u16>,
    /// The VLAN: the outermost VLAN ID, or none.
    pub vlan: Option<VlanMatch>,
    /// The IP protocol (after IPv6 extension headers).
    pub ip_protocol: Option<u8>,
    /// The destination IP address.
    pub dst_addr: Option<IpAddr>,
    /// The destination port; needs `ip_protocol` TCP or UDP.
    pub dst_port: Option<u16>,
    /// The source IP address.
    pub src_addr: Option<IpAddr>,
    /// The source port; needs `ip_protocol` TCP or UDP.
    pub src_port: Option<u16>,
}

/// What a filter steers to the socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FilterSpec {
    /// Frames whose headers match.
    Match(MatchSpec),
    /// Every frame the port receives.
    AllFrames,
    /// Every frame sent to this secondary unicast MAC address.
    SecondaryMac(MacAddr),
}

/// A checked [`FilterSpec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Filter(FilterSpec);

const KIND_MATCH: u8 = 1;
const KIND_ALL: u8 = 2;
const KIND_MAC: u8 = 3;

const HAS_ETHER_TYPE: u16 = 1 << 0;
const HAS_VLAN: u16 = 1 << 1;
const HAS_PROTOCOL: u16 = 1 << 2;
const HAS_DST_ADDR: u16 = 1 << 3;
const HAS_DST_PORT: u16 = 1 << 4;
const HAS_SRC_ADDR: u16 = 1 << 5;
const HAS_SRC_PORT: u16 = 1 << 6;
const HAS_UNTAGGED: u16 = 1 << 7;

impl Filter {
    /// The size of an encoded filter.
    pub const BYTES: usize = 64;

    /// Checks `spec`.
    ///
    /// # Errors
    ///
    /// - [`FilterError::Empty`]: a match with no fields (use [`FilterSpec::AllFrames`]).
    /// - [`FilterError::PortWithoutTransport`]: a port without IP protocol TCP or UDP.
    /// - [`FilterError::AddressFamily`]: an IPv4 and an IPv6 address together.
    /// - [`FilterError::EtherTypeConflict`]: an EtherType with IP fields that it cannot carry.
    /// - [`FilterError::Mac`]: a secondary MAC address that is not unicast.
    pub fn new(spec: FilterSpec) -> Result<Self, FilterError> {
        match spec {
            FilterSpec::Match(fields) => check_match(&fields)?,
            FilterSpec::AllFrames => {}
            FilterSpec::SecondaryMac(mac) if !mac.is_unicast() => return Err(FilterError::Mac),
            FilterSpec::SecondaryMac(_) => {}
        }
        Ok(Self(spec))
    }

    /// The spec.
    pub const fn spec(&self) -> &FilterSpec {
        &self.0
    }

    /// Whether a frame with `headers` goes to the socket.
    pub fn matches(&self, headers: &Headers) -> bool {
        match &self.0 {
            FilterSpec::Match(fields) => match_fields(fields, headers),
            FilterSpec::AllFrames => true,
            FilterSpec::SecondaryMac(mac) => headers.dst_mac == *mac,
        }
    }

    /// The encoding (see the module documentation).
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut out = [0; Self::BYTES];
        match &self.0 {
            FilterSpec::Match(fields) => encode_match(fields, &mut out),
            FilterSpec::AllFrames => out[0] = KIND_ALL,
            FilterSpec::SecondaryMac(mac) => {
                out[0] = KIND_MAC;
                out[48..54].copy_from_slice(&mac.0);
            }
        }
        out
    }

    /// The filter `bytes` encode.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Length`] when `bytes` is not [`Filter::BYTES`] long,
    /// [`DecodeError::Kind`] for an unknown kind, [`DecodeError::Field`] when a field the
    /// filter does not use is not zero (or an address family is unknown), and
    /// [`DecodeError::Invalid`] when the filter decodes but [`Filter::new`] refuses it.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let bytes: &[u8; Self::BYTES] = bytes.try_into().map_err(|_| DecodeError::Length)?;
        let (spec, canonical) = match bytes[0] {
            KIND_MATCH => {
                let fields = decode_match(bytes)?;
                let mut canonical = [0; Self::BYTES];
                encode_match(&fields, &mut canonical);
                (FilterSpec::Match(fields), canonical)
            }
            KIND_ALL => {
                let mut canonical = [0; Self::BYTES];
                canonical[0] = KIND_ALL;
                (FilterSpec::AllFrames, canonical)
            }
            KIND_MAC => {
                let mut mac = [0; 6];
                mac.copy_from_slice(&bytes[48..54]);
                let mut canonical = [0; Self::BYTES];
                canonical[0] = KIND_MAC;
                canonical[48..54].copy_from_slice(&mac);
                (FilterSpec::SecondaryMac(MacAddr(mac)), canonical)
            }
            _ => return Err(DecodeError::Kind),
        };
        // Decoding read only the fields the filter uses; anything else must be zero.
        if canonical != *bytes {
            return Err(DecodeError::Field);
        }
        Ok(Self::new(spec)?)
    }
}

fn check_match(fields: &MatchSpec) -> Result<(), FilterError> {
    if *fields == MatchSpec::default() {
        return Err(FilterError::Empty);
    }
    let has_port = fields.dst_port.is_some() || fields.src_port.is_some();
    if has_port && !matches!(fields.ip_protocol, Some(IP_PROTOCOL_TCP | IP_PROTOCOL_UDP)) {
        return Err(FilterError::PortWithoutTransport);
    }
    let family = match (fields.dst_addr, fields.src_addr) {
        (Some(dst), Some(src)) if dst.is_ipv4() != src.is_ipv4() => {
            return Err(FilterError::AddressFamily);
        }
        (Some(addr), _) | (None, Some(addr)) => Some(addr.is_ipv4()),
        (None, None) => None,
    };
    if let Some(ether_type) = fields.ether_type {
        let is_ip = fields.ip_protocol.is_some() || family.is_some();
        let fits = match family {
            Some(true) => ether_type == ETHERTYPE_IPV4,
            Some(false) => ether_type == ETHERTYPE_IPV6,
            None => !is_ip || ether_type == ETHERTYPE_IPV4 || ether_type == ETHERTYPE_IPV6,
        };
        if !fits {
            return Err(FilterError::EtherTypeConflict);
        }
    }
    Ok(())
}

fn match_fields(fields: &MatchSpec, headers: &Headers) -> bool {
    fn eq<T: PartialEq>(want: Option<T>, have: Option<T>) -> bool {
        want.is_none() || want == have
    }
    let ip = headers.ip.as_ref();
    let ports = ip.and_then(|ip| ip.ports);
    eq(fields.ether_type, Some(headers.ether_type))
        && fields.vlan.is_none_or(|vlan| vlan.matches(headers.vlan))
        && eq(fields.ip_protocol, ip.map(|ip| ip.protocol))
        && eq(fields.dst_addr, ip.map(|ip| ip.dst))
        && eq(fields.src_addr, ip.map(|ip| ip.src))
        && eq(fields.dst_port, ports.map(|ports| ports.dst))
        && eq(fields.src_port, ports.map(|ports| ports.src))
}

fn encode_match(fields: &MatchSpec, out: &mut [u8; Filter::BYTES]) {
    let mut mask = 0;
    out[0] = KIND_MATCH;
    if let Some(ether_type) = fields.ether_type {
        mask |= HAS_ETHER_TYPE;
        out[4..6].copy_from_slice(&ether_type.to_le_bytes());
    }
    match fields.vlan {
        Some(VlanMatch::Id(id)) => {
            mask |= HAS_VLAN;
            out[6..8].copy_from_slice(&id.get().to_le_bytes());
        }
        Some(VlanMatch::Untagged) => mask |= HAS_UNTAGGED,
        None => {}
    }
    if let Some(protocol) = fields.ip_protocol {
        mask |= HAS_PROTOCOL;
        out[8] = protocol;
    }
    if let Some(addr) = fields.dst_addr {
        mask |= HAS_DST_ADDR;
        out[9] = encode_addr(addr, &mut out[16..32]);
    }
    if let Some(port) = fields.dst_port {
        mask |= HAS_DST_PORT;
        out[12..14].copy_from_slice(&port.to_le_bytes());
    }
    if let Some(addr) = fields.src_addr {
        mask |= HAS_SRC_ADDR;
        out[10] = encode_addr(addr, &mut out[32..48]);
    }
    if let Some(port) = fields.src_port {
        mask |= HAS_SRC_PORT;
        out[14..16].copy_from_slice(&port.to_le_bytes());
    }
    out[2..4].copy_from_slice(&mask.to_le_bytes());
}

/// Writes `addr` into a 16-byte field and returns its family code.
fn encode_addr(addr: IpAddr, field: &mut [u8]) -> u8 {
    match addr {
        IpAddr::V4(v4) => {
            field[..4].copy_from_slice(&v4.octets());
            4
        }
        IpAddr::V6(v6) => {
            field.copy_from_slice(&v6.octets());
            6
        }
    }
}

fn decode_match(bytes: &[u8; Filter::BYTES]) -> Result<MatchSpec, DecodeError> {
    let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let mask = word(2);
    let has = |bit: u16| mask & bit != 0;
    let addr = |family: u8, at: usize| -> Result<IpAddr, DecodeError> {
        let mut octets = [0; 16];
        octets.copy_from_slice(&bytes[at..at + 16]);
        match family {
            4 => Ok(IpAddr::V4(Ipv4Addr::new(
                octets[0], octets[1], octets[2], octets[3],
            ))),
            6 => Ok(IpAddr::V6(Ipv6Addr::from(octets))),
            _ => Err(DecodeError::Field),
        }
    };
    Ok(MatchSpec {
        ether_type: has(HAS_ETHER_TYPE).then(|| word(4)),
        // Both VLAN bits at once re-encode with only the ID bit, so `from_bytes` refuses them.
        vlan: if has(HAS_VLAN) {
            Some(VlanMatch::Id(
                VlanId::new(word(6)).ok_or(DecodeError::Field)?,
            ))
        } else if has(HAS_UNTAGGED) {
            Some(VlanMatch::Untagged)
        } else {
            None
        },
        ip_protocol: has(HAS_PROTOCOL).then_some(bytes[8]),
        dst_addr: has(HAS_DST_ADDR).then(|| addr(bytes[9], 16)).transpose()?,
        dst_port: has(HAS_DST_PORT).then(|| word(12)),
        src_addr: has(HAS_SRC_ADDR).then(|| addr(bytes[10], 32)).transpose()?,
        src_port: has(HAS_SRC_PORT).then(|| word(14)),
    })
}

/// Why a [`FilterSpec`] was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FilterError {
    /// A match with no fields.
    #[error("a match filter needs at least one field")]
    Empty,
    /// A port without IP protocol TCP or UDP.
    #[error("a port filter needs IP protocol TCP or UDP")]
    PortWithoutTransport,
    /// An IPv4 and an IPv6 address together.
    #[error("the filter's addresses are of different families")]
    AddressFamily,
    /// An EtherType that cannot carry the filter's IP fields.
    #[error("the filter's EtherType cannot carry its IP fields")]
    EtherTypeConflict,
    /// A secondary MAC address that is not unicast.
    #[error("a secondary MAC address must be unicast")]
    Mac,
}

/// Why bytes are not an encoded [`Filter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The buffer is not [`Filter::BYTES`] long.
    #[error("an encoded filter is {} bytes", Filter::BYTES)]
    Length,
    /// The kind is unknown.
    #[error("unknown filter kind")]
    Kind,
    /// A field is out of range, or a field the filter does not use is not zero.
    #[error("a filter field is out of range or not zero")]
    Field,
    /// The filter decodes but is refused.
    #[error("the filter is not valid")]
    Invalid(#[from] FilterError),
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::frame::tests::{HOST, ethernet, ipv4, ipv6, udp};
    use std::string::ToString;

    fn filter(fields: MatchSpec) -> Filter {
        Filter::new(FilterSpec::Match(fields)).expect("a valid filter")
    }

    fn udp4(dst_port: u16) -> Headers {
        let ip = ipv4(
            IP_PROTOCOL_UDP,
            [10, 0, 0, 9],
            [10, 0, 0, 1],
            0,
            &udp(5000, dst_port),
        );
        Headers::parse(&ethernet(&[], ETHERTYPE_IPV4, &ip)).expect("well-formed")
    }

    #[test]
    fn port_filters_match_only_their_port() {
        let echo = filter(MatchSpec {
            ip_protocol: Some(IP_PROTOCOL_UDP),
            dst_port: Some(7),
            ..MatchSpec::default()
        });
        assert!(echo.matches(&udp4(7)));
        assert!(!echo.matches(&udp4(8)));
        let tcp = filter(MatchSpec {
            ip_protocol: Some(IP_PROTOCOL_TCP),
            dst_port: Some(7),
            ..MatchSpec::default()
        });
        assert!(!tcp.matches(&udp4(7)));
    }

    #[test]
    fn address_vlan_and_ether_type_filters() {
        let to_host = filter(MatchSpec {
            dst_addr: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            ether_type: Some(ETHERTYPE_IPV4),
            ..MatchSpec::default()
        });
        assert!(to_host.matches(&udp4(1)));
        let from_other = filter(MatchSpec {
            src_addr: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
            ..MatchSpec::default()
        });
        assert!(!from_other.matches(&udp4(1)));

        let v6 = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
        let ip = ipv6(IP_PROTOCOL_UDP, v6, v6, &udp(1, 2));
        let tagged = Headers::parse(&ethernet(&[100], ETHERTYPE_IPV6, &ip)).expect("well-formed");
        let vlan = filter(MatchSpec {
            vlan: VlanId::new(100).map(VlanMatch::Id),
            ..MatchSpec::default()
        });
        assert!(vlan.matches(&tagged));
        assert!(!vlan.matches(&udp4(1)));
        let untagged = filter(MatchSpec {
            vlan: Some(VlanMatch::Untagged),
            ..MatchSpec::default()
        });
        assert!(untagged.matches(&udp4(1)));
        assert!(!untagged.matches(&tagged));

        // A priority tag (VLAN ID 0) names no VLAN: the frame is untagged traffic, and the
        // VLAN ID of a tag inside it is the frame's VLAN.
        let priority = Headers::parse(&ethernet(&[0], ETHERTYPE_IPV6, &ip)).expect("well-formed");
        assert!(untagged.matches(&priority));
        assert!(!vlan.matches(&priority));
        let inner = Headers::parse(&ethernet(&[0, 100], ETHERTYPE_IPV6, &ip)).expect("well-formed");
        assert!(vlan.matches(&inner));
        assert!(!untagged.matches(&inner));
        let ptp = filter(MatchSpec {
            ether_type: Some(0x88f7),
            ..MatchSpec::default()
        });
        assert!(!ptp.matches(&tagged));
        assert!(
            ptp.matches(&Headers::parse(&ethernet(&[], 0x88f7, &[0; 44])).expect("well-formed"))
        );
    }

    #[test]
    fn whole_port_and_secondary_mac_filters() {
        let all = Filter::new(FilterSpec::AllFrames).expect("valid");
        assert!(all.matches(&udp4(1)));
        let mac =
            Filter::new(FilterSpec::SecondaryMac(MacAddr([2, 0, 0, 0, 0, 0x42]))).expect("unicast");
        assert!(!mac.matches(&udp4(1)));
        let mut to_mac = udp4(1);
        to_mac.dst_mac = MacAddr([2, 0, 0, 0, 0, 0x42]);
        assert!(mac.matches(&to_mac));
        assert_ne!(MacAddr(HOST), to_mac.dst_mac);
    }

    #[test]
    fn invalid_specs_are_refused() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        for (fields, error) in [
            (MatchSpec::default(), FilterError::Empty),
            (
                MatchSpec {
                    dst_port: Some(80),
                    ..MatchSpec::default()
                },
                FilterError::PortWithoutTransport,
            ),
            (
                MatchSpec {
                    ip_protocol: Some(1),
                    src_port: Some(80),
                    ..MatchSpec::default()
                },
                FilterError::PortWithoutTransport,
            ),
            (
                MatchSpec {
                    dst_addr: Some(v4),
                    src_addr: Some(v6),
                    ..MatchSpec::default()
                },
                FilterError::AddressFamily,
            ),
            (
                MatchSpec {
                    ether_type: Some(ETHERTYPE_IPV6),
                    dst_addr: Some(v4),
                    ..MatchSpec::default()
                },
                FilterError::EtherTypeConflict,
            ),
            (
                MatchSpec {
                    ether_type: Some(0x0806),
                    ip_protocol: Some(IP_PROTOCOL_UDP),
                    ..MatchSpec::default()
                },
                FilterError::EtherTypeConflict,
            ),
        ] {
            assert_eq!(
                Filter::new(FilterSpec::Match(fields)),
                Err(error),
                "{fields:?}"
            );
            assert!(!error.to_string().is_empty());
        }
        assert_eq!(
            Filter::new(FilterSpec::SecondaryMac(MacAddr([1, 0, 0x5e, 0, 0, 1]))),
            Err(FilterError::Mac)
        );
        assert_eq!(VlanId::new(0), None);
        assert_eq!(VlanId::new(4095), None);
    }

    #[test]
    fn encodings_round_trip() {
        let v6 = IpAddr::V6(Ipv6Addr::new(0xfd00, 1, 2, 3, 4, 5, 6, 7));
        for spec in [
            FilterSpec::AllFrames,
            FilterSpec::SecondaryMac(MacAddr([2, 1, 2, 3, 4, 5])),
            FilterSpec::Match(MatchSpec {
                ether_type: Some(ETHERTYPE_IPV6),
                vlan: VlanId::new(4094).map(VlanMatch::Id),
                ip_protocol: Some(IP_PROTOCOL_TCP),
                dst_addr: Some(v6),
                dst_port: Some(443),
                src_addr: Some(v6),
                src_port: Some(0),
            }),
            FilterSpec::Match(MatchSpec {
                dst_addr: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
                ..MatchSpec::default()
            }),
            FilterSpec::Match(MatchSpec {
                vlan: Some(VlanMatch::Untagged),
                ip_protocol: Some(IP_PROTOCOL_UDP),
                dst_port: Some(319),
                ..MatchSpec::default()
            }),
        ] {
            let filter = Filter::new(spec).expect("valid");
            assert_eq!(Filter::from_bytes(&filter.to_bytes()), Ok(filter));
            assert_eq!(filter.spec(), &spec);
        }
    }

    #[test]
    fn bad_encodings_are_refused() {
        let good = filter(MatchSpec {
            ip_protocol: Some(IP_PROTOCOL_UDP),
            dst_port: Some(7),
            ..MatchSpec::default()
        })
        .to_bytes();
        assert_eq!(Filter::from_bytes(&good[..63]), Err(DecodeError::Length));
        let mut bad = good;
        bad[0] = 9;
        assert_eq!(Filter::from_bytes(&bad), Err(DecodeError::Kind));
        let mut bad = good;
        bad[60] = 1;
        assert_eq!(Filter::from_bytes(&bad), Err(DecodeError::Field));
        // A source port that the mask does not announce.
        let mut bad = good;
        bad[14] = 1;
        assert_eq!(Filter::from_bytes(&bad), Err(DecodeError::Field));
        // An unknown mask bit.
        let mut bad = good;
        bad[3] = 0x80;
        assert_eq!(Filter::from_bytes(&bad), Err(DecodeError::Field));
        // A VLAN ID of 0 (a priority tag names no VLAN; untagged has its own bit), and a VLAN
        // ID together with untagged.
        let mut bad = good;
        bad[2] |= HAS_VLAN as u8;
        assert_eq!(Filter::from_bytes(&bad), Err(DecodeError::Field));
        bad[6] = 5;
        assert!(Filter::from_bytes(&bad).is_ok());
        bad[2] |= HAS_UNTAGGED as u8;
        assert_eq!(Filter::from_bytes(&bad), Err(DecodeError::Field));
        // A destination address with an unknown family.
        let mut bad = good;
        bad[2] |= HAS_DST_ADDR as u8;
        bad[9] = 5;
        assert_eq!(Filter::from_bytes(&bad), Err(DecodeError::Field));
        // Well-formed but invalid: a port without a protocol.
        let mut bad = good;
        bad[2] &= !(HAS_PROTOCOL as u8);
        bad[8] = 0;
        assert_eq!(
            Filter::from_bytes(&bad),
            Err(DecodeError::Invalid(FilterError::PortWithoutTransport))
        );
        let error = DecodeError::Invalid(FilterError::Empty);
        assert!(core::error::Error::source(&error).is_some());
    }

    /// Whatever bytes an application sends, decoding never panics, and a filter it accepts is
    /// valid and has exactly the bytes it was decoded from.
    #[test]
    fn hostile_encodings_are_canonical_or_refused() {
        let mut seed: u64 = 0xdead_beef_cafe_f00d;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut accepted = 0;
        for _ in 0..50_000 {
            let mut bytes = [0u8; Filter::BYTES];
            // Mostly zero, with a few random fields, so some decode.
            bytes[0] = (next() % 4) as u8;
            for _ in 0..(next() % 4) {
                let at = (next() % Filter::BYTES as u64) as usize;
                bytes[at] = next() as u8;
            }
            if let Ok(filter) = Filter::from_bytes(&bytes) {
                accepted += 1;
                assert_eq!(filter.to_bytes(), bytes);
                assert_eq!(Filter::new(*filter.spec()), Ok(filter));
            }
        }
        assert!(accepted > 0);
    }
}
