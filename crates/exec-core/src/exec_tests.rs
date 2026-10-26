//! Deterministic and threaded tests of the executor core.
//!
//! The deterministic tests fix the state machine and its race pairings; the
//! threaded tests race producers against the poll loop. Everything here runs on
//! the host, and the crate is safe Rust, so the same tests are Miri-compatible
//! (run `cargo +nightly miri test` in `crates/exec-core`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

extern crate std;

use std::cell::Cell;
use std::rc::Rc;

use crate::Executor;
use crate::clock::{Clock, FakeClock};
use crate::task::Step;

/// Counts its own drops; captured by task bodies to prove exactly-once
/// destruction of a task's captured state.
struct Guard(Rc<Cell<u64>>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock::new())
}

fn executor(clock: &Arc<FakeClock>) -> Executor {
    Executor::new(Arc::clone(clock) as Arc<dyn Clock>, Default::default())
}

/// A body that counts its runs and finishes with the count.
fn counter(counter: Rc<Cell<u64>>, result: u64) -> impl FnMut(&mut crate::TaskCtx) -> Step {
    move |_| {
        counter.set(counter.get() + 1);
        Step::Done { result }
    }
}

/// A body that waits until its `runs` reach `target`, then finishes.
fn wait_until(target: u64, runs: Rc<Cell<u64>>) -> impl FnMut(&mut crate::TaskCtx) -> Step {
    move |_| {
        let n = runs.get() + 1;
        runs.set(n);
        if n >= target {
            Step::Done { result: n }
        } else {
            Step::Wait
        }
    }
}

#[test]
fn a_task_completes_and_joins_exactly_once() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec.spawn(counter(Rc::clone(&runs), 42)).expect("spawn");
    assert_eq!(exec.join(id), None, "nothing ran yet");
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 1);
    assert_eq!(runs.get(), 1);
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(42)));
    assert_eq!(exec.join(id), None, "the slot is freed");
    let stats = exec.stats();
    assert_eq!(stats.spawned, 1);
    assert_eq!(stats.completed, 1);
}

#[test]
fn spawn_refuses_when_the_pool_is_full_and_changes_nothing() {
    let clock = clock();
    let mut exec = Executor::new(
        Arc::clone(&clock) as Arc<dyn Clock>,
        crate::Config {
            capacity: 2,
            budget: 2,
        },
    );
    let runs = Rc::new(Cell::new(0));
    let _a = exec.spawn(counter(Rc::clone(&runs), 1)).expect("spawn a");
    let _b = exec.spawn(counter(Rc::clone(&runs), 2)).expect("spawn b");
    assert_eq!(
        exec.spawn(counter(runs, 3)),
        Err(crate::Admission::PoolFull)
    );
    let stats = exec.stats();
    assert_eq!(stats.spawned, 2, "the refused spawn changed nothing");
    // Both live tasks still run and settle.
    exec.poll();
    assert_eq!(exec.poll().ran, 0);
}

#[test]
fn the_poll_budget_bounds_a_batch_and_never_a_single_poll() {
    let clock = clock();
    let mut exec = Executor::new(
        Arc::clone(&clock) as Arc<dyn Clock>,
        crate::Config {
            capacity: 16,
            budget: 2,
        },
    );
    let runs = Rc::new(Cell::new(0));
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(exec.spawn(counter(Rc::clone(&runs), 1)).expect("spawn"));
    }
    let first = exec.poll();
    assert_eq!(first.ran, 2);
    assert_eq!(first.ready_after, 2);
    let second = exec.poll();
    assert_eq!(second.ran, 2);
    assert_eq!(second.ready_after, 0);
    for id in ids {
        assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(1)));
    }
}

#[test]
fn self_wake_via_the_context_never_sleeps() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec
        .spawn({
            let runs = Rc::clone(&runs);
            move |ctx| {
                runs.set(runs.get() + 1);
                if runs.get() >= 3 {
                    Step::Done { result: runs.get() }
                } else {
                    ctx.wake();
                    Step::Wait
                }
            }
        })
        .expect("spawn");
    // The budget is 64, so the self-waking task runs three times in one poll and
    // finishes; it never sleeps with a pending wake.
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 3);
    assert_eq!(runs.get(), 3);
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(3)));
}

#[test]
fn duplicate_wakes_coalesce() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec.spawn(wait_until(2, Rc::clone(&runs))).expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    assert_eq!(exec.poll().ran, 1, "first run waits");
    // Ten wakes before the next poll: one request, one re-run.
    for _ in 0..10 {
        waker.wake();
    }
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 1);
    assert_eq!(runs.get(), 2);
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(2)));
}

#[test]
fn a_wake_before_the_wait_is_not_lost() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec.spawn(wait_until(2, Rc::clone(&runs))).expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    // The wake lands while the task is ready but before it has run.
    waker.wake();
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 1, "one run");
    assert_eq!(runs.get(), 1, "the wake was spent by the run itself");
    assert_eq!(exec.join(id), None, "the task waits again without a wake");
}

#[test]
fn a_wake_after_completion_is_dropped() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec.spawn(counter(runs, 7)).expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    exec.poll();
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(7)));
    // The slot is free; the stale handle's wake must not resurrect or disturb
    // anything, and a respawned task in the same slot is untouched.
    waker.wake();
    let new_id = exec
        .spawn(counter(Rc::new(Cell::new(0)), 9))
        .expect("spawn");
    assert_ne!(new_id, id, "the slot freed and could be reused");
    waker.wake();
    assert!(
        exec.schedulable(),
        "the new task is ready; the stale wake woke nothing else"
    );
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 1, "only the new task ran");
    assert_eq!(exec.join(new_id), Some(crate::JoinOutcome::Done(9)));
}

#[test]
fn a_stale_generation_waker_cannot_touch_the_new_occupant() {
    let clock = clock();
    let mut exec = executor(&clock);
    let id = exec
        .spawn(wait_until(2, Rc::new(Cell::new(0))))
        .expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    exec.poll();
    exec.cancel(id).expect("cancel");
    exec.join(id);
    // The waker is stale now: the slot belongs to a new generation.
    assert!(waker.is_stale());
    let runs = Rc::new(Cell::new(0));
    let new_id = exec.spawn(counter(Rc::clone(&runs), 5)).expect("spawn");
    assert!(
        waker.is_stale(),
        "the old handle is stale for the new occupant"
    );
    waker.wake();
    exec.poll();
    assert_eq!(
        runs.get(),
        1,
        "only the new task ran; the stale wake was dropped"
    );
    assert_eq!(exec.join(new_id), Some(crate::JoinOutcome::Done(5)));
}

#[test]
fn self_wake_storm_is_bounded_by_the_budget() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec
        .spawn({
            let runs = Rc::clone(&runs);
            move |ctx| {
                runs.set(runs.get() + 1);
                ctx.wake();
                Step::Wait
            }
        })
        .expect("spawn");
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 64, "exactly the budget ran in one batch");
    assert_eq!(runs.get(), 64);
    assert!(exec.schedulable(), "the task is still queued");
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 64);
    exec.cancel(id).expect("cancel");
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Cancelled));
}

#[test]
fn a_deadline_task_runs_only_when_due() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec
        .spawn({
            let runs = Rc::clone(&runs);
            move |_| {
                runs.set(runs.get() + 1);
                if runs.get() >= 2 {
                    Step::Done { result: runs.get() }
                } else {
                    Step::WaitDeadline { deadline: 1_000 }
                }
            }
        })
        .expect("spawn");
    assert_eq!(exec.poll().ran, 1);
    assert_eq!(runs.get(), 1);
    assert_eq!(exec.next_deadline(), Some(1_000));
    // Nothing is schedulable until the deadline; the worker could sleep.
    assert!(!exec.schedulable());
    // A long tick-free gap: the clock jumps far past the deadline.
    clock.advance(1_000_000);
    assert!(
        exec.schedulable(),
        "the deadline is due; the worker must not sleep"
    );
    assert_eq!(exec.poll().ran, 1);
    assert_eq!(runs.get(), 2);
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(2)));
}

#[test]
fn an_already_due_deadline_is_never_discarded() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec
        .spawn({
            let runs = Rc::clone(&runs);
            move |_| {
                let n = runs.get() + 1;
                runs.set(n);
                if n >= 2 {
                    Step::Done { result: n }
                } else {
                    // The deadline is already due: it must run now, and its
                    // again-due request runs it again, so it is never discarded.
                    Step::WaitDeadline { deadline: 0 }
                }
            }
        })
        .expect("spawn");
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 2);
    assert_eq!(
        runs.get(),
        2,
        "a deadline at or before now runs immediately"
    );
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(2)));
}

#[test]
fn deadlines_order_earliest_first_and_cancel_reorders() {
    let clock = clock();
    let mut exec = executor(&clock);
    let a_runs = Rc::new(Cell::new(0));
    let b_runs = Rc::new(Cell::new(0));
    let a = exec
        .spawn({
            let runs = Rc::clone(&a_runs);
            move |_| {
                let n = runs.get() + 1;
                runs.set(n);
                if n >= 2 {
                    Step::Done { result: n }
                } else {
                    Step::WaitDeadline { deadline: 2_000 }
                }
            }
        })
        .expect("spawn a");
    let b = exec
        .spawn({
            let runs = Rc::clone(&b_runs);
            move |_| {
                runs.set(runs.get() + 1);
                Step::WaitDeadline { deadline: 1_000 }
            }
        })
        .expect("spawn b");
    exec.poll();
    assert_eq!(exec.next_deadline(), Some(1_000), "the earliest deadline");
    // Cancel the earliest sleeper: its stale heap entry settles nothing when it
    // comes due, and the other sleeper wakes at its own deadline.
    exec.cancel(b).expect("cancel");
    clock.advance(1_500);
    let outcome = exec.poll();
    assert_eq!(outcome.due, 0, "nothing is due at 1500");
    assert_eq!(outcome.ran, 0);
    clock.advance(1_000);
    let outcome = exec.poll();
    assert_eq!(outcome.due, 1, "only the surviving sleeper came due");
    assert_eq!(a_runs.get(), 2, "a ran again at its deadline");
    assert_eq!(b_runs.get(), 1, "b never ran again");
    assert_eq!(exec.join(a), Some(crate::JoinOutcome::Done(2)));
    assert_eq!(exec.join(b), Some(crate::JoinOutcome::Cancelled));
}

#[test]
fn a_wake_beats_a_deadline_sleep() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec
        .spawn({
            let runs = Rc::clone(&runs);
            move |_| {
                runs.set(runs.get() + 1);
                Step::WaitDeadline { deadline: 1_000 }
            }
        })
        .expect("spawn");
    exec.poll();
    assert_eq!(runs.get(), 1);
    let waker = exec.waker(id).expect("a live waker");
    waker.wake();
    let outcome = exec.poll();
    assert_eq!(
        outcome.ran, 1,
        "the wake ran the sleeper before its deadline"
    );
    assert_eq!(runs.get(), 2);
}

#[test]
fn cancel_settles_queued_waiting_and_sleeping_tasks() {
    let clock = clock();
    let mut exec = executor(&clock);

    // Queued: cancelled before it ever runs.
    let queued_runs = Rc::new(Cell::new(0));
    let queued = exec
        .spawn(counter(Rc::clone(&queued_runs), 1))
        .expect("spawn");
    exec.cancel(queued).expect("cancel");
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 0);
    assert_eq!(outcome.cancelled, 1, "the queued task was filtered");
    assert_eq!(queued_runs.get(), 0, "it never ran");
    assert_eq!(exec.join(queued), Some(crate::JoinOutcome::Cancelled));

    // Waiting: settled by the cancel on the owner thread.
    let waiting_runs = Rc::new(Cell::new(0));
    let waiting = exec
        .spawn(wait_until(9, Rc::clone(&waiting_runs)))
        .expect("spawn");
    exec.poll();
    assert_eq!(waiting_runs.get(), 1);
    exec.cancel(waiting).expect("cancel");
    assert_eq!(exec.join(waiting), Some(crate::JoinOutcome::Cancelled));

    // Sleeping: the deadline entry becomes stale; nothing ever runs it again.
    let sleeping_runs = Rc::new(Cell::new(0));
    let sleeping = exec
        .spawn({
            let running = Rc::clone(&sleeping_runs);
            move |_| {
                running.set(running.get() + 1);
                Step::WaitDeadline { deadline: 1_000 }
            }
        })
        .expect("spawn");
    exec.poll();
    exec.cancel(sleeping).expect("cancel");
    clock.advance(10_000);
    let outcome = exec.poll();
    assert_eq!(outcome.due, 0);
    assert_eq!(outcome.ran, 0);
    assert_eq!(sleeping_runs.get(), 1);
    assert_eq!(exec.join(sleeping), Some(crate::JoinOutcome::Cancelled));

    // Errors: a settled task and a stale id refuse.
    assert_eq!(exec.cancel(queued), Err(crate::CancelError::InvalidId));
    let stale = crate::TaskId::new(0, 0);
    assert_eq!(exec.cancel(stale), Err(crate::CancelError::InvalidId));
}

#[test]
fn cancel_via_the_waker_settles_a_waiting_task_at_the_next_poll() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec.spawn(wait_until(5, Rc::clone(&runs))).expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    exec.poll();
    assert_eq!(runs.get(), 1);
    waker.cancel();
    let outcome = exec.poll();
    assert_eq!(outcome.ran, 0);
    assert_eq!(
        outcome.cancelled, 0,
        "a waiting task is not a queued filter"
    );
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Cancelled));
    assert_eq!(runs.get(), 1, "it never ran again");
}

#[test]
fn bodies_are_dropped_exactly_once() {
    let clock = clock();
    let mut exec = executor(&clock);
    let drops = Rc::new(Cell::new(0));

    // Completed without a join: the captured state is dropped at completion.
    let done = exec
        .spawn({
            let guard = Guard(Rc::clone(&drops));
            move |_| {
                let _keep = &guard;
                Step::Done { result: 1 }
            }
        })
        .expect("spawn");
    exec.poll();
    assert_eq!(drops.get(), 1, "dropped at completion");
    exec.drop_interest(done);
    assert_eq!(drops.get(), 1, "nothing was dropped twice");

    // A task that stays live (waiting) keeps its captured state.
    exec.spawn({
        let guard = Guard(Rc::clone(&drops));
        move |_| {
            let _keep = &guard;
            Step::Wait
        }
    })
    .expect("spawn");

    // Cancelled while ready: dropped at the cancel, exactly once.
    let id = exec
        .spawn({
            let guard = Guard(Rc::clone(&drops));
            move |_| {
                let _keep = &guard;
                Step::Done { result: 2 }
            }
        })
        .expect("spawn");
    exec.cancel(id).expect("cancel");
    assert_eq!(drops.get(), 2);

    // The executor teardown drops whatever is still live.
    drop(exec);
    assert_eq!(drops.get(), 3);
}

#[test]
fn fire_and_forget_tasks_free_their_slots_at_completion() {
    let clock = clock();
    let mut exec = executor(&clock);
    let id = exec.spawn(|_| Step::Done { result: 3 }).expect("spawn");
    exec.drop_interest(id);
    exec.poll();
    // The slot is free again (the free list is a queue, so the next spawn takes
    // the earliest free slot, not necessarily this one) and can be respawned.
    let again = exec.spawn(|_| Step::Done { result: 4 }).expect("spawn");
    assert_ne!(again, id);
    exec.poll();
    assert_eq!(exec.join(again), Some(crate::JoinOutcome::Done(4)));
}

#[test]
fn yield_re_enqueues_at_the_tail() {
    let clock = clock();
    let mut exec = executor(&clock);
    let first_runs = Rc::new(Cell::new(0));
    let second_runs = Rc::new(Cell::new(0));
    let a = exec
        .spawn({
            let runs = Rc::clone(&first_runs);
            move |_| {
                runs.set(runs.get() + 1);
                if runs.get() >= 3 {
                    Step::Done { result: 1 }
                } else {
                    Step::Yield
                }
            }
        })
        .expect("spawn a");
    let b = exec
        .spawn(counter(Rc::clone(&second_runs), 2))
        .expect("spawn b");
    exec.poll();
    // FIFO with tail re-enqueue: a, b, a, a (a yields twice, then completes).
    assert_eq!(first_runs.get(), 3);
    assert_eq!(second_runs.get(), 1, "b ran between a's yields");
    exec.drop_interest(a);
    exec.drop_interest(b);
}

// --- Threaded ---

#[test]
fn wake_during_a_poll_is_seen_by_the_sleep_handshake() {
    // The waker fires while the body runs: the flag is set before the body
    // returns `Wait`, so the handshake re-enqueues the task instead of letting
    // it sleep, and the same poll runs it again.
    let clock = clock();
    let mut exec = executor(&clock);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (notified_tx, notified_rx) = std::sync::mpsc::channel();
    let runs = Arc::new(AtomicU64::new(0));
    let id = exec
        .spawn({
            let runs = Arc::clone(&runs);
            move |_| {
                let n = runs.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 {
                    // Hold this poll until the producer confirms its wake.
                    let _ = started_tx.send(());
                    notified_rx
                        .recv()
                        .expect("notification happened during this poll");
                    Step::Wait
                } else {
                    Step::Done { result: n }
                }
            }
        })
        .expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    let waker_thread = std::thread::spawn(move || {
        started_rx.recv().expect("the task started");
        waker.wake();
        notified_tx.send(()).expect("the task is still polling");
    });
    let outcome = exec.poll();
    waker_thread.join().expect("the waker thread");
    assert_eq!(runs.load(Ordering::Relaxed), 2, "the wake re-ran the task");
    assert_eq!(outcome.ran, 2, "the wake was seen inside the same poll");
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Done(2)));
}

#[test]
fn a_cancel_during_a_poll_settles_at_the_step_boundary() {
    let clock = clock();
    let mut exec = executor(&clock);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (notified_tx, notified_rx) = std::sync::mpsc::channel();
    let runs = Arc::new(AtomicU64::new(0));
    let id = exec
        .spawn({
            let runs = Arc::clone(&runs);
            move |_| {
                runs.fetch_add(1, Ordering::Relaxed);
                let _ = started_tx.send(());
                notified_rx
                    .recv()
                    .expect("notification happened during this poll");
                Step::Wait
            }
        })
        .expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    let waker_thread = std::thread::spawn(move || {
        started_rx.recv().expect("the task started");
        waker.cancel();
        notified_tx.send(()).expect("the task is still polling");
    });
    let outcome = exec.poll();
    waker_thread.join().expect("the waker thread");
    // The cancel landed during the first poll: the run returned Wait, and the
    // step-boundary drain settled the task cancelled.
    assert_eq!(outcome.ran, 1);
    assert_eq!(runs.load(Ordering::Relaxed), 1, "it never ran again");
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Cancelled));
}

#[test]
fn wake_storm_from_many_threads_loses_nothing() {
    let clock = clock();
    let mut exec = executor(&clock);
    let runs = Rc::new(Cell::new(0));
    let id = exec
        .spawn(wait_until(u64::MAX, Rc::clone(&runs)))
        .expect("spawn");
    let waker = exec.waker(id).expect("a live waker");
    exec.poll();
    assert_eq!(runs.get(), 1);
    // Four threads fire 2,500 wakes each while the owner keeps polling.
    std::thread::scope(|scope| {
        let waker = waker.clone();
        for _ in 0..4 {
            let waker = waker.clone();
            scope.spawn(move || {
                for _ in 0..2_500 {
                    waker.wake();
                }
            });
        }
        for _ in 0..200 {
            exec.poll();
        }
    });
    // Let any producer wedged between its claim and its store finish, then settle.
    for _ in 0..20 {
        exec.poll();
    }
    exec.cancel(id).expect("cancel");
    // The task ran at least the initial time; no assertion on the exact count
    // (wakes coalesce), but the task never lost a wake: after the cancellations
    // settle, nothing is pending and the worker may sleep.
    exec.poll();
    assert!(!exec.schedulable());
    assert_eq!(exec.join(id), Some(crate::JoinOutcome::Cancelled));
}

#[test]
fn many_sleeping_tasks_wake_each_exactly_once() {
    let clock = clock();
    let mut exec = executor(&clock);
    let completed = Arc::new(AtomicUsize::new(0));
    let mut wakers = Vec::new();
    for i in 0..256u64 {
        let completed = Arc::clone(&completed);
        let id = exec
            .spawn({
                let mut armed = false;
                move |_| {
                    if armed {
                        completed.fetch_add(1, Ordering::SeqCst);
                        Step::Done { result: i }
                    } else {
                        // The first run only arms the sleep; the completion is
                        // counted on the wake's run.
                        armed = true;
                        Step::WaitDeadline { deadline: u64::MAX }
                    }
                }
            })
            .expect("spawn");
        wakers.push(exec.waker(id).expect("a live waker"));
    }
    for _ in 0..8 {
        exec.poll();
    }
    assert_eq!(completed.load(Ordering::SeqCst), 0, "sleeping, not ready");
    for waker in &wakers {
        waker.wake();
    }
    for _ in 0..8 {
        exec.poll();
    }
    // Every task ran exactly twice: once to sleep, once after its wake. The
    // wake count is 256: no task was woken twice, none was skipped.
    assert_eq!(
        completed.load(Ordering::SeqCst),
        256,
        "each ran exactly once"
    );
    assert_eq!(exec.stats().notifications, 256);
    assert_eq!(exec.poll().ran, 0);
    for waker in &wakers {
        waker.wake(); // Wakes after completion settle nothing.
    }
    assert_eq!(exec.poll().ran, 0);
    let _ = wakers;
}
