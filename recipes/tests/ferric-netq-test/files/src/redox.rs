//! The test proper; see the crate documentation for the steps.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::process::ExitCode;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use etherparse::err::packet::BuildVecWriteError;
use etherparse::{
    ArpEthIpv4Packet, ArpOperation, IcmpEchoHeader, Icmpv4Type, NetSlice, PacketBuilder,
    SlicedPacket, TransportSlice,
};
use ferric_netq::call::{
    ActiveMode, Bind, FilterId, Mode, QUEUE_PATH, Request as QueueRequest, RingSize, Stats,
    StatsError,
};
use ferric_netq::copy::{read_frame, write_frame};
use ferric_netq::desc::{DescError, Packet};
use ferric_netq::filter::{Filter, FilterSpec, MatchSpec};
use ferric_netq::ring::{AppRings, FrameSlot, PacketSlot, RingLayout};
use ferric_netq::umem::{Frame, FrameError, FrameSize, Umem};
use ferric_regbuf::abi::{
    Access, BytesError, GrantInfo, GrantSpec, REGION_PATH, Request as RegbufRequest, SCHEME_NAME,
};
use ferric_ring::queue::{PushError, RingError};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_RDWR};
use syscall::CallFlags;
use syscall::error::{EAGAIN, EBUSY, EINVAL, EIO, ENOENT, ENOSPC, ENOTCONN};

/// The UMEM's frame size.
const FRAME_SIZE: FrameSize = FrameSize::Bytes2048;
/// Frames in the UMEM.
const FRAMES: u32 = 64;
/// The UMEM's length.
const UMEM_BYTES: usize = FRAMES as usize * 2048;
/// Entries in each of the four rings.
const ENTRIES: u32 = 64;
/// Frames `0..RX_FRAMES` go on Fill; the rest carry what the test sends.
const RX_FRAMES: u32 = 32;
/// The ICMP echo identifier of the requests the test sends through the socket.
const ICMP_ID: u16 = 0xf0e1;
/// The ICMP protocol number (RFC 790).
const IP_PROTOCOL_ICMP: u8 = 1;
/// The ARP EtherType (RFC 826).
const ETHERTYPE_ARP: u16 = 0x0806;
/// The payload of every echo request.
const ECHO_PAYLOAD: &[u8] = b"ferric-netq copy mode";
/// How long a step waits for an answer.
const WAIT: Duration = Duration::from_secs(3);
/// How long the first step waits for netstack's answer, as the link may only just be up.
const FIRST_WAIT: Duration = Duration::from_secs(15);
/// How often an unanswered request is sent again.
const RESEND: Duration = Duration::from_millis(500);
/// How long the test sleeps between looks at the RX ring.
const POLL: Duration = Duration::from_millis(2);
/// Echo requests left in flight when the socket is closed.
const IN_FLIGHT: u16 = 8;

/// Why a step failed.
#[derive(Debug)]
enum Failure {
    /// A call failed with an errno.
    Errno { what: &'static str, errno: i32 },
    /// A call that must fail with `expected` returned `got` (an errno, or 0 for success).
    Expected {
        what: &'static str,
        expected: i32,
        got: i32,
    },
    /// Something the driver reported or the test received is not what it should be.
    Data { what: &'static str },
    /// No answer came in time.
    Timeout { what: &'static str, wait: Duration },
    /// A file of the network configuration is missing or does not hold an IPv4 address.
    Config { path: &'static str },
    /// The kernel answered `Info` with a payload the ABI crate rejects.
    Payload(BytesError),
    /// The driver answered `Stats` with a payload the ABI crate rejects.
    Stats(StatsError),
    /// The driver wrote an impossible ring index.
    Ring(RingError),
    /// A ring the test produces on was full or unusable.
    Push {
        ring: &'static str,
        source: PushError,
    },
    /// The driver published an RX entry that is not a packet of the UMEM.
    Desc(DescError),
    /// The driver handed back a Completion entry that is not a frame of the UMEM.
    Frame(FrameError),
    /// A frame the test built does not fit in a UMEM frame.
    FrameTooLong { len: usize },
    /// Building a frame failed.
    Build(BuildVecWriteError),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno { what, errno } => write!(f, "{what} failed with errno {errno}"),
            Self::Expected {
                what,
                expected,
                got,
            } => write!(f, "{what} should fail with errno {expected}, got {got}"),
            Self::Data { what } => write!(f, "{what}: wrong contents"),
            Self::Timeout { what, wait } => write!(f, "no {what} within {} s", wait.as_secs()),
            Self::Config { path } => write!(f, "{path} holds no IPv4 address"),
            Self::Payload(err) => write!(f, "bad grant info from the kernel: {err}"),
            Self::Stats(err) => write!(f, "bad statistics from the driver: {err}"),
            Self::Ring(err) => write!(f, "the driver corrupted a ring: {err}"),
            Self::Push { ring, source } => write!(f, "cannot push on {ring}: {source}"),
            Self::Desc(err) => write!(f, "bad RX entry from the driver: {err}"),
            Self::Frame(err) => write!(f, "bad Completion entry from the driver: {err}"),
            Self::FrameTooLong { len } => write!(f, "a {len}-byte frame does not fit a frame"),
            Self::Build(err) => write!(f, "cannot build a frame: {err}"),
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Payload(err) => Some(err),
            Self::Stats(err) => Some(err),
            Self::Ring(err) => Some(err),
            Self::Push { source, .. } => Some(source),
            Self::Desc(err) => Some(err),
            Self::Frame(err) => Some(err),
            Self::Build(err) => Some(err),
            _ => None,
        }
    }
}

/// Wraps a libredox result with what it was doing.
fn ctx<T>(what: &'static str, r: libredox::error::Result<T>) -> Result<T, Failure> {
    r.map_err(|e| Failure::Errno {
        what,
        errno: e.errno(),
    })
}

/// Fails unless `r` failed with `expected`.
fn expect_errno<T>(
    what: &'static str,
    expected: i32,
    r: Result<T, Failure>,
) -> Result<(), Failure> {
    let got = match r {
        Ok(_) => 0,
        Err(Failure::Errno { errno, .. }) => errno,
        Err(other) => return Err(other),
    };
    if got == expected {
        Ok(())
    } else {
        Err(Failure::Expected {
            what,
            expected,
            got,
        })
    }
}

/// A page-aligned anonymous buffer: the UMEM, then the ring region.
struct Buffer {
    base: NonNull<AtomicU64>,
    len: usize,
}

impl Buffer {
    fn new(len: usize) -> Result<Self, Failure> {
        // SAFETY: a fresh anonymous mapping at an address the kernel chooses.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        let base = (ptr != libc::MAP_FAILED)
            .then(|| NonNull::new(ptr.cast::<AtomicU64>()))
            .flatten()
            .ok_or_else(|| Failure::Errno {
                what: "mmap the buffer",
                errno: std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(EIO),
            })?;
        Ok(Self { base, len })
    }

    fn addr(&self) -> u64 {
        self.base.as_ptr() as u64
    }

    /// The buffer as words.
    fn words(&self) -> &[AtomicU64] {
        // SAFETY: `base` is a page-aligned mapping of `len` bytes (whole pages, so whole
        // words), readable and writable until `Drop`, which needs `&mut self`. The driver
        // writes the same pages through its grant mappings at any time, which is why they are
        // only ever accessed as atomics.
        unsafe { std::slice::from_raw_parts(self.base.as_ptr(), self.len / 8) }
    }

    /// The UMEM's words.
    fn umem(&self) -> &[AtomicU64] {
        &self.words()[..UMEM_BYTES / 8]
    }

    /// The ring region's words.
    fn rings(&self) -> &[AtomicU64] {
        &self.words()[UMEM_BYTES / 8..]
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: unmaps the buffer's own mapping, which nothing borrows any more.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

/// Opens a read-write grant of `len` bytes at `offset` in `region`.
fn grant(region: &Fd, offset: u64, len: u64) -> Result<Fd, Failure> {
    let spec = GrantSpec::new(Access::ReadWrite, offset, len).map_err(|_| Failure::Errno {
        what: "grant range",
        errno: EINVAL,
    })?;
    let mut path = [0; GrantSpec::PATH_MAX];
    let path = spec.encode_path(&mut path);
    let fd = ctx("open a grant", region.openat(path, O_RDWR | O_CLOEXEC, 0))?;
    let mut info = [0; GrantInfo::BYTES];
    let (words, n) = RegbufRequest::Info.encode();
    ctx(
        "Info",
        fd.call_ro(&mut info, CallFlags::empty(), &words[..n]),
    )?;
    GrantInfo::from_bytes(&info).map_err(Failure::Payload)?;
    Ok(fd)
}

/// A queue socket call that writes `payload` to the driver.
fn queue_call(
    fd: &Fd,
    what: &'static str,
    request: QueueRequest,
    payload: &[u8],
) -> Result<usize, Failure> {
    let (words, n) = request.encode();
    ctx(what, fd.call_wo(payload, CallFlags::empty(), &words[..n]))
}

/// `Stats`, read into a payload of `len` bytes.
fn stats_call(fd: &Fd, len: usize) -> Result<Stats, Failure> {
    let mut bytes = vec![0; len];
    let (words, n) = QueueRequest::Stats.encode();
    ctx(
        "Stats",
        fd.call_ro(&mut bytes, CallFlags::empty(), &words[..n]),
    )?;
    Stats::from_bytes(&bytes).map_err(Failure::Stats)
}

/// The `Bind` record the test binds with, in `mode`.
fn bind_record(mode: Mode) -> Bind {
    let size = RingSize::new(ENTRIES).expect("64 entries is a valid ring size");
    Bind {
        frame_size: FRAME_SIZE,
        fill: size,
        rx: size,
        tx: size,
        completion: size,
        mode,
        cpu: 0,
    }
}

/// `AddFilter`, returning the filter's ID.
fn add_filter(fd: &Fd, spec: MatchSpec) -> Result<FilterId, Failure> {
    let filter = Filter::new(FilterSpec::Match(spec)).map_err(|_| Failure::Errno {
        what: "build a filter",
        errno: EINVAL,
    })?;
    let id = queue_call(fd, "AddFilter", QueueRequest::AddFilter, &filter.to_bytes())?;
    Ok(FilterId::new(id as u64))
}

/// The first network driver's scheme (`network.<nic>`), if there is one.
fn network_scheme() -> Option<String> {
    std::fs::read_dir("/scheme")
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|name| name.starts_with("network."))
}

/// The IPv4 address in the network configuration file at `path`.
fn config_addr(path: &'static str) -> Result<Ipv4Addr, Failure> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or(Failure::Config { path })
}

/// netcfg's live address of `eth0`: `ifaces/eth0/addr/list` holds it as `a.b.c.d/len`.
const NETCFG_ADDR: &str = "/scheme/netcfg/ifaces/eth0/addr/list";
/// netcfg's live route table, one rule a line; the default route reads `default via a.b.c.d …`.
const NETCFG_ROUTES: &str = "/scheme/netcfg/route/list";

/// The address netstack uses now. DHCP can change it after boot, so `/etc/net/ip` (the static
/// configuration) can be stale; it is only the fallback when netcfg has none.
fn live_ip() -> Result<Ipv4Addr, Failure> {
    std::fs::read_to_string(NETCFG_ADDR)
        .ok()
        .and_then(|text| text.trim().split('/').next()?.parse().ok())
        .map_or_else(|| config_addr("/etc/net/ip"), Ok)
}

/// The gateway of netstack's default route now, with `/etc/net/ip_router` as the fallback.
fn live_gateway() -> Result<Ipv4Addr, Failure> {
    std::fs::read_to_string(NETCFG_ROUTES)
        .ok()
        .and_then(|text| {
            text.lines()
                .filter(|line| line.starts_with("default"))
                .find_map(|line| {
                    let mut words = line.split_whitespace();
                    words.find(|&word| word == "via")?;
                    words.next()?.parse().ok()
                })
        })
        .map_or_else(|| config_addr("/etc/net/ip_router"), Ok)
}

/// Has netstack send an ICMP echo request to `to` and waits up to `wait` for the reply.
fn netstack_echo(to: Ipv4Addr, seq: u16, wait: Duration) -> Result<(), Failure> {
    let fd = ctx(
        "open an ICMP echo socket",
        Fd::open(
            &format!("/scheme/icmp/echo/{to}"),
            O_RDWR | O_NONBLOCK | O_CLOEXEC,
            0,
        ),
    )?;
    let mut request = seq.to_be_bytes().to_vec();
    request.extend_from_slice(ECHO_PAYLOAD);
    let mut reply = [0; 256];
    let start = Instant::now();
    let mut sent: Option<Instant> = None;
    loop {
        if sent.is_none_or(|at| at.elapsed() >= RESEND) {
            match fd.write(&request) {
                Ok(_) => sent = Some(Instant::now()),
                Err(e) if e.errno() == EAGAIN => {}
                Err(e) => return ctx("send an echo through netstack", Err(e)),
            }
        }
        match fd.read(&mut reply) {
            Ok(n) if reply[..n] == request[..] => return Ok(()),
            Ok(_) => {}
            Err(e) if e.errno() == EAGAIN => {}
            Err(e) => return ctx("receive an echo through netstack", Err(e)),
        }
        if start.elapsed() > wait {
            return Err(Failure::Timeout {
                what: "echo reply through netstack",
                wait,
            });
        }
        thread::sleep(POLL);
    }
}

/// The application's side of a bound queue socket.
struct Socket {
    fd: Fd,
    rings: AppRings,
    umem: Umem,
    /// TX frames not on TX.
    free_tx: Vec<Frame>,
    /// Packets taken off RX.
    received: u64,
    /// Packets the driver said `Kick` sent.
    sent: u64,
}

impl Socket {
    /// Hands the driver every receive frame.
    fn fill(&mut self, buffer: &Buffer) -> Result<(), Failure> {
        for index in 0..RX_FRAMES {
            let frame = self
                .umem
                .frame(index)
                .expect("RX_FRAMES lie inside the UMEM");
            self.push_fill(buffer, frame)?;
        }
        Ok(())
    }

    fn push_fill(&mut self, buffer: &Buffer, frame: Frame) -> Result<(), Failure> {
        self.rings
            .fill
            .push(buffer.rings(), &FrameSlot::new(frame))
            .map_err(|source| Failure::Push {
                ring: "Fill",
                source,
            })
    }

    /// Takes back the frames the driver has sent.
    fn reclaim(&mut self, buffer: &Buffer) -> Result<(), Failure> {
        while let Some(slot) = self
            .rings
            .completion
            .pop(buffer.rings())
            .map_err(Failure::Ring)?
        {
            self.free_tx
                .push(slot.frame(&self.umem).map_err(Failure::Frame)?);
        }
        Ok(())
    }

    /// Puts `bytes` on TX and kicks the driver.
    fn send(&mut self, buffer: &Buffer, bytes: &[u8]) -> Result<(), Failure> {
        self.reclaim(buffer)?;
        let frame = self.free_tx.pop().ok_or(Failure::Push {
            ring: "TX (no free frame)",
            source: PushError::Full,
        })?;
        let len =
            u32::try_from(bytes.len()).map_err(|_| Failure::FrameTooLong { len: bytes.len() })?;
        let packet = Packet::new(&self.umem, frame, len)
            .map_err(|_| Failure::FrameTooLong { len: bytes.len() })?;
        write_frame(buffer.umem(), frame, bytes);
        self.rings
            .tx
            .push(buffer.rings(), &PacketSlot::new(&packet))
            .map_err(|source| Failure::Push { ring: "TX", source })?;
        let sent = queue_call(&self.fd, "Kick", QueueRequest::Kick, &[])?;
        self.sent += sent as u64;
        Ok(())
    }

    /// The next received frame, with its frame given back to Fill.
    fn receive(&mut self, buffer: &Buffer) -> Result<Option<Vec<u8>>, Failure> {
        let Some(slot) = self.rings.rx.pop(buffer.rings()).map_err(Failure::Ring)? else {
            return Ok(None);
        };
        let packet = slot.packet(&self.umem).map_err(Failure::Desc)?;
        let mut bytes = vec![0; packet.len() as usize];
        read_frame(buffer.umem(), packet.frame(), &mut bytes);
        self.received += 1;
        self.push_fill(buffer, packet.frame())?;
        Ok(Some(bytes))
    }

    /// Sends `request` until a received frame satisfies `answer`, which returns what it found.
    fn exchange<T>(
        &mut self,
        buffer: &Buffer,
        what: &'static str,
        request: &[u8],
        mut answer: impl FnMut(&[u8]) -> Option<T>,
    ) -> Result<T, Failure> {
        let start = Instant::now();
        let mut sent: Option<Instant> = None;
        loop {
            if sent.is_none_or(|at| at.elapsed() >= RESEND) {
                self.send(buffer, request)?;
                sent = Some(Instant::now());
            }
            while let Some(frame) = self.receive(buffer)? {
                if let Some(found) = answer(&frame) {
                    return Ok(found);
                }
            }
            if start.elapsed() > WAIT {
                return Err(Failure::Timeout { what, wait: WAIT });
            }
            thread::sleep(POLL);
        }
    }
}

/// Everything the steps share.
struct Test {
    /// The network driver's scheme.
    scheme: String,
    /// The queue socket until `Bind`.
    unbound: Option<Fd>,
    /// The socket once bound.
    socket: Option<Socket>,
    region: Fd,
    buffer: Buffer,
    layout: RingLayout,
    mac: [u8; 6],
    ip: Ipv4Addr,
    gateway: Ipv4Addr,
    gateway_mac: [u8; 6],
    /// The ARP filter's ID, once added.
    arp_filter: Option<FilterId>,
}

impl Test {
    fn unbound(&self) -> Result<&Fd, Failure> {
        self.unbound.as_ref().ok_or(Failure::Errno {
            what: "the unbound socket",
            errno: ENOTCONN,
        })
    }

    /// An echo request to the gateway with sequence number `seq`, from the test's own
    /// identifier.
    fn echo_request(&self, seq: u16) -> Result<Vec<u8>, Failure> {
        let builder = PacketBuilder::ethernet2(self.mac, self.gateway_mac)
            .ipv4(self.ip.octets(), self.gateway.octets(), 64)
            .icmpv4_echo_request(ICMP_ID, seq);
        let mut out = Vec::with_capacity(builder.size(ECHO_PAYLOAD.len()));
        builder
            .write_to_vec(&mut out, ECHO_PAYLOAD)
            .map_err(Failure::Build)?;
        Ok(out)
    }
}

fn netstack_step(t: &mut Test) -> Result<(), Failure> {
    netstack_echo(t.gateway, 1, FIRST_WAIT)
}

fn attach_step(t: &mut Test) -> Result<(), Failure> {
    let fd = t.unbound()?;
    expect_errno(
        "Bind before Attach",
        ENOTCONN,
        queue_call(
            fd,
            "Bind",
            QueueRequest::Bind,
            &bind_record(Mode::Copy).to_bytes(),
        ),
    )?;
    let (words, n) = RegbufRequest::Register {
        addr: t.buffer.addr(),
        len: t.buffer.len as u64,
    }
    .encode();
    ctx(
        "Register",
        t.region.call_wo(&[], CallFlags::empty(), &words[..n]),
    )?;
    attach(fd, &t.region, &t.layout)
}

/// `Attach` on `fd` with fresh grants of the UMEM and the rings in `region`.
fn attach(fd: &Fd, region: &Fd, layout: &RingLayout) -> Result<(), Failure> {
    let umem = grant(region, 0, UMEM_BYTES as u64)?;
    let rings = grant(region, UMEM_BYTES as u64, layout.map_len() as u64)?;
    // The descriptors move to the driver, so they must not be closed here.
    let mut payload = umem.into_raw().to_ne_bytes().to_vec();
    payload.extend_from_slice(&rings.into_raw().to_ne_bytes());
    let (words, n) = QueueRequest::Attach.encode();
    ctx(
        "Attach",
        libredox::call::call_wo(fd.raw(), &payload, CallFlags::FD, &words[..n]),
    )?;
    Ok(())
}

/// The application's side of a socket bound on `fd`, with every receive frame on Fill.
fn bound_socket(fd: Fd, buffer: &Buffer, layout: &RingLayout) -> Result<Socket, Failure> {
    let umem = Umem::new(UMEM_BYTES as u64, FRAME_SIZE).expect("UMEM_BYTES is whole frames");
    let free_tx = (RX_FRAMES..FRAMES)
        .map(|index| umem.frame(index).expect("TX frames lie inside the UMEM"))
        .collect();
    let mut socket = Socket {
        fd,
        rings: layout.app(),
        umem,
        free_tx,
        received: 0,
        sent: 0,
    };
    socket.fill(buffer)?;
    Ok(socket)
}

fn checks_step(t: &mut Test) -> Result<(), Failure> {
    let fd = t.unbound()?;
    let arp = MatchSpec {
        ether_type: Some(ETHERTYPE_ARP),
        ..MatchSpec::default()
    };
    expect_errno("AddFilter before Bind", ENOTCONN, add_filter(fd, arp))?;
    expect_errno("Stats before Bind", ENOTCONN, stats_call(fd, Stats::BYTES))?;
    expect_errno(
        "a zero-copy Bind in copy mode",
        ENOSPC,
        queue_call(
            fd,
            "Bind",
            QueueRequest::Bind,
            &bind_record(Mode::ZeroCopy).to_bytes(),
        ),
    )?;
    let auto = bind_record(Mode::Auto).to_bytes();
    queue_call(fd, "Bind", QueueRequest::Bind, &auto)?;
    expect_errno(
        "a second Bind",
        EBUSY,
        queue_call(fd, "Bind", QueueRequest::Bind, &auto),
    )?;
    expect_errno(
        "RemoveFilter of an unknown ID",
        ENOENT,
        queue_call(
            fd,
            "RemoveFilter",
            QueueRequest::RemoveFilter(FilterId::new(999)),
            &[],
        ),
    )?;
    expect_errno("a short Stats payload", EINVAL, stats_call(fd, 8))?;
    let stats = stats_call(fd, Stats::BYTES)?;
    if stats.mode != ActiveMode::Copy || stats.rx_packets != 0 || stats.tx_packets != 0 {
        return Err(Failure::Data {
            what: "Stats of a fresh socket bound with Mode::Auto",
        });
    }
    let fd = t.unbound.take().ok_or(Failure::Errno {
        what: "the unbound socket",
        errno: ENOTCONN,
    })?;
    t.socket = Some(bound_socket(fd, &t.buffer, &t.layout)?);
    Ok(())
}

/// The gateway's MAC address, if `frame` is its ARP reply to the test.
fn arp_reply(frame: &[u8], mac: [u8; 6], ip: Ipv4Addr, gateway: Ipv4Addr) -> Option<[u8; 6]> {
    let packet = SlicedPacket::from_ethernet(frame).ok()?;
    let Some(NetSlice::Arp(arp)) = packet.net else {
        return None;
    };
    let arp = arp.to_packet().try_eth_ipv4().ok()?;
    (arp.operation == ArpOperation::REPLY
        && arp.sender_ipv4 == gateway.octets()
        && arp.target_ipv4 == ip.octets()
        && arp.target_mac == mac)
        .then_some(arp.sender_mac)
}

/// Whether `frame` is the gateway's reply to the test's echo request `seq`.
fn is_echo_reply(frame: &[u8], seq: u16) -> bool {
    let Ok(packet) = SlicedPacket::from_ethernet(frame) else {
        return false;
    };
    let Some(TransportSlice::Icmpv4(icmp)) = packet.transport else {
        return false;
    };
    icmp.icmp_type() == Icmpv4Type::EchoReply(IcmpEchoHeader { id: ICMP_ID, seq })
        && icmp.payload() == ECHO_PAYLOAD
}

fn arp_step(t: &mut Test) -> Result<(), Failure> {
    let (mac, ip, gateway) = (t.mac, t.ip, t.gateway);
    let request = ArpEthIpv4Packet {
        operation: ArpOperation::REQUEST,
        sender_mac: mac,
        sender_ipv4: ip.octets(),
        target_mac: [0; 6],
        target_ipv4: gateway.octets(),
    };
    let builder = PacketBuilder::ethernet2(mac, [0xff; 6]).arp(request.to_arp_packet());
    let mut frame = Vec::with_capacity(builder.size());
    builder.write_to_vec(&mut frame).map_err(Failure::Build)?;
    let buffer = &t.buffer;
    let socket = t.socket.as_mut().ok_or(Failure::Errno {
        what: "the bound socket",
        errno: ENOTCONN,
    })?;
    let arp = MatchSpec {
        ether_type: Some(ETHERTYPE_ARP),
        ..MatchSpec::default()
    };
    t.arp_filter = Some(add_filter(&socket.fd, arp)?);
    t.gateway_mac = socket.exchange(buffer, "ARP reply from the gateway", &frame, |f| {
        arp_reply(f, mac, ip, gateway)
    })?;
    Ok(())
}

fn steer_step(t: &mut Test) -> Result<(), Failure> {
    // The socket takes ARP only, so netstack's echo (its neighbour cache already has the
    // gateway, from the netstack step) must still be answered.
    netstack_echo(t.gateway, 2, WAIT)
}

fn ping_step(t: &mut Test) -> Result<(), Failure> {
    let request = t.echo_request(1)?;
    let gateway = t.gateway;
    let buffer = &t.buffer;
    let socket = t.socket.as_mut().ok_or(Failure::Errno {
        what: "the bound socket",
        errno: ENOTCONN,
    })?;
    let arp = t.arp_filter.take().ok_or(Failure::Errno {
        what: "the ARP filter",
        errno: ENOENT,
    })?;
    queue_call(
        &socket.fd,
        "RemoveFilter",
        QueueRequest::RemoveFilter(arp),
        &[],
    )?;
    add_filter(&socket.fd, icmp_from(gateway))?;
    socket.exchange(buffer, "echo reply through the socket", &request, |f| {
        is_echo_reply(f, 1).then_some(())
    })
}

/// A filter for ICMP from `gateway`.
fn icmp_from(gateway: Ipv4Addr) -> MatchSpec {
    MatchSpec {
        ip_protocol: Some(IP_PROTOCOL_ICMP),
        src_addr: Some(IpAddr::V4(gateway)),
        ..MatchSpec::default()
    }
}

fn stats_step(t: &mut Test) -> Result<(), Failure> {
    let buffer = &t.buffer;
    let socket = t.socket.as_mut().ok_or(Failure::Errno {
        what: "the bound socket",
        errno: ENOTCONN,
    })?;
    while socket.receive(buffer)?.is_some() {}
    let stats = stats_call(&socket.fd, Stats::BYTES)?;
    println!(
        "netq: rx_packets={} tx_packets={} fill_empty_drops={} overflow_drops={} \
         invalid_descriptors={} (test received {}, sent {})",
        stats.rx_packets,
        stats.tx_packets,
        stats.fill_empty_drops,
        stats.overflow_drops,
        stats.invalid_descriptors,
        socket.received,
        socket.sent
    );
    // A frame can arrive between the drain and `Stats`, so RX may be ahead by a few; TX is
    // the test's alone.
    let ok = stats.mode == ActiveMode::Copy
        && stats.rx_packets >= socket.received
        && socket.received >= 2
        && stats.tx_packets == socket.sent
        && socket.sent >= 2
        && stats.invalid_descriptors == 0
        && stats.fill_empty_drops == 0
        && stats.overflow_drops == 0;
    if ok {
        Ok(())
    } else {
        Err(Failure::Data {
            what: "the socket's counters",
        })
    }
}

fn close_step(t: &mut Test) -> Result<(), Failure> {
    let mut requests = Vec::new();
    for seq in 0..IN_FLIGHT {
        requests.push(t.echo_request(100 + seq)?);
    }
    let buffer = &t.buffer;
    let mut socket = t.socket.take().ok_or(Failure::Errno {
        what: "the bound socket",
        errno: ENOTCONN,
    })?;
    for request in &requests {
        socket.send(buffer, request)?;
    }
    // Closing unmaps the grants in the driver while the replies are on their way.
    drop(socket);
    netstack_echo(t.gateway, 3, WAIT)?;
    // The driver serves a new socket after the close: on fresh grants of the same memory
    // (the rings zeroed, as for a new socket), it binds, takes ICMP from the gateway again,
    // and answers an echo the test sends through it.
    let path = format!("/scheme/{}/{QUEUE_PATH}", t.scheme);
    let fd = ctx(
        "open a queue socket again",
        Fd::open(&path, O_RDWR | O_CLOEXEC, 0),
    )?;
    for word in t.buffer.rings() {
        word.store(0, Ordering::SeqCst);
    }
    attach(&fd, &t.region, &t.layout)?;
    queue_call(
        &fd,
        "Bind the second socket",
        QueueRequest::Bind,
        &bind_record(Mode::Copy).to_bytes(),
    )?;
    let mut again = bound_socket(fd, &t.buffer, &t.layout)?;
    add_filter(&again.fd, icmp_from(t.gateway))?;
    let request = t.echo_request(200)?;
    again.exchange(
        &t.buffer,
        "echo reply through the second socket",
        &request,
        |f| is_echo_reply(f, 200).then_some(()),
    )?;
    drop(again);
    netstack_echo(t.gateway, 4, WAIT)
}

/// One step of the test.
type Step = fn(&mut Test) -> Result<(), Failure>;

/// What setting up found.
enum Setup {
    /// Ready to test.
    Ready(Box<Test>),
    /// The steps cannot run, for this reason.
    Skip(&'static str),
}

fn setup() -> Result<Setup, Failure> {
    let Some(scheme) = network_scheme() else {
        return Ok(Setup::Skip("no network driver"));
    };
    let queue = match Fd::open(
        &format!("/scheme/{scheme}/{QUEUE_PATH}"),
        O_RDWR | O_CLOEXEC,
        0,
    ) {
        Ok(fd) => fd,
        Err(e) if e.errno() == EINVAL => {
            return Ok(Setup::Skip(
                "the network driver has no queue sockets (built without drivers.net_bypass)",
            ));
        }
        Err(e) => return ctx("open a queue socket", Err(e)),
    };
    let region_path = format!("/scheme/{SCHEME_NAME}/{REGION_PATH}");
    let Ok(region) = Fd::open(&region_path, O_RDWR | O_CLOEXEC, 0) else {
        return Ok(Setup::Skip("no kernel.regbuf scheme"));
    };
    let mac_file = ctx(
        "open the MAC address",
        Fd::open(&format!("/scheme/{scheme}/mac"), O_RDONLY | O_CLOEXEC, 0),
    )?;
    let mut mac = [0; 6];
    if ctx("read the MAC address", mac_file.read(&mut mac))? != mac.len() {
        return Err(Failure::Data {
            what: "the MAC address",
        });
    }
    let ip = live_ip()?;
    let gateway = live_gateway()?;
    let size = RingSize::new(ENTRIES).expect("64 entries is a valid ring size");
    let layout = RingLayout::new(size, size, size, size);
    let buffer = Buffer::new(UMEM_BYTES + layout.map_len())?;
    println!("netq: {scheme}, {ip} via {gateway}");
    Ok(Setup::Ready(Box::new(Test {
        scheme,
        unbound: Some(queue),
        socket: None,
        region,
        buffer,
        layout,
        mac,
        ip,
        gateway,
        gateway_mac: [0; 6],
        arp_filter: None,
    })))
}

/// Runs the test: sets up, runs each step in order, and stops at the first failure.
pub fn main() -> ExitCode {
    let mut t = match setup() {
        Ok(Setup::Ready(t)) => t,
        Ok(Setup::Skip(why)) => {
            println!("FERRIC_TEST_SKIP netq: {why}");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL netq-setup: {err}");
            return ExitCode::FAILURE;
        }
    };
    let steps: [(&str, Step); 8] = [
        ("netq-netstack", netstack_step),
        ("netq-attach", attach_step),
        ("netq-checks", checks_step),
        ("netq-arp", arp_step),
        ("netq-steer", steer_step),
        ("netq-ping", ping_step),
        ("netq-stats", stats_step),
        ("netq-close", close_step),
    ];
    for (name, f) in steps {
        match f(&mut t) {
            Ok(()) => println!("FERRIC_TEST_PASS {name}"),
            Err(err) => {
                println!("FERRIC_TEST_FAIL {name}: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
