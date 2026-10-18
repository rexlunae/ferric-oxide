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

use super::{Op, SCHEME, errno, server};

/// Pages in the test buffer.
const PAGES: usize = 4;
const LEN: usize = PAGES * PAGE_SIZE as usize;
/// Slots the application names its grants by.
const SLOT_RW: u64 = 1;
const SLOT_RO: u64 = 2;

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
    /// Forks the server and opens its scheme once it is registered.
    fn start() -> Result<Self, Failure> {
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
            let code = server::serve(ready[1]);
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
        let path = format!("/scheme/{SCHEME}");
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

/// The application's buffer: private anonymous memory.
struct Buffer {
    ptr: *mut u8,
}

impl Buffer {
    fn new() -> Result<Self, Failure> {
        // SAFETY: a fresh private mapping at an address the kernel chooses.
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
    let steps: [(&str, Step); 8] = [
        ("regbuf-register", register),
        ("regbuf-file-backed", file_backed),
        ("regbuf-grant-rw", grant_rw),
        ("regbuf-grant-ro", grant_ro),
        ("regbuf-pinned", pinned_mapping),
        ("regbuf-revoke", revoke),
        ("regbuf-unmap", unmap),
        ("regbuf-close", close),
    ];
    let ok = steps.iter().all(|(name, f)| step(name, f(&mut test)));
    test.scheme.stop();
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn setup(root: Fd) -> Result<Test, Failure> {
    let pinned_before = pinned(&root)?;
    let scheme = Scheme::start()?;
    let mut buffer = Buffer::new()?;
    buffer.bytes().fill(0x5a);
    Ok(Test {
        root,
        scheme,
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
        let unchanged = std::fs::read(&path).map_err(io("read the backing file"))?;
        if unchanged.iter().any(|&byte| byte != 0x77) {
            return Err(Failure::Data {
                what: "a private mapping changed its backing file",
            });
        }
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
