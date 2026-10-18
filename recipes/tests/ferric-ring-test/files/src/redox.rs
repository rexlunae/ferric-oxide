//! The test on Redox: a small ring client over the `kernel.ring` scheme, and the steps.

use std::ffi::c_int;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ferric_ring::abi::{RING_PATH, SCHEME_NAME, header};
use ferric_ring::entry::{CURRENT_POSITION, SqeFlags};
use ferric_ring::queue::HeaderInfo;
use ferric_ring::{Consumer, Cqe, Layout, NotifyMode, Producer, Sqe, abi::Request};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_NONBLOCK, O_RDWR};
use syscall::error::{
    EAGAIN, EBADF, EBUSY, ECANCELED, EEXIST, EFAULT, EINVAL, ENODEV, ENOENT, EOPNOTSUPP,
};
use syscall::{CallFlags, EVENT_READ};

/// A page, the unit of the file transfers.
const PAGE: usize = 4096;

/// Why a step failed.
#[derive(Debug)]
enum Failure {
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
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno { what, errno } => write!(f, "{what}: errno {errno}"),
            Self::Expected {
                what,
                expected,
                got,
            } => write!(f, "{what}: expected {expected}, got {got}"),
            Self::Mismatch { what } => write!(f, "{what}: the bytes differ"),
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
        outcome(call(&self.fd, Request::Enter { to_submit: n }))
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

/// Checks `cqes` are, in order, the `(user_data, result)` pairs of `expected`.
fn check_cqes(what: &'static str, cqes: &[Cqe], expected: &[(u64, i64)]) -> Result<(), Failure> {
    expect(what, expected.len() as i64, cqes.len() as i64)?;
    for (cqe, &(user_data, result)) in cqes.iter().zip(expected) {
        expect(what, user_data as i64, cqe.user_data as i64)?;
        expect(what, result, cqe.result)?;
    }
    Ok(())
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
        outcome(call(&fd, Request::Enter { to_submit: 1 })),
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
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe() writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return io("pipe", Err(std::io::Error::last_os_error()));
    }
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
    // SAFETY: both descriptors came from pipe() and are not used again.
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
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
            (3, neg(EOPNOTSUPP)),
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

/// Operations per benchmark.
const BENCH_OPS: usize = 20_000;
/// SQEs per `Enter` in the ring benchmarks.
const BENCH_BATCH: usize = 32;

/// Nanoseconds per operation of `ops` operations that took `start` until now.
fn ns_per_op(start: Instant, ops: usize) -> f64 {
    start.elapsed().as_nanos() as f64 / ops as f64
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
    Ok(())
}

/// A test step.
type Step = fn() -> Result<(), Failure>;

/// Runs the test.
pub fn main() -> ExitCode {
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
    let steps: [(&str, Step); 12] = [
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
        ("ring-bench", bench),
    ];
    for (name, step) in steps {
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
