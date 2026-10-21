//! The test on Redox (see the crate documentation for the steps).

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::sync::atomic::AtomicU64;

use ferric_trace::abi::{self, EventLine, StatsLine};
use ferric_trace::abi::{CPU_MAP_BYTES, CpuCommand};
use ferric_trace::clock::ClockLine;
use ferric_trace::config::{BootConfig, INITFS_PATH};
use ferric_trace::kernel::{
    CATALOG, MM_PAGE_FAULT, PageFaultFlags, PageFaultOutcome, SCHED_SWITCH,
    SCHEME_CANCEL, SCHEME_REQUEST, SCHEME_RESPONSE, SYSCALL_ENTER, SYSCALL_EXIT,
};
use ferric_trace::record::{Records, WORD_BYTES};
use ferric_trace::ring::View;
use ferric_trace::{Event, EventId};
use syscall::schemev2::Opcode;
use syscall::{EACCES, EBADF, EFAULT, EINVAL, MapFlags, SYS_READ, SYS_WRITE, SYS_YIELD};

mod crash;

/// A file descriptor no process has open: the `write` to it fails with `EBADF` at once.
const BAD_FD: usize = 0x7e57;

/// The length of the marker system calls' buffers, which their records carry.
const MARK_LEN: usize = 3217;

/// The length of the marker write to the null scheme, which `zerod` serves in user space.
const SCHEME_MARK_LEN: usize = 2749;

/// The traced events this test turns on and off.
const TESTED: [&Event; 5] = [
    &SYSCALL_ENTER,
    &SYSCALL_EXIT,
    &SCHEME_REQUEST,
    &SCHEME_RESPONSE,
    &SCHEME_CANCEL,
];

/// How much one read of a CPU's ring asks for (the scheme's maximum).
const READ_LEN: usize = 64 * 1024;

/// Why a step failed.
#[derive(Debug)]
struct Failure {
    step: &'static str,
    why: Why,
}

/// What went wrong in a step.
#[derive(Debug)]
enum Why {
    /// A file of the scheme could not be used.
    Io { path: String, source: io::Error },
    /// A system call gave the wrong answer.
    Call {
        call: &'static str,
        result: syscall::Result<usize>,
    },
    /// The trace or a listing did not have what it should.
    Check(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.step)?;
        match &self.why {
            Why::Io { path, source } => write!(f, "{path}: {source}"),
            Why::Call { call, result } => write!(f, "{call} returned {result:?}"),
            Why::Check(what) => f.write_str(what),
        }
    }
}

/// A failed check in `step`.
fn check(step: &'static str, what: impl Into<String>) -> Failure {
    Failure {
        step,
        why: Why::Check(what.into()),
    }
}

/// An I/O failure on `path` in `step`.
fn io_failure<'a>(step: &'static str, path: &'a str) -> impl FnOnce(io::Error) -> Failure + 'a {
    move |source| Failure {
        step,
        why: Why::Io {
            path: path.to_owned(),
            source,
        },
    }
}

/// The scheme path of `file`.
fn path(file: &str) -> String {
    format!("/scheme/{}/{file}", abi::SCHEME_NAME)
}

/// A decoded record, copied out of the read buffer.
#[derive(Clone, Debug)]
struct Entry {
    event: EventId,
    timestamp: u64,
    args: Vec<u64>,
}

impl Entry {
    fn is(&self, event: &Event) -> bool {
        self.event == event.id()
    }

    fn arg(&self, index: usize) -> Option<u64> {
        self.args.get(index).copied()
    }
}

/// Reads a whole text file of the scheme.
fn read_text(step: &'static str, file: &str) -> Result<String, Failure> {
    let path = path(file);
    let mut text = String::new();
    File::open(&path)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(io_failure(step, &path))?;
    Ok(text)
}

/// The event listing, as `(name, on)` pairs.
fn listing(step: &'static str) -> Result<Vec<(String, bool)>, Failure> {
    read_text(step, abi::EVENTS_PATH)?
        .lines()
        .map(|line| {
            EventLine::parse(line)
                .map(|e| (e.name.to_owned(), e.enabled))
                .map_err(|err| check(step, format!("bad event line {line:?}: {err}")))
        })
        .collect()
}

/// Whether the listing says `event` is on; a failure when it is not listed.
fn is_on(step: &'static str, list: &[(String, bool)], event: &Event) -> Result<bool, Failure> {
    list.iter()
        .find(|(name, _)| name == event.name())
        .map(|&(_, on)| on)
        .ok_or_else(|| check(step, format!("{} is not listed", event.name())))
}

/// Writes a command to the event listing.
fn command(step: &'static str, text: &str) -> Result<(), Failure> {
    let path = path(abi::EVENTS_PATH);
    OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(io_failure(step, &path))
}

/// The CPUs, from the `stats` file.
fn cpus(step: &'static str) -> Result<Vec<u32>, Failure> {
    read_text(step, abi::STATS_PATH)?
        .lines()
        .map(|line| {
            StatsLine::parse(line)
                .map(|s| s.cpu)
                .map_err(|err| check(step, format!("bad stats line {line:?}: {err}")))
        })
        .collect()
}

/// Drains every CPU's ring and decodes the records, one list per CPU.
fn drain(step: &'static str, cpus: &[u32]) -> Result<Vec<Vec<Entry>>, Failure> {
    cpus.iter()
        .map(|cpu| {
            let path = path(&format!("{}/{cpu}", abi::CPU_DIR));
            let mut file = File::open(&path).map_err(io_failure(step, &path))?;
            let mut entries = Vec::new();
            let mut buf = vec![0u8; READ_LEN];
            loop {
                let len = file.read(&mut buf).map_err(io_failure(step, &path))?;
                if len == 0 {
                    return Ok(entries);
                }
                decode(step, *cpu, buf.get(..len).unwrap_or_default(), &mut entries)?;
            }
        })
        .collect()
}

/// Decodes one read of CPU `cpu`'s ring into `entries` (`trace-decode`).
fn decode(
    step: &'static str,
    cpu: u32,
    bytes: &[u8],
    entries: &mut Vec<Entry>,
) -> Result<(), Failure> {
    for record in Records::new(bytes) {
        let record = record.map_err(|err| check(step, format!("cpu {cpu}: {err}")))?;
        let event = CATALOG
            .get(record.event())
            .ok_or_else(|| check(step, format!("cpu {cpu}: unknown event {}", record.event())))?;
        if record.arg_count() != event.args().len() {
            return Err(check(
                step,
                format!(
                    "cpu {cpu}: {} with {} arguments",
                    event.name(),
                    record.arg_count()
                ),
            ));
        }
        entries.push(Entry {
            event: record.event(),
            timestamp: record.timestamp(),
            args: record.args().collect(),
        });
    }
    Ok(())
}

/// The marker system calls, and what they returned.
struct Marks {
    /// The read buffer's address and the `stats` descriptor.
    read_buf: u64,
    read_fd: u64,
    /// The `read`'s byte count.
    read_len: u64,
    /// The `write` buffer's address, which the write to the null scheme uses too.
    write_buf: u64,
    /// The null scheme descriptor.
    null_fd: u64,
}

/// Makes the marker system calls.
fn mark(step: &'static str) -> Result<Marks, Failure> {
    let buf = vec![0x5au8; MARK_LEN];
    let result = syscall::write(BAD_FD, &buf);
    if result.map_err(|e| e.errno) != Err(EBADF) {
        return Err(Failure {
            step,
            why: Why::Call {
                call: "write to a closed fd",
                result,
            },
        });
    }
    let path = path(abi::STATS_PATH);
    let stats = File::open(&path).map_err(io_failure(step, &path))?;
    let mut read_buf = vec![0u8; MARK_LEN];
    let fd = usize::try_from(stats.as_raw_fd()).map_err(|_| check(step, "negative fd"))?;
    let result = syscall::read(fd, &mut read_buf);
    let Ok(read_len) = result else {
        return Err(Failure {
            step,
            why: Why::Call {
                call: "read of stats",
                result,
            },
        });
    };
    let null_path = "/scheme/null";
    let null = OpenOptions::new()
        .write(true)
        .open(null_path)
        .map_err(io_failure(step, null_path))?;
    let null_fd = usize::try_from(null.as_raw_fd()).map_err(|_| check(step, "negative fd"))?;
    let result = syscall::write(null_fd, &buf[..SCHEME_MARK_LEN]);
    if result != Ok(SCHEME_MARK_LEN) {
        return Err(Failure {
            step,
            why: Why::Call {
                call: "write to the null scheme",
                result,
            },
        });
    }
    let result = syscall::sched_yield();
    if result != Ok(0) {
        return Err(Failure {
            step,
            why: Why::Call {
                call: "yield",
                result,
            },
        });
    }
    Ok(Marks {
        read_buf: read_buf.as_ptr() as u64,
        read_fd: fd as u64,
        read_len: read_len as u64,
        write_buf: buf.as_ptr() as u64,
        null_fd: null_fd as u64,
    })
}

/// `syscall::Error::mux` of `Err(errno)`: the value the kernel hands back for an error.
fn errno_result(errno: i32) -> u64 {
    (-i64::from(errno)) as u64
}

/// Finds an enter record whose first arguments are `enter`, followed on the same CPU by the exit
/// record `[nr, result]`.
fn find_pair(cpus: &[Vec<Entry>], enter: &[u64], result: u64) -> Option<()> {
    cpus.iter().find_map(|entries| {
        entries.windows(2).find_map(|pair| {
            let [first, second] = pair else { return None };
            let matches_enter = first.is(&SYSCALL_ENTER)
                && enter
                    .iter()
                    .enumerate()
                    .all(|(i, &v)| first.arg(i) == Some(v));
            let matches_exit = second.is(&SYSCALL_EXIT)
                && second.arg(0) == enter.first().copied()
                && second.arg(1) == Some(result);
            (matches_enter && matches_exit).then_some(())
        })
    })
}

/// `trace-scheme`: the write to the null scheme has a `scheme.request` (a write of
/// [`SCHEME_MARK_LEN`] bytes) and, later and on any CPU, a `scheme.response` with the same scheme
/// and tag, the byte count as its result, and another process as the responder.
///
/// The request is found through this program's own `syscall.enter` for the write (its null
/// scheme descriptor, buffer address and length): the kernel queues the request on the same CPU
/// before the caller blocks, so it is the first `scheme.request` after that enter and before the
/// CPU switches away. Another process's write of the same length cannot match.
fn check_scheme(trace: &[Vec<Entry>], marks: &Marks) -> Result<(), Failure> {
    let step = "trace-scheme";
    let enter = [
        SYS_WRITE as u64,
        marks.null_fd,
        marks.write_buf,
        SCHEME_MARK_LEN as u64,
    ];
    let request = trace
        .iter()
        .find_map(|entries| {
            let start = entries.iter().position(|e| {
                e.is(&SYSCALL_ENTER) && enter.iter().enumerate().all(|(i, &v)| e.arg(i) == Some(v))
            })?;
            entries[start + 1..]
                .iter()
                .take_while(|e| !e.is(&SCHED_SWITCH) && !e.is(&SYSCALL_EXIT))
                .find(|e| e.is(&SCHEME_REQUEST))
        })
        .ok_or_else(|| check(step, "no scheme.request for the write to the null scheme"))?;
    if request.arg(2) != Some(Opcode::Write as u64)
        || request.arg(5) != Some(SCHEME_MARK_LEN as u64)
    {
        return Err(check(step, format!("the write's request is {request:?}")));
    }
    let (scheme, tag, caller) = (request.arg(0), request.arg(1), request.arg(3));
    let all = || trace.iter().flatten();
    let response = all()
        .filter(|e| e.is(&SCHEME_RESPONSE) && e.arg(0) == scheme && e.arg(1) == tag)
        .filter(|e| e.timestamp >= request.timestamp)
        .min_by_key(|e| e.timestamp)
        .ok_or_else(|| check(step, format!("no scheme.response for {request:?}")))?;
    if response.arg(2) != Some(SCHEME_MARK_LEN as u64) {
        return Err(check(step, format!("response {response:?} to {request:?}")));
    }
    if response.arg(3) == caller {
        return Err(check(
            step,
            format!("the caller answered its own request: {response:?}"),
        ));
    }
    Ok(())
}

/// `trace-initfs`: the events the initfs `trace.toml` leaves on are on and the events it turns
/// off are off, as bootstrap applied it before init started. Then the state the later steps
/// expect is restored: the events they test are turned off and `sched.switch`, on from boot,
/// is turned on again if the file turned it off. Without the file it prints a skip.
fn check_initfs() -> Result<(), Failure> {
    let step = "trace-initfs";
    let path = format!("/scheme/initfs/{INITFS_PATH}");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("FERRIC_TEST_SKIP trace-initfs: no {path}");
            return Ok(());
        }
        Err(source) => return Err(io_failure(step, &path)(source)),
    };
    let config = BootConfig::parse(&bytes).map_err(|err| check(step, format!("{path}: {err}")))?;
    let invalid = |err| check(step, format!("{path}: {err}"));
    let on = config.check(&CATALOG).map_err(invalid)?;
    let off = config.disabled(&CATALOG).map_err(invalid)?;
    let list = listing(step)?;
    let (mut checked_on, mut checked_off) = (0, 0);
    for event in CATALOG.events() {
        let expected = if on.contains(event.id()) {
            true
        } else if off.contains(event.id()) {
            false
        } else {
            continue;
        };
        // Only compiled-in events are listed, and only those can be on.
        let Some(&(_, enabled)) = list.iter().find(|(name, _)| name == event.name()) else {
            continue;
        };
        if enabled != expected {
            let (is, turns) = if expected {
                ("off", "on")
            } else {
                ("on", "off")
            };
            return Err(check(
                step,
                format!("{} is {is}, but {path} turns it {turns}", event.name()),
            ));
        }
        if expected {
            checked_on += 1;
            if TESTED.iter().any(|tested| tested.id() == event.id()) {
                command(step, &format!("disable {}", event.name()))?;
            }
        } else {
            checked_off += 1;
            if event.id() == SCHED_SWITCH.id() {
                command(step, &format!("enable {}", event.name()))?;
            }
        }
    }
    if checked_on + checked_off == 0 {
        return Err(check(
            step,
            format!("{path} turns no compiled-in event on or off"),
        ));
    }
    println!(
        "FERRIC_TEST_PASS trace-initfs ({checked_on} events on and {checked_off} off from {path})"
    );
    Ok(())
}

/// A CPU's ring mapped into this process, until dropped.
struct Mapped {
    addr: usize,
}

impl Mapped {
    /// Maps [`CPU_MAP_BYTES`] of `fd` from offset 0 with `flags`, or returns the error.
    fn new(fd: usize, size: usize, flags: MapFlags) -> syscall::Result<Self> {
        let map = syscall::Map {
            offset: 0,
            size,
            flags,
            address: 0,
        };
        // SAFETY: a fresh mapping at an address the kernel picks; nothing in this process
        // refers to that memory yet.
        let addr = unsafe { syscall::fmap(fd, &map) }?;
        Ok(Self { addr })
    }

    /// The ring's words.
    fn words(&self) -> &[AtomicU64] {
        // SAFETY: the mapping is CPU_MAP_BYTES of page-aligned shared memory, mapped until
        // `drop`, which needs `&mut self`, so it outlives this borrow. `AtomicU64` has the size
        // and alignment of a `u64` and any bit pattern is valid, and the kernel writes the
        // same memory only atomically.
        unsafe {
            core::slice::from_raw_parts(self.addr as *const AtomicU64, CPU_MAP_BYTES / WORD_BYTES)
        }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: the mapping made by `new`; no borrow of it outlives `&mut self`.
        let _ = unsafe { syscall::funmap(self.addr, CPU_MAP_BYTES) };
    }
}

/// Checks that `result` failed with `errno`.
fn expect_errno(
    step: &'static str,
    call: &'static str,
    result: syscall::Result<usize>,
    errno: i32,
) -> Result<(), Failure> {
    if result.map_err(|e| e.errno) == Err(errno) {
        Ok(())
    } else {
        Err(Failure {
            step,
            why: Why::Call { call, result },
        })
    }
}

/// Writes `commit <end>` to a CPU's descriptor.
fn commit(fd: usize, end: u64) -> syscall::Result<usize> {
    syscall::write(fd, CpuCommand::Commit(end).to_string().as_bytes())
}

/// A CPU's ring, open and mapped.
struct CpuMap {
    cpu: u32,
    file: File,
    map: Mapped,
}

impl CpuMap {
    fn fd(&self) -> usize {
        // File descriptors are never negative.
        self.file.as_raw_fd() as usize
    }
}

/// Opens and maps every CPU's ring, checking that only a read-only shared mapping of the
/// whole ring is allowed and that it cannot be made writable.
fn map_cpus(step: &'static str, cpus: &[u32]) -> Result<Vec<CpuMap>, Failure> {
    let read_shared = MapFlags::PROT_READ | MapFlags::MAP_SHARED;
    cpus.iter()
        .map(|&cpu| {
            let path = path(&format!("{}/{cpu}", abi::CPU_DIR));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(io_failure(step, &path))?;
            let fd = file.as_raw_fd() as usize;
            let refused = |flags, size| Mapped::new(fd, size, flags).map(|map| map.addr);
            expect_errno(
                step,
                "writable mmap of a ring",
                refused(read_shared | MapFlags::PROT_WRITE, CPU_MAP_BYTES),
                EACCES,
            )?;
            expect_errno(
                step,
                "mmap of part of a ring",
                refused(read_shared, CPU_MAP_BYTES / 2),
                EINVAL,
            )?;
            expect_errno(
                step,
                "mmap of a ring without PROT_READ",
                refused(MapFlags::MAP_SHARED, CPU_MAP_BYTES),
                EINVAL,
            )?;
            expect_errno(
                step,
                "private mmap of a ring",
                refused(MapFlags::PROT_READ | MapFlags::MAP_PRIVATE, CPU_MAP_BYTES),
                EINVAL,
            )?;
            let map = Mapped::new(fd, CPU_MAP_BYTES, read_shared).map_err(|err| Failure {
                step,
                why: Why::Call {
                    call: "mmap of a ring",
                    result: Err(err),
                },
            })?;
            // SAFETY: changes only the protection of the mapping made above, which nothing
            // writes through; the kernel must refuse it anyway.
            let result = unsafe {
                syscall::mprotect(
                    map.addr,
                    CPU_MAP_BYTES,
                    MapFlags::PROT_READ | MapFlags::PROT_WRITE,
                )
            };
            expect_errno(step, "mprotect of a ring to writable", result, EACCES)?;
            Ok(CpuMap { cpu, file, map })
        })
        .collect()
}

/// Reads one mapped ring through a [`View`] without consuming it, and returns the records
/// and the position after them.
fn read_mapped(step: &'static str, ring: &CpuMap) -> Result<(Vec<Entry>, u64, u64), Failure> {
    let cpu = ring.cpu;
    let view =
        View::new(ring.map.words()).map_err(|err| check(step, format!("cpu {cpu}: {err}")))?;
    let start = view.tail();
    let mut pos = start;
    let mut entries = Vec::new();
    let mut buf = vec![0u8; READ_LEN];
    loop {
        let peek = view
            .peek(pos, &mut buf)
            .map_err(|err| check(step, format!("cpu {cpu}: {err}")))?;
        if peek.is_corrupt() {
            return Err(check(step, format!("cpu {cpu}: a corrupt record")));
        }
        if peek.is_empty() {
            return Ok((entries, start, pos));
        }
        decode(
            step,
            cpu,
            buf.get(..peek.len()).unwrap_or_default(),
            &mut entries,
        )?;
        pos = peek.end();
    }
}

/// `trace-mmap`: maps every CPU's ring, makes the marker calls again, finds them by reading
/// the mappings, and consumes what it read with `commit`, which takes only a record boundary
/// the kernel published.
fn check_mmap(cpus: &[u32]) -> Result<(), Failure> {
    let step = "trace-mmap";
    let rings = map_cpus(step, cpus)?;
    drain(step, cpus)?;
    command(step, "enable syscall")?;
    let marks = mark(step)?;
    command(step, "disable syscall")?;
    let mut trace = Vec::new();
    for ring in &rings {
        let (entries, start, end) = read_mapped(step, ring)?;
        let fd = ring.fd();
        let view = View::new(ring.map.words())
            .map_err(|err| check(step, format!("cpu {}: {err}", ring.cpu)))?;
        expect_errno(
            step,
            "commit past head",
            commit(fd, view.head() + 1),
            EINVAL,
        )?;
        if end > start {
            // The shortest record is two words, so one word in is never a boundary.
            expect_errno(
                step,
                "commit inside a record",
                commit(fd, start + 1),
                EINVAL,
            )?;
        }
        if view.tail() != start {
            return Err(check(
                step,
                format!("cpu {}: a refused commit moved tail", ring.cpu),
            ));
        }
        let result = commit(fd, end);
        if result.is_err() {
            return Err(Failure {
                step,
                why: Why::Call {
                    call: "commit of a mapped read",
                    result,
                },
            });
        }
        if view.tail() < end {
            return Err(check(
                step,
                format!("cpu {}: tail {} after commit {end}", ring.cpu, view.tail()),
            ));
        }
        trace.push(entries);
    }
    let write = [
        SYS_WRITE as u64,
        BAD_FD as u64,
        marks.write_buf,
        MARK_LEN as u64,
    ];
    find_pair(&trace, &write, errno_result(EBADF))
        .ok_or_else(|| check(step, "no enter/exit pair for the write in the mapped rings"))?;
    // What was committed is gone: reading the rings does not give the calls again.
    let after = drain(step, cpus)?;
    if find_pair(&after, &write, errno_result(EBADF)).is_some() {
        return Err(check(step, "a read gave records the commit consumed"));
    }
    Ok(())
}

/// Whether the kernel has `event` compiled in (the listing names only those); prints a skip for
/// `step` when it does not.
fn has_event(step: &'static str, event: &Event) -> Result<bool, Failure> {
    let compiled = listing(step)?.iter().any(|(name, _)| name == event.name());
    if !compiled {
        println!(
            "FERRIC_TEST_SKIP {step}: {} is not compiled in",
            event.name()
        );
    }
    Ok(compiled)
}

/// The pages `trace-fault` touches.
const FAULT_PAGES: usize = 4;

/// The page size.
const PAGE: usize = 4096;

/// `trace-fault`: with the `mm` events on, writes to each page of a fresh anonymous mapping,
/// which the kernel maps on first touch, then unmaps it and has the kernel copy into it with
/// a `read`, which fails with `EFAULT`. The writes must have `mm.page_fault` records from user
/// mode, writes, ending `mapped`; the read one from the kernel at the unmapped buffer, ending
/// `copy failed`.
fn check_fault(cpus: &[u32]) -> Result<(), Failure> {
    let step = "trace-fault";
    if !has_event(step, &MM_PAGE_FAULT)? {
        return Ok(());
    }
    let len = FAULT_PAGES * PAGE;
    let map = syscall::Map {
        offset: 0,
        size: len,
        flags: MapFlags::PROT_READ | MapFlags::PROT_WRITE | MapFlags::MAP_PRIVATE,
        address: 0,
    };
    // Opened first: nothing between the unmap and the read may allocate, or the allocator
    // could map memory at the unmapped address again.
    let stats_path = path(abi::STATS_PATH);
    let stats = File::open(&stats_path).map_err(io_failure(step, &stats_path))?;
    let stats_fd = stats.as_raw_fd() as usize;
    drain(step, cpus)?;
    command(step, "enable mm")?;
    // SAFETY: an anonymous mapping (no file) at an address the kernel picks; nothing in this
    // process refers to that memory yet.
    let addr = unsafe { syscall::fmap(!0, &map) }.map_err(|err| Failure {
        step,
        why: Why::Call {
            call: "anonymous mmap",
            result: Err(err),
        },
    })?;
    for page in 0..FAULT_PAGES {
        // SAFETY: inside the mapping made above, which is readable and writable and which
        // nothing else uses; the write is volatile so it happens, one per page.
        unsafe { ((addr + page * PAGE) as *mut u64).write_volatile(0x5a) };
    }
    // SAFETY: the mapping made above, which nothing refers to after this.
    let unmapped = unsafe { syscall::funmap(addr, len) };
    // SAFETY: the kernel writes to `addr`, which is not mapped any more, so the call fails
    // with EFAULT and writes nothing; no memory of this process changes.
    let copy = unsafe { syscall::syscall3(SYS_READ, stats_fd, addr, PAGE) };
    command(step, "disable mm")?;
    if unmapped.is_err() {
        return Err(Failure {
            step,
            why: Why::Call {
                call: "munmap",
                result: unmapped,
            },
        });
    }
    expect_errno(step, "read into an unmapped buffer", copy, EFAULT)?;
    let trace = drain(step, cpus)?;
    let faults: Vec<&Entry> = trace
        .iter()
        .flatten()
        .filter(|e| e.is(&MM_PAGE_FAULT))
        .collect();
    let in_page = |e: &Entry, page: usize| {
        e.arg(0)
            .is_some_and(|a| (a as usize) / PAGE == addr / PAGE + page)
    };
    let flags = |e: &Entry| PageFaultFlags::from_raw(e.arg(2).unwrap_or(0));
    for page in 0..FAULT_PAGES {
        faults
            .iter()
            .find(|e| {
                in_page(e, page)
                    && flags(e).contains(PageFaultFlags::WRITE.with(PageFaultFlags::USER, true))
                    && e.arg(3) == Some(PageFaultOutcome::Mapped.raw())
            })
            .ok_or_else(|| {
                check(
                    step,
                    format!(
                        "no user write fault ending mapped for page {page} at {:#x} ({} mm.page_fault records)",
                        addr + page * PAGE,
                        faults.len()
                    ),
                )
            })?;
    }
    faults
        .iter()
        .find(|e| {
            in_page(e, 0)
                && !flags(e).contains(PageFaultFlags::USER)
                && e.arg(3) == Some(PageFaultOutcome::CopyFailed.raw())
        })
        .ok_or_else(|| {
            check(
                step,
                format!("no kernel fault ending copy failed at {addr:#x}"),
            )
        })?;
    println!(
        "FERRIC_TEST_PASS trace-fault ({} mm.page_fault records)",
        faults.len()
    );
    Ok(())
}

/// The steps; returns the first failure.
fn run() -> Result<(), Failure> {
    check_initfs()?;
    let list = listing("trace-boot")?;
    if !is_on("trace-boot", &list, &SCHED_SWITCH)? {
        return Err(check("trace-boot", "sched.switch is not on from boot"));
    }
    for event in TESTED {
        if is_on("trace-boot", &list, event)? {
            return Err(check(
                "trace-boot",
                format!("{} is on before it was enabled", event.name()),
            ));
        }
    }
    println!("FERRIC_TEST_PASS trace-boot");

    let cpus = cpus("trace-enable")?;
    // Empty the rings first: sched.switch has been filling them since boot, and a full ring
    // drops new records.
    drain("trace-enable", &cpus)?;
    command("trace-enable", "enable syscall scheme")?;
    let list = listing("trace-enable")?;
    for event in TESTED {
        if !is_on("trace-enable", &list, event)? {
            return Err(check(
                "trace-enable",
                format!("{} is still off", event.name()),
            ));
        }
    }
    println!("FERRIC_TEST_PASS trace-enable");

    let marks = mark("trace-enable")?;
    command("trace-disable", "disable syscall scheme")?;
    let trace = drain("trace-decode", &cpus)?;
    let total: usize = trace.iter().map(Vec::len).sum();
    println!(
        "FERRIC_TEST_PASS trace-decode ({total} records on {} CPUs)",
        cpus.len()
    );

    for (cpu, entries) in cpus.iter().zip(&trace) {
        if let Some(pair) = entries.windows(2).find(|p| p[1].timestamp < p[0].timestamp) {
            return Err(check(
                "trace-order",
                format!(
                    "cpu {cpu}: timestamp {} after {}",
                    pair[1].timestamp, pair[0].timestamp
                ),
            ));
        }
    }
    println!("FERRIC_TEST_PASS trace-order");

    let write = [
        SYS_WRITE as u64,
        BAD_FD as u64,
        marks.write_buf,
        MARK_LEN as u64,
    ];
    find_pair(&trace, &write, errno_result(EBADF)).ok_or_else(|| {
        check(
            "trace-write",
            "no enter/exit pair for the write to a closed fd",
        )
    })?;
    println!("FERRIC_TEST_PASS trace-write");

    let read = [
        SYS_READ as u64,
        marks.read_fd,
        marks.read_buf,
        MARK_LEN as u64,
    ];
    find_pair(&trace, &read, marks.read_len)
        .ok_or_else(|| check("trace-read", "no enter/exit pair for the read of stats"))?;
    println!("FERRIC_TEST_PASS trace-read");

    let all = || trace.iter().flatten();
    let enter = all()
        .filter(|e| e.is(&SYSCALL_ENTER) && e.arg(0) == Some(SYS_YIELD as u64))
        .map(|e| e.timestamp)
        .min()
        .ok_or_else(|| check("trace-yield", "no enter record for yield"))?;
    all()
        .find(|e| {
            e.is(&SYSCALL_EXIT)
                && e.arg(0) == Some(SYS_YIELD as u64)
                && e.arg(1) == Some(0)
                && e.timestamp >= enter
        })
        .ok_or_else(|| check("trace-yield", "no exit record for yield"))?;
    println!("FERRIC_TEST_PASS trace-yield");

    let switches = all().filter(|e| e.is(&SCHED_SWITCH)).count();
    if switches == 0 {
        return Err(check("trace-sched", "no sched.switch records"));
    }
    println!("FERRIC_TEST_PASS trace-sched ({switches} switches)");

    check_scheme(&trace, &marks)?;
    println!("FERRIC_TEST_PASS trace-scheme");

    // Records written before the disable finished may still be in the rings: drop them, then
    // repeat the calls, which must leave nothing.
    drain("trace-disable", &cpus)?;
    mark("trace-disable")?;
    let after = drain("trace-disable", &cpus)?;
    let stray = after
        .iter()
        .flatten()
        .filter(|e| TESTED.iter().any(|event| e.is(event)))
        .count();
    if stray != 0 {
        return Err(check(
            "trace-disable",
            format!("{stray} syscall or scheme records after disable"),
        ));
    }
    println!("FERRIC_TEST_PASS trace-disable");

    check_mmap(&cpus)?;
    println!("FERRIC_TEST_PASS trace-mmap");

    check_fault(&cpus)?;
    check_clock(&cpus)?;
    Ok(())
}

/// The length of `trace-clock`'s first marker write to a closed descriptor; the second is one
/// byte longer, so the two are told apart and their order checked.
const CLOCK_MARK_LEN: usize = 1931;

/// How long `trace-clock` sleeps between its two marker writes.
const CLOCK_SLEEP: std::time::Duration = std::time::Duration::from_millis(200);

/// `trace-clock`: the `clock` file names the clock timestamps come from, and two marker
/// writes [`CLOCK_SLEEP`] apart have `syscall.enter` timestamps that agree with the wall
/// time between them: the second later than the first, and within a factor of two of the
/// wall time when the clock's tick length is known (the KVM clock, the HPET); for the TSC,
/// whose frequency the kernel does not know, only the order is checked.
/// The two writes may run on different CPUs, so this also compares clocks across CPUs.
fn check_clock(cpus: &[u32]) -> Result<(), Failure> {
    let step = "trace-clock";
    let text = read_text(step, abi::CLOCK_PATH)?;
    let line = ClockLine::parse(&text)
        .map_err(|err| check(step, format!("clock line {text:?}: {err}")))?;
    drain(step, cpus)?;
    command(step, "enable syscall")?;
    let marker = vec![0x5au8; CLOCK_MARK_LEN + 1];
    let first = syscall::write(BAD_FD, &marker[..CLOCK_MARK_LEN]);
    let start = std::time::Instant::now();
    std::thread::sleep(CLOCK_SLEEP);
    let wall = start.elapsed();
    let second = syscall::write(BAD_FD, &marker);
    command(step, "disable syscall")?;
    for result in [first, second] {
        if result.map_err(|e| e.errno) != Err(EBADF) {
            return Err(Failure {
                step,
                why: Why::Call {
                    call: "write to a closed fd",
                    result,
                },
            });
        }
    }
    let trace = drain(step, cpus)?;
    // The timestamps of the writes of `len` bytes.
    let marks = |len: usize| -> Vec<u64> {
        trace
            .iter()
            .flatten()
            .filter(|e| {
                e.is(&SYSCALL_ENTER)
                    && e.arg(0) == Some(SYS_WRITE as u64)
                    && e.arg(1) == Some(BAD_FD as u64)
                    && e.arg(3) == Some(len as u64)
            })
            .map(|e| e.timestamp)
            .collect()
    };
    let (firsts, seconds) = (marks(CLOCK_MARK_LEN), marks(CLOCK_MARK_LEN + 1));
    let ([before], [after]) = (&firsts[..], &seconds[..]) else {
        return Err(check(
            step,
            format!(
                "{} first and {} second marker writes recorded, not 1 of each",
                firsts.len(),
                seconds.len()
            ),
        ));
    };
    // The second write came 200 ms after the first, so a timestamp that is not later ran
    // backward (or stood still) between the CPUs or reads.
    let Some(ticks) = after.checked_sub(*before).filter(|&ticks| ticks > 0) else {
        return Err(check(
            step,
            format!("the second marker's timestamp {after} is not after the first's {before}"),
        ));
    };
    let wall_us = wall.as_secs_f64() * 1e6;
    let detail = match line.ticks_per_us() {
        Some(rate) => {
            let traced_us = ticks as f64 / rate;
            let ratio = traced_us / wall_us;
            if !(0.5..=2.0).contains(&ratio) {
                return Err(check(
                    step,
                    format!(
                        "{}: {traced_us:.0} us between the markers' timestamps, {wall_us:.0} us of wall time",
                        line.clock
                    ),
                ));
            }
            format!(
                "{}, {} fs ticks: {traced_us:.0} us traced over {wall_us:.0} us",
                line.clock,
                line.period_fs.unwrap_or(0)
            )
        }
        None => format!(
            "{}: {ticks} ticks over {wall_us:.0} us, about {:.0} MHz",
            line.clock,
            ticks as f64 / wall_us
        ),
    };
    println!("FERRIC_TEST_PASS trace-clock ({detail})");
    Ok(())
}

/// Runs the test.
pub fn main() -> ExitCode {
    // The /scheme listing says whether the kernel has the scheme. Opening a missing scheme's
    // file fails with an errno that depends on the namespace manager, so it is no test.
    let listed = std::fs::read_dir("/scheme").map(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == abi::SCHEME_NAME)
    });
    match listed {
        Ok(true) => {}
        Ok(false) => {
            println!("FERRIC_TEST_SKIP trace: no {} scheme", abi::SCHEME_NAME);
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL trace-boot: cannot list /scheme: {err}");
            return ExitCode::FAILURE;
        }
    }
    // `ferric-trace-test crash` runs the crash-dump step alone (see `crash`).
    let steps = match std::env::args().nth(1).as_deref() {
        Some("crash") => crash::run,
        _ => run,
    };
    match steps() {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            println!("FERRIC_TEST_FAIL {failure}");
            ExitCode::FAILURE
        }
    }
}
