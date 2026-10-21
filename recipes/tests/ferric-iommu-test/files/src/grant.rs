//! Registered-buffer grants mapped into the driver's DMA domain (`MapGrant` and `UnmapGrant`
//! in `ferric_iommu::abi`; io/registered-buffers-design.md in the project files, PR 3).
//!
//! The driver (this process, with its domain bound) serves a small scheme,
//! [`SCHEME`], and forks a child that plays the *application*: it registers a buffer of its
//! own in a `kernel.regbuf` region, opens a read-write grant of it and a sub-grant of the
//! grant's second page, and passes both to the driver over the scheme (fd passing). Then it
//! asks the driver to do requests ([`Op`]): map a grant into the domain, load a byte pattern
//! into the device, and have the device DMA it into the grant. The driver answers a request
//! with `EIO` when the grant turns out to be revoked after the DMA, as a real driver fails a
//! request whose DMA faulted (the `edu` device itself reports no DMA errors).
//!
//! The application checks, printing a result line for each:
//!
//! - `iommu-grant-nodomain`: `MapGrant` on the domain descriptor it inherited, which it has
//!   not bound (a forked child has no bound domain), fails with `ENODEV`;
//! - `iommu-grant-dma`: the device's DMA through the mapped grant, and through the mapped
//!   sub-grant, lands in the application's own buffer, page by page as asked, after refusing
//!   the unbound child's attempt to unmap the driver's mapping using its inherited descriptor;
//! - `iommu-grant-unmap`: the driver's own `UnmapGrant` works, and a second one of the same IOVA
//!   fails with `ENOENT`;
//! - `iommu-grant-revoke`: after `Revoke`, the same DMA to the grant's old IOVA faults at the
//!   IOMMU (the driver announces the fault, which the kernel must log), the request fails with
//!   `EIO`, the buffer is unchanged, and mapping the grant again fails with `ECANCELED`;
//! - `iommu-subgrant-revoke`: the sub-grant was revoked with its parent: the same holds for
//!   its IOVA.

use std::collections::BTreeMap;
use std::fmt;

use ferric_iommu::abi::Request as IommuRequest;
use ferric_regbuf::abi::{
    Access, GrantId, GrantInfo, GrantSpec, PAGE_SIZE, REGION_PATH, Request, SCHEME_NAME,
};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_RDWR};
use redox_scheme::scheme::{SchemeState, SchemeSync, register_sync_scheme};
use redox_scheme::{
    CallerCtx, OpenResult, RequestKind, Response, SendFdRequest, SignalBehavior, Socket,
};
use syscall::CallFlags;
use syscall::error::{EBADF, EINVAL, EIO, ENOENT, Error};
use syscall::flag::FobtainFdFlags;
use syscall::schemev2::NewFdFlags;

use crate::{Edu, Failure, GRANT_TESTS as TESTS, Mapped, PAGE, Translator};

/// The scheme the driver serves the application on.
const SCHEME: &str = "ferric-iommu-test-grants";

/// The errno values the test expects, as relibc numbers them.
const ENODEV: usize = 19;
const ECANCELED: usize = 125;

/// The grant slots the application names its grants by.
const SLOT_GRANT: u64 = 1;
const SLOT_SUB: u64 = 2;

/// The bytes the device writes, and the one the application fills its buffer with to see
/// that nothing changed.
const FIRST: u8 = 0x3c;
const SECOND: u8 = 0x77;
const AFTER_REVOKE: u8 = 0x99;
const CANARY: u8 = 0xa5;

/// The buffer's length: two pages, the grant's first and the sub-grant's.
const LEN: usize = 2 * PAGE;

/// A request the application makes, as a `SYS_CALL` on the scheme with metadata
/// `[op, slot, arg]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
enum Op {
    /// `MapGrant` the grant in `slot` and remember its IOVA.
    Map = 1,
    /// Fill the device's buffer with the byte `arg`.
    Load = 2,
    /// Have the device write its buffer to page `arg` of the grant in `slot`; `EIO` when the
    /// grant is revoked afterwards.
    Dma = 3,
    /// `MapGrant` the grant in `slot` once more, then `UnmapGrant` that IOVA twice: 0 when the
    /// first worked and the second failed with `ENOENT`.
    UnmapTwice = 4,
    /// `MapGrant` the grant in `slot` once more, returning the errno it failed with (0 if it
    /// worked, and then unmapping it again).
    Remap = 5,
}

impl Op {
    fn from_raw(raw: u64) -> Option<Self> {
        Some(match raw {
            1 => Self::Map,
            2 => Self::Load,
            3 => Self::Dma,
            4 => Self::UnmapTwice,
            5 => Self::Remap,
            _ => return None,
        })
    }
}

/// Why an application step failed.
#[derive(Debug)]
enum AppFailure {
    /// A call failed with an errno.
    Errno { what: &'static str, errno: usize },
    /// A call that must fail with `expected` returned `got` (an errno, or 0 for success).
    Expected {
        what: &'static str,
        expected: usize,
        got: usize,
    },
    /// The buffer did not hold what it should: the first wrong byte.
    Data {
        what: &'static str,
        offset: usize,
        byte: u8,
    },
    /// The kernel's grant description could not be decoded.
    Payload(ferric_regbuf::abi::BytesError),
    /// An earlier step failed.
    NotRun,
}

impl fmt::Display for AppFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno { what, errno } => write!(f, "{what} failed with errno {errno}"),
            Self::Expected {
                what,
                expected,
                got,
            } => write!(f, "{what} should give errno {expected}, got {got}"),
            Self::Data { what, offset, byte } => {
                write!(f, "{what}: byte {offset:#x} is {byte:#04x}")
            }
            Self::Payload(err) => write!(f, "bad grant description from the kernel: {err}"),
            Self::NotRun => f.write_str("not run: an earlier step failed"),
        }
    }
}

/// Wraps a libredox result with what it was doing.
fn ctx<T>(what: &'static str, r: libredox::error::Result<T>) -> Result<T, AppFailure> {
    r.map_err(|e| AppFailure::Errno {
        what,
        errno: usize::try_from(e.errno()).unwrap_or(EIO as usize),
    })
}

/// Fails unless `got` (an errno, or 0) is `expected`.
fn expect(what: &'static str, expected: usize, got: usize) -> Result<(), AppFailure> {
    if got == expected {
        Ok(())
    } else {
        Err(AppFailure::Expected {
            what,
            expected,
            got,
        })
    }
}

/// The errno `r` failed with, or 0.
fn errno_of<T>(r: Result<T, AppFailure>) -> Result<usize, AppFailure> {
    match r {
        Ok(_) => Ok(0),
        Err(AppFailure::Errno { errno, .. }) => Ok(errno),
        Err(other) => Err(other),
    }
}

/// `MapGrant` of the grant behind `grant` (this process's descriptor) in `domain`.
fn map_grant(domain: usize, grant: usize) -> libredox::error::Result<usize> {
    let (words, n) = IommuRequest::MapGrant {
        grant_fd: grant as u64,
    }
    .encode();
    libredox::call::call_wo(domain, &[], CallFlags::empty(), &words[..n])
}

/// `UnmapGrant` of `iova` in `domain`.
fn unmap_grant(domain: usize, iova: usize) -> libredox::error::Result<usize> {
    let (words, n) = IommuRequest::UnmapGrant { iova: iova as u64 }.encode();
    libredox::call::call_wo(domain, &[], CallFlags::empty(), &words[..n])
}

/// The kernel's description of the grant behind `fd`.
fn grant_info(fd: usize) -> libredox::error::Result<GrantInfo> {
    let mut info = [0; GrantInfo::BYTES];
    let (words, n) = Request::Info.encode();
    libredox::call::call_ro(fd, &mut info, CallFlags::empty(), &words[..n])?;
    GrantInfo::from_bytes(&info).map_err(|_| libredox::error::Error::new(EIO))
}

/// The driver's side: the scheme's state.
struct Driver<'a> {
    socket: &'a Socket,
    edu: &'a Edu,
    domain: usize,
    /// A page of the driver's own, translated in its domain, that loads the device's buffer.
    source: Mapped,
    source_iova: usize,
    /// Grant descriptors by slot.
    grants: BTreeMap<u64, usize>,
    /// The IOVA `MapGrant` gave each slot's grant.
    iovas: BTreeMap<u64, usize>,
    /// Set when the application closes its handle.
    done: bool,
}

/// The scheme's handle for the application.
const HANDLE: usize = 1;

/// A system call errno as a scheme error.
fn sys(err: libredox::error::Error) -> Error {
    Error::new(err.errno())
}

impl Driver<'_> {
    fn grant(&self, slot: u64) -> syscall::Result<usize> {
        self.grants.get(&slot).copied().ok_or(Error::new(ENOENT))
    }

    fn op(&mut self, op: Op, slot: u64, arg: u64) -> syscall::Result<usize> {
        match op {
            Op::Map => {
                let iova = map_grant(self.domain, self.grant(slot)?).map_err(sys)?;
                self.iovas.insert(slot, iova);
                Ok(iova)
            }
            Op::Load => {
                let byte = u8::try_from(arg).map_err(|_| Error::new(EINVAL))?;
                self.source.bytes().fill(byte);
                self.edu
                    .dma(self.source_iova as u64, false)
                    .map_err(|_| Error::new(EIO))?;
                Ok(0)
            }
            Op::Dma => self.dma(slot, arg),
            Op::UnmapTwice => {
                let iova = map_grant(self.domain, self.grant(slot)?).map_err(sys)?;
                unmap_grant(self.domain, iova).map_err(sys)?;
                match unmap_grant(self.domain, iova) {
                    Err(err) if err.errno() == ENOENT => Ok(0),
                    Err(err) => Err(sys(err)),
                    Ok(_) => Err(Error::new(EIO)),
                }
            }
            Op::Remap => match map_grant(self.domain, self.grant(slot)?) {
                Ok(iova) => {
                    unmap_grant(self.domain, iova).map_err(sys)?;
                    Ok(0)
                }
                Err(err) => Ok(usize::try_from(err.errno()).unwrap_or(EIO as usize)),
            },
        }
    }

    /// Has the device write its buffer to page `page` of the grant in `slot`, then fails the
    /// request with `EIO` if the grant is revoked: its mapping is gone, so the DMA faulted. A
    /// grant already revoked before the DMA (the application revoked it between requests,
    /// as it would with a request in flight) makes the driver announce the fault first, so the
    /// boot check can match the kernel's fault line to it.
    fn dma(&mut self, slot: u64, page: u64) -> syscall::Result<usize> {
        let fd = self.grant(slot)?;
        let iova = *self.iovas.get(&slot).ok_or(Error::new(ENOENT))?;
        let page = usize::try_from(page).map_err(|_| Error::new(EINVAL))?;
        let target = page
            .checked_mul(PAGE)
            .and_then(|off| iova.checked_add(off))
            .ok_or(Error::new(EINVAL))?;
        if grant_info(fd).map_err(sys)?.revoked {
            crate::trace::expect_dma_fault(true, target as u64);
        }
        self.edu
            .dma(target as u64, true)
            .map_err(|_| Error::new(EIO))?;
        if grant_info(fd).map_err(sys)?.revoked {
            return Err(Error::new(EIO));
        }
        Ok(0)
    }
}

impl SchemeSync for Driver<'_> {
    fn scheme_root(&mut self) -> syscall::Result<usize> {
        Ok(0)
    }

    fn openat(
        &mut self,
        fd: usize,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<OpenResult> {
        if fd != 0 || !path.trim_matches('/').is_empty() {
            return Err(Error::new(ENOENT));
        }
        Ok(OpenResult::ThisScheme {
            number: HANDLE,
            flags: NewFdFlags::empty(),
        })
    }

    fn call(
        &mut self,
        id: usize,
        _payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        if id != HANDLE {
            return Err(Error::new(EBADF));
        }
        let &[op, slot, arg] = metadata else {
            return Err(Error::new(EINVAL));
        };
        let op = Op::from_raw(op).ok_or(Error::new(EINVAL))?;
        self.op(op, slot, arg)
    }

    fn on_sendfd(&mut self, request: &SendFdRequest) -> syscall::Result<usize> {
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

    fn on_close(&mut self, id: usize) {
        if id == HANDLE {
            self.done = true;
        }
    }
}

/// Serves the application until it closes its handle.
fn serve(socket: &Socket, driver: &mut Driver<'_>) -> syscall::Result<()> {
    let mut state = SchemeState::new();
    while !driver.done {
        let Some(request) = socket.next_request(SignalBehavior::Restart)? else {
            break;
        };
        let response = match request.kind() {
            RequestKind::Call(call) => call.handle_sync(driver, &mut state),
            RequestKind::SendFd(sendfd) => {
                let result = driver.on_sendfd(&sendfd);
                Response::new(result, sendfd)
            }
            RequestKind::OnClose { id } => {
                driver.on_close(id);
                continue;
            }
            _ => continue,
        };
        socket.write_response(response, SignalBehavior::Restart)?;
    }
    Ok(())
}

/// Runs the grant tests with the driver's bound domain `domain`; returns whether they all
/// passed. The application prints the result lines.
pub fn run(edu: &Edu, translator: &Translator, domain: usize) -> Result<bool, Failure> {
    let source = Mapped::dma(PAGE)?;
    let source_iova = translator.translate(source.addr(0))?;
    let socket =
        Socket::create().map_err(|err| Failure::Call("creating the scheme", errno(err)))?;
    let mut driver = Driver {
        socket: &socket,
        edu,
        domain,
        source,
        source_iova,
        grants: BTreeMap::new(),
        iovas: BTreeMap::new(),
        done: false,
    };
    register_sync_scheme(&socket, SCHEME, &mut driver)
        .map_err(|err| Failure::Call("registering the scheme", errno(err)))?;
    // SAFETY: the program is single-threaded, so the child is a consistent copy; it runs the
    // application and leaves with `_exit`, never returning into the driver's code.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(Failure::Io(std::io::Error::last_os_error()));
    }
    if child == 0 {
        let code = if application(domain) { 0 } else { 1 };
        // SAFETY: ends the child at once, as `fork` requires.
        unsafe { libc::_exit(code) }
    }
    let served = serve(&socket, &mut driver);
    for (_, fd) in core::mem::take(&mut driver.grants) {
        let _ = libredox::call::close(fd);
    }
    let mut status = 0;
    // SAFETY: `status` is a live integer for the call to fill in.
    if unsafe { libc::waitpid(child, &mut status, 0) } != child {
        return Err(Failure::Io(std::io::Error::last_os_error()));
    }
    served.map_err(|err| Failure::Call("serving the application", errno(err)))?;
    // WIFEXITED with status 0.
    Ok(status == 0)
}

/// A scheme error's errno.
fn errno(err: Error) -> usize {
    usize::try_from(err.errno).unwrap_or(EIO as usize)
}

/// The application's buffer: anonymous private memory of its own.
struct Buffer {
    ptr: *mut u8,
}

impl Buffer {
    fn new() -> Result<Self, AppFailure> {
        // SAFETY: a fresh anonymous mapping at an address the kernel chooses.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(AppFailure::Errno {
                what: "mmap the buffer",
                errno: std::io::Error::last_os_error()
                    .raw_os_error()
                    .and_then(|e| usize::try_from(e).ok())
                    .unwrap_or(EIO as usize),
            });
        }
        Ok(Self { ptr: ptr.cast() })
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the buffer is LEN bytes of this process's memory, mapped until drop; the
        // device may write it through a grant, which is what the test checks for.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, LEN) }
    }

    /// Fails unless page `page` holds `byte` throughout.
    fn check(&mut self, what: &'static str, page: usize, byte: u8) -> Result<(), AppFailure> {
        let bytes = &self.bytes()[page * PAGE..(page + 1) * PAGE];
        match bytes.iter().position(|&b| b != byte) {
            Some(i) => Err(AppFailure::Data {
                what,
                offset: page * PAGE + i,
                byte: bytes[i],
            }),
            None => Ok(()),
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: unmaps the buffer's own mapping, which nothing uses afterwards.
        unsafe { libc::munmap(self.ptr.cast(), LEN) };
    }
}

/// What the application keeps between steps.
struct App {
    scheme: Fd,
    region: Fd,
    buffer: Buffer,
    grant: Option<Fd>,
    sub: Option<Fd>,
    grant_id: GrantId,
    domain: usize,
}

/// Opens the grant `spec` names from `from` (a region, or a grant with `sub`) and returns it
/// with its ID.
fn open_grant(from: &Fd, spec: GrantSpec, sub: bool) -> Result<(Fd, GrantId), AppFailure> {
    let mut path = [0; GrantSpec::PATH_MAX];
    let path = if sub {
        spec.encode_sub_path(&mut path)
    } else {
        spec.encode_path(&mut path)
    };
    let fd = ctx("open a grant", from.openat(path, O_RDWR | O_CLOEXEC, 0))?;
    let mut info = [0; GrantInfo::BYTES];
    let (words, n) = Request::Info.encode();
    ctx(
        "Info",
        fd.call_ro(&mut info, CallFlags::empty(), &words[..n]),
    )?;
    let info = GrantInfo::from_bytes(&info).map_err(AppFailure::Payload)?;
    Ok((fd, info.id))
}

impl App {
    /// Opens the driver's scheme, registers the buffer and opens the grant and the sub-grant.
    fn new(domain: usize) -> Result<Self, AppFailure> {
        let scheme = ctx(
            "open the driver's scheme",
            Fd::open(&format!("/scheme/{SCHEME}"), O_RDWR | O_CLOEXEC, 0),
        )?;
        let region = ctx(
            "open a region",
            Fd::open(
                &format!("/scheme/{SCHEME_NAME}/{REGION_PATH}"),
                O_RDWR | O_CLOEXEC,
                0,
            ),
        )?;
        let mut buffer = Buffer::new()?;
        buffer.bytes().fill(0);
        let (words, n) = Request::Register {
            addr: buffer.ptr as u64,
            len: LEN as u64,
        }
        .encode();
        ctx(
            "Register",
            region.call_wo(&[], CallFlags::empty(), &words[..n]),
        )?;
        let whole =
            GrantSpec::new(Access::ReadWrite, 0, LEN as u64).map_err(|_| AppFailure::Errno {
                what: "grant range",
                errno: EINVAL as usize,
            })?;
        let (grant, grant_id) = open_grant(&region, whole, false)?;
        // The sub-grant is the grant's second page, from the grant's first byte.
        let second = GrantSpec::new(Access::ReadWrite, PAGE_SIZE, PAGE_SIZE).map_err(|_| {
            AppFailure::Errno {
                what: "sub-grant range",
                errno: EINVAL as usize,
            }
        })?;
        let (sub, _) = open_grant(&grant, second, true)?;
        Ok(Self {
            scheme,
            region,
            buffer,
            grant: Some(grant),
            sub: Some(sub),
            grant_id,
            domain,
        })
    }

    /// Asks the driver to do `op` on the grant in `slot`.
    fn op(&self, what: &'static str, op: Op, slot: u64, arg: u64) -> Result<usize, AppFailure> {
        ctx(
            what,
            self.scheme
                .call_wo(&[], CallFlags::empty(), &[op as u64, slot, arg]),
        )
    }

    /// Passes a grant to the driver into `slot`.
    fn send(&self, grant: Fd, slot: u64) -> Result<(), AppFailure> {
        // The descriptor moves to the driver, so it must not be closed here.
        let raw = grant.into_raw();
        ctx(
            "send a grant",
            self.scheme
                .call_wo(&raw.to_ne_bytes(), CallFlags::FD, &[slot]),
        )?;
        Ok(())
    }

    /// `MapGrant` on the inherited, unbound domain fails with `ENODEV`; then the grants go to
    /// the driver.
    fn no_domain(&mut self) -> Result<(), AppFailure> {
        let grant = self.grant.take().ok_or(AppFailure::NotRun)?;
        let sub = self.sub.take().ok_or(AppFailure::NotRun)?;
        let got = errno_of(ctx("MapGrant", map_grant(self.domain, grant.raw())))?;
        expect("MapGrant without a bound domain", ENODEV, got)?;
        self.send(grant, SLOT_GRANT)?;
        self.send(sub, SLOT_SUB)
    }

    /// The device DMAs through the grant into page 0, and through the sub-grant into page 1.
    fn dma(&mut self) -> Result<(), AppFailure> {
        let iova = self.op("MapGrant of the grant", Op::Map, SLOT_GRANT, 0)?;
        let got = errno_of(ctx(
            "UnmapGrant from the unbound child",
            unmap_grant(self.domain, iova),
        ))?;
        expect("UnmapGrant without a bound domain", ENODEV, got)?;
        // The parent's DMA below must still reach the mapping the child could not remove.
        self.op("loading the device", Op::Load, 0, u64::from(FIRST))?;
        self.op("DMA through the grant", Op::Dma, SLOT_GRANT, 0)?;
        self.buffer.check("page 0 after the DMA", 0, FIRST)?;
        self.buffer.check("page 1 after the DMA", 1, 0)?;
        self.op("MapGrant of the sub-grant", Op::Map, SLOT_SUB, 0)?;
        self.op("loading the device", Op::Load, 0, u64::from(SECOND))?;
        self.op("DMA through the sub-grant", Op::Dma, SLOT_SUB, 0)?;
        self.buffer.check("page 0 after the second DMA", 0, FIRST)?;
        self.buffer.check("page 1 after the second DMA", 1, SECOND)
    }

    fn unmap(&mut self) -> Result<(), AppFailure> {
        self.op("UnmapGrant twice", Op::UnmapTwice, SLOT_GRANT, 0)
            .map(drop)
    }

    /// Revokes the grant (and with it the sub-grant), then reloads the device.
    fn revoke(&mut self) -> Result<(), AppFailure> {
        let (words, n) = Request::Revoke(self.grant_id).encode();
        ctx(
            "Revoke",
            self.region.call_wo(&[], CallFlags::empty(), &words[..n]),
        )?;
        self.buffer.bytes().fill(CANARY);
        self.op("loading the device", Op::Load, 0, u64::from(AFTER_REVOKE))
            .map(drop)
    }

    /// The DMA to page 0 of the revoked grant in `slot` fails with `EIO` and leaves the buffer
    /// alone, and the grant cannot be mapped again.
    fn revoked(&mut self, slot: u64, what: &'static str) -> Result<(), AppFailure> {
        let got = errno_of(self.op(what, Op::Dma, slot, 0))?;
        expect(what, EIO as usize, got)?;
        self.buffer
            .check("page 0 after the revoked DMA", 0, CANARY)?;
        self.buffer
            .check("page 1 after the revoked DMA", 1, CANARY)?;
        let got = self.op("MapGrant after the revoke", Op::Remap, slot, 0)?;
        expect("MapGrant after the revoke", ECANCELED, got)
    }
}

/// Prints one result line; returns whether it passed.
fn report(name: &str, result: &Result<(), AppFailure>, pass: &str) -> bool {
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS {name}: {pass}");
            true
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL {name}: {err}");
            false
        }
    }
}

/// Prints a failure line for every test: none of them ran.
pub fn report_not_run(err: &dyn fmt::Display) {
    for name in TESTS {
        println!("FERRIC_TEST_FAIL {name}: not run: {err}");
    }
}

/// The application, in the forked child: runs the steps in order, stopping at the first that
/// fails, and prints a result line for each. Returns whether all passed.
fn application(domain: usize) -> bool {
    let mut app = match App::new(domain) {
        Ok(app) => app,
        Err(err) => {
            report_not_run(&err);
            return false;
        }
    };
    type Step = fn(&mut App) -> Result<(), AppFailure>;
    let steps: [(Step, &str); 5] = [
        (
            App::no_domain,
            "MapGrant on a domain the caller has not bound is refused (ENODEV)",
        ),
        (
            App::dma,
            "the device DMAed into the application's buffer through a grant and a sub-grant mapped with MapGrant",
        ),
        (
            App::unmap,
            "UnmapGrant took a grant mapping down; a second UnmapGrant gave ENOENT",
        ),
        (
            |app| {
                app.revoke()?;
                app.revoked(SLOT_GRANT, "DMA to the revoked grant")
            },
            "after Revoke the device's DMA to the grant's IOVA faulted (the kernel logs it), the request failed with EIO, the buffer is unchanged, MapGrant gives ECANCELED",
        ),
        (
            |app| app.revoked(SLOT_SUB, "DMA to the revoked sub-grant"),
            "revoking the parent revoked the sub-grant: its DMA faulted, the request failed with EIO, MapGrant gives ECANCELED",
        ),
    ];
    let mut ok = true;
    for ((step, pass), name) in steps.into_iter().zip(TESTS) {
        let result = if ok {
            step(&mut app)
        } else {
            Err(AppFailure::NotRun)
        };
        ok &= report(name, &result, pass);
    }
    ok
}
