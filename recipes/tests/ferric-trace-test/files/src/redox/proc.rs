//! `trace-proc`: procmgr's process lifecycle records (`sched.proc_fork`, `sched.proc_exec`,
//! `sched.proc_exit`, `sched.proc_wait`) for a child that exits with a known status, and the
//! kernel's refusal of records user space may not write.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant};

use ferric_trace::Event;
use ferric_trace::abi::{self, UserRecord};
use ferric_trace::kernel::{
    SCHED_CONTEXT_START, SCHED_PROC_EXEC, SCHED_PROC_EXIT, SCHED_PROC_FORK, SCHED_PROC_WAIT,
    USER_EVENTS,
};
use syscall::EINVAL;

use super::lifecycle::{CHILD_ARG, CHILD_STATUS};
use super::{Entry, Failure, check, command, drain, io_failure, listing, path};

const STEP: &str = "trace-proc";
const EVENTS: &str = "sched.proc_fork sched.proc_exec sched.proc_exit sched.proc_wait";

/// Runs the child that exits with [`CHILD_STATUS`] and waits for it; returns its pid.
fn spawn_child() -> Result<u32, Failure> {
    let exe = std::env::current_exe().map_err(io_failure(STEP, "current_exe"))?;
    let mut child = Command::new(exe)
        .arg(CHILD_ARG)
        .spawn()
        .map_err(io_failure(STEP, "spawn child"))?;
    let pid = child.id();
    let status = child.wait().map_err(io_failure(STEP, "wait child"))?;
    if status.code() != Some(i32::from(CHILD_STATUS)) {
        return Err(check(STEP, format!("child exited with {status}")));
    }
    Ok(pid)
}

/// The first record of `event` whose arguments start with `args`.
fn find<'a>(trace: &'a [Vec<Entry>], event: &Event, args: &[u64]) -> Option<&'a Entry> {
    trace
        .iter()
        .flatten()
        .filter(|e| e.is(event) && e.args.starts_with(args))
        .min_by_key(|e| e.timestamp)
}

/// The child's fork, exec, exit and wait records, once all four are in `trace`.
fn records(trace: &[Vec<Entry>], parent: u64, child: u64) -> Option<[&Entry; 4]> {
    Some([
        find(trace, &SCHED_PROC_FORK, &[parent, child])?,
        find(trace, &SCHED_PROC_EXEC, &[child])?,
        find(trace, &SCHED_PROC_EXIT, &[child])?,
        find(trace, &SCHED_PROC_WAIT, &[parent, child])?,
    ])
}

/// Checks that the four records are in order and carry the child's exit status.
fn verify(found: [&Entry; 4]) -> Result<(), Failure> {
    let [fork, exec, exit, wait] = found;
    let status = u64::from(CHILD_STATUS);
    if !(fork.timestamp <= exec.timestamp
        && exec.timestamp <= exit.timestamp
        && exit.timestamp <= wait.timestamp)
    {
        return Err(check(
            STEP,
            format!("out of order: {fork:?} / {exec:?} / {exit:?} / {wait:?}"),
        ));
    }
    if exit.args.get(1..) != Some(&[status, 0][..]) || wait.arg(2) != Some(status << 8) {
        return Err(check(STEP, format!("wrong status: {exit:?} / {wait:?}")));
    }
    Ok(())
}

/// Writes `bytes` to `emit` and checks that the kernel refuses them with `EINVAL`.
fn refused(what: &str, bytes: &[u8]) -> Result<(), Failure> {
    let path = path(abi::EMIT_PATH);
    let result = OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|mut f| f.write(bytes));
    match result {
        Err(err) if err.raw_os_error() == Some(EINVAL) => Ok(()),
        other => Err(check(STEP, format!("{what} gave {other:?}, not EINVAL"))),
    }
}

/// A pid no process has: the probe record's.
const PROBE_PID: u64 = u64::MAX;

/// Writes a `sched.proc_exec` record for [`PROBE_PID`] to `emit` itself, so a failure says
/// whether the kernel path or `procmgr` lost the records.
fn probe(cpus: &[u32]) -> Result<(), Failure> {
    let mut bytes = [0; UserRecord::MAX_BYTES];
    let record = UserRecord::new(&SCHED_PROC_EXEC, &[PROBE_PID])
        .map_err(|err| check(STEP, format!("record: {err}")))?;
    let len = record.encode(&mut bytes);
    let path = path(abi::EMIT_PATH);
    OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|mut f| f.write(&bytes[..len]))
        .map_err(io_failure(STEP, "emit"))?;
    let trace = drain(STEP, cpus)?;
    if find(&trace, &SCHED_PROC_EXEC, &[PROBE_PID]).is_none() {
        return Err(check(STEP, "a record written to emit was not stored"));
    }
    Ok(())
}

/// What `trace` holds of the process events, for a failure message.
fn summary(trace: &[Vec<Entry>]) -> String {
    let seen: Vec<_> = trace
        .iter()
        .flatten()
        .filter(|e| USER_EVENTS.iter().any(|event| e.is(event)))
        .take(16)
        .map(|e| format!("{}{:?}", e.event.get(), e.args))
        .collect();
    format!("process records seen: {seen:?}")
}

/// User space may write only the process events: a kernel event's record, a record with
/// reserved header bits and a partial word are all refused.
fn check_refusals() -> Result<(), Failure> {
    let mut bytes = [0; UserRecord::MAX_BYTES];
    let forged = UserRecord::new(&SCHED_CONTEXT_START, &[1, 2])
        .map_err(|err| check(STEP, format!("record: {err}")))?;
    let len = forged.encode(&mut bytes);
    refused("a sched.context_start record", &bytes[..len])?;
    let mut reserved = bytes;
    let exit = UserRecord::new(&SCHED_PROC_EXIT, &[1, 0, 0])
        .map_err(|err| check(STEP, format!("record: {err}")))?;
    let len = exit.encode(&mut reserved);
    reserved[7] = 1;
    refused("a header with reserved bits", &reserved[..len])?;
    refused("a partial word", &bytes[..4])
}

/// `trace-proc`: with the process events on, a child that exits with [`CHILD_STATUS`] has a
/// fork record naming this process as its parent, then an exec, an exit with the status and
/// no signal, and this process's wait with the status as `wait` reports it (`status << 8`),
/// in that order. With the events off, a second child leaves none. The kernel refuses
/// records of events user space may not write.
///
/// # Errors
///
/// Reports failed trace I/O, a child that did not exit as expected, missing, misordered or
/// wrong records, records after disable, or a forged record the kernel stored.
pub(super) fn check_proc(cpus: &[u32]) -> Result<(), Failure> {
    let list = listing(STEP)?;
    if !list.iter().any(|(name, _)| name == SCHED_PROC_FORK.name()) {
        println!("FERRIC_TEST_SKIP trace-proc: no process lifecycle events");
        return Ok(());
    }
    command(STEP, &format!("disable {EVENTS}"))?;
    drain(STEP, cpus)?;
    command(STEP, &format!("enable {EVENTS}"))?;
    probe(cpus)?;
    let parent = u64::from(std::process::id());
    let child = u64::from(spawn_child()?);
    let mut trace = Vec::new();
    let limit = Instant::now() + Duration::from_secs(5);
    let found = loop {
        trace.extend(drain(STEP, cpus)?);
        if let Some(found) = records(&trace, parent, child) {
            break found;
        }
        if Instant::now() >= limit {
            return Err(check(
                STEP,
                format!(
                    "no complete fork/exec/exit/wait for pid {child} of {parent}; {}",
                    summary(&trace)
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    verify(found)?;
    check_refusals()?;
    command(STEP, &format!("disable {EVENTS}"))?;
    drain(STEP, cpus)?;
    let untraced = spawn_child()?;
    let after = drain(STEP, cpus)?;
    if after
        .iter()
        .flatten()
        .any(|e| USER_EVENTS.iter().any(|event| e.is(event)))
    {
        return Err(check(
            STEP,
            format!("process records after disable (child {untraced})"),
        ));
    }
    println!(
        "FERRIC_TEST_PASS trace-proc (fork, exec, exit {CHILD_STATUS} and wait of pid {child}, in order; none after disable; forged records refused)"
    );
    Ok(())
}
