//! The test scheme, run in a forked child: it holds the grants the application passes it and
//! acts on them when asked ([`super::Op`]). It also tries to share what it maps onward (`mmap`
//! of the scheme answered with a grant mapping, registering borrowed pages), which the kernel
//! must refuse, and passes on a borrowed request buffer to a peer instance of itself.

use std::collections::BTreeMap;
use std::ffi::c_int;

use ferric_regbuf::abi::{GrantInfo, PAGE_SIZE, REGION_PATH, Request, SCHEME_NAME};
use libredox::Fd;
use libredox::flag::{MAP_SHARED, O_CLOEXEC, O_RDONLY, O_RDWR, PROT_READ, PROT_WRITE};
use redox_scheme::scheme::{SchemeState, SchemeSync, register_sync_scheme};
use redox_scheme::{
    CallerCtx, OpenResult, RequestKind, Response, SendFdRequest, SignalBehavior, Socket,
};
use syscall::error::{EBADF, EINVAL, EIO, ENOENT, Error, Result};
use syscall::flag::FobtainFdFlags;
use syscall::schemev2::NewFdFlags;
use syscall::{CallFlags, MapFlags};

use super::{FILL_BYTE, Op, errno, last_errno, path};

/// The scheme's root handle, and the handles opening its paths give.
const ROOT: usize = 0;
const HANDLE: usize = 1;
const REGISTER_BORROW: usize = 2;
const FORWARD: usize = 3;
const FILL: usize = 4;

/// A mapping of a grant's window the scheme keeps.
struct Mapping {
    ptr: *mut u8,
    /// The window's length: whole pages.
    len: usize,
    /// Where the granted bytes start in it.
    lead: usize,
    /// How many bytes are granted.
    granted: usize,
}

/// The scheme's state.
struct TestScheme<'s> {
    socket: &'s Socket,
    /// Grant descriptors by the slot the application named.
    grants: BTreeMap<u64, usize>,
    /// The kept mapping of each slot.
    mappings: BTreeMap<u64, Mapping>,
    /// A pipe a probe writes a mapping's first byte into.
    probe: [c_int; 2],
    /// The peer scheme `FORWARD` reads go to, if any.
    peer: Option<&'static str>,
    exit: bool,
}

/// The errno of a libredox error as a scheme error.
fn sys(err: libredox::error::Error) -> Error {
    Error::new(err.errno())
}

/// What the kernel says of the grant behind `fd`.
fn grant_info(fd: usize) -> Result<GrantInfo> {
    let mut info = [0; GrantInfo::BYTES];
    let (words, n) = Request::Info.encode();
    libredox::call::call_ro(fd, &mut info, CallFlags::empty(), &words[..n]).map_err(sys)?;
    GrantInfo::from_bytes(&info).map_err(|_| Error::new(EIO))
}

/// A byte count from the ABI as a `usize`.
fn size(n: u64) -> Result<usize> {
    usize::try_from(n).map_err(|_| Error::new(EIO))
}

/// Maps the whole window of the grant behind `fd`.
fn map_grant(fd: usize, write: bool) -> Result<Mapping> {
    let spec = grant_info(fd)?.spec;
    let len = size(spec.window_len())?;
    let prot = if write {
        PROT_READ | PROT_WRITE
    } else {
        PROT_READ
    };
    // SAFETY: a fresh shared mapping at an address the kernel chooses touches no existing
    // memory.
    let ptr = unsafe {
        libredox::call::mmap(libredox::call::MmapArgs {
            addr: core::ptr::null_mut(),
            length: len,
            prot,
            flags: MAP_SHARED,
            fd,
            offset: 0,
        })
    }
    .map_err(sys)?;
    Ok(Mapping {
        ptr: ptr.cast(),
        len,
        lead: size(spec.lead())?,
        granted: size(spec.len())?,
    })
}

/// Opens a fresh regbuf region and registers `len` bytes from `addr` in it, returning the
/// errno `Register` failed with as the error. The region closes again at once.
fn try_register(addr: *const u8, len: usize) -> Result<()> {
    let region = Fd::open(
        &format!("/scheme/{SCHEME_NAME}/{REGION_PATH}"),
        O_RDWR | O_CLOEXEC,
        0,
    )
    .map_err(sys)?;
    let (words, n) = Request::Register {
        addr: addr as u64,
        len: len as u64,
    }
    .encode();
    region
        .call_wo(&[], CallFlags::empty(), &words[..n])
        .map_err(sys)?;
    Ok(())
}

/// Drops a mapping of the grant behind `fd` with the regbuf `Unmap` call.
fn unmap_grant(fd: usize, mapping: &Mapping) -> Result<()> {
    let (words, n) = Request::Unmap {
        addr: mapping.ptr as u64,
    }
    .encode();
    libredox::call::call_wo(fd, &[], CallFlags::empty(), &words[..n]).map_err(sys)?;
    Ok(())
}

impl TestScheme<'_> {
    fn grant(&self, slot: u64) -> Result<usize> {
        self.grants.get(&slot).copied().ok_or(Error::new(ENOENT))
    }

    fn mapping(&self, slot: u64) -> Result<&Mapping> {
        self.mappings.get(&slot).ok_or(Error::new(ENOENT))
    }

    fn op(&mut self, op: Op, slot: u64, arg: u64) -> Result<usize> {
        match op {
            Op::Map => {
                let mapping = map_grant(self.grant(slot)?, arg == 1)?;
                self.mappings.insert(slot, mapping);
            }
            Op::Remap => {
                let fd = self.grant(slot)?;
                let mapping = map_grant(fd, arg == 1)?;
                unmap_grant(fd, &mapping)?;
            }
            Op::Check => {
                // A scheme handler runs by direct switch from its caller. Its timed wait
                // must wake even when the caller still owns the scheduler's timeslice.
                std::thread::sleep(std::time::Duration::from_millis(10));
                let m = self.mapping(slot)?;
                let byte = u8::try_from(arg).map_err(|_| Error::new(EINVAL))?;
                // SAFETY: the application asks for a check only while the mapping is there;
                // if the kernel took it away anyway, the fault ends this child and the
                // application's call fails, which fails the test.
                let data = unsafe { core::slice::from_raw_parts(m.ptr, m.len) };
                if data.iter().any(|&b| b != byte) {
                    return Err(Error::new(EIO));
                }
            }
            Op::Fill => {
                let m = self.mapping(slot)?;
                let byte = u8::try_from(arg).map_err(|_| Error::new(EINVAL))?;
                // SAFETY: as for `Check`; the mapping is writable when the application asks
                // for a fill.
                unsafe { core::ptr::write_bytes(m.ptr, byte, m.len) };
            }
            Op::Probe => self.probe(slot)?,
            Op::Munmap => {
                let m = self.mapping(slot)?;
                // SAFETY: munmap of a grant mapping must fail; if it did not, the mapping is
                // forgotten below and never touched again.
                if unsafe { libc::munmap(m.ptr.cast(), m.len) } == 0 {
                    self.mappings.remove(&slot);
                    return Ok(0);
                }
                return Err(Error::new(last_errno() as i32));
            }
            Op::Unmap => {
                // The mapping stays recorded here, so a later probe can check it is gone.
                unmap_grant(self.grant(slot)?, self.mapping(slot)?)?;
            }
            Op::Exit => self.exit = true,
            Op::CheckGranted => {
                let m = self.mapping(slot)?;
                let byte = u8::try_from(arg).map_err(|_| Error::new(EINVAL))?;
                // SAFETY: as for `Check`.
                let data = unsafe { core::slice::from_raw_parts(m.ptr, m.len) };
                let granted = m.lead..m.lead + m.granted;
                let wrong = data.iter().enumerate().any(|(i, &b)| {
                    let want = if granted.contains(&i) { byte } else { 0 };
                    b != want
                });
                if wrong {
                    return Err(Error::new(EIO));
                }
            }
            Op::Register => {
                let m = self.mapping(slot)?;
                return match try_register(m.ptr, m.len) {
                    Ok(()) => Ok(0),
                    Err(err) => Ok(err.errno as usize),
                };
            }
        }
        Ok(0)
    }

    /// `read` on a handle other than the root's, which `buf` is the reader's buffer for.
    fn read_handle(&mut self, id: usize, buf: &mut [u8]) -> Result<usize> {
        match id {
            REGISTER_BORROW => {
                // Only the buffer's whole pages are borrowed (the rest are bounce pages), so
                // the application passes a page-aligned buffer of whole pages.
                try_register(buf.as_ptr(), buf.len())?;
                Ok(buf.len())
            }
            FORWARD => {
                let peer = self.peer.ok_or(Error::new(ENOENT))?;
                let fd = Fd::open(
                    &format!("/scheme/{peer}/{}", path::FILL),
                    O_RDONLY | O_CLOEXEC,
                    0,
                )
                .map_err(sys)?;
                fd.read(buf).map_err(sys)
            }
            FILL => {
                buf.fill(FILL_BYTE);
                Ok(buf.len())
            }
            _ => Err(Error::new(EBADF)),
        }
    }

    /// Passes the kept mapping's first byte to the kernel through a pipe write, which reads it
    /// without this process touching it.
    fn probe(&self, slot: u64) -> Result<()> {
        let m = self.mapping(slot)?;
        // SAFETY: the kernel copies from the pointer itself and fails with EFAULT when it is
        // not mapped; this process never dereferences it here.
        let written = unsafe { libc::write(self.probe[1], m.ptr.cast(), 1) };
        if written != 1 {
            return Err(Error::new(last_errno() as i32));
        }
        let mut byte = 0_u8;
        // SAFETY: reads one byte into a local.
        if unsafe { libc::read(self.probe[0], (&raw mut byte).cast(), 1) } != 1 {
            return Err(Error::new(EIO));
        }
        Ok(())
    }
}

impl SchemeSync for TestScheme<'_> {
    fn scheme_root(&mut self) -> Result<usize> {
        Ok(ROOT)
    }

    fn openat(
        &mut self,
        fd: usize,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if fd != ROOT {
            return Err(Error::new(ENOENT));
        }
        let number = match path.trim_matches('/') {
            "" => HANDLE,
            path::REGISTER_BORROW => REGISTER_BORROW,
            path::FORWARD => FORWARD,
            path::FILL => FILL,
            _ => return Err(Error::new(ENOENT)),
        };
        Ok(OpenResult::ThisScheme {
            number,
            flags: NewFdFlags::empty(),
        })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        self.read_handle(id, buf)
    }

    /// Answers `mmap` of the scheme with the kept grant mapping of slot `offset / PAGE_SIZE`:
    /// an attempt to share it onward, which the kernel must refuse.
    fn mmap_prep(
        &mut self,
        id: usize,
        offset: u64,
        size: usize,
        _flags: MapFlags,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        if id != HANDLE {
            return Err(Error::new(EBADF));
        }
        let m = self.mapping(offset / PAGE_SIZE)?;
        if size > m.len {
            return Err(Error::new(EINVAL));
        }
        Ok(m.ptr as usize)
    }

    fn call(
        &mut self,
        id: usize,
        _payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        if id != HANDLE {
            return Err(Error::new(EBADF));
        }
        let &[op, slot, arg] = metadata else {
            return Err(Error::new(EINVAL));
        };
        let op = Op::from_raw(op).ok_or(Error::new(EINVAL))?;
        self.op(op, slot, arg)
    }

    fn on_sendfd(&mut self, request: &SendFdRequest) -> Result<usize> {
        if request.num_fds() != 1 {
            return Err(Error::new(EINVAL));
        }
        let mut fds = [usize::MAX];
        request.obtain_fd(self.socket, FobtainFdFlags::empty(), &mut fds)?;
        if let Some(old) = self.grants.insert(request.arg(), fds[0]) {
            let _ = libredox::call::close(old);
        }
        Ok(1)
    }
}

/// Registers the scheme as `name`, writes one byte to `ready`, and serves requests until the
/// application asks it to exit; `FORWARD` reads go to the scheme `peer`. Returns the exit code
/// for the child.
pub fn serve(ready: c_int, name: &'static str, peer: Option<&'static str>) -> i32 {
    match serve_inner(ready, name, peer) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("regbuf-test scheme: {err}");
            1
        }
    }
}

fn serve_inner(ready: c_int, name: &'static str, peer: Option<&'static str>) -> Result<()> {
    let socket = Socket::create()?;
    let mut probe = [0; 2];
    // SAFETY: `probe` has room for the two descriptors.
    if unsafe { libc::pipe(probe.as_mut_ptr()) } != 0 {
        return Err(Error::new(last_errno() as i32));
    }
    let mut scheme = TestScheme {
        socket: &socket,
        grants: BTreeMap::new(),
        mappings: BTreeMap::new(),
        probe,
        peer,
        exit: false,
    };
    register_sync_scheme(&socket, name, &mut scheme)?;
    // SAFETY: writes one byte from a local.
    if unsafe { libc::write(ready, [1_u8].as_ptr().cast(), 1) } != 1 {
        return Err(Error::new(errno::EIO as i32));
    }
    let mut state = SchemeState::new();
    while !scheme.exit {
        let Some(request) = socket.next_request(SignalBehavior::Restart)? else {
            break;
        };
        let response = match request.kind() {
            RequestKind::Call(call) => call.handle_sync(&mut scheme, &mut state),
            RequestKind::SendFd(sendfd) => {
                let result = scheme.on_sendfd(&sendfd);
                Response::new(result, sendfd)
            }
            RequestKind::OnClose { id } => {
                scheme.on_close(id);
                continue;
            }
            _ => continue,
        };
        socket.write_response(response, SignalBehavior::Restart)?;
    }
    Ok(())
}
