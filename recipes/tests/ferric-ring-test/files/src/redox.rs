//! The test on Redox: a small ring client over the `kernel.ring` scheme, and the steps.
//!
//! `--async-only` runs the asynchronous chain and close steps without the other tests or
//! benchmarks; the trace lifecycle test uses it to exercise worker identity restoration.
//! `--scale N [--quiet-seconds S]` runs only the waiting-ring baseline (see [`scale`]).

use std::ffi::c_int;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferric_ring::abi::{MAX_FIXED_FILES, RING_PATH, SCHEME_NAME, header};
use ferric_ring::entry::{CURRENT_POSITION, SqeFlags};
use ferric_ring::queue::HeaderInfo;
use ferric_ring::{Consumer, Cqe, Layout, NotifyMode, Producer, Sqe, abi::Request};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_NONBLOCK, O_RDWR};
use syscall::error::{
    EAGAIN, EBADF, EBUSY, ECANCELED, EEXIST, EFAULT, EINTR, EINVAL, ENODEV, ENOENT, EOPNOTSUPP,
};
use syscall::{CallFlags, EVENT_READ};

/// A page, the unit of the file transfers.
const PAGE: usize = 4096;

/// Why a step failed.
#[derive(Debug)]
enum Failure {
    /// A completion batch failed its count, identity or result check.
    Completion {
        what: &'static str,
        source: crate::completions::Mismatch,
    },
    /// A call failed with an errno.
    Errno { what: &'static str, errno: i32 },
    /// A call that must fail with `expected` returned `got` (minus an errno, or a result).
    Expected {
        what: &'static str,
        expected: i64,
        got: i64,
    },
    /// Something read back is not what was written.
    Mismatch { what: &'static str },
    /// A completion did not arrive within the waiting-ring workload's deadline.
    Timeout { what: &'static str },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completion { what, source } => write!(f, "{what}: {source}"),
            Self::Errno { what, errno } => write!(f, "{what}: errno {errno}"),
            Self::Expected {
                what,
                expected,
                got,
            } => write!(f, "{what}: expected {expected}, got {got}"),
            Self::Mismatch { what } => write!(f, "{what}: the bytes differ"),
            Self::Timeout { what } => write!(f, "{what}: no completion in time"),
        }
    }
}

/// Adds `what` to a libredox error.
fn ctx<T>(what: &'static str, res: libredox::error::Result<T>) -> Result<T, Failure> {
    res.map_err(|err| Failure::Errno {
        what,
        errno: err.errno(),
    })
}

/// Adds `what` to a std I/O error.
fn io<T>(what: &'static str, res: std::io::Result<T>) -> Result<T, Failure> {
    res.map_err(|err| Failure::Errno {
        what,
        errno: err.raw_os_error().unwrap_or(0),
    })
}

/// Checks `got` (a result, or minus an errno) is `expected`.
fn expect(what: &'static str, expected: i64, got: i64) -> Result<(), Failure> {
    if expected == got {
        Ok(())
    } else {
        Err(Failure::Expected {
            what,
            expected,
            got,
        })
    }
}

/// A call's outcome as a CQE-style number: the result, or minus the errno.
fn outcome(res: libredox::error::Result<usize>) -> i64 {
    match res {
        Ok(n) => n as i64,
        Err(err) => -i64::from(err.errno()),
    }
}

/// Minus `errno`, as CQEs and [`outcome`] carry failures.
fn neg(errno: i32) -> i64 {
    -i64::from(errno)
}

/// Opens a new, unset ring.
fn open_ring() -> libredox::error::Result<Fd> {
    Fd::open(
        &format!("/scheme/{SCHEME_NAME}/{RING_PATH}"),
        O_RDWR | O_CLOEXEC,
        0,
    )
}

/// A `SYS_CALL` on a ring.
fn call(fd: &Fd, request: Request) -> libredox::error::Result<usize> {
    let (words, len) = request.encode();
    fd.call_wo(&[], CallFlags::empty(), &words[..len])
}

/// Sends `fds` to the ring's file table from slot `first`, keeping them open here
/// (`FD_CLONE`), as a result or minus the errno.
fn register(ring: &Fd, first: u32, fds: &[usize]) -> i64 {
    let (words, len) = Request::RegisterFiles { first }.encode();
    let payload: Vec<u8> = fds.iter().flat_map(|fd| fd.to_ne_bytes()).collect();
    outcome(ring.call_wo(&payload, CallFlags::FD | CallFlags::FD_CLONE, &words[..len]))
}

/// Empties `count` slots of the ring's file table from `first` on, as a result or minus the
/// errno.
fn unregister(ring: &Fd, first: u32, count: u32) -> i64 {
    outcome(call(ring, Request::UnregisterFiles { first, count }))
}

/// Maps `len` bytes of `fd` shared and read-write, or returns the errno.
fn map(fd: &Fd, len: usize) -> Result<NonNull<u8>, i32> {
    // SAFETY: a fresh mapping of the descriptor at an address the kernel chooses; nothing
    // else in this process refers to that memory yet.
    let addr = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.raw() as c_int,
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    NonNull::new(addr.cast()).ok_or(0)
}

/// A ring's memory, mapped until dropped.
struct Mapping {
    base: NonNull<u8>,
    layout: Layout,
}

impl Mapping {
    /// The ring memory as words.
    fn words(&self) -> &[AtomicU64] {
        // SAFETY: the mapping is `map_len` bytes of page-aligned shared memory, mapped until
        // `drop`, which needs `&mut self`, so it outlives this borrow. `AtomicU64` has the size
        // and alignment of a `u64` and any bit pattern is valid, and the kernel accesses the
        // same memory only atomically.
        unsafe {
            core::slice::from_raw_parts(self.base.as_ptr().cast::<AtomicU64>(), self.layout.words())
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping made by `map`; no borrow of it outlives `&mut self`.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.layout.map_len()) };
    }
}

/// A ring set up and mapped, with the submitter's SQ producer and CQ consumer.
struct Ring {
    fd: Fd,
    map: Mapping,
    sq: Producer<Sqe>,
    cq: Consumer<Cqe>,
}

impl Ring {
    /// Sets up a ring with `sq` and `cq` entries and maps it.
    fn new(sq: u32, cq: u32) -> Result<Self, Failure> {
        let layout = Layout::new(sq, cq).map_err(|_| Failure::Expected {
            what: "the test's ring layout",
            expected: 0,
            got: -1,
        })?;
        let fd = ctx("open a ring", open_ring())?;
        ctx(
            "Setup",
            call(
                &fd,
                Request::Setup {
                    sq_entries: sq,
                    cq_entries: cq,
                    notify: NotifyMode::Event,
                },
            ),
        )?;
        let base = map(&fd, layout.map_len()).map_err(|errno| Failure::Errno {
            what: "mmap the ring",
            errno,
        })?;
        Ok(Self {
            fd,
            map: Mapping { base, layout },
            sq: Producer::new(layout.sq()),
            cq: Consumer::new(layout.cq()),
        })
    }

    /// The ring memory.
    fn mem(&self) -> &[AtomicU64] {
        self.map.words()
    }

    /// Writes `sqes` into the SQ and publishes them.
    fn submit(&mut self, sqes: &[Sqe]) -> Result<(), Failure> {
        let mem = self.map.words();
        for sqe in sqes {
            self.sq.stage(mem, sqe).map_err(|_| Failure::Expected {
                what: "room in the SQ",
                expected: 1,
                got: 0,
            })?;
        }
        self.sq.publish(mem).map_err(|_| Failure::Mismatch {
            what: "publishing the SQ",
        })
    }

    /// `Enter` for up to `n` SQEs, as a result or minus the errno.
    fn enter(&self, n: u32) -> i64 {
        self.enter_wait(n, 0)
    }

    /// `Enter` for up to `n` SQEs that then waits for `min_complete` completions, as a result
    /// or minus the errno.
    fn enter_wait(&self, n: u32, min_complete: u32) -> i64 {
        enter_on(&self.fd, n, min_complete)
    }

    /// Takes every completion from the CQ.
    fn reap(&mut self) -> Result<Vec<Cqe>, Failure> {
        let mem = self.map.words();
        let mut cqes = Vec::new();
        while let Some(cqe) = self
            .cq
            .pop(mem)
            .map_err(|_| Failure::Mismatch { what: "a valid CQ" })?
        {
            cqes.push(cqe);
        }
        Ok(cqes)
    }

    /// Submits `sqes`, enters once, and returns their completions.
    fn run(&mut self, sqes: &[Sqe]) -> Result<Vec<Cqe>, Failure> {
        self.submit(sqes)?;
        expect("Enter", sqes.len() as i64, self.enter(sqes.len() as u32))?;
        self.reap()
    }
}

/// `Enter` on the ring `fd` for up to `n` SQEs, then waiting for `min_complete` completions, as
/// a result or minus the errno; unlike [`Ring::enter_wait`] it needs only the descriptor, so
/// several threads can call it at once.
fn enter_on(fd: &Fd, n: u32, min_complete: u32) -> i64 {
    outcome(call(
        fd,
        Request::Enter {
            to_submit: n,
            min_complete,
        },
    ))
}

/// Checks `cqes` are, in order, the `(user_data, result)` pairs of `expected`.
fn check_cqes(what: &'static str, cqes: &[Cqe], expected: &[(u64, i64)]) -> Result<(), Failure> {
    crate::completions::check(cqes, expected).map_err(|source| Failure::Completion { what, source })
}

/// A RedoxFS file of `len` bytes of a pattern, opened read-write.
fn scratch_file(name: &str, len: usize) -> Result<(File, Vec<u8>), Failure> {
    let path = format!("/tmp/ferric-ring-test-{name}");
    let mut file = io(
        "create a file in /tmp",
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path),
    )?;
    let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
    io("write the file", file.write_all(&data))?;
    io("rewind the file", file.seek(SeekFrom::Start(0)))?;
    Ok((file, data))
}

fn setup() -> Result<(), Failure> {
    let layout = Layout::new(8, 16).expect("valid");
    let ring = Ring::new(8, 16)?;
    expect(
        "a second Setup",
        neg(EEXIST),
        outcome(call(
            &ring.fd,
            Request::Setup {
                sq_entries: 8,
                cq_entries: 16,
                notify: NotifyMode::Event,
            },
        )),
    )?;
    let info =
        HeaderInfo::read(ring.mem()).map_err(|_| Failure::Mismatch { what: "the header" })?;
    if !info.matches(&layout, NotifyMode::Event) {
        return Err(Failure::Mismatch {
            what: "the header's setup words",
        });
    }
    Ok(())
}

fn refuse_setup() -> Result<(), Failure> {
    let fd = ctx("open a ring", open_ring())?;
    expect(
        "Enter before Setup",
        neg(EINVAL),
        outcome(call(
            &fd,
            Request::Enter {
                to_submit: 1,
                min_complete: 0,
            },
        )),
    )?;
    expect(
        "mmap before Setup",
        neg(EINVAL),
        map(&fd, 3 * PAGE).map_or_else(neg, |_| 0),
    )?;
    let setup = |sq_entries, notify| {
        outcome(call(
            &fd,
            Request::Setup {
                sq_entries,
                cq_entries: 0,
                notify,
            },
        ))
    };
    expect(
        "Setup with SqPoll",
        neg(EOPNOTSUPP),
        setup(8, NotifyMode::SqPoll),
    )?;
    expect(
        "Setup with 6 entries",
        neg(EINVAL),
        setup(6, NotifyMode::Event),
    )?;
    expect("Setup with 8 entries", 0, setup(8, NotifyMode::Event))?;
    expect(
        "mmap of a wrong length",
        neg(EINVAL),
        map(&fd, PAGE).map_or_else(neg, |_| 0),
    )?;
    Ok(())
}

fn nop() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let cqes = ring.run(&[Sqe::nop(10), Sqe::nop(11), Sqe::nop(12)])?;
    check_cqes("Nop completions", &cqes, &[(10, 0), (11, 0), (12, 0)])?;
    expect("Enter of nothing", 0, ring.enter(8))
}

fn file() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let (file, _) = scratch_file("file", 0)?;
    let fd = file.as_raw_fd() as u32;
    let data: Vec<u8> = (0..PAGE).map(|i| (i % 251) as u8).collect();
    let mut back = vec![0u8; PAGE];
    let cqes = ring.run(&[
        Sqe::write(fd, PAGE as u64, data.as_ptr() as u64, PAGE as u32, 1),
        Sqe::fsync(fd, 2),
        Sqe::read(fd, PAGE as u64, back.as_mut_ptr() as u64, PAGE as u32, 3),
    ])?;
    check_cqes(
        "write, fsync, read at an offset",
        &cqes,
        &[(1, PAGE as i64), (2, 0), (3, PAGE as i64)],
    )?;
    if back != data {
        return Err(Failure::Mismatch {
            what: "the page read back",
        });
    }
    // At the current position, as write(2): the descriptor's offset moves.
    let cqes = ring.run(&[Sqe::write(
        fd,
        CURRENT_POSITION,
        data.as_ptr() as u64,
        100,
        4,
    )])?;
    check_cqes("write at the current position", &cqes, &[(4, 100)])?;
    let mut file = file;
    let pos = io("the file position", file.stream_position())?;
    expect("the file position", 100, pos as i64)
}

fn chain() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let (file, data) = scratch_file("chain", PAGE)?;
    let fds = pipe()?;
    let null = io(
        "open the null scheme",
        OpenOptions::new().write(true).open("/scheme/null"),
    )?;
    let mut buf = vec![0u8; PAGE];
    let addr = buf.as_mut_ptr() as u64;
    let cqes = ring.run(&[
        Sqe::read(file.as_raw_fd() as u32, 0, addr, PAGE as u32, 1).linked(),
        Sqe::write(fds[1] as u32, CURRENT_POSITION, addr, PAGE as u32, 2).linked(),
        Sqe::write(
            null.as_raw_fd() as u32,
            CURRENT_POSITION,
            addr,
            PAGE as u32,
            3,
        ),
    ])?;
    check_cqes(
        "a chain across RedoxFS, the pipe scheme and the null scheme",
        &cqes,
        &[(1, PAGE as i64), (2, PAGE as i64), (3, PAGE as i64)],
    )?;
    let mut piped = vec![0u8; PAGE];
    // SAFETY: the read end from pipe(), and a buffer of `PAGE` bytes.
    let n = unsafe { libc::read(fds[0], piped.as_mut_ptr().cast(), PAGE) };
    close_fd(fds[0]);
    close_fd(fds[1]);
    expect("bytes in the pipe", PAGE as i64, n as i64)?;
    if piped != data {
        return Err(Failure::Mismatch {
            what: "the pipe's bytes against the file's",
        });
    }
    Ok(())
}

fn cancel() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let (file, _) = scratch_file("cancel", PAGE)?;
    let fd = file.as_raw_fd() as u32;
    let mut buf = vec![0u8; 2 * PAGE];
    let addr = buf.as_mut_ptr() as u64;
    let cqes = ring.run(&[
        Sqe::read(9999, 0, addr, 16, 1).linked(),
        Sqe::nop(2).linked(),
        Sqe::nop(3),
        // The file has one page: a two-page read comes up short and breaks its chain.
        Sqe::read(fd, 0, addr, 2 * PAGE as u32, 4).linked(),
        Sqe::nop(5),
        Sqe::nop(6),
    ])?;
    check_cqes(
        "chains broken by a failure and by a short read",
        &cqes,
        &[
            (1, neg(EBADF)),
            (2, neg(ECANCELED)),
            (3, neg(ECANCELED)),
            (4, PAGE as i64),
            (5, neg(ECANCELED)),
            (6, 0),
        ],
    )
}

fn refuse_sqe() -> Result<(), Failure> {
    let mut ring = Ring::new(64, 0)?;
    let mut reserved = Sqe::nop(1);
    reserved.reserved[1] = 1;
    let mut unknown = Sqe::nop(2);
    unknown.opcode = 99;
    let mut fixed = Sqe::fsync(0, 3);
    fixed.flags = SqeFlags::FIXED_FILE;
    let kernel = Sqe::read(0, 0, 0xffff_8000_0000_0000, 16, 4);
    let cqes = ring.run(&[reserved, unknown, fixed, kernel])?;
    check_cqes(
        "refused SQEs",
        &cqes,
        &[
            (1, neg(EINVAL)),
            (2, neg(EOPNOTSUPP)),
            (3, neg(EBADF)),
            (4, neg(EFAULT)),
        ],
    )?;
    // A chain longer than MAX_CHAIN: every entry up to its end is refused.
    let long = ferric_ring::abi::MAX_CHAIN + 3;
    let mut sqes: Vec<Sqe> = (0..long as u64)
        .map(|i| Sqe::nop(100 + i).linked())
        .collect();
    sqes.push(Sqe::nop(100 + long as u64));
    sqes.push(Sqe::nop(999));
    ring.submit(&sqes)?;
    expect(
        "Enter of an overlong chain",
        sqes.len() as i64,
        ring.enter(sqes.len() as u32),
    )?;
    let cqes = ring.reap()?;
    let mut expected: Vec<(u64, i64)> = (0..=long as u64).map(|i| (100 + i, neg(EINVAL))).collect();
    expected.push((999, 0));
    check_cqes("an overlong chain", &cqes, &expected)
}

fn incomplete() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    ring.submit(&[Sqe::nop(1).linked()])?;
    expect("Enter with the chain's end missing", 0, ring.enter(8))?;
    expect("completions so far", 0, ring.reap()?.len() as i64)?;
    ring.submit(&[Sqe::nop(2)])?;
    expect("Enter once the chain is complete", 2, ring.enter(8))?;
    let cqes = ring.reap()?;
    check_cqes("the completed chain", &cqes, &[(1, 0), (2, 0)])
}

fn backpressure() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let batch: Vec<Sqe> = (0..8).map(Sqe::nop).collect();
    ring.submit(&batch)?;
    expect("the first batch", 8, ring.enter(8))?;
    ring.submit(&batch)?;
    expect("the second batch, filling the CQ", 8, ring.enter(8))?;
    ring.submit(&[Sqe::nop(100)])?;
    expect("Enter with the CQ full", neg(EBUSY), ring.enter(1))?;
    expect("completions held", 16, ring.reap()?.len() as i64)?;
    expect("Enter after reaping", 1, ring.enter(1))?;
    check_cqes("the held SQE", &ring.reap()?, &[(100, 0)])
}

fn corrupt() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    ring.submit(&[Sqe::nop(1)])?;
    let tail = &ring.mem()[header::SQ_TAIL];
    tail.store(1000, Ordering::Release);
    expect("Enter with a corrupt tail", neg(EINVAL), ring.enter(8))?;
    tail.store(1, Ordering::Release);
    expect("Enter with the tail put back", 1, ring.enter(8))?;
    check_cqes("after the corrupt tail", &ring.reap()?, &[(1, 0)])
}

fn event() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let queue = ctx(
        "open an event queue",
        Fd::open("/scheme/event", O_RDWR | O_CLOEXEC | O_NONBLOCK, 0),
    )?;
    let watch = syscall::data::Event {
        id: ring.fd.raw(),
        flags: EVENT_READ,
        data: 0x5eed,
    };
    ctx("watch the ring", queue.write(&watch))?;
    let mut got = syscall::data::Event::default();
    expect(
        "an event before any completion",
        neg(EAGAIN),
        outcome(queue.read(&mut got)),
    )?;
    ring.submit(&[Sqe::nop(1)])?;
    expect("Enter", 1, ring.enter(1))?;
    expect(
        "an event after a completion",
        size_of::<syscall::data::Event>() as i64,
        outcome(queue.read(&mut got)),
    )?;
    expect("the event's data", 0x5eed, got.data as i64)?;
    if !got.flags.contains(EVENT_READ) {
        return Err(Failure::Mismatch {
            what: "the event's flags",
        });
    }
    ring.reap()?;
    Ok(())
}

fn fixed() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let (file, data) = scratch_file("fixed", PAGE)?;
    let null = io(
        "open the null scheme",
        OpenOptions::new().write(true).open("/scheme/null"),
    )?;
    let null_fd = null.as_raw_fd() as usize;
    expect(
        "register a file and the null scheme",
        2,
        register(&ring.fd, 0, &[file.as_raw_fd() as usize, null_fd]),
    )?;
    // The slot holds the file open after the descriptor is closed.
    drop(file);
    let mut buf = vec![0u8; PAGE];
    let addr = buf.as_mut_ptr() as u64;
    let cqes = ring.run(&[
        Sqe::read(0, 0, addr, PAGE as u32, 1).fixed().linked(),
        Sqe::write(1, CURRENT_POSITION, addr, PAGE as u32, 2).fixed(),
        Sqe::fsize(0, 3).fixed(),
        Sqe::fsize(5, 4).fixed(),
    ])?;
    check_cqes(
        "fixed-file read, write and size",
        &cqes,
        &[
            (1, PAGE as i64),
            (2, PAGE as i64),
            (3, PAGE as i64),
            (4, neg(EBADF)),
        ],
    )?;
    if buf != data {
        return Err(Failure::Mismatch {
            what: "the page read through a slot",
        });
    }

    let other = ctx("open a second ring", open_ring())?;
    let refused = [
        ("register into a full slot", neg(EBUSY), 1, null_fd),
        (
            "register past the table",
            neg(EINVAL),
            MAX_FIXED_FILES,
            null_fd,
        ),
        ("register the ring itself", neg(EINVAL), 4, ring.fd.raw()),
        ("register another ring", neg(EINVAL), 4, other.raw()),
    ];
    for (what, errno, first, fd) in refused {
        expect(what, errno, register(&ring.fd, first, &[fd]))?;
    }
    expect(
        "RegisterFiles without descriptors",
        neg(EINVAL),
        outcome(call(&ring.fd, Request::RegisterFiles { first: 4 })),
    )?;
    expect(
        "RegisterFiles as an FD call with an empty payload",
        neg(EINVAL),
        register(&ring.fd, 4, &[]),
    )?;
    let cqes = ring.run(&[Sqe::fsize(4, 5).fixed()])?;
    check_cqes("a slot a refused call named", &cqes, &[(5, neg(EBADF))])?;

    expect("unregister", 2, unregister(&ring.fd, 0, 8))?;
    expect("unregister again", 0, unregister(&ring.fd, 0, 8))?;
    expect(
        "unregister past the table",
        neg(EINVAL),
        unregister(&ring.fd, MAX_FIXED_FILES, 1),
    )?;
    let cqes = ring.run(&[Sqe::fsize(0, 6).fixed()])?;
    check_cqes("an emptied slot", &cqes, &[(6, neg(EBADF))])?;
    // An emptied slot takes a file again.
    expect("register again", 1, register(&ring.fd, 0, &[null_fd]))?;
    Ok(())
}

fn fstat() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let len = 3 * PAGE + 5;
    let (file, _) = scratch_file("fstat", len)?;
    let fd = file.as_raw_fd() as u32;
    let mut direct = syscall::data::Stat::default();
    syscall::fstat(fd as usize, &mut direct).map_err(|err| Failure::Errno {
        what: "fstat",
        errno: err.errno,
    })?;
    let mut via_ring = syscall::data::Stat::default();
    let size = size_of::<syscall::data::Stat>() as u32;
    let cqes = ring.run(&[
        Sqe::fstat(fd, &raw mut via_ring as u64, size, 1),
        Sqe::fsize(fd, 2),
    ])?;
    check_cqes("Fstat and Fsize", &cqes, &[(1, 0), (2, len as i64)])?;
    expect("st_size", len as i64, via_ring.st_size as i64)?;
    expect("st_ino", direct.st_ino as i64, via_ring.st_ino as i64)?;
    // The kernel fills in the scheme, as fstat does.
    expect("st_dev", direct.st_dev as i64, via_ring.st_dev as i64)
}

/// A pipe's read and write ends.
fn pipe() -> Result<[c_int; 2], Failure> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe() writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return io("pipe", Err(std::io::Error::last_os_error()));
    }
    Ok(fds)
}

/// Closes a descriptor from [`pipe`].
fn close_fd(fd: c_int) {
    // SAFETY: a descriptor this test opened and does not use again.
    unsafe { libc::close(fd) };
}

/// Reads what is left in a pipe whose write ends are all closed.
fn drain(fd: c_int) -> i64 {
    let mut buf = [0u8; 64];
    // SAFETY: a read end from pipe(), and a buffer of 64 bytes.
    unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) as i64 }
}

/// How long the other thread waits before swapping the descriptor, so that the chain has
/// been taken and is blocked by then.
const HOLD_DELAY: Duration = Duration::from_millis(500);

fn hold() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    // The chain blocks reading `wake` until the other thread writes it.
    let [wake_rd, wake_wr] = pipe()?;
    // The chain's second SQE writes to `named`'s write end.
    let [named_rd, named_wr] = pipe()?;
    // Moved onto `named_wr`'s number while the chain is blocked.
    let [other_rd, other_wr] = pipe()?;
    let mut buf = [0u8; 16];
    let addr = buf.as_mut_ptr() as u64;
    ring.submit(&[
        Sqe::read(wake_rd as u32, CURRENT_POSITION, addr, 16, 1).linked(),
        Sqe::write(named_wr as u32, CURRENT_POSITION, addr, 16, 2),
    ])?;
    let swap = std::thread::spawn(move || {
        std::thread::sleep(HOLD_DELAY);
        // SAFETY: both descriptors are open; dup2 closes `named_wr`'s description in this
        // table and puts `other_wr`'s at its number.
        let moved = unsafe { libc::dup2(other_wr, named_wr) };
        let bytes = [0x5a_u8; 16];
        // SAFETY: the write end from pipe(), and a buffer of 16 bytes.
        let woke = unsafe { libc::write(wake_wr, bytes.as_ptr().cast(), bytes.len()) };
        (moved, woke)
    });
    let entered = ring.enter(2);
    let (moved, woke) = swap.join().map_err(|_| Failure::Mismatch {
        what: "the swapping thread",
    })?;
    expect(
        "dup2 onto the chain's descriptor",
        named_wr as i64,
        moved as i64,
    )?;
    expect("waking the chain", 16, woke as i64)?;
    expect("Enter", 2, entered)?;
    check_cqes(
        "a chain whose descriptor was replaced while it waited",
        &ring.reap()?,
        &[(1, 16), (2, 16)],
    )?;
    // `named_wr`'s number now holds `other`'s write end; closing it and `other_wr` leaves
    // both pipes without writers, so the reads below return what each pipe holds.
    for fd in [named_wr, other_wr, wake_wr, wake_rd] {
        close_fd(fd);
    }
    let (named, other) = (drain(named_rd), drain(other_rd));
    close_fd(named_rd);
    close_fd(other_rd);
    expect("bytes in the pipe the chain named", 16, named)?;
    expect("bytes in the pipe moved onto its number", 0, other)
}

/// Sets or clears `O_NONBLOCK` on a descriptor's description.
fn set_nonblocking(fd: c_int, on: bool) -> Result<(), Failure> {
    // SAFETY: fcntl on a descriptor this test opened, with no pointer arguments.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    let flags = if on {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return io("fcntl", Err(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Writes pages to `fd`, a pipe's write end, until the pipe is full; returns the bytes it
/// holds.
fn fill(fd: c_int) -> Result<usize, Failure> {
    set_nonblocking(fd, true)?;
    let page = [0x11_u8; PAGE];
    let mut held = 0;
    loop {
        // SAFETY: the write end from pipe(), and a buffer of `PAGE` bytes.
        let n = unsafe { libc::write(fd, page.as_ptr().cast(), PAGE) };
        if n <= 0 {
            break;
        }
        held += n as usize;
    }
    set_nonblocking(fd, false)?;
    Ok(held)
}

/// The waits `ring-release` tries in turn before the closing thread closes the descriptor.
const RELEASE_DELAYS_MS: [u64; 5] = [500, 1000, 2000, 4000, 8000];

fn release() -> Result<(), Failure> {
    // Nothing tells the closing thread that the writer is blocked in `write` yet; if the
    // writer had not started when the descriptor went, its `write` fails with `EBADF` and
    // nothing was tested, so the step runs again with a longer wait.
    for delay in RELEASE_DELAYS_MS {
        if release_attempt(Duration::from_millis(delay))? {
            return Ok(());
        }
    }
    Err(Failure::Mismatch {
        what: "a writer blocked in write before the close, after every wait",
    })
}

/// One try of `ring-release`, whose closing thread waits `delay`; `false` when the writer
/// had not yet started its `write` when the descriptor was closed.
fn release_attempt(delay: Duration) -> Result<bool, Failure> {
    let mut ring = Ring::new(8, 16)?;
    let [wake_rd, wake_wr] = pipe()?;
    let [rd, wr] = pipe()?;
    let held = fill(wr)?;
    let mut buf = [0u8; 16];
    let addr = buf.as_mut_ptr() as u64;
    // The chain holds `wr` from when it is taken; its second SQE fails (a read of a write
    // end), which does not matter: only the hold does.
    ring.submit(&[
        Sqe::read(wake_rd as u32, CURRENT_POSITION, addr, 1, 1).linked(),
        Sqe::read(wr as u32, CURRENT_POSITION, addr, 1, 2),
    ])?;
    // An ordinary write that blocks on the full pipe, so it holds `wr` too.
    let writer = std::thread::spawn(move || {
        let byte = [0x22_u8];
        // SAFETY: the write end from pipe(), and a buffer of 1 byte.
        let n = unsafe { libc::write(wr, byte.as_ptr().cast(), 1) };
        // errno is per thread, so it is taken here.
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        (n, errno)
    });
    let closer = std::thread::spawn(move || {
        std::thread::sleep(delay);
        // The descriptor goes while the ring and the write both hold its description.
        close_fd(wr);
        let byte = [0x33_u8];
        // SAFETY: the write end from pipe(), and a buffer of 1 byte.
        unsafe { libc::write(wake_wr, byte.as_ptr().cast(), 1) }
    });
    // The ring lets go first: Enter returns before the blocked write can finish.
    let entered = ring.enter(2);
    let woke = closer.join().map_err(|_| Failure::Mismatch {
        what: "the closing thread",
    })?;
    expect("waking the chain", 1, woke as i64)?;
    expect("Enter", 2, entered)?;
    check_cqes(
        "a chain holding a descriptor closed meanwhile",
        &ring.reap()?,
        &[(1, 1), (2, neg(EBADF))],
    )?;
    // Room in the pipe lets the write finish; it holds the last reference.
    let mut page = vec![0u8; PAGE];
    // SAFETY: the read end from pipe(), and a buffer of `PAGE` bytes.
    let first = unsafe { libc::read(rd, page.as_mut_ptr().cast(), PAGE) };
    let (wrote, errno) = writer.join().map_err(|_| Failure::Mismatch {
        what: "the writing thread",
    })?;
    if wrote < 0 {
        for fd in [rd, wake_rd, wake_wr] {
            close_fd(fd);
        }
        if errno == EBADF {
            return Ok(false);
        }
        return Err(Failure::Errno {
            what: "the blocked write",
            errno,
        });
    }
    expect("the blocked write", 1, wrote as i64)?;
    // With the last reference gone the pipe has no writer: after its bytes, a read sees the
    // end of the file, not `EAGAIN` as it would if the description had never been closed.
    set_nonblocking(rd, true)?;
    let mut left = held.saturating_sub(first.max(0) as usize).saturating_add(1);
    let end = loop {
        // SAFETY: as above.
        let n = unsafe { libc::read(rd, page.as_mut_ptr().cast(), PAGE) };
        if n <= 0 {
            break if n == 0 {
                0
            } else {
                neg(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
            };
        }
        left = left.saturating_sub(n as usize);
    };
    for fd in [rd, wake_rd, wake_wr] {
        close_fd(fd);
    }
    expect("bytes left in the pipe", 0, left as i64)?;
    expect("a read once the last holder let go", 0, end)?;
    Ok(true)
}

fn async_chains() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    let queue = ctx(
        "open an event queue",
        Fd::open("/scheme/event", O_RDWR | O_CLOEXEC | O_NONBLOCK, 0),
    )?;
    let watch = syscall::data::Event {
        id: ring.fd.raw(),
        flags: EVENT_READ,
        data: 7,
    };
    ctx("watch the ring", queue.write(&watch))?;

    // A read of an empty pipe blocks on the worker; Enter returns without its completion.
    let [rd, wr] = pipe()?;
    let mut buf = [0u8; 16];
    ring.submit(&[
        Sqe::read(rd as u32, CURRENT_POSITION, buf.as_mut_ptr() as u64, 16, 1).run_async(),
    ])?;
    expect("Enter of an asynchronous read", 1, ring.enter(1))?;
    expect(
        "completions before the pipe has data",
        0,
        ring.reap()?.len() as i64,
    )?;
    let mut got = syscall::data::Event::default();
    expect(
        "an event before the completion",
        neg(EAGAIN),
        outcome(queue.read(&mut got)),
    )?;
    // The read was looked up when it was submitted: closing the descriptor does not stop it.
    close_fd(rd);
    let data = *b"ferric-ring-asyn";
    // SAFETY: the pipe's write end and a buffer of 16 bytes.
    let n = unsafe { libc::write(wr, data.as_ptr().cast(), data.len()) };
    expect("write to the pipe", 16, n as i64)?;
    expect("Enter waiting for one completion", 0, ring.enter_wait(0, 1))?;
    check_cqes("the asynchronous read", &ring.reap()?, &[(1, 16)])?;
    if buf != data {
        return Err(Failure::Mismatch {
            what: "the bytes the asynchronous read got",
        });
    }
    expect(
        "an event after the completion",
        size_of::<syscall::data::Event>() as i64,
        outcome(queue.read(&mut got)),
    )?;
    close_fd(wr);

    // An asynchronous chain runs by the same rules, in one go.
    let (file, _) = scratch_file("async", 0)?;
    let fd = file.as_raw_fd() as u32;
    let page: Vec<u8> = (0..PAGE).map(|i| (i % 253) as u8).collect();
    let mut back = vec![0u8; PAGE];
    ring.submit(&[
        Sqe::write(fd, 0, page.as_ptr() as u64, PAGE as u32, 2)
            .run_async()
            .linked(),
        Sqe::fsync(fd, 3).linked(),
        Sqe::read(fd, 0, back.as_mut_ptr() as u64, PAGE as u32, 4),
        Sqe::read(9999, 0, back.as_mut_ptr() as u64, 8, 5)
            .run_async()
            .linked(),
        Sqe::nop(6),
    ])?;
    expect("Enter of two asynchronous chains", 5, ring.enter_wait(5, 5))?;
    check_cqes(
        "asynchronous chains",
        &ring.reap()?,
        &[
            (2, PAGE as i64),
            (3, 0),
            (4, PAGE as i64),
            (5, neg(EBADF)),
            (6, neg(ECANCELED)),
        ],
    )?;
    if back != page {
        return Err(Failure::Mismatch {
            what: "the page an asynchronous chain read back",
        });
    }

    expect(
        "min_complete above the CQ",
        neg(EINVAL),
        ring.enter_wait(0, 17),
    )?;
    // Nothing is running, so there is nothing to wait for.
    expect(
        "min_complete with nothing running",
        0,
        ring.enter_wait(0, 4),
    )?;
    Ok(())
}

fn wait_sync() -> Result<(), Failure> {
    // The sleeps below only make the overlap likely; `wait_sync_once` checks from timestamps
    // that it happened, and a run where scheduling broke it is tried again.
    for _ in 0..WAIT_SYNC_TRIES {
        if wait_sync_once()? {
            return Ok(());
        }
    }
    Err(Failure::Mismatch {
        what: "an overlap of a blocked Enter and a wait-only Enter (scheduling delayed every try)",
    })
}

/// How many times `ring-wait-sync` tries to make its two `Enter`s overlap.
const WAIT_SYNC_TRIES: usize = 3;

/// One try of `ring-wait-sync`: `Ok(false)` when the two `Enter`s did not provably overlap.
fn wait_sync_once() -> Result<bool, Failure> {
    let mut ring = Ring::new(8, 16)?;
    let [rd, wr] = pipe()?;
    let mut buf = [0u8; 16];
    let addr = buf.as_mut_ptr() as u64;
    // A synchronous read of an empty pipe, which blocks the `Enter` that runs it.
    ring.submit(&[Sqe::read(rd as u32, CURRENT_POSITION, addr, 16, 1)])?;
    let fd = &ring.fd;
    let timed = |n, min_complete| {
        let start = Instant::now();
        let result = enter_on(fd, n, min_complete);
        (start, result, Instant::now())
    };
    let (submitted, waited, early, wrote, wrote_at) = std::thread::scope(|scope| {
        let submitter = scope.spawn(|| timed(1, 0));
        std::thread::sleep(HOLD_DELAY);
        // A wait-only Enter while the read runs in the other thread's Enter must wait for it.
        let waiter = scope.spawn(|| timed(0, 1));
        std::thread::sleep(HOLD_DELAY);
        let early = waiter.is_finished();
        let byte = [0x44_u8];
        let wrote_at = Instant::now();
        // SAFETY: the pipe's write end and a buffer of 1 byte.
        let wrote = unsafe { libc::write(wr, byte.as_ptr().cast(), 1) };
        let failed = (wrote_at, i64::MIN, wrote_at);
        (
            submitter.join().unwrap_or(failed),
            waiter.join().unwrap_or(failed),
            early,
            wrote,
            wrote_at,
        )
    });
    close_fd(rd);
    close_fd(wr);
    expect("write to the pipe", 1, wrote as i64)?;
    expect("the submitting Enter", 1, submitted.1)?;
    expect("the waiting Enter", 0, waited.1)?;
    if early {
        return Err(Failure::Mismatch {
            what: "a wait-only Enter that waited for a chain another Enter runs",
        });
    }
    check_cqes("the synchronous read", &ring.reap()?, &[(1, 1)])?;
    // Both Enters were called before the write and returned after it, the waiter after the
    // submitter had started: so the waiter was inside its Enter while the read was blocked, and
    // returned only once the write let the read finish.
    let overlapped = submitted.0 < waited.0
        && waited.0 < wrote_at
        && submitted.2 > wrote_at
        && waited.2 > wrote_at;
    Ok(overlapped)
}

/// How long `ring-async-close` waits for the closed ring's worker to start the chain, and
/// then to post its completions.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Waits for the byte the worker writes to `started_rd`'s pipe when it starts a chain.
fn wait_started(started_rd: c_int) -> Result<(), Failure> {
    let start = Instant::now();
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: a non-blocking read end from pipe(), and a buffer of 1 byte.
        let got = unsafe { libc::read(started_rd, byte.as_mut_ptr().cast(), 1) } as i64;
        if got == 1 || start.elapsed() >= CLOSE_WAIT {
            return expect(
                "the byte the worker writes when it starts the chain",
                1,
                got,
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn async_close() -> Result<(), Failure> {
    let mut ring = Ring::new(8, 16)?;
    // A job holds its files until it is done, so one holding its own ring would keep the ring
    // from closing: rings are refused in asynchronous chains.
    ring.submit(&[Sqe::fsize(ring.fd.raw() as u32, 1).run_async()])?;
    expect("Enter of a chain naming its ring", 1, ring.enter_wait(1, 1))?;
    let cqes = ring.reap()?;
    check_cqes(
        "an asynchronous chain naming its own ring",
        &cqes,
        &[(1, neg(EINVAL))],
    )?;

    let [rd, wr] = pipe()?;
    // The chain's first SQE writes a byte to `started`, so the test can see that the worker
    // is running the chain before it closes the ring. Close interrupts the running chain
    // (`EINTR` for the read, whether the worker is already waiting in it or reaches it after
    // the close) and cancels the queued Nop (`ECANCELED`). Closing before the worker has
    // taken the chain would cancel it whole instead, which is also by design but would leave
    // the interruption untested.
    let [started_rd, started_wr] = pipe()?;
    set_nonblocking(started_rd, true)?;
    let mark = [0x5a_u8; 1];
    let mut buf = [0u8; 16];
    ring.submit(&[
        Sqe::write(
            started_wr as u32,
            CURRENT_POSITION,
            mark.as_ptr() as u64,
            1,
            2,
        )
        .linked()
        .run_async(),
        Sqe::read(rd as u32, CURRENT_POSITION, buf.as_mut_ptr() as u64, 16, 3),
        Sqe::nop(4).run_async(),
    ])?;
    expect("Enter of a blocked chain and a Nop", 3, ring.enter(3))?;
    wait_started(started_rd)?;
    let Ring {
        fd, map, mut cq, ..
    } = ring;
    drop(fd);
    let start = Instant::now();
    let mut cqes = Vec::new();
    while cqes.len() < 3 && start.elapsed() < CLOSE_WAIT {
        match cq.pop(map.words()) {
            Ok(Some(cqe)) => cqes.push(cqe),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return Err(Failure::Mismatch { what: "a valid CQ" }),
        }
    }
    check_cqes(
        "the closed ring's interrupted and cancelled jobs",
        &cqes,
        &[(2, 1), (3, neg(EINTR)), (4, neg(ECANCELED))],
    )?;
    close_fd(started_rd);
    close_fd(started_wr);
    drop(map);
    let mut other = Ring::new(8, 16)?;
    let cqes = other.run(&[Sqe::nop(4)])?;
    check_cqes("a new ring after the close", &cqes, &[(4, 0)])?;
    close_fd(rd);
    close_fd(wr);
    Ok(())
}

/// Operations per benchmark.
const BENCH_OPS: usize = 20_000;
/// SQEs per `Enter` in the ring benchmarks.
const BENCH_BATCH: usize = 32;

/// Nanoseconds per operation of `ops` operations that took `start` until now.
fn ns_per_op(start: Instant, ops: usize) -> f64 {
    start.elapsed().as_nanos() as f64 / ops as f64
}

/// How long a worker may take to answer a wake before [`scale`] gives up.
const SCALE_WAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `scale` waits for the workers to block on their reads.
const SCALE_SETTLE: Duration = Duration::from_millis(250);
/// How hard `scale` polls a ring for a completion after waking it.
const SCALE_POLL: Duration = Duration::from_micros(50);
/// The byte each waiting read waits for.
const SCALE_BYTE: u8 = b'x';

/// The waiting-ring baseline (`ferric-ring-test --scale N [--quiet-seconds S]`): how many
/// kernel contexts and how much scheduler work `N` rings with a genuinely pending read
/// cost, and how long an individual wake takes to reach its completion. The
/// scheduler-baseline image (config/ferric-sched-bench.toml) captures the kernel trace
/// around the whole run; this step only drives the workload and prints the markers a
/// capture can be matched to, and the wake-latency `FERRIC_BENCH` line.
///
/// Every ring submits one asynchronous 1-byte read of an empty pipe, so its worker blocks
/// in the read. After a short settle, the step stays quiet for `S` seconds (the window the
/// harness's capture attributes to the waiting rings), then wakes each ring with one byte
/// and times write to completion, checks exactly one completion with the byte, and closes
/// everything.
fn scale(rings_n: usize, quiet_seconds: u64) -> Result<(), Failure> {
    let mut rings: Vec<Ring> = Vec::with_capacity(rings_n);
    let mut writers: Vec<c_int> = Vec::with_capacity(rings_n);
    let mut buffers: Vec<Vec<u8>> = Vec::with_capacity(rings_n);
    let started = Instant::now();
    for i in 0..rings_n {
        let mut ring = Ring::new(8, 8)?;
        let [rd, wr] = pipe()?;
        let mut buf = vec![0u8; 1];
        ring.submit(&[Sqe::read(
            rd as u32,
            CURRENT_POSITION,
            buf.as_mut_ptr() as u64,
            1,
            i as u64,
        )
        .run_async()])?;
        expect("Enter of an asynchronous read", 1, ring.enter(1))?;
        // The read names the descriptor at submission; the worker's reference keeps it
        // alive, so the submitter can close it, as `ring-async` does.
        close_fd(rd);
        rings.push(ring);
        writers.push(wr);
        buffers.push(buf);
    }
    let setup_ms = started.elapsed().as_millis();
    println!("FERRIC_SCALE_SETUP rings={rings_n} setup_ms={setup_ms}");
    std::thread::sleep(SCALE_SETTLE);
    // The quiet window: nothing runs but the workers' blocked reads (and, on the harness,
    // whatever the scheduler does around them). The harness's capture that ends with this
    // step is the run's window.
    std::thread::sleep(Duration::from_secs(quiet_seconds));
    println!("FERRIC_SCALE_WAKE rings={rings_n}");
    let mut latencies = ferric_iobench::latency::Histogram::new();
    for (i, (ring, &wr)) in rings.iter_mut().zip(&writers).enumerate() {
        let wake = Instant::now();
        // SAFETY: the pipe's write end with one byte to write.
        let n = unsafe { libc::write(wr, [SCALE_BYTE].as_ptr().cast(), 1) };
        expect("write to the pipe", 1, n as i64)?;
        let completions = loop {
            let cqes = ring.reap()?;
            if !cqes.is_empty() {
                break cqes;
            }
            if wake.elapsed() > SCALE_WAKE_TIMEOUT {
                return Err(Failure::Timeout {
                    what: "a wake to reach its completion",
                });
            }
            std::thread::sleep(SCALE_POLL);
        };
        latencies.record(wake.elapsed().as_nanos() as u64);
        check_cqes(
            "one successful waiting-ring read",
            &completions,
            &[(i as u64, 1)],
        )?;
    }
    for (ring, buf) in rings.iter_mut().zip(&buffers) {
        check_cqes("no duplicate waiting-ring completion", &ring.reap()?, &[])?;
        if buf != &[SCALE_BYTE] {
            return Err(Failure::Mismatch {
                what: "the byte a waiting read got",
            });
        }
    }
    println!(
        "FERRIC_BENCH ring-scale rings={rings_n} setup_ms={setup_ms} wake_ns_mean={} \
         wake_ns_p50={} wake_ns_p99={} wake_ns_p999={} wake_ns_max={}",
        latencies.mean(),
        latencies.percentile(500_000),
        latencies.percentile(990_000),
        latencies.percentile(999_000),
        latencies.max(),
    );
    for wr in writers {
        close_fd(wr);
    }
    Ok(())
}

/// Runs `BENCH_OPS` copies of `sqe` through `ring` in batches, checking each result.
fn bench_ring(ring: &mut Ring, sqe: Sqe, result: i64) -> Result<f64, Failure> {
    let batch = vec![sqe; BENCH_BATCH];
    let start = Instant::now();
    for _ in 0..BENCH_OPS / BENCH_BATCH {
        for cqe in ring.run(&batch)? {
            expect("a benchmark completion", result, cqe.result)?;
        }
    }
    Ok(ns_per_op(start, BENCH_OPS / BENCH_BATCH * BENCH_BATCH))
}

fn bench() -> Result<(), Failure> {
    let mut ring = Ring::new(64, 0)?;
    let mut null = io(
        "open the null scheme",
        OpenOptions::new().write(true).open("/scheme/null"),
    )?;
    let small = [0x5au8; 64];
    let start = Instant::now();
    for _ in 0..BENCH_OPS {
        io("write to null", null.write(&small))?;
    }
    let syscall_ns = ns_per_op(start, BENCH_OPS);
    let sqe = Sqe::write(
        null.as_raw_fd() as u32,
        CURRENT_POSITION,
        small.as_ptr() as u64,
        64,
        0,
    );
    let ring_ns = bench_ring(&mut ring, sqe, 64)?;
    let (syscall_null_ns, ring_null_ns) = (syscall_ns, ring_ns);
    println!(
        "FERRIC_BENCH ring null-write-64: write {syscall_ns:.0} ns/op, ring (batch {BENCH_BATCH}) {ring_ns:.0} ns/op, {:.2}x",
        syscall_ns / ring_ns
    );

    let (mut file, _) = scratch_file("bench", PAGE)?;
    let mut page = vec![0u8; PAGE];
    let start = Instant::now();
    for _ in 0..BENCH_OPS {
        io("rewind", file.seek(SeekFrom::Start(0)))?;
        io("read the file", file.read_exact(&mut page))?;
    }
    let syscall_ns = ns_per_op(start, BENCH_OPS);
    let sqe = Sqe::read(
        file.as_raw_fd() as u32,
        0,
        page.as_mut_ptr() as u64,
        PAGE as u32,
        0,
    );
    let ring_ns = bench_ring(&mut ring, sqe, PAGE as i64)?;
    println!(
        "FERRIC_BENCH ring file-read-4k: lseek+read {syscall_ns:.0} ns/op, ring pread (batch {BENCH_BATCH}) {ring_ns:.0} ns/op, {:.2}x",
        syscall_ns / ring_ns
    );

    // The same reads through a slot of the file table: no descriptor lookup per SQE.
    expect(
        "register the benchmark file",
        1,
        register(&ring.fd, 0, &[file.as_raw_fd() as usize]),
    )?;
    let fixed_ns = bench_ring(&mut ring, Sqe { fd: 0, ..sqe }.fixed(), PAGE as i64)?;
    println!(
        "FERRIC_BENCH ring file-read-4k-fixed: ring pread by descriptor {ring_ns:.0} ns/op, by slot {fixed_ns:.0} ns/op, {:.2}x",
        ring_ns / fixed_ns
    );

    // The null writes again on the worker: Enter hands each over and waits for the batch.
    let batch = vec![
        Sqe::write(
            null.as_raw_fd() as u32,
            CURRENT_POSITION,
            small.as_ptr() as u64,
            64,
            0
        )
        .run_async();
        BENCH_BATCH
    ];
    let start = Instant::now();
    for _ in 0..BENCH_OPS / BENCH_BATCH {
        ring.submit(&batch)?;
        expect(
            "Enter of an asynchronous batch",
            BENCH_BATCH as i64,
            ring.enter_wait(BENCH_BATCH as u32, BENCH_BATCH as u32),
        )?;
        for cqe in ring.reap()? {
            expect("an asynchronous benchmark completion", 64, cqe.result)?;
        }
    }
    let async_ns = ns_per_op(start, BENCH_OPS / BENCH_BATCH * BENCH_BATCH);
    println!(
        "FERRIC_BENCH ring null-write-64-async: write {syscall_null_ns:.0} ns/op, ring in Enter {ring_null_ns:.0} ns/op, ring on the worker {async_ns:.0} ns/op"
    );
    Ok(())
}

/// A test step.
type Step = fn() -> Result<(), Failure>;

/// Runs the test.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let async_only = args.get(1).map(String::as_str) == Some("--async-only");
    // `--scale N [--hold PATH]` runs only the waiting-ring baseline.
    if let Some(position) = args.iter().position(|arg| arg == "--scale") {
        let count = args.get(position + 1).and_then(|n| n.parse::<usize>().ok());
        let quiet = args
            .iter()
            .position(|arg| arg == "--quiet-seconds")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);
        let result = match open_ring() {
            Ok(_) => match count {
                Some(count) => scale(count, quiet),
                None => Err(Failure::Expected {
                    what: "--scale to name a number of rings",
                    expected: 1,
                    got: -1,
                }),
            },
            Err(err) if matches!(err.errno(), ENODEV | ENOENT) => {
                println!("FERRIC_TEST_SKIP ring: the kernel has no {SCHEME_NAME} scheme");
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                println!(
                    "FERRIC_TEST_FAIL ring-scale: open a ring: errno {}",
                    err.errno()
                );
                return ExitCode::FAILURE;
            }
        };
        return match result {
            Ok(()) => {
                println!("FERRIC_TEST_PASS ring-scale");
                ExitCode::SUCCESS
            }
            Err(why) => {
                println!("FERRIC_TEST_FAIL ring-scale: {why}");
                ExitCode::FAILURE
            }
        };
    }
    match open_ring() {
        Ok(_) => {}
        Err(err) if matches!(err.errno(), ENODEV | ENOENT) => {
            println!("FERRIC_TEST_SKIP ring: the kernel has no {SCHEME_NAME} scheme");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            println!(
                "FERRIC_TEST_FAIL ring-setup: open a ring: errno {}",
                err.errno()
            );
            return ExitCode::FAILURE;
        }
    }
    let steps: [(&str, Step); 19] = [
        ("ring-setup", setup),
        ("ring-refuse-setup", refuse_setup),
        ("ring-nop", nop),
        ("ring-file", file),
        ("ring-chain", chain),
        ("ring-cancel", cancel),
        ("ring-refuse-sqe", refuse_sqe),
        ("ring-incomplete", incomplete),
        ("ring-backpressure", backpressure),
        ("ring-corrupt", corrupt),
        ("ring-event", event),
        ("ring-fixed", fixed),
        ("ring-fstat", fstat),
        ("ring-hold", hold),
        ("ring-release", release),
        ("ring-async", async_chains),
        ("ring-async-close", async_close),
        ("ring-wait-sync", wait_sync),
        ("ring-bench", bench),
    ];
    for (name, step) in steps {
        if async_only && !matches!(name, "ring-async" | "ring-async-close") {
            continue;
        }
        match step() {
            Ok(()) => println!("FERRIC_TEST_PASS {name}"),
            Err(why) => {
                println!("FERRIC_TEST_FAIL {name}: {why}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
