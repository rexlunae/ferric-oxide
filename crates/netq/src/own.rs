//! Port ownership and TX validation (design section 4, "Port ownership" and "TX validation").
//!
//! A filter on a TCP or UDP destination port must not take a port a netstack socket is
//! using. To own a port, the application binds an ordinary netstack socket on it, at one of
//! the interface's addresses (`udp:10.0.2.15:5353`, say), and passes that socket's descriptor
//! with
//! [`Request::Own`](crate::call::Request::Own). The driver keeps the descriptor open, so
//! netstack keeps the port reserved for as long as the queue socket lives, and reads which port
//! it is from the descriptor's path ([`OwnedPort::from_fpath`]): netstack's `udp` and `tcp`
//! schemes write `/scheme/<udp|tcp>/<remote>/<local>`, with the local endpoint last.
//!
//! An owned port is always an address and a port. A netstack socket bound to every address
//! (`0.0.0.0` or `::`) is refused ([`OwnError::UnspecifiedAddress`]): owning it would let the
//! queue socket send from any source address, and the driver does not know which addresses
//! are the interface's. Binding to one address names the source the socket may use.
//!
//! [`Ownership`] is one socket's owned ports. [`Ownership::check_filter`] refuses a filter on a
//! destination port the socket does not own at the filter's destination address (a filter
//! with a port must name the address), and [`Ownership::check_tx`] is TX validation: a frame
//! the socket sends must carry the NIC's source MAC, no VLAN tag, and be TCP or UDP from an
//! owned address and port.
//! A socket bound with [`BindFlags::RAW_TX`](crate::call::BindFlags::RAW_TX) skips TX
//! validation.
//!
//! What the path proves: scheme names are registered once, so a `/scheme/udp/` prefix names
//! netstack's scheme as long as the scheme answering `fpath` is honest. Queue sockets are
//! root's for now, and root can serve any scheme, so this guards against mistakes, not against
//! a hostile root. Unprivileged sockets (design PR 5) need the kernel to vouch for the scheme
//! that answered.

use core::net::{IpAddr, SocketAddr};

use thiserror::Error;

use crate::filter::{Filter, FilterSpec};
use crate::frame::{Headers, IP_PROTOCOL_TCP, IP_PROTOCOL_UDP, MacAddr};

/// A transport protocol whose ports can be owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// UDP (RFC 768).
    Udp,
    /// TCP (RFC 9293).
    Tcp,
}

impl Transport {
    /// The IP protocol number.
    pub const fn protocol(self) -> u8 {
        match self {
            Self::Udp => IP_PROTOCOL_UDP,
            Self::Tcp => IP_PROTOCOL_TCP,
        }
    }

    /// The transport with IP protocol number `protocol`, if its ports can be owned.
    pub const fn from_protocol(protocol: u8) -> Option<Self> {
        match protocol {
            IP_PROTOCOL_UDP => Some(Self::Udp),
            IP_PROTOCOL_TCP => Some(Self::Tcp),
            _ => None,
        }
    }

    /// The netstack scheme serving this transport.
    const fn scheme(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

/// A port a socket owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OwnedPort {
    /// The protocol.
    pub transport: Transport,
    /// The address the netstack socket is bound to, never unspecified.
    pub addr: IpAddr,
    /// The port, never zero.
    pub port: u16,
}

impl OwnedPort {
    /// The port the netstack socket whose path is `path` is bound on.
    ///
    /// # Errors
    ///
    /// [`OwnError::NotASocket`] when `path` is not a path of netstack's `udp` or `tcp`
    /// scheme with a remote and a local endpoint, [`OwnError::Unbound`] when the local
    /// endpoint has port 0, and [`OwnError::UnspecifiedAddress`] when it is bound to every
    /// address.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::net::{IpAddr, Ipv4Addr};
    /// use ferric_netq::own::{OwnError, OwnedPort, Transport};
    ///
    /// let owned = OwnedPort::from_fpath("/scheme/udp/0.0.0.0:0/10.0.2.15:5353").expect("a socket");
    /// assert_eq!(owned.transport, Transport::Udp);
    /// assert_eq!(owned.addr, IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)));
    /// assert_eq!(owned.port, 5353);
    /// // Every address is not one the socket can own.
    /// assert_eq!(
    ///     OwnedPort::from_fpath("/scheme/udp/0.0.0.0:0/0.0.0.0:5353"),
    ///     Err(OwnError::UnspecifiedAddress)
    /// );
    /// ```
    pub fn from_fpath(path: &str) -> Result<Self, OwnError> {
        let rest = path.strip_prefix("/scheme/").ok_or(OwnError::NotASocket)?;
        let (scheme, endpoints) = rest.split_once('/').ok_or(OwnError::NotASocket)?;
        let transport = [Transport::Udp, Transport::Tcp]
            .into_iter()
            .find(|t| t.scheme() == scheme)
            .ok_or(OwnError::NotASocket)?;
        let (remote, local) = endpoints.split_once('/').ok_or(OwnError::NotASocket)?;
        let parse = |s: &str| s.parse::<SocketAddr>().map_err(|_| OwnError::NotASocket);
        parse(remote)?;
        let local = parse(local)?;
        if local.port() == 0 {
            return Err(OwnError::Unbound);
        }
        if local.ip().is_unspecified() {
            return Err(OwnError::UnspecifiedAddress);
        }
        Ok(Self {
            transport,
            addr: local.ip(),
            port: local.port(),
        })
    }

    /// Whether this ownership covers `transport` port `port` at `addr`.
    fn covers(&self, transport: Transport, port: u16, addr: IpAddr) -> bool {
        self.transport == transport && self.port == port && self.addr == addr
    }
}

/// Why a port cannot be owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum OwnError {
    /// The descriptor is not a netstack TCP or UDP socket.
    #[error("the descriptor is not a netstack tcp or udp socket")]
    NotASocket,
    /// The socket is not bound to a port.
    #[error("the netstack socket is not bound to a port")]
    Unbound,
    /// The socket is bound to every address (`0.0.0.0` or `::`), which would let the queue
    /// socket send from any source address. Bind it to one of the interface's addresses.
    #[error("the netstack socket is bound to every address, not to one")]
    UnspecifiedAddress,
    /// The socket already owns [`Ownership::MAX_PORTS`] ports.
    #[error("the queue socket owns as many ports as it may")]
    Full,
}

/// Why a filter was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FilterOwnError {
    /// The filter names a destination port the socket does not own at the filter's
    /// destination address, or names a port without an address.
    #[error("the queue socket does not own the filter's destination port")]
    NotOwned,
}

/// Why TX validation dropped a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TxReject {
    /// The frame is not a well-formed Ethernet frame.
    #[error("not a well-formed Ethernet frame")]
    Malformed,
    /// The source MAC address is not the NIC's.
    #[error("the source MAC address is not the NIC's")]
    SourceMac,
    /// The frame carries a VLAN header, a priority tag (VLAN ID 0) included.
    #[error("the frame carries a VLAN tag")]
    Tagged,
    /// The frame is not TCP or UDP over IP with its ports. IP fragments are refused too, the
    /// first one included: a fragment's ports cannot be checked without reassembly, so
    /// validated TX does not send fragments. A socket that must send them binds with
    /// `BindFlags::RAW_TX`, the supported path for anything validation does not cover.
    #[error("the frame is not TCP or UDP with its ports (fragments are not sent)")]
    NotTransport,
    /// The source address and port are not ones the socket owns.
    #[error("the source address and port are not ones the socket owns")]
    NotOwned,
}

/// One socket's owned ports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ownership {
    ports: [Option<OwnedPort>; Self::MAX_PORTS],
}

impl Ownership {
    /// The most ports one socket may own.
    pub const MAX_PORTS: usize = 16;

    /// No owned ports.
    pub const fn new() -> Self {
        Self {
            ports: [None; Self::MAX_PORTS],
        }
    }

    /// Adds `port`, and returns whether it was new. Owning a port twice is not an error, but
    /// returns `false`, so a caller keeping something per owned port (a driver keeps the
    /// netstack socket that reserves it) keeps only one.
    ///
    /// # Errors
    ///
    /// [`OwnError::Full`] when the socket already owns [`MAX_PORTS`](Self::MAX_PORTS) ports.
    pub fn own(&mut self, port: OwnedPort) -> Result<bool, OwnError> {
        if self.ports.contains(&Some(port)) {
            return Ok(false);
        }
        let slot = self
            .ports
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(OwnError::Full)?;
        *slot = Some(port);
        Ok(true)
    }

    /// Whether the socket owns `transport` port `port` at `addr`.
    pub fn owns(&self, transport: Transport, port: u16, addr: IpAddr) -> bool {
        self.ports
            .iter()
            .flatten()
            .any(|owned| owned.covers(transport, port, addr))
    }

    /// Whether the socket may add `filter`: a match on a destination port needs that port
    /// owned at the filter's destination address, which it must name. Other filters (ARP, ICMP, whole protocols, a secondary MAC) are allowed, as
    /// queue sockets are root's.
    ///
    /// # Errors
    ///
    /// [`FilterOwnError::NotOwned`] for a destination port the socket does not own at the
    /// filter's destination address, or one without an address.
    pub fn check_filter(&self, filter: &Filter) -> Result<(), FilterOwnError> {
        let FilterSpec::Match(fields) = filter.spec() else {
            return Ok(());
        };
        let Some(port) = fields.dst_port else {
            return Ok(());
        };
        // A valid filter with a port names TCP or UDP. Without an address it would take the
        // port on every address, and an owned port is owned at one.
        let transport = fields
            .ip_protocol
            .and_then(Transport::from_protocol)
            .ok_or(FilterOwnError::NotOwned)?;
        let addr = fields.dst_addr.ok_or(FilterOwnError::NotOwned)?;
        if self.owns(transport, port, addr) {
            Ok(())
        } else {
            Err(FilterOwnError::NotOwned)
        }
    }

    /// TX validation of a frame with `headers` on a NIC with MAC address `nic`: the NIC's
    /// source MAC, no VLAN header (not even a priority tag), and TCP or UDP from an owned
    /// address and port.
    /// IP fragments, the first one included, are refused ([`TxReject::NotTransport`]); raw TX
    /// (`BindFlags::RAW_TX`) is the path for them.
    ///
    /// # Errors
    ///
    /// The [`TxReject`] reason the frame must be dropped.
    pub fn check_tx(&self, headers: &Headers, nic: MacAddr) -> Result<(), TxReject> {
        if headers.src_mac != nic {
            return Err(TxReject::SourceMac);
        }
        // A priority tag has no VLAN ID, but it is a VLAN header all the same.
        if headers.tagged {
            return Err(TxReject::Tagged);
        }
        let ip = headers.ip.as_ref().ok_or(TxReject::NotTransport)?;
        let transport = Transport::from_protocol(ip.protocol).ok_or(TxReject::NotTransport)?;
        let ports = ip.ports.ok_or(TxReject::NotTransport)?;
        if self.owns(transport, ports.src, ip.src) {
            Ok(())
        } else {
            Err(TxReject::NotOwned)
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::net::Ipv4Addr;
    use std::string::ToString;
    use std::vec::Vec;

    use super::*;
    use crate::filter::MatchSpec;
    use crate::frame::tests::{PEER, ethernet, ipv4, udp};
    use crate::frame::{ETHERTYPE_IPV4, IP_PROTOCOL_TCP};

    const ICMP: u8 = 1;

    /// The address the tests' sockets are bound to.
    const HOST: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15));

    fn udp_port(port: u16) -> OwnedPort {
        OwnedPort {
            transport: Transport::Udp,
            addr: HOST,
            port,
        }
    }

    fn port_filter(protocol: u8, port: u16, dst_addr: Option<IpAddr>) -> Filter {
        Filter::new(FilterSpec::Match(MatchSpec {
            ip_protocol: Some(protocol),
            dst_port: Some(port),
            dst_addr,
            ..MatchSpec::default()
        }))
        .expect("valid")
    }

    /// A UDP frame from `src` port `sport`, with `vlans` tags, from MAC [`PEER`].
    fn udp_from(src: [u8; 4], sport: u16, vlans: &[u16]) -> Vec<u8> {
        let ip = ipv4(IP_PROTOCOL_UDP, src, [10, 0, 0, 1], 0, &udp(sport, 53));
        ethernet(vlans, ETHERTYPE_IPV4, &ip)
    }

    fn headers(frame: &[u8]) -> Headers {
        Headers::parse(frame).expect("well-formed")
    }

    #[test]
    fn netstack_paths_name_the_owned_port() {
        assert_eq!(
            OwnedPort::from_fpath("/scheme/udp/0.0.0.0:0/10.0.2.15:5353"),
            Ok(udp_port(5353))
        );
        assert_eq!(
            OwnedPort::from_fpath("/scheme/tcp/10.0.2.2:80/10.0.2.15:40000"),
            Ok(OwnedPort {
                transport: Transport::Tcp,
                addr: HOST,
                port: 40000,
            })
        );
        let v6 = OwnedPort::from_fpath("/scheme/udp/[::]:0/[fe80::1]:7").expect("a socket");
        assert_eq!((v6.port, v6.addr.is_ipv6()), (7, true));
        // Every address is refused, IPv4 or IPv6: the owner would send from any source.
        for every in [
            "/scheme/udp/0.0.0.0:0/0.0.0.0:5353",
            "/scheme/tcp/10.0.2.2:80/0.0.0.0:40000",
            "/scheme/udp/[::]:0/[::]:9",
        ] {
            assert_eq!(
                OwnedPort::from_fpath(every),
                Err(OwnError::UnspecifiedAddress),
                "{every}"
            );
        }
        assert!(!OwnError::UnspecifiedAddress.to_string().is_empty());
        for bad in [
            "",
            "udp/0.0.0.0:0/0.0.0.0:5353",
            "/scheme/icmp/echo/10.0.2.2",
            "/scheme/udpx/0.0.0.0:0/0.0.0.0:5353",
            "/scheme/udp/0.0.0.0:5353",
            "/scheme/udp/0.0.0.0:0/0.0.0.0:5353/extra",
            "/scheme/udp/nonsense/0.0.0.0:5353",
            "/scheme/file/0.0.0.0:0/0.0.0.0:5353",
        ] {
            assert_eq!(
                OwnedPort::from_fpath(bad),
                Err(OwnError::NotASocket),
                "{bad}"
            );
        }
        assert_eq!(
            OwnedPort::from_fpath("/scheme/udp/0.0.0.0:0/0.0.0.0:0"),
            Err(OwnError::Unbound)
        );
        assert!(!OwnError::Unbound.to_string().is_empty());
    }

    #[test]
    fn the_table_holds_a_bounded_set_of_ports() {
        let mut owned = Ownership::new();
        assert!(!owned.owns(Transport::Udp, 1, HOST));
        assert_eq!(owned.own(udp_port(1)), Ok(true));
        // Owning twice is fine, reports nothing new, and takes no second slot.
        assert_eq!(owned.own(udp_port(1)), Ok(false));
        for port in 2..=Ownership::MAX_PORTS as u16 {
            assert_eq!(owned.own(udp_port(port)), Ok(true));
        }
        assert_eq!(owned.own(udp_port(1)), Ok(false));
        assert_eq!(owned.own(udp_port(999)), Err(OwnError::Full));
        assert!(owned.owns(Transport::Udp, 16, HOST));
        assert!(
            !owned.owns(Transport::Tcp, 16, HOST),
            "TCP and UDP ports differ"
        );
        assert!(
            !owned.owns(Transport::Udp, 16, IpAddr::V4(Ipv4Addr::new(10, 0, 2, 16))),
            "a port is owned at one address"
        );
    }

    #[test]
    fn filters_on_ports_need_ownership() {
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 2, 16));
        let mut owned = Ownership::new();
        let udp_5353 = port_filter(IP_PROTOCOL_UDP, 5353, Some(HOST));
        assert_eq!(owned.check_filter(&udp_5353), Err(FilterOwnError::NotOwned));
        owned.own(udp_port(5353)).expect("room");
        assert_eq!(owned.check_filter(&udp_5353), Ok(()));
        // A port is owned at one address: a filter must name it, and only it.
        for unowned in [None, Some(other)] {
            assert_eq!(
                owned.check_filter(&port_filter(IP_PROTOCOL_UDP, 5353, unowned)),
                Err(FilterOwnError::NotOwned)
            );
        }
        assert_eq!(
            owned.check_filter(&port_filter(IP_PROTOCOL_TCP, 5353, Some(HOST))),
            Err(FilterOwnError::NotOwned)
        );
        // Filters without a destination port stay allowed for root's sockets.
        let icmp = Filter::new(FilterSpec::Match(MatchSpec {
            ip_protocol: Some(ICMP),
            ..MatchSpec::default()
        }))
        .expect("valid");
        assert_eq!(Ownership::new().check_filter(&icmp), Ok(()));
        let all = Filter::new(FilterSpec::AllFrames).expect("valid");
        assert_eq!(Ownership::new().check_filter(&all), Ok(()));
    }

    #[test]
    fn tx_validation_takes_only_owned_traffic_from_the_nic() {
        let nic = MacAddr(PEER);
        let mut owned = Ownership::new();
        owned.own(udp_port(5353)).expect("room");
        let good = udp_from([10, 0, 2, 15], 5353, &[]);
        assert_eq!(owned.check_tx(&headers(&good), nic), Ok(()));
        assert_eq!(
            owned.check_tx(&headers(&good), MacAddr([2, 0, 0, 0, 0, 0x77])),
            Err(TxReject::SourceMac)
        );
        assert_eq!(
            owned.check_tx(&headers(&udp_from([10, 0, 2, 15], 5354, &[])), nic),
            Err(TxReject::NotOwned)
        );
        assert_eq!(
            owned.check_tx(&headers(&udp_from([10, 0, 2, 15], 5353, &[7])), nic),
            Err(TxReject::Tagged)
        );
        // A priority tag (VLAN ID 0) names no VLAN, but it is a VLAN header: refused too.
        let priority = headers(&udp_from([10, 0, 2, 15], 5353, &[0]));
        assert_eq!(priority.vlan, None);
        assert!(priority.tagged);
        assert_eq!(owned.check_tx(&priority, nic), Err(TxReject::Tagged));
        // Validated TX sends no fragments, not even the first (more-fragments set, offset
        // 0), whose UDP header is there: raw TX is the path for them.
        let first_fragment = ethernet(
            &[],
            ETHERTYPE_IPV4,
            &ipv4(
                IP_PROTOCOL_UDP,
                [10, 0, 2, 15],
                [10, 0, 0, 1],
                0x2000,
                &udp(5353, 53),
            ),
        );
        assert_eq!(
            owned.check_tx(&headers(&first_fragment), nic),
            Err(TxReject::NotTransport)
        );
        let icmp = ethernet(
            &[],
            ETHERTYPE_IPV4,
            &ipv4(
                ICMP,
                [10, 0, 2, 15],
                [10, 0, 2, 2],
                0,
                &[8, 0, 0, 0, 0, 0, 0, 0],
            ),
        );
        assert_eq!(
            owned.check_tx(&headers(&icmp), nic),
            Err(TxReject::NotTransport)
        );
        let arp = ethernet(&[], 0x0806, &[0; 28]);
        assert_eq!(
            owned.check_tx(&headers(&arp), nic),
            Err(TxReject::NotTransport)
        );
        // The source address is checked too: an owned port from another address is spoofed.
        assert_eq!(
            owned.check_tx(&headers(&udp_from([10, 0, 2, 99], 5353, &[])), nic),
            Err(TxReject::NotOwned)
        );
        assert_eq!(
            owned.check_tx(&headers(&udp_from([0, 0, 0, 0], 5353, &[])), nic),
            Err(TxReject::NotOwned)
        );
        assert!(!TxReject::Tagged.to_string().is_empty());
    }
}
