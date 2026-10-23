//! Ferric Oxide: kernel-bypass queue sockets in copy mode (`bypass` feature, the
//! `drivers.net_bypass` option; io/netbypass-design.md in the project files, PR 2).
//!
//! `open("/scheme/network.<nic>/queue")` gives a queue socket, set up with the calls in
//! `ferric_netq::call`: `Attach` passes the UMEM grant and the ring grant (`kernel.regbuf`
//! grants, mapped here read-write and shared), `Bind` starts the queue, and `AddFilter` says
//! which frames to take. The socket's work is `ferric_netq::copy::CopyQueue`'s:
//!
//! - [`steer`] runs on every tick while a socket is bound. It drains the adapter, gives each
//!   frame to the first socket with a matching filter, and keeps the rest, in order, for
//!   netstack's reads ([`State::take_pending`]). With no socket bound it does nothing, and
//!   netstack reads straight from the adapter as upstream.
//! - `Kick` sends what the application put on TX through the adapter, after TX validation
//!   (`ferric_netq::own`) unless the socket was bound with `BindFlags::RAW_TX`.
//! - `Own` takes a netstack `udp` or `tcp` socket bound on a port. The driver keeps it open,
//!   so netstack keeps the port reserved while the queue socket lives, and from then on the
//!   socket may filter on that destination port and send from it. A filter on a port the
//!   socket does not own fails with `EACCES`.
//!
//! A socket whose rings turn out corrupt, or whose grant its owner revokes (checked on every
//! tick and call), is stopped: its calls fail with `EIO` and it takes no more frames. A revoke
//! is only safe once the kernel replaces a revoked grant's CPU mappings with zeroed pages
//! instead of unmapping them; until then a revoke between the check and an access faults the
//! driver. Closing it unmaps both grants and closes the driver's descriptors of them.
//!
//! Not here yet: zero-copy mode (`Bind` with `Mode::ZeroCopy` fails with `ENOSPC`),
//! unprivileged sockets (every queue socket is root's, as the scheme root is root-only, so any
//! socket may ask for raw TX), and per-core workers.

use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::AtomicU64;

use ferric_netq::call::{Bind, Mode, Request, Stats};
use ferric_netq::copy::{offer, CopyQueue, FilterTableError, Offered, Received, Receiver, Shared};
use ferric_netq::filter::Filter;
use ferric_netq::frame::{Headers, MacAddr};
use ferric_netq::own::{OwnError, OwnedPort};
use ferric_regbuf::abi::{GrantInfo, Request as RegbufRequest};
use libredox::call::MmapArgs;
use libredox::flag::{MAP_SHARED, PROT_READ, PROT_WRITE};
use libredox::Fd;
use redox_scheme::{CallerCtx, SendFdRequest, Socket};
use scheme_utils::{FpathWriter, ResourceSync};
use syscall::{
    CallFlags, Error, EventFlags, FobtainFdFlags, Result, EACCES, EBUSY, ECANCELED, EINVAL, EIO,
    ENOENT, ENOSPC, ENOTCONN,
};

use crate::{NetworkAdapter, NetworkResource, NetworkSchemeData};

/// Frames kept for netstack while sockets are bound, before the oldest is dropped.
const PENDING_MAX: usize = 256;
/// The longest frame read from the adapter.
const FRAME_MAX: usize = 16 << 10;
/// Packets sent per `Kick` at most, so one socket cannot hold the driver.
const KICK_BUDGET: usize = 256;
/// The longest path a netstack socket's `fpath` gives (`/scheme/udp/[v6]:port/[v6]:port`).
const SOCKET_PATH_MAX: usize = 128;

/// The scheme's bypass state.
pub(crate) struct State {
    socket: Rc<Socket>,
    pending: VecDeque<Vec<u8>>,
    buf: Vec<u8>,
    /// Whether a socket was bound at the last steering pass.
    steering: bool,
}

impl State {
    /// The state of a scheme served on `socket`.
    pub(crate) fn new(socket: Rc<Socket>) -> Self {
        Self {
            socket,
            pending: VecDeque::new(),
            buf: vec![0; FRAME_MAX],
            steering: false,
        }
    }

    /// Frames steering kept for netstack.
    pub(crate) fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Whether frames go through steering, so netstack reads only what it kept: a frame read
    /// straight from the adapter could be one a socket's filter wants.
    pub(crate) fn steering(&self) -> bool {
        self.steering
    }

    /// Copies the oldest frame kept for netstack into `buf`, if there is one.
    pub(crate) fn take_pending(&mut self, buf: &mut [u8]) -> Option<usize> {
        let frame = self.pending.pop_front()?;
        let len = frame.len().min(buf.len());
        buf[..len].copy_from_slice(&frame[..len]);
        Some(len)
    }
}

/// Whether `path` opens a queue socket.
pub(crate) fn is_queue_path(path: &str) -> bool {
    path == ferric_netq::call::QUEUE_PATH
}

/// Drains the adapter while a socket is bound, giving each frame to the first bound socket
/// whose filters match it and keeping the others for netstack. Returns the handles of the
/// sockets that received frames, to be woken.
pub(crate) fn steer<'a, T: NetworkAdapter + 'a>(
    handles: impl Iterator<Item = (&'a usize, &'a mut NetworkResource<T>)>,
    data: &mut NetworkSchemeData<T>,
) -> Vec<usize> {
    let mut queues: Vec<Steered<'_>> = handles
        .filter_map(|(&id, handle)| match handle {
            NetworkResource::Queue(Queue {
                bound: Some(bound), ..
            }) => Some(Steered { id, bound }),
            _ => None,
        })
        .filter_map(|steered| steered.bound.check_grants().then_some(steered))
        .collect();
    let mut woken = Vec::new();
    let state = &mut data.bypass;
    state.steering = !queues.is_empty();
    if !state.steering {
        return woken;
    }
    loop {
        let len = match data.adapter.read_packet(&mut state.buf) {
            Ok(Some(len)) => len,
            Ok(None) => break,
            Err(err) => {
                log::warn!("driver-network: reading a frame failed: {err}");
                break;
            }
        };
        let frame = &state.buf[..len];
        // A socket a frame stops is skipped for the rest of the pass, and only a frame that
        // reached a socket's RX ring wakes it.
        match offer(&mut queues, frame) {
            Offered::Unclaimed => {
                if state.pending.len() == PENDING_MAX {
                    state.pending.pop_front();
                }
                state.pending.push_back(frame.to_vec());
            }
            offered => {
                if let Some(id) = offered.woken().and_then(|i| queues.get(i)).map(|q| q.id) {
                    if !woken.contains(&id) {
                        woken.push(id);
                    }
                }
            }
        }
    }
    woken
}

/// A bound socket in one steering pass.
struct Steered<'a> {
    id: usize,
    bound: &'a mut Bound,
}

impl Receiver for Steered<'_> {
    fn active(&self) -> bool {
        !self.bound.stopped
    }

    fn wants(&self, headers: &Headers) -> bool {
        self.bound.queue.wants(headers)
    }

    fn deliver(&mut self, frame: &[u8]) -> Option<Received> {
        match self.bound.queue.receive(self.bound.grants.shared(), frame) {
            Ok(received) => Some(received),
            Err(err) => {
                log::warn!("driver-network: queue socket stopped: {err}");
                self.bound.stopped = true;
                None
            }
        }
    }
}

/// A queue socket.
pub(crate) struct Queue<T> {
    attached: Option<Grants>,
    bound: Option<Bound>,
    _adapter: PhantomData<T>,
}

impl<T> Queue<T> {
    /// A new, unbound socket.
    pub(crate) fn new() -> Option<Self> {
        Some(Self {
            attached: None,
            bound: None,
            _adapter: PhantomData,
        })
    }
}

impl<T> fmt::Debug for Queue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Queue")
            .field("attached", &self.attached.is_some())
            .field("bound", &self.bound.is_some())
            .finish()
    }
}

/// A bound socket: its grants and its copy-mode queue.
struct Bound {
    grants: Grants,
    queue: CopyQueue,
    /// The netstack sockets that prove its owned ports, kept open so the ports stay reserved.
    _owners: Vec<Fd>,
    /// Set once the application corrupted the rings.
    stopped: bool,
}

impl Bound {
    /// Whether the socket can be served this tick: not stopped, and neither grant revoked.
    /// A revoked grant stops the socket for good.
    ///
    /// This check alone cannot make a revoke safe, since one can land between it and an
    /// access; it relies on the kernel replacing a revoked grant's CPU mappings with zeroed
    /// pages rather than unmapping them, so the driver's accesses never fault.
    fn check_grants(&mut self) -> bool {
        if !self.stopped && (self.grants.umem.revoked() || self.grants.rings.revoked()) {
            log::warn!("driver-network: queue socket stopped: its grant was revoked");
            self.stopped = true;
        }
        !self.stopped
    }
}

/// The two grants an application attaches.
struct Grants {
    umem: Mapping,
    rings: Mapping,
}

impl Grants {
    /// The application's memory, as `CopyQueue` sees it.
    fn shared(&self) -> Shared<'_> {
        Shared {
            rings: self.rings.words(),
            umem: self.umem.words(),
        }
    }
}

/// A grant mapped read-write and shared into the driver.
struct Mapping {
    /// The driver's descriptor of the grant, closed after the unmap.
    _fd: Fd,
    base: NonNull<AtomicU64>,
    len: usize,
}

impl Mapping {
    /// Maps the whole of `grant`, which must be a read-write grant of whole pages.
    ///
    /// # Errors
    ///
    /// The kernel's errno for a descriptor that is not a grant, `EINVAL` for a grant that does
    /// not start and end on a page, `EACCES` for a read-only grant, and `ECANCELED` for a
    /// revoked one.
    fn map(grant: Fd) -> Result<Self> {
        let info = grant_info(&grant)?;
        let spec = info.spec;
        if info.revoked {
            return Err(Error::new(ECANCELED));
        }
        if !spec.access().allows_write() {
            return Err(Error::new(EACCES));
        }
        if !spec.is_page_aligned() || spec.is_empty() {
            return Err(Error::new(EINVAL));
        }
        let len = usize::try_from(spec.len()).map_err(|_| Error::new(EINVAL))?;
        // SAFETY: a fresh shared mapping of the grant at an address the kernel picks, so it
        // replaces nothing of ours. The kernel keeps the pages mapped until `Drop` unmaps them.
        let addr = unsafe {
            libredox::call::mmap(MmapArgs {
                fd: grant.raw(),
                addr: core::ptr::null_mut(),
                offset: 0,
                length: len,
                prot: PROT_READ | PROT_WRITE,
                flags: MAP_SHARED,
            })
        }?;
        let base = NonNull::new(addr.cast::<AtomicU64>()).ok_or(Error::new(EIO))?;
        Ok(Self {
            _fd: grant,
            base,
            len,
        })
    }

    /// Whether the grant's owner has revoked it. A grant whose state cannot be read counts as
    /// revoked.
    fn revoked(&self) -> bool {
        grant_info(&self._fd).map_or(true, |info| info.revoked)
    }

    /// The mapping as words.
    fn words(&self) -> &[AtomicU64] {
        // SAFETY: `base` is a page-aligned mapping of `len` bytes (a multiple of the page
        // size, so of 8), readable and writable until `Drop`, which needs `&mut self`. The
        // application writes the same pages at any time, which is why they are only ever
        // accessed as atomics: no access is a data race, whatever the application does.
        unsafe { std::slice::from_raw_parts(self.base.as_ptr(), self.len / 8) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made, unmapped once; nothing borrows `words` any more.
        if let Err(err) = unsafe { libredox::call::munmap(self.base.as_ptr().cast(), self.len) } {
            log::warn!("driver-network: unmapping a queue socket grant failed: {err}");
        }
    }
}

/// What the kernel reports about `grant`.
fn grant_info(grant: &Fd) -> Result<GrantInfo> {
    let mut bytes = [0; GrantInfo::BYTES];
    let (words, len) = RegbufRequest::Info.encode();
    grant.call_ro(&mut bytes, CallFlags::empty(), &words[..len])?;
    GrantInfo::from_bytes(&bytes).map_err(|_| Error::new(EINVAL))
}

impl<T: NetworkAdapter> ResourceSync for Queue<T> {
    type SchemeData = NetworkSchemeData<T>;
    type ResourceEnum = NetworkResource<T>;

    /// `Attach` (the UMEM grant and the ring grant, in that order) or `Own` (one netstack
    /// socket).
    fn on_sendfd(&mut self, data: &mut Self::SchemeData, request: &SendFdRequest) -> Result<usize> {
        match Request::decode(&[request.arg()]) {
            Ok(Request::Attach) if request.num_fds() == 2 => {}
            Ok(Request::Own) if request.num_fds() == 1 => return self.own(data, request),
            _ => return Err(Error::new(EINVAL)),
        }
        if self.bound.is_some() {
            return Err(Error::new(EBUSY));
        }
        let mut fds = [usize::MAX; 2];
        request.obtain_fd(&data.bypass.socket, FobtainFdFlags::empty(), &mut fds)?;
        let [umem, rings] = fds.map(Fd::new);
        self.attached = Some(Grants {
            umem: Mapping::map(umem)?,
            rings: Mapping::map(rings)?,
        });
        Ok(2)
    }

    fn call(
        &mut self,
        data: &mut Self::SchemeData,
        payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let request = Request::decode(metadata).map_err(|_| Error::new(EINVAL))?;
        if request.passes_descriptors() {
            return Err(Error::new(EINVAL));
        }
        if let Request::Bind = request {
            return self.bind(payload, MacAddr(data.adapter.mac_address()));
        }
        let bound = self.bound.as_mut().ok_or(Error::new(ENOTCONN))?;
        if !bound.check_grants() {
            return Err(Error::new(EIO));
        }
        match request {
            Request::AddFilter => {
                let filter = Filter::from_bytes(payload).map_err(|_| Error::new(EINVAL))?;
                let id = bound.queue.add_filter(filter).map_err(|err| match err {
                    FilterTableError::NotOwned(_) => Error::new(EACCES),
                    _ => Error::new(ENOSPC),
                })?;
                usize::try_from(id.get()).map_err(|_| Error::new(ENOSPC))
            }
            Request::RemoveFilter(id) => bound
                .queue
                .remove_filter(id)
                .map(|()| 0)
                .map_err(|_| Error::new(ENOENT)),
            Request::Kick => kick(bound, &mut data.adapter),
            Request::Stats => {
                if payload.len() != Stats::BYTES {
                    return Err(Error::new(EINVAL));
                }
                payload.copy_from_slice(&bound.queue.stats().to_bytes());
                Ok(Stats::BYTES)
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn fevent(&mut self, _data: &mut Self::SchemeData, _flags: EventFlags) -> Result<EventFlags> {
        Ok(EventFlags::empty())
    }

    fn fpath(&mut self, _data: &mut Self::SchemeData, w: &mut FpathWriter) -> Result<()> {
        w.push_str(ferric_netq::call::QUEUE_PATH);
        Ok(())
    }
}

impl<T: NetworkAdapter> Queue<T> {
    /// `Own`: takes the netstack socket and records the port it is bound on.
    ///
    /// # Errors
    ///
    /// `ENOTCONN` before `Bind`, `EIO` on a stopped socket, `EINVAL` for a descriptor that is
    /// not a netstack TCP or UDP socket bound to one address (a socket bound to `0.0.0.0` is
    /// refused, so every owned port names the source address TX checks), and `ENOSPC` when the
    /// socket owns as many ports as it may.
    fn own(&mut self, data: &mut NetworkSchemeData<T>, request: &SendFdRequest) -> Result<usize> {
        let bound = self.bound.as_mut().ok_or(Error::new(ENOTCONN))?;
        if !bound.check_grants() {
            return Err(Error::new(EIO));
        }
        let mut fds = [usize::MAX; 1];
        request.obtain_fd(&data.bypass.socket, FobtainFdFlags::empty(), &mut fds)?;
        let [owner] = fds.map(Fd::new);
        let mut path = [0; SOCKET_PATH_MAX];
        let len = owner.fpath(&mut path)?;
        let port = core::str::from_utf8(&path[..len])
            .map_err(|_| Error::new(EINVAL))
            .and_then(|path| OwnedPort::from_fpath(path).map_err(own_errno))?;
        // One netstack socket per owned port is enough to keep it reserved: a repeated `Own`
        // of a port already owned closes its descriptor here (when `owner` drops), so repeated
        // calls cannot pile up descriptors in the driver.
        if bound.queue.own(port).map_err(own_errno)? {
            bound._owners.push(owner);
        }
        Ok(1)
    }

    /// `Bind`: starts the queue on the attached grants, on the NIC with MAC address `mac`.
    fn bind(&mut self, payload: &[u8], mac: MacAddr) -> Result<usize> {
        if self.bound.is_some() {
            return Err(Error::new(EBUSY));
        }
        let bind = Bind::from_bytes(payload).map_err(|_| Error::new(EINVAL))?;
        if bind.mode == Mode::ZeroCopy {
            return Err(Error::new(ENOSPC));
        }
        let grants = self.attached.take().ok_or(Error::new(ENOTCONN))?;
        let umem_bytes = grants.umem.words().len() as u64 * 8;
        let ring_bytes = grants.rings.words().len() * 8;
        let queue = match CopyQueue::bind(&bind, mac, umem_bytes, ring_bytes) {
            Ok(queue) => queue,
            Err(_) => {
                self.attached = Some(grants);
                return Err(Error::new(EINVAL));
            }
        };
        self.bound = Some(Bound {
            grants,
            queue,
            _owners: Vec::new(),
            stopped: false,
        });
        Ok(0)
    }
}

/// The errno for an `Own` that failed.
fn own_errno(err: OwnError) -> Error {
    match err {
        OwnError::Full => Error::new(ENOSPC),
        _ => Error::new(EINVAL),
    }
}

/// `Kick`: sends what the application put on TX.
fn kick(bound: &mut Bound, adapter: &mut impl NetworkAdapter) -> Result<usize> {
    let result = bound
        .queue
        .transmit(bound.grants.shared(), KICK_BUDGET, |frame| {
            adapter.write_packet(frame).is_ok()
        });
    match result {
        Ok(sent) => Ok(sent),
        Err(err) => {
            log::warn!("driver-network: queue socket stopped: {err}");
            bound.stopped = true;
            Err(Error::new(EIO))
        }
    }
}
