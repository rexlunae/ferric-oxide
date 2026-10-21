//! On-target futex records: queued 32/64-bit waits, timeout cleanup, wake counts and disable.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use ferric_trace::kernel::{SCHED_FUTEX_RESUME, SCHED_FUTEX_WAIT, SCHED_FUTEX_WAKE};
use syscall::{EAGAIN, ETIMEDOUT, FUTEX_WAIT, FUTEX_WAIT64, FUTEX_WAKE, TimeSpec};

use super::{Entry, Failure, check, command, drain, errno_result, listing};

const EVENTS: &str = "sched.futex_wait sched.futex_resume sched.futex_wake";
const VALUE: u64 = 0x1234_5678_9abc_def0;

/// All calls use a live, aligned word and either no timeout or a live timespec.
fn call(
    word: &AtomicU64,
    op: usize,
    val: usize,
    timeout: Option<&TimeSpec>,
) -> syscall::Result<usize> {
    // SAFETY: the atomic word is aligned for both widths and remains alive for the call;
    // the optional timespec remains readable, and FUTEX_WAKE ignores the timeout.
    unsafe {
        syscall::syscall5(
            syscall::SYS_FUTEX,
            word.as_ptr() as usize,
            op,
            val,
            timeout.map_or(0, |time| time as *const TimeSpec as usize),
            0,
        )
    }
}

fn expect(result: syscall::Result<usize>, expected: syscall::Result<usize>) -> Result<(), Failure> {
    if result == expected {
        Ok(())
    } else {
        Err(check(
            "trace-futex",
            format!("futex returned {result:?}, expected {expected:?}"),
        ))
    }
}

/// Exercises mismatch, timeout and a zero-count wake without scheduling a peer.
fn timeouts(word: &AtomicU64) -> Result<(), Failure> {
    let expired = TimeSpec::default();
    for (op, value) in [
        (FUTEX_WAIT, VALUE as u32 as usize),
        (FUTEX_WAIT64, VALUE as usize),
    ] {
        expect(
            call(word, op, value ^ 1, Some(&expired)),
            Err(syscall::Error::new(EAGAIN)),
        )?;
        expect(
            call(word, op, value, Some(&expired)),
            Err(syscall::Error::new(ETIMEDOUT)),
        )?;
    }
    expect(call(word, FUTEX_WAKE, 0, None), Ok(0))?;
    // Expired entries were removed, so a later wake must not consume one.
    expect(call(word, FUTEX_WAKE, 7, None), Ok(0))
}

/// Wakes an actual queued peer. Both the waiter and the wake loop have bounded deadlines.
fn wake_peer(word: &Arc<AtomicU64>) -> Result<(), Failure> {
    let mut deadline = TimeSpec::default();
    syscall::clock_gettime(syscall::CLOCK_MONOTONIC, &mut deadline)
        .map_err(|err| check("trace-futex", format!("clock_gettime: {err}")))?;
    deadline.tv_sec += 5;
    let peer_word = Arc::clone(word);
    let peer =
        std::thread::spawn(move || call(&peer_word, FUTEX_WAIT64, VALUE as usize, Some(&deadline)));
    let limit = Instant::now() + Duration::from_secs(5);
    let mut woke = false;
    while !peer.is_finished() && Instant::now() < limit {
        if call(word, FUTEX_WAKE, 1, None) == Ok(1) {
            woke = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let result = peer
        .join()
        .map_err(|_| check("trace-futex", "waiter panicked"))?;
    expect(result, Ok(0))?;
    if !woke {
        return Err(check(
            "trace-futex",
            "no wake consumed the peer's queue entry",
        ));
    }
    Ok(())
}

fn records_for<'a>(trace: &'a [Vec<Entry>], word: &AtomicU64) -> Vec<&'a Entry> {
    let mut entries: Vec<_> = trace
        .iter()
        .flatten()
        .filter(|entry| {
            entry.arg(1) == Some(word.as_ptr() as u64)
                && [&SCHED_FUTEX_WAIT, &SCHED_FUTEX_RESUME, &SCHED_FUTEX_WAKE]
                    .iter()
                    .any(|event| entry.is(event))
        })
        .collect();
    entries.sort_by_key(|entry| entry.timestamp);
    entries
}

/// Matches waits to resumes by physical key and context, including after migration.
fn pairs(entries: &[&Entry], expected_results: &[u64]) -> Result<(), Failure> {
    let waits: Vec<_> = entries
        .iter()
        .copied()
        .filter(|e| e.is(&SCHED_FUTEX_WAIT))
        .collect();
    let resumes: Vec<_> = entries
        .iter()
        .copied()
        .filter(|e| e.is(&SCHED_FUTEX_RESUME))
        .collect();
    if waits.len() != expected_results.len() || resumes.len() != waits.len() {
        return Err(check(
            "trace-futex",
            format!("{} waits and {} resumes", waits.len(), resumes.len()),
        ));
    }
    for ((wait, resume), result) in waits.iter().zip(resumes).zip(expected_results) {
        if wait.arg(0) != resume.arg(0)
            || wait.arg(2) != resume.arg(2)
            || wait.timestamp > resume.timestamp
            || resume.arg(3) != Some(*result)
            || wait.arg(5) != Some(1)
        {
            return Err(check(
                "trace-futex",
                format!("unmatched wait/resume: {wait:?} / {resume:?}"),
            ));
        }
    }
    Ok(())
}

pub(super) fn check_futex(cpus: &[u32]) -> Result<(), Failure> {
    let step = "trace-futex";
    let list = listing(step)?;
    if !list.iter().any(|(name, _)| name == SCHED_FUTEX_WAIT.name()) {
        println!("FERRIC_TEST_SKIP trace-futex: no scheduler futex events");
        return Ok(());
    }
    command(step, &format!("disable {EVENTS}"))?;
    drain(step, cpus)?;
    command(step, &format!("enable {EVENTS}"))?;
    let timed_word = AtomicU64::new(VALUE);
    let peer_word = Arc::new(AtomicU64::new(VALUE));
    timeouts(&timed_word)?;
    wake_peer(&peer_word)?;
    command(step, &format!("disable {EVENTS}"))?;
    let trace = drain(step, cpus)?;
    let timed = records_for(&trace, &timed_word);
    pairs(&timed, &[errno_result(ETIMEDOUT); 2])?;
    let waits: Vec<_> = timed.iter().filter(|e| e.is(&SCHED_FUTEX_WAIT)).collect();
    if waits[0].arg(3) != Some(32)
        || waits[0].arg(4) != Some(VALUE as u32 as u64)
        || waits[1].arg(3) != Some(64)
        || waits[1].arg(4) != Some(VALUE)
    {
        return Err(check(step, "wait width or compared value was truncated"));
    }
    for requested in [0, 7] {
        if !timed
            .iter()
            .any(|e| e.is(&SCHED_FUTEX_WAKE) && e.arg(2) == Some(requested) && e.arg(3) == Some(0))
        {
            return Err(check(step, "missing empty wake count"));
        }
    }
    let peer = records_for(&trace, &peer_word);
    pairs(&peer, &[0])?;
    let wait = peer
        .iter()
        .find(|e| e.is(&SCHED_FUTEX_WAIT))
        .ok_or_else(|| check(step, "no peer wait"))?;
    if !peer.iter().any(|e| {
        e.is(&SCHED_FUTEX_WAKE)
            && e.arg(0) == wait.arg(0)
            && e.arg(2) == Some(1)
            && e.arg(3) == Some(1)
    }) {
        return Err(check(step, "missing successful wake of the peer's key"));
    }
    // Keep both words alive until the records were checked, so keys cannot be recycled.
    timeouts(&timed_word)?;
    let disabled = drain(step, cpus)?;
    if !records_for(&disabled, &timed_word).is_empty() {
        return Err(check(step, "futex events after disable"));
    }
    println!("FERRIC_TEST_PASS trace-futex (32/64-bit waits, timeout, wake, mismatch and disable)");
    Ok(())
}
