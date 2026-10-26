//! Records child, thread and ring-worker contexts, including their cleanup.

use std::collections::BTreeSet;
use std::process::Command;
use std::time::{Duration, Instant};

use ferric_trace::kernel::{SCHED_CONTEXT_CREATE, SCHED_CONTEXT_EXIT, SCHED_CONTEXT_START};

use super::{Entry, Failure, check, command, drain, has_scheme, io_failure, listing};

/// Selects the child that exits immediately without running the tracing suite.
pub(super) const CHILD_ARG: &str = "lifecycle-child";
/// Its process exit status, checked with wait rather than inferred from kernel records.
pub(super) const CHILD_STATUS: u8 = 23;
const EVENTS: &str = "sched.context_create sched.context_start sched.context_exit";

/// Runs this program as the child that exits with `CHILD_STATUS` at once (a fork and an
/// exec), checks its status for `step`, and returns its process id.
pub(super) fn spawn_child(step: &'static str) -> Result<u32, Failure> {
    let exe = std::env::current_exe().map_err(io_failure(step, "current_exe"))?;
    let mut child = Command::new(exe)
        .arg(CHILD_ARG)
        .spawn()
        .map_err(io_failure(step, "spawn child"))?;
    let pid = child.id();
    let status = child.wait().map_err(io_failure(step, "wait child"))?;
    if status.code() != Some(i32::from(CHILD_STATUS)) {
        return Err(check(step, format!("child exited with {status}")));
    }
    Ok(pid)
}

fn starts(trace: &[Vec<Entry>], pid: u32) -> impl Iterator<Item = &Entry> {
    trace
        .iter()
        .flatten()
        .filter(move |e| e.is(&SCHED_CONTEXT_START) && e.arg(0) == Some(u64::from(pid)))
}

/// A libc thread join can finish before the kernel has finished that context's cleanup.
fn exited(trace: &[Vec<Entry>], pid: u32) -> bool {
    let started: BTreeSet<_> = starts(trace, pid).filter_map(|e| e.arg(1)).collect();
    !started.is_empty()
        && started.iter().all(|id| {
            trace.iter().flatten().any(|e| {
                e.is(&SCHED_CONTEXT_EXIT)
                    && e.arg(0) == Some(u64::from(pid))
                    && e.arg(1) == Some(*id)
            })
        })
}

fn verify(trace: &[Vec<Entry>], pid: u32, user_mode: u64) -> Result<usize, Failure> {
    let mut count = 0;
    for start in starts(trace, pid) {
        let id = start.arg(1);
        let create = trace
            .iter()
            .flatten()
            .find(|e| e.is(&SCHED_CONTEXT_CREATE) && e.arg(0) == id)
            .ok_or_else(|| check("trace-lifecycle", format!("no create for {id:?}")))?;
        let exit = trace
            .iter()
            .flatten()
            .find(|e| e.is(&SCHED_CONTEXT_EXIT) && e.arg(1) == id)
            .ok_or_else(|| check("trace-lifecycle", format!("no exit for {id:?}")))?;
        if create.timestamp > start.timestamp
            || start.timestamp > exit.timestamp
            || create.arg(1) != Some(user_mode)
            || exit.arg(0) != Some(u64::from(pid))
            || exit.args.get(2..) != Some(&[0, 0, 0, 0][..])
        {
            return Err(check(
                "trace-lifecycle",
                format!("incorrect lifecycle: {create:?} / {start:?} / {exit:?}"),
            ));
        }
        count += 1;
    }
    if count == 0 {
        return Err(check("trace-lifecycle", format!("no starts for PID {pid}")));
    }
    Ok(count)
}

/// Runs the existing asynchronous ring tests while continuously collecting worker lifecycles.
fn worker_probe(cpus: &[u32], trace: &mut Vec<Vec<Entry>>) -> Result<bool, Failure> {
    let step = "trace-lifecycle";
    if !has_scheme("kernel.ring").map_err(io_failure(step, "/scheme"))? {
        return Ok(false);
    }
    let mut child = Command::new("/usr/bin/ferric-ring-test")
        .arg("--async-only")
        .spawn()
        .map_err(io_failure(step, "spawn ring worker probe"))?;
    let limit = Instant::now() + Duration::from_secs(30);
    loop {
        trace.extend(drain(step, cpus)?);
        if let Some(status) = child
            .try_wait()
            .map_err(io_failure(step, "poll ring worker probe"))?
        {
            if !status.success() {
                return Err(check(
                    step,
                    format!("ring worker probe exited with {status}"),
                ));
            }
            return Ok(true);
        }
        if Instant::now() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(check(step, "ring worker probe timed out"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Checks child, thread and worker lifecycles and suppression after live disable.
///
/// # Errors
///
/// Reports failed trace I/O, child/thread creation or cleanup, missing or incorrectly
/// ordered records, wrong metadata, or records emitted after disable.
pub(super) fn check_lifecycle(cpus: &[u32]) -> Result<(), Failure> {
    let step = "trace-lifecycle";
    let list = listing(step)?;
    if !list
        .iter()
        .any(|(name, _)| name == SCHED_CONTEXT_CREATE.name())
    {
        println!("FERRIC_TEST_SKIP trace-lifecycle: no context lifecycle events");
        return Ok(());
    }
    command(step, &format!("disable {EVENTS}"))?;
    drain(step, cpus)?;
    command(step, &format!("enable {EVENTS}"))?;
    let child = spawn_child(step)?;
    let parent = std::process::id();
    std::thread::Builder::new()
        .spawn(|| ())
        .map_err(io_failure(step, "spawn thread"))?
        .join()
        .map_err(|_| check(step, "thread panicked"))?;
    let mut trace = Vec::new();
    let workers = worker_probe(cpus, &mut trace)?;
    let limit = Instant::now() + Duration::from_secs(5);
    loop {
        trace.extend(drain(step, cpus)?);
        if exited(&trace, child) && exited(&trace, parent) && (!workers || exited(&trace, 0)) {
            break;
        }
        if Instant::now() >= limit {
            return Err(check(
                step,
                "child, thread or worker cleanup did not finish",
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    command(step, &format!("disable {EVENTS}"))?;
    trace.extend(drain(step, cpus)?);
    let child_count = verify(&trace, child, 1)?;
    let thread_count = verify(&trace, parent, 1)?;
    let worker_count = if workers { verify(&trace, 0, 0)? } else { 0 };
    let untraced_child = spawn_child(step)?;
    let disabled = drain(step, cpus)?;
    if disabled.iter().flatten().any(|e| {
        e.is(&SCHED_CONTEXT_CREATE)
            || (e.is(&SCHED_CONTEXT_START) || e.is(&SCHED_CONTEXT_EXIT))
                && e.arg(0) == Some(u64::from(untraced_child))
    }) {
        return Err(check(step, "context lifecycle records after disable"));
    }
    println!(
        "FERRIC_TEST_PASS trace-lifecycle ({child_count} child contexts, {thread_count} thread contexts, {worker_count} kernel workers, ordered create/start/exit and disable)"
    );
    Ok(())
}
