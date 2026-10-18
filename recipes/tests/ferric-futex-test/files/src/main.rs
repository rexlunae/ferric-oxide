//! Checks futex wake-ups by exchanging turns between two threads through a condition
//! variable. A wake racing with entry into the kernel wait queue must either prevent the
//! wait or wake the queued thread. Bounded waits turn a lost wake into a test failure.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fmt;
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Exchanges in each run, with a yield after each worker reply to vary the race timing.
const ROUNDS: usize = 2_000;
/// A normal exchange takes microseconds; allow a busy runner several seconds.
const WAIT_LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum Failure {
    Poisoned,
    TimedOut { turn: usize },
    Stopped,
    WorkerPanicked,
    TimeoutDidNotExpire,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => f.write_str("handshake mutex poisoned"),
            Self::TimedOut { turn } => write!(f, "lost wake waiting for turn {turn}"),
            Self::Stopped => f.write_str("peer stopped after a failed exchange"),
            Self::WorkerPanicked => f.write_str("worker panicked"),
            Self::TimeoutDidNotExpire => f.write_str("initial timed wait never expired"),
        }
    }
}

impl std::error::Error for Failure {}

#[derive(Default)]
struct State {
    turn: usize,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

fn await_turn<'a>(
    shared: &'a Shared,
    mut state: MutexGuard<'a, State>,
    turn: usize,
) -> Result<MutexGuard<'a, State>, Failure> {
    while state.turn != turn && !state.stopped {
        let (next, timeout) = shared
            .changed
            .wait_timeout(state, WAIT_LIMIT)
            .map_err(|_| Failure::Poisoned)?;
        state = next;
        // Check even when the desired turn has arrived: waiting until the timeout
        // after the peer changed it still means its notification was lost.
        if timeout.timed_out() {
            return Err(Failure::TimedOut { turn });
        }
    }
    if state.stopped {
        Err(Failure::Stopped)
    } else {
        Ok(state)
    }
}

fn worker(shared: &Shared) -> Result<(), Failure> {
    for round in 1..=ROUNDS {
        let state = shared.state.lock().map_err(|_| Failure::Poisoned)?;
        let mut state = await_turn(shared, state, round * 2 - 1)?;
        state.turn = round * 2;
        shared.changed.notify_one();
        drop(state);
        std::thread::yield_now();
    }
    Ok(())
}

fn exchange(shared: &Shared) -> Result<(), Failure> {
    for round in 1..=ROUNDS {
        let state = shared.state.lock().map_err(|_| Failure::Poisoned)?;
        let mut state = await_turn(shared, state, (round - 1) * 2)?;
        state.turn = round * 2 - 1;
        shared.changed.notify_one();
        drop(await_turn(shared, state, round * 2)?);
    }
    Ok(())
}

fn expire_waiter(shared: &Arc<Shared>) -> Result<(), Failure> {
    let peer = Arc::clone(shared);
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + WAIT_LIMIT;
        let mut state = peer.state.lock().map_err(|_| Failure::Poisoned)?;
        while Instant::now() < deadline {
            let (next, timeout) = peer
                .changed
                .wait_timeout(state, Duration::from_millis(1))
                .map_err(|_| Failure::Poisoned)?;
            state = next;
            if timeout.timed_out() {
                return Ok(());
            }
        }
        Err(Failure::TimeoutDidNotExpire)
    });
    thread.join().map_err(|_| Failure::WorkerPanicked)?
}

fn run() -> Result<(), Failure> {
    let shared = Arc::new(Shared::default());
    // A timed-out thread must leave no waiter behind to consume a later notify_one
    // meant for one of the live handshake threads on the same condition variable.
    expire_waiter(&shared)?;
    let peer = Arc::clone(&shared);
    let thread = std::thread::spawn(move || worker(&peer));
    let result = exchange(&shared);
    {
        // Recover only to release the peer before joining it; poisoning or a panic
        // still fails through the original result or the join below.
        let mut state = match shared.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.stopped = true;
        shared.changed.notify_all();
    }
    let peer_result = thread.join().map_err(|_| Failure::WorkerPanicked)?;
    result?;
    peer_result
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("FERRIC_TEST_PASS futex-wake-race rounds={ROUNDS}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL futex-wake-race: {err}");
            ExitCode::FAILURE
        }
    }
}
