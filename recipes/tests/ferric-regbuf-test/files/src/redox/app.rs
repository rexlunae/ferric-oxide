//! The application side: registers a buffer, grants it to the test scheme, revokes it, and
//! checks each step.

use std::ffi::c_int;
use std::fmt;
use std::process::ExitCode;

use ferric_regbuf::abi::{
    Access, GrantId, GrantInfo, GrantSpec, PAGE_SIZE, REGION_PATH, Request, SCHEME_NAME, Usage,
};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_RDWR};
use syscall::CallFlags;

use super::{FILL_BYTE, Op, PEER, SCHEME, errno, path, server};

/// Pages in the test buffer.
const PAGES: usize = 4;
const LEN: usize = PAGES * PAGE_SIZE as usize;
/// A page, as a buffer length.
const PAGE: usize = PAGE_SIZE as usize;
/// Slots the application names its grants by.
const SLOT_RW: u64 = 1;
const SLOT_RO: u64 = 2;
const SLOT_RESHARE: u64 = 3;
const SLOT_SHARED: u64 = 4;
const SLOT_BYTES_RW: u64 = 5;
const SLOT_BYTES_RO: u64 = 6;
const SLOT_BOUNCE_IDLE: u64 = 7;
const SLOT_BOUNCE_FILL: u64 = 8;

/// Why a step failed.
#[derive(Debug)]
enum Failure {
    /// A call failed with an errno.
    Errno { what: &'static str, errno: usize },
    /// A call that must fail with `expected` returned `got` (an errno, or 0 for success).
    Expected {
        what: &'static str,
        expected: usize,
        got: usize,
    },
    /// Memory did not hold what it should.
    Data { what: &'static str },
    /// The kernel's accounting did not change as it should.
    Pinned {
        what: &'static str,
        expected: u64,
        got: u64,
    },
    /// The kernel answered with a payload the ABI crate rejects.
    Payload(ferric_regbuf::abi::BytesError),
    /// Pages the scheme only borrows reached this process through it: the re-share gap.
    Reshared { what: &'static str },
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
            Self::Data { what } => write!(f, "{what}: memory has the wrong contents"),
            Self::Pinned {
                what,
                expected,
                got,
            } => write!(f, "{what}: {got} pages pinned, expected {expected}"),
            Self::Payload(err) => write!(f, "bad payload from the kernel: {err}"),
            Self::Reshared { what } => write!(f, "{what}: borrowed pages were shared onward"),
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Payload(err) => Some(err),
            _ => None,
        }
    }
}

/// Wraps a libredox result with what it was doing.
fn ctx<T>(what: &'static str, r: libredox::error::Result<T>) -> Result<T, Failure> {
    r.map_err(|e| Failure::Errno {
        what,
        errno: usize::try_from(e.errno()).unwrap_or(errno::EIO),
    })
}

/// Fails unless `r` failed with `expected`.
fn expect_errno<T>(
    what: &'static str,
    expected: usize,
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

/// A call on a regbuf descriptor without payload.
fn call(fd: &Fd, what: &'static str, request: Request) -> Result<usize, Failure> {
    let (words, n) = request.encode();
    ctx(what, fd.call_wo(&[], CallFlags::empty(), &words[..n]))
}

/// The pinned pages of this user.
fn pinned(root: &Fd) -> Result<u64, Failure> {
    let mut payload = [0; Usage::BYTES];
    let (words, n) = Request::Usage.encode();
    ctx(
        "Usage",
        root.call_ro(&mut payload, CallFlags::empty(), &words[..n]),
    )?;
    Ok(Usage::from_bytes(&payload)
        .map_err(Failure::Payload)?
        .pinned)
}

/// The test scheme, served by a forked child.
struct Scheme {
    fd: Fd,
    child: libc::pid_t,
}

impl Scheme {
    /// Forks a server registered as `name`, forwarding to `peer`, and opens its scheme once it
    /// is registered.
    fn start(name: &'static str, peer: Option<&'static str>) -> Result<Self, Failure> {
        let mut ready: [c_int; 2] = [0; 2];
        // SAFETY: `ready` has room for the two descriptors.
        if unsafe { libc::pipe(ready.as_mut_ptr()) } != 0 {
            return Err(Failure::Errno {
                what: "pipe",
                errno: super::last_errno(),
            });
        }
        // SAFETY: the program is single-threaded here, so the child is a consistent copy; it
        // serves the scheme and leaves with `_exit`, never returning into the parent's code.
        let child = unsafe { libc::fork() };
        if child < 0 {
            return Err(Failure::Errno {
                what: "fork",
                errno: super::last_errno(),
            });
        }
        if child == 0 {
            let code = server::serve(ready[1], name, peer);
            // SAFETY: ends the child at once, as `fork` requires.
            unsafe { libc::_exit(code) }
        }
        let mut byte = 0_u8;
        // SAFETY: reads one byte into a local.
        if unsafe { libc::read(ready[0], (&raw mut byte).cast(), 1) } != 1 {
            return Err(Failure::Errno {
                what: "waiting for the test scheme",
                errno: errno::EIO,
            });
        }
        let path = format!("/scheme/{name}");
        let fd = ctx(
            "open the test scheme",
            Fd::open(&path, O_RDWR | O_CLOEXEC, 0),
        )?;
        Ok(Self { fd, child })
    }

    /// Asks the scheme to do `op` on the grant in `slot`.
    fn op(&self, what: &'static str, op: Op, slot: u64, arg: u64) -> Result<usize, Failure> {
        ctx(
            what,
            self.fd
                .call_wo(&[], CallFlags::empty(), &[op as u64, slot, arg]),
        )
    }

    /// Passes `grant` to the scheme into `slot`.
    fn send(&self, grant: Fd, slot: u64) -> Result<(), Failure> {
        // The descriptor moves to the scheme, so it must not be closed here.
        let raw = grant.into_raw();
        ctx(
            "send the grant",
            self.fd.call_wo(&raw.to_ne_bytes(), CallFlags::FD, &[slot]),
        )?;
        Ok(())
    }

    /// Stops the server and waits for it.
    fn stop(self) {
        let _ = self.op("exit", Op::Exit, 0, 0);
        let mut status = 0;
        // SAFETY: `status` is a live integer for the call to fill in.
        unsafe { libc::waitpid(self.child, &mut status, 0) };
    }
}

/// The application's buffer: anonymous memory, private unless made with [`Buffer::shared`].
struct Buffer {
    ptr: *mut u8,
}

impl Buffer {
    fn new() -> Result<Self, Failure> {
        Self::map(libc::MAP_PRIVATE)
    }

    /// A `MAP_SHARED` anonymous buffer.
    fn shared() -> Result<Self, Failure> {
        Self::map(libc::MAP_SHARED)
    }

    fn map(kind: c_int) -> Result<Self, Failure> {
        // SAFETY: a fresh anonymous mapping at an address the kernel chooses.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                kind | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Failure::Errno {
                what: "mmap the buffer",
                errno: super::last_errno(),
            });
        }
        Ok(Self { ptr: ptr.cast() })
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the buffer is LEN bytes of this process's memory, mapped until drop; the
        // scheme may write it through a grant, which is what the test checks for.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, LEN) }
    }

    fn addr(&self) -> u64 {
        self.ptr as u64
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: unmaps the buffer's own mapping, which nothing uses afterwards.
        unsafe { libc::munmap(self.ptr.cast(), LEN) };
    }
}

/// Opens a grant of `region` and returns it with its ID.
fn grant(region: &Fd, access: Access, offset: u64, len: u64) -> Result<(Fd, GrantId), Failure> {
    let spec = GrantSpec::new(access, offset, len).map_err(|_| Failure::Errno {
        what: "grant range",
        errno: errno::EINVAL,
    })?;
    let mut path = [0; GrantSpec::PATH_MAX];
    let path = spec.encode_path(&mut path);
    let fd = ctx("open a grant", region.openat(path, O_RDWR | O_CLOEXEC, 0))?;
    let mut info = [0; GrantInfo::BYTES];
    let (words, n) = Request::Info.encode();
    ctx(
        "Info",
        fd.call_ro(&mut info, CallFlags::empty(), &words[..n]),
    )?;
    let info = GrantInfo::from_bytes(&info).map_err(Failure::Payload)?;
    Ok((fd, info.id))
}

/// Everything the steps share.
struct Test {
    root: Fd,
    scheme: Scheme,
    /// A second instance the first forwards reads to.
    peer: Scheme,
    buffer: Buffer,
    region: Option<Fd>,
    rw: GrantId,
    pinned_before: u64,
}

/// One step of the test.
type Step = fn(&mut Test) -> Result<(), Failure>;

fn step(name: &str, result: Result<(), Failure>) -> bool {
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS {name}");
            true
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL {name}: {err}");
            false
        }
    }
}

/// Runs every step; stops at the first failure.
pub fn run() -> ExitCode {
    let root = match Fd::open(&format!("/scheme/{SCHEME_NAME}"), O_RDWR | O_CLOEXEC, 0) {
        Ok(root) => root,
        Err(err) => {
            println!("FERRIC_TEST_SKIP regbuf: no {SCHEME_NAME} scheme ({err})");
            return ExitCode::SUCCESS;
        }
    };
    let mut test = match setup(root) {
        Ok(test) => test,
        Err(err) => {
            println!("FERRIC_TEST_FAIL regbuf-setup: {err}");
            return ExitCode::FAILURE;
        }
    };
    let steps: [(&str, Step); 15] = [
        ("regbuf-register", register),
        ("regbuf-file-backed", file_backed),
        ("regbuf-grant-rw", grant_rw),
        ("regbuf-grant-ro", grant_ro),
        ("regbuf-pinned", pinned_mapping),
        ("regbuf-reshare-mmap", reshare_mmap),
        ("regbuf-reshare-register", reshare_register),
        ("regbuf-reshare-borrow", reshare_borrow),
        ("regbuf-forward-borrow", forward_borrow),
        ("regbuf-revoke", revoke),
        ("regbuf-unmap", unmap),
        ("regbuf-close", close),
        ("regbuf-shared", shared),
        ("regbuf-bytes", bytes),
        ("regbuf-bounce-newer", bounce_newer),
    ];
    let ok = steps.iter().all(|(name, f)| step(name, f(&mut test)));
    test.scheme.stop();
    test.peer.stop();
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn setup(root: Fd) -> Result<Test, Failure> {
    let pinned_before = pinned(&root)?;
    let scheme = Scheme::start(SCHEME, Some(PEER))?;
    let peer = Scheme::start(PEER, None)?;
    let mut buffer = Buffer::new()?;
    buffer.bytes().fill(0x5a);
    Ok(Test {
        root,
        scheme,
        peer,
        buffer,
        region: None,
        rw: GrantId::new(0),
        pinned_before,
    })
}

fn region_path() -> String {
    format!("/scheme/{SCHEME_NAME}/{REGION_PATH}")
}

fn register(t: &mut Test) -> Result<(), Failure> {
    let unaligned = ctx(
        "open a region",
        Fd::open(&region_path(), O_RDWR | O_CLOEXEC, 0),
    )?;
    expect_errno(
        "Register of an unaligned range",
        errno::EINVAL,
        call(
            &unaligned,
            "Register",
            Request::Register {
                addr: t.buffer.addr() + 1,
                len: PAGE_SIZE,
            },
        ),
    )?;
    let region = ctx(
        "open a region",
        Fd::open(&region_path(), O_RDWR | O_CLOEXEC, 0),
    )?;
    let whole = Request::Register {
        addr: t.buffer.addr(),
        len: LEN as u64,
    };
    call(&region, "Register", whole)?;
    expect_errno(
        "a second Register",
        errno::EEXIST,
        call(&region, "Register", whole),
    )?;
    let expected = t.pinned_before + PAGES as u64;
    let got = pinned(&t.root)?;
    if got != expected {
        return Err(Failure::Pinned {
            what: "after Register",
            expected,
            got,
        });
    }
    t.region = Some(region);
    Ok(())
}

/// A private writable mapping of a file cannot be registered (`EPERM`), even with its page
/// populated: only anonymous memory can.
fn file_backed(t: &mut Test) -> Result<(), Failure> {
    let path = std::env::temp_dir().join("ferric-regbuf-test.bin");
    let io = |what: &'static str| {
        move |err: std::io::Error| Failure::Errno {
            what,
            errno: err
                .raw_os_error()
                .and_then(|e| usize::try_from(e).ok())
                .unwrap_or(errno::EIO),
        }
    };
    std::fs::write(&path, vec![0x77; PAGE_SIZE as usize]).map_err(io("write the file"))?;
    // Read-write: RedoxFS refuses a PROT_WRITE mapping of a read-only descriptor (EBADF),
    // even a private one.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(io("open the file"))?;
    // SAFETY: a fresh private mapping of the file at an address the kernel chooses.
    let ptr = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGE_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            std::os::fd::AsRawFd::as_raw_fd(&file),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(Failure::Errno {
            what: "mmap the file",
            errno: super::last_errno(),
        });
    }
    // SAFETY: the page is mapped readable and writable until the munmap below; writing it
    // gives the mapping a private copy, which is still not anonymous memory.
    unsafe { ptr.cast::<u8>().write_volatile(0x78) };
    let region = ctx(
        "open a region",
        Fd::open(&region_path(), O_RDWR | O_CLOEXEC, 0),
    );
    let result = region.and_then(|region| {
        expect_errno(
            "Register of a file-backed private mapping",
            errno::EPERM,
            call(
                &region,
                "Register",
                Request::Register {
                    addr: ptr as u64,
                    len: PAGE_SIZE,
                },
            ),
        )
    });
    // SAFETY: unmaps the mapping made above, which nothing uses afterwards.
    unsafe { libc::munmap(ptr, PAGE_SIZE as usize) };
    let _ = std::fs::remove_file(&path);
    result?;
    let expected = t.pinned_before + PAGES as u64;
    let got = pinned(&t.root)?;
    if got != expected {
        return Err(Failure::Pinned {
            what: "after a refused Register",
            expected,
            got,
        });
    }
    Ok(())
}

fn region(t: &Test) -> Result<&Fd, Failure> {
    t.region.as_ref().ok_or(Failure::Errno {
        what: "region",
        errno: errno::EINVAL,
    })
}

fn grant_rw(t: &mut Test) -> Result<(), Failure> {
    let (fd, id) = grant(region(t)?, Access::ReadWrite, 0, LEN as u64)?;
    t.rw = id;
    t.scheme.send(fd, SLOT_RW)?;
    t.scheme.op("map the rw grant", Op::Map, SLOT_RW, 1)?;
    t.scheme
        .op("scheme reads the app's data", Op::Check, SLOT_RW, 0x5a)?;
    t.scheme.op("scheme writes", Op::Fill, SLOT_RW, 0xc3)?;
    if t.buffer.bytes().iter().any(|&b| b != 0xc3) {
        return Err(Failure::Data {
            what: "the scheme's write in the app's buffer",
        });
    }
    Ok(())
}

fn grant_ro(t: &mut Test) -> Result<(), Failure> {
    let (fd, _) = grant(region(t)?, Access::Read, PAGE_SIZE, 2 * PAGE_SIZE)?;
    t.scheme.send(fd, SLOT_RO)?;
    expect_errno(
        "a writable map of an r grant",
        errno::EACCES,
        t.scheme.op("map writable", Op::Map, SLOT_RO, 1),
    )?;
    t.scheme.op("map the r grant", Op::Map, SLOT_RO, 0)?;
    t.scheme.op("check through r", Op::Check, SLOT_RO, 0xc3)?;
    Ok(())
}

fn pinned_mapping(t: &mut Test) -> Result<(), Failure> {
    expect_errno(
        "munmap of a grant mapping",
        errno::EBUSY,
        t.scheme.op("munmap", Op::Munmap, SLOT_RW, 0),
    )
}

fn revoke(t: &mut Test) -> Result<(), Failure> {
    t.scheme.op("probe before revoke", Op::Probe, SLOT_RW, 0)?;
    call(region(t)?, "Revoke", Request::Revoke(t.rw))?;
    expect_errno(
        "a system call on the revoked mapping",
        errno::EFAULT,
        t.scheme.op("probe", Op::Probe, SLOT_RW, 0),
    )?;
    expect_errno(
        "mapping a revoked grant",
        errno::ECANCELED,
        t.scheme.op("remap", Op::Remap, SLOT_RW, 0),
    )?;
    // The other grant is untouched, and still the application's pages.
    t.buffer.bytes().fill(0x11);
    t.scheme.op("probe the r grant", Op::Probe, SLOT_RO, 0)?;
    t.scheme
        .op("check through r after revoke", Op::Check, SLOT_RO, 0x11)?;
    Ok(())
}

fn unmap(t: &mut Test) -> Result<(), Failure> {
    t.scheme.op("Unmap", Op::Unmap, SLOT_RO, 0)?;
    expect_errno(
        "a system call on an unmapped grant mapping",
        errno::EFAULT,
        t.scheme.op("probe", Op::Probe, SLOT_RO, 0),
    )?;
    t.scheme.op("map the r grant again", Op::Map, SLOT_RO, 0)?;
    t.scheme
        .op("probe the new mapping", Op::Probe, SLOT_RO, 0)?;
    Ok(())
}

fn close(t: &mut Test) -> Result<(), Failure> {
    drop(t.region.take());
    expect_errno(
        "a system call on a mapping of a closed region",
        errno::EFAULT,
        t.scheme.op("probe", Op::Probe, SLOT_RO, 0),
    )?;
    expect_errno(
        "mapping a grant of a closed region",
        errno::ECANCELED,
        t.scheme.op("remap", Op::Remap, SLOT_RO, 0),
    )?;
    let got = pinned(&t.root)?;
    if got != t.pinned_before {
        return Err(Failure::Pinned {
            what: "after closing the region",
            expected: t.pinned_before,
            got,
        });
    }
    // The application's memory is its own again, and still holds what it wrote.
    if t.buffer.bytes().iter().any(|&b| b != 0x11) {
        return Err(Failure::Data {
            what: "the app's buffer after close",
        });
    }
    Ok(())
}

/// Maps `len` bytes of the test scheme with `flags` at offset `offset`: the scheme answers with
/// one of its grant mappings ([`server`]'s `mmap_prep`).
fn map_scheme(t: &Test, offset: u64, flags: c_int) -> Result<*mut u8, Failure> {
    let fd = c_int::try_from(t.scheme.fd.raw()).map_err(|_| Failure::Errno {
        what: "the scheme's descriptor",
        errno: errno::EINVAL,
    })?;
    let offset = libc::off_t::try_from(offset).map_err(|_| Failure::Errno {
        what: "the mmap offset",
        errno: errno::EINVAL,
    })?;
    // SAFETY: a new mapping at an address the kernel chooses; the scheme provides its pages.
    let ptr = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGE,
            libc::PROT_READ,
            flags,
            fd,
            offset,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(Failure::Errno {
            what: "mmap of the scheme",
            errno: super::last_errno(),
        });
    }
    Ok(ptr.cast())
}

/// Whether this process can still read `ptr`: the kernel copies a byte from it into a pipe
/// and fails with `EFAULT` once it is not mapped.
fn readable(ptr: *const u8) -> Result<bool, Failure> {
    let mut pipe: [c_int; 2] = [0; 2];
    // SAFETY: `pipe` has room for the two descriptors.
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return Err(Failure::Errno {
            what: "pipe",
            errno: super::last_errno(),
        });
    }
    // SAFETY: the kernel reads from the pointer itself and fails with EFAULT when it is not
    // mapped; this process never dereferences it.
    let written = unsafe { libc::write(pipe[1], ptr.cast(), 1) };
    let errno = super::last_errno();
    // SAFETY: closes the two descriptors just made.
    unsafe {
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }
    match written {
        1 => Ok(true),
        _ if errno == errno::EFAULT => Ok(false),
        _ => Err(Failure::Errno {
            what: "probe a mapping",
            errno,
        }),
    }
}

/// The scheme answers `mmap` with the address of its grant mapping, trying to share pages it
/// only borrows; the kernel must refuse (`EPERM`) both shared and private maps. If it did not,
/// the pages would outlive revocation: the step checks that too, and fails either way.
fn reshare_mmap(t: &mut Test) -> Result<(), Failure> {
    let (fd, id) = grant(region(t)?, Access::ReadWrite, 0, PAGE_SIZE)?;
    t.scheme.send(fd, SLOT_RESHARE)?;
    t.scheme.op("map the grant", Op::Map, SLOT_RESHARE, 1)?;
    let offset = SLOT_RESHARE * PAGE_SIZE;
    for (what, flags) in [
        ("MAP_SHARED re-share", libc::MAP_SHARED),
        ("MAP_PRIVATE re-share", libc::MAP_PRIVATE),
    ] {
        match map_scheme(t, offset, flags) {
            Err(Failure::Errno { errno, .. }) if errno == errno::EPERM => {}
            Err(other) => return Err(other),
            Ok(ptr) => {
                call(region(t)?, "Revoke", Request::Revoke(id))?;
                return Err(Failure::Reshared {
                    what: if readable(ptr)? {
                        "the re-shared mapping survived revocation"
                    } else {
                        what
                    },
                });
            }
        }
    }
    // Revoking still takes the scheme's own mapping away.
    call(region(t)?, "Revoke", Request::Revoke(id))?;
    expect_errno(
        "a system call on the revoked mapping",
        errno::EFAULT,
        t.scheme.op("probe", Op::Probe, SLOT_RESHARE, 0),
    )
}

/// The scheme cannot register its grant mapping in a region of its own and grant it on.
fn reshare_register(t: &mut Test) -> Result<(), Failure> {
    let got = t
        .scheme
        .op("Register a grant mapping", Op::Register, SLOT_RW, 0)?;
    match got {
        0 => Err(Failure::Reshared {
            what: "Register of a grant mapping",
        }),
        errno::EPERM => Ok(()),
        got => Err(Failure::Expected {
            what: "Register of a grant mapping",
            expected: errno::EPERM,
            got,
        }),
    }
}

/// Nor its per-request borrow of the buffer of a `read` this process makes.
fn reshare_borrow(_t: &mut Test) -> Result<(), Failure> {
    let fd = ctx(
        "open the register-borrow handle",
        Fd::open(
            &format!("/scheme/{SCHEME}/{}", path::REGISTER_BORROW),
            O_RDWR | O_CLOEXEC,
            0,
        ),
    )?;
    let mut buffer = Buffer::new()?;
    expect_errno(
        "Register of a request buffer",
        errno::EPERM,
        ctx("read", fd.read(&mut buffer.bytes()[..2 * PAGE])),
    )
}

/// A scheme can still pass a buffer it borrows on in a request of its own (the kernel copies
/// it instead of borrowing it again): the peer's data reaches this process's buffer.
fn forward_borrow(_t: &mut Test) -> Result<(), Failure> {
    let fd = ctx(
        "open the forward handle",
        Fd::open(
            &format!("/scheme/{SCHEME}/{}", path::FORWARD),
            O_RDWR | O_CLOEXEC,
            0,
        ),
    )?;
    let mut buffer = Buffer::new()?;
    // Whole pages and a partial one, so the forwarded buffer has borrowed middle pages.
    let len = 2 * PAGE + 100;
    let got = ctx("forwarded read", fd.read(&mut buffer.bytes()[..len]))?;
    if got != len || buffer.bytes()[..len].iter().any(|&b| b != FILL_BYTE) {
        return Err(Failure::Data {
            what: "a read forwarded through a borrowed buffer",
        });
    }
    Ok(())
}

/// Opens a region of `buffer`, all of it registered.
fn register_region(buffer: &Buffer) -> Result<Fd, Failure> {
    let region = ctx(
        "open a region",
        Fd::open(&region_path(), O_RDWR | O_CLOEXEC, 0),
    )?;
    call(
        &region,
        "Register",
        Request::Register {
            addr: buffer.addr(),
            len: LEN as u64,
        },
    )?;
    Ok(region)
}

/// Fails unless the pinned count is back where it started.
fn unpinned(t: &Test, what: &'static str) -> Result<(), Failure> {
    let got = pinned(&t.root)?;
    if got != t.pinned_before {
        return Err(Failure::Pinned {
            what,
            expected: t.pinned_before,
            got,
        });
    }
    Ok(())
}

/// `MAP_SHARED` anonymous memory registers too, and grants of it are the same pages.
fn shared(t: &mut Test) -> Result<(), Failure> {
    let mut buffer = Buffer::shared()?;
    buffer.bytes().fill(0x21);
    let region = register_region(&buffer)?;
    let (fd, id) = grant(&region, Access::ReadWrite, 0, LEN as u64)?;
    t.scheme.send(fd, SLOT_SHARED)?;
    t.scheme
        .op("map the shared grant", Op::Map, SLOT_SHARED, 1)?;
    t.scheme
        .op("scheme reads shared memory", Op::Check, SLOT_SHARED, 0x21)?;
    t.scheme.op("scheme writes", Op::Fill, SLOT_SHARED, 0x34)?;
    if buffer.bytes().iter().any(|&b| b != 0x34) {
        return Err(Failure::Data {
            what: "the scheme's write in the shared buffer",
        });
    }
    buffer.bytes().fill(0x35);
    t.scheme
        .op("scheme sees the app's write", Op::Check, SLOT_SHARED, 0x35)?;
    call(&region, "Revoke", Request::Revoke(id))?;
    expect_errno(
        "a system call on the revoked shared mapping",
        errno::EFAULT,
        t.scheme.op("probe", Op::Probe, SLOT_SHARED, 0),
    )?;
    drop(region);
    unpinned(t, "after closing the shared region")
}

/// Byte-granular grants: a read-write grant from 0x100 into the first page to 0x100 into the
/// third maps three pages, the middle one the application's own; the scheme sees only the
/// granted bytes, and its writes reach only those, the partial pages once the grant is
/// revoked. A read-only grant of 0x20 bytes inside the last page shows just those.
fn bytes(t: &mut Test) -> Result<(), Failure> {
    let mut buffer = Buffer::new()?;
    buffer.bytes().fill(0x77);
    let region = register_region(&buffer)?;
    let granted = 0x100..2 * PAGE + 0x100;
    let (fd, id) = grant(&region, Access::ReadWrite, 0x100, 2 * PAGE_SIZE)?;
    t.scheme.send(fd, SLOT_BYTES_RW)?;
    t.scheme.op("map the window", Op::Map, SLOT_BYTES_RW, 1)?;
    t.scheme.op(
        "scheme sees only the granted bytes",
        Op::CheckGranted,
        SLOT_BYTES_RW,
        0x77,
    )?;
    t.scheme.op(
        "scheme writes the whole window",
        Op::Fill,
        SLOT_BYTES_RW,
        0x99,
    )?;
    if buffer.bytes()[PAGE..2 * PAGE].iter().any(|&b| b != 0x99) {
        return Err(Failure::Data {
            what: "the scheme's write in the wholly granted page",
        });
    }
    call(&region, "Revoke", Request::Revoke(id))?;
    let wrong = buffer.bytes().iter().enumerate().any(|(i, &b)| {
        let want = if granted.contains(&i) { 0x99 } else { 0x77 };
        b != want
    });
    if wrong {
        return Err(Failure::Data {
            what: "the app's buffer after a byte-granular grant is revoked",
        });
    }
    let (fd, _) = grant(&region, Access::Read, 3 * PAGE_SIZE + 0x10, 0x20)?;
    t.scheme.send(fd, SLOT_BYTES_RO)?;
    t.scheme.op("map the r window", Op::Map, SLOT_BYTES_RO, 0)?;
    t.scheme.op(
        "scheme sees only the 0x20 granted bytes",
        Op::CheckGranted,
        SLOT_BYTES_RO,
        0x77,
    )?;
    drop(region);
    expect_errno(
        "a system call on a mapping of a closed region",
        errno::EFAULT,
        t.scheme.op("probe", Op::Probe, SLOT_BYTES_RO, 0),
    )?;
    unpinned(t, "after closing the byte-granular region")
}

/// A writable partial-page mapping copies back only what its holder wrote: an untouched one
/// leaves the application's newer writes to the granted bytes alone, while one the scheme
/// filled still lands its bytes, and neither touches bytes it did not change.
fn bounce_newer(t: &mut Test) -> Result<(), Failure> {
    let mut buffer = Buffer::new()?;
    buffer.bytes().fill(0x77);
    let region = register_region(&buffer)?;
    // Two grants of bytes inside the first page, mapped writable through bounce pages.
    let filled = 0x300..0x400;
    let (fd, idle_id) = grant(&region, Access::ReadWrite, 0x100, 0x100)?;
    t.scheme.send(fd, SLOT_BOUNCE_IDLE)?;
    t.scheme
        .op("map the idle window", Op::Map, SLOT_BOUNCE_IDLE, 1)?;
    let (fd, fill_id) = grant(&region, Access::ReadWrite, 0x300, 0x100)?;
    t.scheme.send(fd, SLOT_BOUNCE_FILL)?;
    t.scheme
        .op("map the filled window", Op::Map, SLOT_BOUNCE_FILL, 1)?;
    t.scheme
        .op("scheme writes one window", Op::Fill, SLOT_BOUNCE_FILL, 0x99)?;
    // The application writes the whole page after both mappings took their snapshots.
    buffer.bytes()[..PAGE].fill(0x42);
    call(&region, "Revoke", Request::Revoke(idle_id))?;
    call(&region, "Revoke", Request::Revoke(fill_id))?;
    let wrong = buffer.bytes()[..PAGE].iter().enumerate().any(|(i, &b)| {
        let want = if filled.contains(&i) { 0x99 } else { 0x42 };
        b != want
    });
    if wrong {
        return Err(Failure::Data {
            what: "the app's newer writes after untouched and filled bounce pages are revoked",
        });
    }
    drop(region);
    unpinned(t, "after closing the bounce region")
}
