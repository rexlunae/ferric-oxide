//! The test proper; see the crate documentation for the steps.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::{Duration, Instant};

use etherparse::err::packet::BuildVecWriteError;
use etherparse::{
    ArpEthIpv4Packet, ArpOperation, IcmpEchoHeader, Icmpv4Type, NetSlice, PacketBuilder,
    SlicedPacket, TransportSlice,
};
use ferric_netq::call::{
    ActiveMode, Bind, BindFlags, FilterId, Mode, QUEUE_PATH, Request as QueueRequest, RingSize,
    Stats, StatsError,
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
use syscall::error::{EACCES, EAGAIN, EBUSY, EINVAL, EIO, ENOENT, ENOSPC, ENOTCONN};

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
/// The UDP port the second socket owns, through a netstack socket bound on it.
const OWNED_PORT: u16 = 47353;
/// The UDP protocol number (RFC 768).
const IP_PROTOCOL_UDP: u8 = 17;
/// How long the DNS round trip may take: the configured server is past QEMU's gateway.
const DNS_WAIT: Duration = Duration::from_secs(10);
/// The DNS query's ID.
const DNS_ID: u16 = 0x4e71;
/// Frames each side sends in the TX benchmark.
const BENCH_FRAMES: u64 = 20_000;
/// The UDP payload of a benchmark frame: 18 bytes make a 60-byte frame, the Ethernet minimum
/// without the frame check sequence.
const BENCH_PAYLOAD: [u8; 18] = *b"ferric-netq bench!";
/// How long either side of the benchmark may take before the step fails.
const BENCH_LIMIT: Duration = Duration::from_secs(60);
/// The discard port (RFC 863), where the benchmark sends.
const DISCARD_PORT: u16 = 9;

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

/// Wraps a `std::io` result with what it was doing.
fn io<T>(what: &'static str, r: std::io::Result<T>) -> Result<T, Failure> {
    r.map_err(|e| Failure::Errno {
        what,
        errno: e.raw_os_error().unwrap_or(EIO),
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

/// The `Bind` record the test binds with, in `mode` with `flags`.
fn bind_record(mode: Mode, flags: BindFlags) -> Bind {
    let size = RingSize::new(ENTRIES).expect("64 entries is a valid ring size");
    Bind {
        frame_size: FRAME_SIZE,
        fill: size,
        rx: size,
        tx: size,
        completion: size,
        mode,
        cpu: 0,
        flags,
    }
}

/// Registers `buffer` in `region` and attaches grants of its UMEM and its rings to the queue
/// socket `fd`.
fn attach(fd: &Fd, region: &Fd, buffer: &Buffer, layout: &RingLayout) -> Result<(), Failure> {
    let (words, n) = RegbufRequest::Register {
        addr: buffer.addr(),
        len: buffer.len as u64,
    }
    .encode();
    ctx(
        "Register",
        region.call_wo(&[], CallFlags::empty(), &words[..n]),
    )?;
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

/// `Own` with descriptor `raw`, which stays open here too (`FD_CLONE`).
fn own(fd: &Fd, raw: usize) -> Result<(), Failure> {
    let (words, n) = QueueRequest::Own.encode();
    ctx(
        "Own",
        libredox::call::call_wo(
            fd.raw(),
            &raw.to_ne_bytes(),
            CallFlags::FD | CallFlags::FD_CLONE,
            &words[..n],
        ),
    )?;
    Ok(())
}

/// A filter for UDP to `port`.
fn udp_port(port: u16) -> MatchSpec {
    MatchSpec {
        ip_protocol: Some(IP_PROTOCOL_UDP),
        dst_port: Some(port),
        ..MatchSpec::default()
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
    /// The application's side of the bound socket `fd`, with every receive frame on Fill.
    fn new(fd: Fd, layout: &RingLayout, buffer: &Buffer) -> Result<Self, Failure> {
        let umem = Umem::new(UMEM_BYTES as u64, FRAME_SIZE).expect("UMEM_BYTES is whole frames");
        let free_tx = (RX_FRAMES..FRAMES)
            .map(|index| umem.frame(index).expect("TX frames lie inside the UMEM"))
            .collect();
        let mut socket = Self {
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

    /// Puts up to `count` copies of `bytes` on TX, as many as there are free frames, and
    /// kicks once. Returns how many the driver sent.
    fn send_batch(&mut self, buffer: &Buffer, bytes: &[u8], count: usize) -> Result<u64, Failure> {
        self.reclaim(buffer)?;
        let len =
            u32::try_from(bytes.len()).map_err(|_| Failure::FrameTooLong { len: bytes.len() })?;
        for _ in 0..count {
            let Some(frame) = self.free_tx.pop() else {
                break;
            };
            let packet = Packet::new(&self.umem, frame, len)
                .map_err(|_| Failure::FrameTooLong { len: bytes.len() })?;
            write_frame(buffer.umem(), frame, bytes);
            self.rings
                .tx
                .push(buffer.rings(), &PacketSlot::new(&packet))
                .map_err(|source| Failure::Push { ring: "TX", source })?;
        }
        let sent = queue_call(&self.fd, "Kick", QueueRequest::Kick, &[])? as u64;
        self.sent += sent;
        Ok(sent)
    }

    /// TX frames not on TX, once Completion is drained.
    fn free_tx(&mut self, buffer: &Buffer) -> Result<usize, Failure> {
        self.reclaim(buffer)?;
        Ok(self.free_tx.len())
    }

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
        answer: impl FnMut(&[u8]) -> Option<T>,
    ) -> Result<T, Failure> {
        self.exchange_within(buffer, what, request, WAIT, answer)
    }

    /// [`exchange`](Self::exchange), waiting up to `wait`.
    fn exchange_within<T>(
        &mut self,
        buffer: &Buffer,
        what: &'static str,
        request: &[u8],
        wait: Duration,
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
            if start.elapsed() > wait {
                return Err(Failure::Timeout { what, wait });
            }
            thread::sleep(POLL);
        }
    }
}

/// The second socket: bound without raw TX, owning [`OWNED_PORT`].
struct Owned {
    socket: Socket,
    buffer: Buffer,
    _region: Fd,
    /// The netstack socket that proves the port; the benchmark also sends through it.
    netstack: UdpSocket,
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
    /// The DNS server netstack is configured with.
    dns: Ipv4Addr,
    /// The second socket, from the own step.
    owned: Option<Owned>,
}

impl Test {
    fn unbound(&self) -> Result<&Fd, Failure> {
        self.unbound.as_ref().ok_or(Failure::Errno {
            what: "the unbound socket",
            errno: ENOTCONN,
        })
    }

    /// A UDP frame from `src_mac` and port `src_port` to `to` port `dst_port`, through the
    /// gateway.
    fn udp_frame(
        &self,
        src_mac: [u8; 6],
        src_port: u16,
        to: Ipv4Addr,
        dst_port: u16,
        payload: &[u8],
    ) -> Result<Vec<u8>, Failure> {
        let builder = PacketBuilder::ethernet2(src_mac, self.gateway_mac)
            .ipv4(self.ip.octets(), to.octets(), 64)
            .udp(src_port, dst_port);
        let mut out = Vec::with_capacity(builder.size(payload.len()));
        builder
            .write_to_vec(&mut out, payload)
            .map_err(Failure::Build)?;
        Ok(out)
    }

    fn owned(&mut self) -> Result<&mut Owned, Failure> {
        self.owned.as_mut().ok_or(Failure::Errno {
            what: "the owning socket",
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
            &bind_record(Mode::Copy, BindFlags::empty()).to_bytes(),
        ),
    )?;
    attach(fd, &t.region, &t.buffer, &t.layout)
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
            &bind_record(Mode::ZeroCopy, BindFlags::empty()).to_bytes(),
        ),
    )?;
    // This socket sends ARP and ICMP, which only raw TX allows.
    let auto = bind_record(Mode::Auto, BindFlags::RAW_TX).to_bytes();
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
    t.socket = Some(Socket::new(fd, &t.layout, &t.buffer)?);
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
    let icmp = MatchSpec {
        ip_protocol: Some(IP_PROTOCOL_ICMP),
        src_addr: Some(IpAddr::V4(gateway)),
        ..MatchSpec::default()
    };
    add_filter(&socket.fd, icmp)?;
    socket.exchange(buffer, "echo reply through the socket", &request, |f| {
        is_echo_reply(f, 1).then_some(())
    })
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
         invalid_descriptors={} tx_rejected={} (test received {}, sent {})",
        stats.rx_packets,
        stats.tx_packets,
        stats.fill_empty_drops,
        stats.overflow_drops,
        stats.invalid_descriptors,
        stats.tx_rejected,
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
        && stats.tx_rejected == 0;
    if ok {
        Ok(())
    } else {
        Err(Failure::Data {
            what: "the socket's counters",
        })
    }
}

fn own_step(t: &mut Test) -> Result<(), Failure> {
    let fd = ctx(
        "open a second queue socket",
        Fd::open(
            &format!("/scheme/{}/{QUEUE_PATH}", t.scheme),
            O_RDWR | O_CLOEXEC,
            0,
        ),
    )?;
    // A region holds one registration, so the second socket's memory has its own.
    let region = ctx(
        "open a second region",
        Fd::open(
            &format!("/scheme/{SCHEME_NAME}/{REGION_PATH}"),
            O_RDWR | O_CLOEXEC,
            0,
        ),
    )?;
    let buffer = Buffer::new(UMEM_BYTES + t.layout.map_len())?;
    attach(&fd, &region, &buffer, &t.layout)?;
    let netstack = io(
        "bind a netstack UDP socket",
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, OWNED_PORT)),
    )?;
    let raw = netstack.as_raw_fd() as usize;
    expect_errno("Own before Bind", ENOTCONN, own(&fd, raw))?;
    let bind = bind_record(Mode::Copy, BindFlags::empty()).to_bytes();
    queue_call(&fd, "Bind", QueueRequest::Bind, &bind)?;
    expect_errno(
        "AddFilter on a port the socket does not own",
        EACCES,
        add_filter(&fd, udp_port(OWNED_PORT)),
    )?;
    let file = ctx(
        "open a file",
        Fd::open("/etc/net/ip", O_RDONLY | O_CLOEXEC, 0),
    )?;
    expect_errno("Own with a file", EINVAL, own(&fd, file.raw()))?;
    own(&fd, raw)?;
    add_filter(&fd, udp_port(OWNED_PORT))?;
    expect_errno(
        "AddFilter on another port",
        EACCES,
        add_filter(&fd, udp_port(OWNED_PORT + 1)),
    )?;
    let socket = Socket::new(fd, &t.layout, &buffer)?;
    t.owned = Some(Owned {
        socket,
        buffer,
        _region: region,
        netstack,
    });
    Ok(())
}

/// A DNS query (RFC 1035) for the root's name servers, which any recursive server answers.
fn dns_query() -> Vec<u8> {
    let mut query = Vec::with_capacity(17);
    query.extend_from_slice(&DNS_ID.to_be_bytes());
    // Recursion desired; one question.
    query.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    // The root name, type NS (2), class IN (1).
    query.extend_from_slice(&[0, 0, 2, 0, 1]);
    query
}

/// Whether `frame` is the DNS server's answer to [`dns_query`], to [`OWNED_PORT`].
fn is_dns_answer(frame: &[u8], dns: Ipv4Addr) -> bool {
    let Ok(packet) = SlicedPacket::from_ethernet(frame) else {
        return false;
    };
    let from_dns =
        matches!(&packet.net, Some(NetSlice::Ipv4(ip)) if ip.header().source() == dns.octets());
    let Some(TransportSlice::Udp(udp)) = packet.transport else {
        return false;
    };
    let payload = udp.payload();
    from_dns
        && udp.source_port() == 53
        && udp.destination_port() == OWNED_PORT
        && payload.len() >= 3
        && payload[..2] == DNS_ID.to_be_bytes()
        && payload[2] & 0x80 != 0
}

fn udp_step(t: &mut Test) -> Result<(), Failure> {
    let dns = t.dns;
    let query = t.udp_frame(t.mac, OWNED_PORT, dns, 53, &dns_query())?;
    let owned = t.owned()?;
    owned.socket.exchange_within(
        &owned.buffer,
        "DNS answer through the socket",
        &query,
        DNS_WAIT,
        |f| is_dns_answer(f, dns).then_some(()),
    )
}

fn spoof_step(t: &mut Test) -> Result<(), Failure> {
    let gateway = t.gateway;
    let spoofs = [
        t.udp_frame(
            [2, 0, 0, 0, 0, 0x66],
            OWNED_PORT,
            gateway,
            DISCARD_PORT,
            b"spoof",
        )?,
        t.udp_frame(t.mac, OWNED_PORT + 1, gateway, DISCARD_PORT, b"spoof")?,
        t.echo_request(200)?,
    ];
    let owned = t.owned()?;
    let before = stats_call(&owned.socket.fd, Stats::BYTES)?;
    for frame in &spoofs {
        owned.socket.send(&owned.buffer, frame)?;
    }
    let after = stats_call(&owned.socket.fd, Stats::BYTES)?;
    let free = owned.socket.free_tx(&owned.buffer)?;
    println!(
        "netq: tx_rejected {} -> {}, tx_packets {} -> {}, {free} TX frames free",
        before.tx_rejected, after.tx_rejected, before.tx_packets, after.tx_packets
    );
    let ok = after.tx_rejected == before.tx_rejected + spoofs.len() as u64
        && after.tx_packets == before.tx_packets
        && free == (FRAMES - RX_FRAMES) as usize;
    if ok {
        Ok(())
    } else {
        Err(Failure::Data {
            what: "TX validation of spoofed frames",
        })
    }
}

/// Frames per second for `frames` frames in `elapsed`.
fn per_second(frames: u64, elapsed: Duration) -> u64 {
    (frames as f64 / elapsed.as_secs_f64().max(1e-9)) as u64
}

fn bench_step(t: &mut Test) -> Result<(), Failure> {
    let gateway = t.gateway;
    let frame = t.udp_frame(t.mac, OWNED_PORT, gateway, DISCARD_PORT, &BENCH_PAYLOAD)?;
    let owned = t.owned()?;
    let batch = (FRAMES - RX_FRAMES) as usize;
    let start = Instant::now();
    let mut sent = 0;
    while sent < BENCH_FRAMES {
        let want = batch.min((BENCH_FRAMES - sent) as usize);
        sent += owned.socket.send_batch(&owned.buffer, &frame, want)?;
        if start.elapsed() > BENCH_LIMIT {
            return Err(Failure::Timeout {
                what: "end of the bypass TX benchmark",
                wait: BENCH_LIMIT,
            });
        }
    }
    let bypass = start.elapsed();
    let start = Instant::now();
    for _ in 0..BENCH_FRAMES {
        io(
            "send through netstack",
            owned
                .netstack
                .send_to(&BENCH_PAYLOAD, (gateway, DISCARD_PORT)),
        )?;
        if start.elapsed() > BENCH_LIMIT {
            return Err(Failure::Timeout {
                what: "end of the netstack TX benchmark",
                wait: BENCH_LIMIT,
            });
        }
    }
    let netstack = start.elapsed();
    let bypass_fps = per_second(BENCH_FRAMES, bypass);
    let netstack_fps = per_second(BENCH_FRAMES, netstack);
    println!(
        "FERRIC_BENCH netq-tx-udp60 frames={BENCH_FRAMES} bypass_fps={bypass_fps} \
         netstack_fps={netstack_fps} bypass_vs_netstack={:.2}",
        bypass_fps as f64 / netstack_fps.max(1) as f64
    );
    Ok(())
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
    // Closing unmaps the grants in the driver while the replies are on their way, and closes
    // the driver's copy of the owning netstack socket.
    drop(socket);
    drop(t.owned.take());
    netstack_echo(t.gateway, 3, WAIT)?;
    // The driver serves new sockets after the close: open, attach nothing, close.
    let path = format!("/scheme/{}/{QUEUE_PATH}", t.scheme);
    ctx(
        "open a queue socket again",
        Fd::open(&path, O_RDWR | O_CLOEXEC, 0),
    )?;
    Ok(())
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
    let ip = config_addr("/etc/net/ip")?;
    let gateway = config_addr("/etc/net/ip_router")?;
    let dns = config_addr("/etc/net/dns")?;
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
        dns,
        owned: None,
    })))
}

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
    let steps: [(&str, Step); 12] = [
        ("netq-netstack", netstack_step),
        ("netq-attach", attach_step),
        ("netq-checks", checks_step),
        ("netq-arp", arp_step),
        ("netq-steer", steer_step),
        ("netq-ping", ping_step),
        ("netq-stats", stats_step),
        ("netq-own", own_step),
        ("netq-udp", udp_step),
        ("netq-spoof", spoof_step),
        ("netq-bench", bench_step),
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
