//! Records the kernel contexts of a child process and a thread, including their cleanup.

use std::collections::BTreeSet;
use std::process::Command;
use std::time::{Duration, Instant};

use ferric_trace::kernel::{SCHED_CONTEXT_CREATE, SCHED_CONTEXT_EXIT, SCHED_CONTEXT_START};

use super::{Entry, Failure, check, command, drain, io_failure, listing};

pub(super) const CHILD_ARG: &str = "lifecycle-child";
pub(super) const CHILD_STATUS: u8 = 23;
const EVENTS: &str = "sched.context_create sched.context_start sched.context_exit";

fn spawn_child() -> Result<u32, Failure> {
    let step = "trace-lifecycle";
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

fn verify(trace: &[Vec<Entry>], pid: u32) -> Result<usize, Failure> {
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
            || create.arg(1) != Some(1)
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
    let child = spawn_child()?;
    let parent = std::process::id();
    std::thread::Builder::new()
        .spawn(|| ())
        .map_err(io_failure(step, "spawn thread"))?
        .join()
        .map_err(|_| check(step, "thread panicked"))?;
    let mut trace = Vec::new();
    let limit = Instant::now() + Duration::from_secs(5);
    loop {
        trace.extend(drain(step, cpus)?);
        if exited(&trace, child) && exited(&trace, parent) {
            break;
        }
        if Instant::now() >= limit {
            return Err(check(step, "child or thread cleanup did not finish"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    command(step, &format!("disable {EVENTS}"))?;
    trace.extend(drain(step, cpus)?);
    let child_count = verify(&trace, child)?;
    let thread_count = verify(&trace, parent)?;
    let untraced_child = spawn_child()?;
    let disabled = drain(step, cpus)?;
    if disabled.iter().flatten().any(|e| {
        e.is(&SCHED_CONTEXT_CREATE)
            || (e.is(&SCHED_CONTEXT_START) || e.is(&SCHED_CONTEXT_EXIT))
                && e.arg(0) == Some(u64::from(untraced_child))
    }) {
        return Err(check(step, "context lifecycle records after disable"));
    }
    println!(
        "FERRIC_TEST_PASS trace-lifecycle ({child_count} child contexts, {thread_count} thread contexts, ordered create/start/exit and disable)"
    );
    Ok(())
}
