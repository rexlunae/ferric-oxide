//! Host microbenchmarks of the executor core: spawn, poll, wake, join, and a
//! deadline round, as nanoseconds per operation.
//!
//! These numbers are host-machine evidence only — the crate's issue (#180)
//! explicitly does not treat host microbench speed as kernel performance
//! evidence (QEMU and real hardware differ), and no adoption gate uses them.
//! They exist to catch gross regressions in the core's hot paths while it is
//! still host-testable.
//!
//! Run with `cargo run --release --example microbench` in `crates/exec-core`.

use std::sync::Arc;
use std::time::Instant;

use ferric_exec_core::{Clock, Config, Executor, FakeClock, JoinOutcome, Step};

fn ns_per(op: &str, ops: u64, start: Instant) {
    let ns = start.elapsed().as_nanos();
    let per = ns as f64 / ops as f64;
    println!("{op}: {per:.1} ns/op ({ns} ns over {ops} ops)");
}

fn main() {
    let clock = Arc::new(FakeClock::new());
    // The pool is u16-indexed (a worker pool of 65 535 tasks is far beyond any
    // planned use); 40 000 keeps the loops below it while exercising the pool.
    let mut exec = Executor::new(
        Arc::clone(&clock) as Arc<dyn Clock>,
        Config {
            capacity: 40_000,
            ..Config::default()
        },
    );

    // Spawn: how fast can the pool admit a closure.
    let ops = 20_000;
    let start = Instant::now();
    for _ in 0..ops {
        let _ = exec.spawn(|_| Step::Done { result: 1 }).expect("spawn");
    }
    ns_per("spawn", ops, start);

    // Complete everything that was spawned: the poll loop reading the ready
    // queue and dropping finished bodies.
    let start = Instant::now();
    let mut done = 0u64;
    while done < ops {
        done += exec.poll().ran;
    }
    ns_per("poll-complete", ops, start);

    // Wake: set the flag and push the channel entry (the interrupt-context
    // path), then the owner settles the wakes and cancels each task.
    let mut wakers = Vec::new();
    for _ in 0..ops {
        let id = exec.spawn(|_| Step::Wait).expect("spawn");
        wakers.push(exec.waker(id).expect("a live waker"));
    }
    let start = Instant::now();
    for waker in &wakers {
        waker.wake();
    }
    ns_per("wake", ops, start);
    let start = Instant::now();
    for _ in 0..ops {
        exec.poll();
    }
    ns_per("poll-wake-settle", ops, start);
    let start = Instant::now();
    for waker in &wakers {
        exec.cancel(waker.id()).expect("cancel");
        assert_eq!(exec.join(waker.id()), Some(JoinOutcome::Cancelled));
    }
    ns_per("cancel-join", ops, start);

    // A deadline round: tasks sleep at staggered deadlines; the clock then
    // fires them all at once (a rearm burst, as the kernel's deadline service
    // would after the earliest deadline), and the polls settle the due tasks.
    let ops = 20_000u64;
    for i in 0..ops {
        let _ = exec
            .spawn(move |_| Step::WaitDeadline { deadline: i + 1 })
            .expect("spawn");
    }
    let start = Instant::now();
    for _ in 0..ops {
        exec.poll(); // Every task arms its sleep with its deadline.
    }
    ns_per("sleep-deadline", ops, start);

    // Poll overhead when nothing is schedulable (the worker's idle check).
    let start = Instant::now();
    for _ in 0..ops {
        exec.poll();
    }
    ns_per("poll-idle", ops, start);

    // All deadlines fire at once; the batch budget bounds each poll.
    clock.advance(ops + 1);
    let start = Instant::now();
    let mut done = 0u64;
    while done < ops {
        let outcome = exec.poll();
        done += outcome.ran + outcome.cancelled;
    }
    ns_per("deadline-fire", ops, start);
}
