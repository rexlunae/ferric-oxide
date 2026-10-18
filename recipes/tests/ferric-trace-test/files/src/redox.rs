//! The test on Redox (see the crate documentation for the steps).

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;

use ferric_trace::abi::{self, EventLine, StatsLine};
use ferric_trace::kernel::{
    CATALOG, SCHED_SWITCH, SCHEME_CANCEL, SCHEME_REQUEST, SCHEME_RESPONSE, SYSCALL_ENTER,
    SYSCALL_EXIT,
};
use ferric_trace::record::Records;
use ferric_trace::{Event, EventId};
use syscall::schemev2::Opcode;
use syscall::{EBADF, SYS_READ, SYS_WRITE, SYS_YIELD};

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
    /// The `write` buffer's address.
    write_buf: u64,
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
fn check_scheme(trace: &[Vec<Entry>]) -> Result<(), Failure> {
    let step = "trace-scheme";
    let all = || trace.iter().flatten();
    let request = all()
        .find(|e| {
            e.is(&SCHEME_REQUEST)
                && e.arg(2) == Some(Opcode::Write as u64)
                && e.arg(5) == Some(SCHEME_MARK_LEN as u64)
        })
        .ok_or_else(|| check(step, "no scheme.request for the write to the null scheme"))?;
    let (scheme, tag, caller) = (request.arg(0), request.arg(1), request.arg(3));
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

/// The steps; returns the first failure.
fn run() -> Result<(), Failure> {
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

    check_scheme(&trace)?;
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
    Ok(())
}

/// Runs the test.
pub fn main() -> ExitCode {
    let events = path(abi::EVENTS_PATH);
    if let Err(err) = File::open(&events)
        && err.kind() == io::ErrorKind::NotFound
    {
        println!("FERRIC_TEST_SKIP trace: no {} scheme", abi::SCHEME_NAME);
        return ExitCode::SUCCESS;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            println!("FERRIC_TEST_FAIL {failure}");
            ExitCode::FAILURE
        }
    }
}
