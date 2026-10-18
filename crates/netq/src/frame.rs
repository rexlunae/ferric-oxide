//! The header fields of a received Ethernet frame that steering filters match on.
//!
//! [`Headers::parse`] reads an Ethernet II header (IEEE 802.3), up to three VLAN tags or a
//! MACsec header (IEEE 802.1Q, 802.1AE), an IPv4 (RFC 791) or IPv6 (RFC 8200, with its
//! extension headers) header, and a UDP (RFC 768) or TCP (RFC 9293) header, using the
//! `etherparse` crate. It borrows the frame and copies out only the fields below, so it costs
//! no allocation on the receive path.

use core::fmt;
use core::net::IpAddr;

use etherparse::err::packet::SliceError;
use etherparse::{LinkExtSlice, LinkSlice, NetSlice, SlicedPacket, TransportSlice};
use thiserror::Error;

/// The IP protocol number of TCP.
pub const IP_PROTOCOL_TCP: u8 = 6;

/// The IP protocol number of UDP.
pub const IP_PROTOCOL_UDP: u8 = 17;

/// The EtherType of IPv4.
pub const ETHERTYPE_IPV4: u16 = 0x0800;

/// The EtherType of IPv6.
pub const ETHERTYPE_IPV6: u16 = 0x86dd;

/// An Ethernet MAC address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// Whether this is an individual (unicast) address: the group bit of the first octet is
    /// clear and it is not all zeros.
    pub const fn is_unicast(self) -> bool {
        self.0[0] & 1 == 0 && !matches!(self.0, [0, 0, 0, 0, 0, 0])
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// A frame's transport ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ports {
    /// The source port.
    pub src: u16,
    /// The destination port.
    pub dst: u16,
}

/// A frame's IP header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ip {
    /// The protocol of the payload (after any IPv6 extension headers).
    pub protocol: u8,
    /// The source address.
    pub src: IpAddr,
    /// The destination address.
    pub dst: IpAddr,
    /// The TCP or UDP ports; `None` for other protocols and for fragments, which have no
    /// transport header to match (only reassembly could find it).
    pub ports: Option<Ports>,
}

/// The fields of a frame that filters match on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Headers {
    /// The destination MAC address.
    pub dst_mac: MacAddr,
    /// The VLAN ID of the outermost VLAN tag, if the frame has one.
    pub vlan: Option<u16>,
    /// The EtherType of the payload: after the VLAN tags, if any.
    pub ether_type: u16,
    /// The IP header fields, for IPv4 and IPv6 frames.
    pub ip: Option<Ip>,
}

impl Headers {
    /// Reads the fields of `frame`, which starts with its Ethernet header (no preamble, no
    /// frame check sequence).
    ///
    /// # Errors
    ///
    /// [`ParseError`] when a header the frame announces is truncated or malformed. A driver
    /// gives such a frame to netstack, which drops it, rather than to a bypass socket.
    pub fn parse(frame: &[u8]) -> Result<Self, ParseError> {
        let packet = SlicedPacket::from_ethernet(frame)?;
        let Some(LinkSlice::Ethernet2(ethernet)) = &packet.link else {
            // `from_ethernet` always reads an Ethernet II header first.
            return Err(ParseError::NotEthernet);
        };
        let mut vlan = None;
        let mut ether_type = ethernet.ether_type().0;
        for ext in &packet.link_exts {
            if let LinkExtSlice::Vlan(tag) = ext {
                vlan = vlan.or(Some(tag.vlan_identifier().value()));
                ether_type = tag.ether_type().0;
            }
        }
        Ok(Self {
            dst_mac: MacAddr(ethernet.destination()),
            vlan,
            ether_type,
            ip: ip_fields(&packet),
        })
    }
}

fn ip_fields(packet: &SlicedPacket<'_>) -> Option<Ip> {
    let (src, dst, payload) = match packet.net.as_ref()? {
        NetSlice::Ipv4(ipv4) => {
            let header = ipv4.header();
            (
                IpAddr::V4(header.source_addr()),
                IpAddr::V4(header.destination_addr()),
                ipv4.payload(),
            )
        }
        NetSlice::Ipv6(ipv6) => {
            let header = ipv6.header();
            (
                IpAddr::V6(header.source_addr()),
                IpAddr::V6(header.destination_addr()),
                ipv6.payload(),
            )
        }
        NetSlice::Arp(_) => return None,
    };
    let ports = match &packet.transport {
        _ if payload.fragmented => None,
        Some(TransportSlice::Udp(udp)) => Some(Ports {
            src: udp.source_port(),
            dst: udp.destination_port(),
        }),
        Some(TransportSlice::Tcp(tcp)) => Some(Ports {
            src: tcp.source_port(),
            dst: tcp.destination_port(),
        }),
        _ => None,
    };
    Some(Ip {
        protocol: payload.ip_number.0,
        src,
        dst,
        ports,
    })
}

/// Why a frame's headers could not be read.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ParseError {
    /// A header is truncated or malformed.
    #[error("malformed frame headers")]
    Malformed(#[from] SliceError),
    /// The frame does not start with an Ethernet II header.
    #[error("the frame has no Ethernet II header")]
    NotEthernet,
}

#[cfg(test)]
pub(crate) mod tests {
    extern crate std;

    use super::*;
    use core::net::{Ipv4Addr, Ipv6Addr};
    use std::string::ToString;
    use std::vec::Vec;

    pub(crate) const HOST: [u8; 6] = [2, 0, 0, 0, 0, 1];

    /// An Ethernet frame with optional VLAN tags around `payload` of `ether_type`.
    pub(crate) fn ethernet(vlans: &[u16], ether_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&HOST);
        frame.extend_from_slice(&[2, 0, 0, 0, 0, 9]);
        for &vid in vlans {
            frame.extend_from_slice(&0x8100u16.to_be_bytes());
            frame.extend_from_slice(&vid.to_be_bytes());
        }
        frame.extend_from_slice(&ether_type.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    /// A UDP datagram (no checksum) from `src_port` to `dst_port`.
    pub(crate) fn udp(src_port: u16, dst_port: u16) -> Vec<u8> {
        let mut datagram = Vec::new();
        datagram.extend_from_slice(&src_port.to_be_bytes());
        datagram.extend_from_slice(&dst_port.to_be_bytes());
        datagram.extend_from_slice(&12u16.to_be_bytes());
        datagram.extend_from_slice(&[0, 0, b'p', b'i', b'n', b'g']);
        datagram
    }

    /// An IPv4 packet carrying `payload` of `protocol`; `fragment_offset` in 8-byte units.
    pub(crate) fn ipv4(
        protocol: u8,
        src: [u8; 4],
        dst: [u8; 4],
        fragment_offset: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.push(0x45);
        packet.push(0);
        packet.extend_from_slice(&(20 + payload.len() as u16).to_be_bytes());
        packet.extend_from_slice(&[0, 1]);
        packet.extend_from_slice(&fragment_offset.to_be_bytes());
        packet.extend_from_slice(&[64, protocol, 0, 0]);
        packet.extend_from_slice(&src);
        packet.extend_from_slice(&dst);
        packet.extend_from_slice(payload);
        packet
    }

    /// An IPv6 packet carrying `payload` of `next_header`.
    pub(crate) fn ipv6(next_header: u8, src: Ipv6Addr, dst: Ipv6Addr, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&[0x60, 0, 0, 0]);
        packet.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        packet.extend_from_slice(&[next_header, 64]);
        packet.extend_from_slice(&src.octets());
        packet.extend_from_slice(&dst.octets());
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn udp_over_ipv4() {
        let ip = ipv4(
            IP_PROTOCOL_UDP,
            [10, 0, 0, 9],
            [10, 0, 0, 1],
            0,
            &udp(5000, 7000),
        );
        let headers = Headers::parse(&ethernet(&[], ETHERTYPE_IPV4, &ip)).expect("well-formed");
        assert_eq!(headers.dst_mac, MacAddr(HOST));
        assert_eq!(headers.vlan, None);
        assert_eq!(headers.ether_type, ETHERTYPE_IPV4);
        assert_eq!(
            headers.ip,
            Some(Ip {
                protocol: IP_PROTOCOL_UDP,
                src: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)),
                dst: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                ports: Some(Ports {
                    src: 5000,
                    dst: 7000
                }),
            })
        );
    }

    #[test]
    fn vlan_tags_and_ipv6_extension_headers() {
        let src = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 9);
        let dst = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
        // A destination-options header (next header 60) in front of the UDP datagram.
        let mut payload = Vec::from([IP_PROTOCOL_UDP, 0, 1, 4, 0, 0, 0, 0]);
        payload.extend_from_slice(&udp(5353, 5353));
        let ip = ipv6(60, src, dst, &payload);
        let headers =
            Headers::parse(&ethernet(&[42, 7], ETHERTYPE_IPV6, &ip)).expect("well-formed");
        assert_eq!(headers.vlan, Some(42));
        assert_eq!(headers.ether_type, ETHERTYPE_IPV6);
        let fields = headers.ip.expect("IPv6");
        assert_eq!(fields.protocol, IP_PROTOCOL_UDP);
        assert_eq!(fields.dst, IpAddr::V6(dst));
        assert_eq!(
            fields.ports,
            Some(Ports {
                src: 5353,
                dst: 5353
            })
        );
    }

    #[test]
    fn fragments_and_other_protocols_have_no_ports() {
        // A non-first fragment: its payload is not a UDP header.
        let ip = ipv4(
            IP_PROTOCOL_UDP,
            [10, 0, 0, 9],
            [10, 0, 0, 1],
            0x0010,
            &udp(1, 2),
        );
        let headers = Headers::parse(&ethernet(&[], ETHERTYPE_IPV4, &ip)).expect("well-formed");
        assert_eq!(headers.ip.map(|ip| ip.ports), Some(None));
        // ICMP echo.
        let ip = ipv4(
            1,
            [10, 0, 0, 9],
            [10, 0, 0, 1],
            0,
            &[8, 0, 0xf7, 0xff, 0, 0, 0, 0],
        );
        let headers = Headers::parse(&ethernet(&[], ETHERTYPE_IPV4, &ip)).expect("well-formed");
        assert_eq!(
            headers.ip.map(|ip| (ip.protocol, ip.ports)),
            Some((1, None))
        );
        // ARP has no IP fields.
        let mut arp = Vec::from([0, 1, 8, 0, 6, 4, 0, 1]);
        arp.extend_from_slice(&[0; 20]);
        let headers = Headers::parse(&ethernet(&[], 0x0806, &arp)).expect("well-formed");
        assert_eq!((headers.ether_type, headers.ip), (0x0806, None));
    }

    #[test]
    fn truncated_frames_are_refused() {
        let ip = ipv4(IP_PROTOCOL_UDP, [10, 0, 0, 9], [10, 0, 0, 1], 0, &udp(1, 2));
        let frame = ethernet(&[], ETHERTYPE_IPV4, &ip);
        for len in [0, 13, 20, frame.len() - 1] {
            assert!(Headers::parse(&frame[..len]).is_err(), "{len} bytes");
        }
        let error = Headers::parse(&frame[..5]).expect_err("truncated");
        assert!(core::error::Error::source(&error).is_some());
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn unicast_addresses() {
        assert!(MacAddr(HOST).is_unicast());
        assert!(!MacAddr([1, 0, 0x5e, 0, 0, 1]).is_unicast());
        assert!(!MacAddr([0xff; 6]).is_unicast());
        assert!(!MacAddr::default().is_unicast());
        assert_eq!(MacAddr(HOST).to_string(), "02:00:00:00:00:01");
    }
}
