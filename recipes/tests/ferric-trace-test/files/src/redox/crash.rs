//! `ferric-trace-test crash`: the crash dump across a reboot (`kernel.trace_crash`).
//!
//! `/scheme/kernel.trace/crash` decides what this boot is:
//!
//! - Missing: the kernel keeps no dump, or has none from the previous boot. It prints a skip.
//! - Empty: a crash-test kernel (`kernel.trace_crash_test`) with no dump yet, so this is the
//!   first boot. It drains the rings, which `sched.switch` from boot has filled (a full ring
//!   drops new records, so the dump would hold only old ones), turns the `syscall` events on,
//!   makes a marker `write` of [`CRASH_MARK_LEN`]
//!   bytes to a closed descriptor, prints `FERRIC_TEST_PASS trace-crash-armed`, and writes
//!   `panic` to the file. The kernel writes the dump and resets the machine.
//! - A dump: the second boot. `trace-crash-read` checks that it parses, that its message is the
//!   crash test's panic, and that its records hold the marker `write` and the `write` of
//!   `panic` that ended the first boot.

use std::fs::OpenOptions;
use std::io::{self, Write};

use ferric_trace::abi::{CRASH_PATH, CRASH_TEST_COMMAND};
use ferric_trace::crash::Dump;
use ferric_trace::kernel::{CATALOG, SYSCALL_ENTER};
use ferric_trace::record::Records;
use syscall::{EBADF, SYS_WRITE};

use super::{BAD_FD, Entry, Failure, Why, check, command, cpus, drain, io_failure, path};

/// The length of the marker `write` before the panic.
const CRASH_MARK_LEN: usize = 1931;

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
    // Room in every ring, so the records from here on are the newest the dump keeps.
    drain(step, &cpus(step)?)?;
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
        "FERRIC_TEST_PASS trace-crash-read ({} records of {cpus} CPUs, panic on CPU {cpu}: {})",
        entries.len(),
        message.lines().last().unwrap_or_default()
    );
    Ok(())
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
