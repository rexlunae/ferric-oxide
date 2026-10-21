//! `ferric-trace-test crash`: the crash dump across a reboot (`kernel.trace_crash`).
//!
//! `/scheme/kernel.trace/crash` decides what this boot is:
//!
//! - Missing: the kernel keeps no dump, or has none from the previous boot. It prints a skip.
//! - Empty: a crash-test kernel (`kernel.trace_crash_test`) with no dump yet, so this is the
//!   first boot. It turns the `syscall` events on, makes a marker `write` of
//!   [`CRASH_MARK_LEN`] bytes to a closed descriptor, prints
//!   `FERRIC_TEST_PASS trace-crash-armed`, and writes `panic` to the file. The kernel writes
//!   the dump and resets the machine. The rings are not drained first: `sched.switch` has
//!   filled them since boot, so the dump has the newest records only because the kernel's
//!   rings overwrite their oldest when full.
//! - A dump: the second boot. `trace-crash-read` checks that it parses, that its message is the
//!   crash test's panic, that it records the clock this boot's `clock` file names (the same
//!   machine picks the same clock), and that its records hold the marker `write` and the
//!   `write` of `panic` that ended the first boot. With `kernel.log_persist`, `log-previous`
//!   then checks that the previous boot's kernel log ([`PREVIOUS_PATH`]) ends with that
//!   panic and, when it did not wrap, the line the kernel logs before it has a heap
//!   ([`EARLY_LINE`], which this boot's `/scheme/sys/log` must hold too), and
//!   `log-previous-root-only` that a process that is not root cannot open it.

use std::ffi::c_int;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::process::{Command, ExitCode};

use ferric_trace::abi::{CLOCK_PATH, CRASH_PATH, CRASH_TEST_COMMAND};
use ferric_trace::clock::ClockLine;
use ferric_trace::crash::Dump;
use ferric_trace::kernel::{CATALOG, SYSCALL_ENTER};
use ferric_trace::klog::{DATA_BYTES, PREVIOUS_PATH};
use ferric_trace::record::Records;
use syscall::{EBADF, SYS_WRITE};

use super::{BAD_FD, Entry, Failure, Why, check, command, io_failure, path, read_text};

/// The length of the marker `write` before the panic.
const CRASH_MARK_LEN: usize = 1931;

/// A line the kernel logs before its heap and in-memory log exist.
const EARLY_LINE: &str = "Redox OS starting";

/// This boot's in-memory kernel log.
const LOG_PATH: &str = "/scheme/sys/log";

/// What the kernel panics with in the crash test.
const TEST_PANIC: &str = "ferric-trace crash test";

/// Runs the crash step for this boot.
pub fn run() -> Result<(), Failure> {
    let path = path(CRASH_PATH);
    match std::fs::read(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("FERRIC_TEST_SKIP trace-crash: no {path} (no dump, or no kernel.trace_crash)");
            Ok(())
        }
        Err(source) => Err(io_failure("trace-crash", &path)(source)),
        Ok(bytes) if bytes.is_empty() => trigger(&path),
        Ok(bytes) => read_back(&path, &bytes),
    }
}

/// `trace-crash-armed`: marks the trace and panics the kernel. Returns only on failure.
fn trigger(path: &str) -> Result<(), Failure> {
    let step = "trace-crash-armed";
    // The rings are not drained first: `sched.switch` has been filling them since boot, and
    // the dump must still hold the newest records, which the kernel's rings keep by
    // overwriting their oldest when they are full.
    command(step, "enable syscall")?;
    let buf = vec![0x5au8; CRASH_MARK_LEN];
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
    println!(
        "FERRIC_TEST_PASS trace-crash-armed: panicking the kernel; the next boot reads the dump"
    );
    let _ = io::stdout().flush();
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io_failure(step, path))?;
    file.write_all(CRASH_TEST_COMMAND.as_bytes())
        .map_err(io_failure(step, path))?;
    Err(check(step, "the kernel did not panic"))
}

/// `trace-crash-read`: the dump the first boot left.
fn read_back(path: &str, bytes: &[u8]) -> Result<(), Failure> {
    let step = "trace-crash-read";
    let dump = Dump::parse(bytes).map_err(|err| check(step, format!("{path}: {err}")))?;
    let Some((cpu, message)) = dump.message() else {
        return Err(check(step, "the dump has no panic message"));
    };
    let message = String::from_utf8_lossy(message);
    if !message.contains(TEST_PANIC) {
        return Err(check(
            step,
            format!("the panic was not the test's: {message:?}"),
        ));
    }
    let text = read_text(step, CLOCK_PATH)?;
    let clock = ClockLine::parse(&text)
        .map_err(|err| check(step, format!("clock line {text:?}: {err}")))?;
    if dump.clock() != Some(clock) {
        return Err(check(
            step,
            format!(
                "the dump's clock {:?} is not this boot's {clock:?}",
                dump.clock()
            ),
        ));
    }
    let mut entries = Vec::new();
    // Each CPU's decoded records and `lost` count, for the failure messages.
    let mut per_cpu = Vec::new();
    for cpu in dump.cpus() {
        let decoded = decode_whole(cpu.records);
        per_cpu.push(format!(
            "cpu {}: {} records, {} lost",
            cpu.cpu,
            decoded.len(),
            cpu.lost
        ));
        entries.extend(decoded);
    }
    let cpus = per_cpu.len();
    let per_cpu = per_cpu.join("; ");
    let write_of = |len: usize, fd: Option<usize>| {
        entries.iter().any(|entry| {
            entry.is(&SYSCALL_ENTER)
                && entry.arg(0) == Some(SYS_WRITE as u64)
                && fd.is_none_or(|fd| entry.arg(1) == Some(fd as u64))
                && entry.arg(3) == Some(len as u64)
        })
    };
    if !write_of(CRASH_MARK_LEN, Some(BAD_FD)) {
        return Err(check(
            step,
            format!("the dump has no record of the marker write ({per_cpu})"),
        ));
    }
    if !write_of(CRASH_TEST_COMMAND.len(), None) {
        return Err(check(
            step,
            format!("the dump has no record of the write that panicked ({per_cpu})"),
        ));
    }
    println!(
        "FERRIC_TEST_PASS trace-crash-read ({} records of {cpus} CPUs, clock {}, panic on CPU {cpu}: {}; {per_cpu})",
        entries.len(),
        clock.clock,
        message.lines().last().unwrap_or_default()
    );
    previous_log()
}

/// `log-previous`: the previous boot's kernel log holds the panic the dump recorded. Skipped
/// without the file (no `kernel.log_persist`).
fn previous_log() -> Result<(), Failure> {
    let step = "log-previous";
    let log = match std::fs::read(PREVIOUS_PATH) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("FERRIC_TEST_SKIP log-previous: no {PREVIOUS_PATH} (no kernel.log_persist)");
            return Ok(());
        }
        Err(source) => return Err(io_failure(step, PREVIOUS_PATH)(source)),
        Ok(log) => log,
    };
    let log = String::from_utf8_lossy(&log);
    let Some(panic) = log.rfind("KERNEL PANIC: ") else {
        return Err(check(
            step,
            format!(
                "no KERNEL PANIC line in the previous boot's {} bytes of log",
                log.len()
            ),
        ));
    };
    // A panic prints `panicked at <file>:<line>:<column>:` and the message on the next line.
    let lines: Vec<&str> = log[panic..].lines().take(2).collect();
    let Some(message) = lines.iter().find(|line| line.contains(TEST_PANIC)) else {
        return Err(check(
            step,
            format!("the logged panic is not the test's: {lines:?}"),
        ));
    };
    let early = early_lines(step, &log)?;
    println!(
        "FERRIC_TEST_PASS log-previous ({} bytes, {}; {early})",
        log.len(),
        message.trim()
    );
    root_only()
}

/// Checks that the kernel kept what it logged before its heap existed: this boot's log holds
/// [`EARLY_LINE`], and so does `previous` unless it wrapped. Returns what it checked.
fn early_lines(step: &'static str, previous: &str) -> Result<&'static str, Failure> {
    let this_boot = std::fs::read(LOG_PATH).map_err(io_failure(step, LOG_PATH))?;
    if !String::from_utf8_lossy(&this_boot).contains(EARLY_LINE) {
        return Err(check(
            step,
            format!("this boot's {LOG_PATH} has no {EARLY_LINE:?} line from before the heap"),
        ));
    }
    if previous.len() >= DATA_BYTES {
        return Ok("early lines kept this boot; the previous log wrapped");
    }
    if !previous.contains(EARLY_LINE) {
        return Err(check(
            step,
            format!(
                "the previous boot's {} bytes of log have no {EARLY_LINE:?} line",
                previous.len()
            ),
        ));
    }
    Ok("early lines kept")
}

/// The argument that runs [`denied`] in a child.
pub const DENIED_COMMAND: &str = "log-previous-denied";

/// The user [`denied`] switches to: anyone but root.
const UNPRIVILEGED_UID: c_int = 1000;

/// The exit code of a [`denied`] child that could open the file.
const OPENED: u8 = 2;

unsafe extern "C" {
    // relibc's `setresuid(3)`.
    fn setresuid(ruid: c_int, euid: c_int, suid: c_int) -> c_int;
}

/// `log-previous-root-only`: runs this program again as [`DENIED_COMMAND`], which drops root
/// and must be refused [`PREVIOUS_PATH`].
fn root_only() -> Result<(), Failure> {
    let step = "log-previous-root-only";
    let exe = std::env::current_exe().map_err(io_failure(step, "the test's own path"))?;
    let status = Command::new(&exe)
        .arg(DENIED_COMMAND)
        .status()
        .map_err(io_failure(step, "a child test"))?;
    match status.code() {
        Some(0) => {
            println!("FERRIC_TEST_PASS log-previous-root-only (uid {UNPRIVILEGED_UID} is refused)");
            Ok(())
        }
        Some(code) if code == i32::from(OPENED) => Err(check(
            step,
            format!("uid {UNPRIVILEGED_UID} could open {PREVIOUS_PATH}"),
        )),
        _ => Err(check(step, format!("the child test failed: {status}"))),
    }
}

/// The child of `log-previous-root-only`: drops root, then exits 0 when opening
/// [`PREVIOUS_PATH`] is refused, [`OPENED`] when it is not, and 1 when it cannot run the check.
pub fn denied() -> ExitCode {
    // SAFETY: `setresuid` takes plain integers and changes only this process's user IDs.
    if unsafe { setresuid(UNPRIVILEGED_UID, UNPRIVILEGED_UID, UNPRIVILEGED_UID) } != 0 {
        println!(
            "log-previous-denied: setresuid({UNPRIVILEGED_UID}): {}",
            io::Error::last_os_error()
        );
        return ExitCode::FAILURE;
    }
    match std::fs::File::open(PREVIOUS_PATH) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(OPENED),
        Err(err) => {
            println!("log-previous-denied: {PREVIOUS_PATH}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The records at the start of `bytes` that decode as kernel events; the other CPUs ran on
/// while the dump was taken, so the last one can be torn.
fn decode_whole(bytes: &[u8]) -> Vec<Entry> {
    Records::new(bytes)
        .map_while(Result::ok)
        .map_while(|record| {
            let event = CATALOG.get(record.event())?;
            (event.args().len() == record.arg_count()).then(|| Entry {
                event: record.event(),
                timestamp: record.timestamp(),
                args: record.args().collect(),
            })
        })
        .collect()
}
