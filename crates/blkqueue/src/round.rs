//! Run benchmark submitters together, timing only their work after all are ready.
//!
//! Shared by the disk and registered-buffer benchmarks. Caller-prepared state
//! keeps allocation outside the clock, and each worker records completion before
//! dropping that state. Thread creation, joining and teardown are not timed.
//!
//! # Examples
//!
//! ```
//! use ferric_blkqueue::round::run_each;
//!
//! let timed = run_each(vec![vec![0_u8; 4], vec![0_u8; 4]], |buffer| {
//!     buffer.fill(1);
//!     buffer.len()
//! }).unwrap();
//! assert_eq!(timed.results, vec![Some(4), Some(4)]);
//! ```

use std::fmt;
use std::io;
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// What [`run_together`] measured.
pub struct Timed<T> {
    /// When the clock started: every thread had started and was ready.
    pub start: Instant,
    /// From `start` to the last thread finishing its work.
    pub elapsed: Duration,
    /// Each thread's result, `None` for one that panicked.
    pub results: Vec<Option<T>>,
}

/// [`run_together`] could not start every thread; the ones it started ran no work.
#[derive(Debug)]
pub struct SpawnError {
    /// The threads that had started.
    pub started: usize,
    /// Why the next one did not.
    pub source: io::Error,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not start submitter thread {} of the round",
            self.started + 1
        )
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Where the round's threads are: how many are waiting to start, and whether to go.
#[derive(Default)]
struct Gate {
    ready: usize,
    open: Option<bool>,
}

/// The start gate the threads wait at: opened to run the work, or to give up.
#[derive(Default)]
struct StartGate {
    state: Mutex<Gate>,
    changed: Condvar,
}

impl StartGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, Gate> {
        // The gate's state stays consistent even if a thread panicked holding it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A thread is ready; waits for the gate to open and returns whether to run.
    fn arrive(&self) -> bool {
        let mut gate = self.lock();
        gate.ready += 1;
        self.changed.notify_all();
        loop {
            if let Some(go) = gate.open {
                return go;
            }
            gate = self
                .changed
                .wait(gate)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Waits until `threads` threads are ready.
    fn wait_ready(&self, threads: usize) {
        let mut gate = self.lock();
        while gate.ready < threads {
            gate = self
                .changed
                .wait(gate)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Lets the threads go, to run their work (`go`) or to give up.
    fn open(&self, go: bool) {
        self.lock().open = Some(go);
        self.changed.notify_all();
    }
}

/// Release submitters if the parent unwinds before opening the gate.
struct AbortOnDrop<'a>(&'a StartGate);

impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        if state.open.is_none() {
            state.open = Some(false);
            self.0.changed.notify_all();
        }
    }
}

/// Runs `work` on `threads` threads at once. The threads are started first and wait at a
/// start gate; once all of them are there the clock starts and the gate opens, so thread
/// start-up is not timed, as fio starts its jobs before its clock. The clock stops at the last
/// thread's end.
///
/// # Errors
///
/// [`SpawnError`] when a thread cannot be started: the gate then opens to give up, and the
/// threads already started return without running `work`.
pub fn run_together<T, F>(threads: u32, work: F) -> Result<Timed<T>, SpawnError>
where
    T: Send,
    F: Fn() -> T + Sync,
{
    run_with(
        (0..threads).map(|_| ()).collect(),
        |_| work(),
        || {},
        |_| None,
    )
}

/// Run one submitter per caller-prepared state item, timing its work after every
/// thread is ready. The clock stops before state destruction and thread joining.
/// Results retain the order of `items`; a panicking worker has a `None` result.
///
/// # Errors
/// Returns [`SpawnError`] if a thread cannot start; no submitter runs then.
pub fn run_each<P, T, F>(items: Vec<P>, work: F) -> Result<Timed<T>, SpawnError>
where
    P: Send,
    T: Send,
    F: Fn(&mut P) -> T + Sync,
{
    run_with(items, work, || {}, |_| None)
}

/// [`run_together`], calling `at_start` once every thread is ready, just before the clock
/// starts: the runner snapshots the kernel's TLB counters there, so thread start-up is not
/// counted either.
///
/// # Errors
///
/// As [`run_together`]; `at_start` is not called then.
///
/// # Panics
/// Propagates a panic from `at_start` after releasing the waiting threads.
pub fn run_together_at<T, F>(
    threads: u32,
    work: F,
    at_start: impl FnOnce(),
) -> Result<Timed<T>, SpawnError>
where
    T: Send,
    F: Fn() -> T + Sync,
{
    run_with(
        (0..threads).map(|_| ()).collect(),
        |_| work(),
        at_start,
        |_| None,
    )
}

/// [`run_together_at`], with `fail` able to refuse to start thread `i` (for the tests).
fn run_with<P, T, F>(
    items: Vec<P>,
    work: F,
    at_start: impl FnOnce(),
    fail: impl Fn(usize) -> Option<io::Error>,
) -> Result<Timed<T>, SpawnError>
where
    P: Send,
    T: Send,
    F: Fn(&mut P) -> T + Sync,
{
    let gate = StartGate::default();
    let threads = items.len();
    thread::scope(|scope| {
        let _abort = AbortOnDrop(&gate);
        let mut workers = Vec::new();
        for (i, mut item) in items.into_iter().enumerate() {
            let gate = &gate;
            let work = &work;
            let spawned = match fail(i) {
                Some(err) => Err(err),
                None => thread::Builder::new().spawn_scoped(scope, move || {
                    if !gate.arrive() {
                        return None;
                    }
                    let result = work(&mut item);
                    Some((result, Instant::now()))
                }),
            };
            match spawned {
                Ok(worker) => workers.push(worker),
                Err(source) => {
                    gate.open(false);
                    // The started threads see the gate open to give up and end; the scope
                    // joins them.
                    return Err(SpawnError { started: i, source });
                }
            }
        }
        gate.wait_ready(threads);
        at_start();
        let start = Instant::now();
        gate.open(true);
        let mut end = start;
        let results = workers
            .into_iter()
            .map(|worker| {
                let (result, done) = worker.join().ok()??;
                end = end.max(done);
                Some(result)
            })
            .collect();
        Ok(Timed {
            start,
            elapsed: end - start,
            results,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn the_clock_starts_once_every_thread_is_ready() {
        // Each thread notes when its work began and how long it took.
        let timed = run_together(16, || {
            let began = Instant::now();
            thread::sleep(Duration::from_millis(5));
            (began, began.elapsed())
        })
        .unwrap();
        assert_eq!(timed.results.len(), 16);
        for (began, took) in timed.results.into_iter().map(Option::unwrap) {
            // No work started before the clock, and the round covers all of it.
            assert!(began >= timed.start);
            assert!(timed.elapsed >= took);
        }
    }

    #[test]
    fn the_clock_excludes_thread_startup() {
        let last_spawn = std::cell::Cell::new(None);
        let timed = run_with(
            vec![(); 4],
            |_| (),
            || {},
            |_| {
                last_spawn.set(Some(Instant::now()));
                None
            },
        )
        .unwrap();
        assert!(timed.start >= last_spawn.get().unwrap());
    }

    #[test]
    fn a_panicking_thread_is_reported() {
        let timed = run_together(2, || panic!("submitter")).unwrap();
        assert!(timed.results.iter().all(Option::<()>::is_none));
    }

    #[test]
    fn a_thread_that_cannot_start_releases_the_others() {
        let ran = AtomicU32::new(0);
        let started = AtomicU32::new(0);
        let err = run_with(
            vec![(); 8],
            |_| ran.fetch_add(1, Ordering::Relaxed),
            || {
                started.fetch_add(1, Ordering::Relaxed);
            },
            |i| (i == 5).then(|| io::Error::from(io::ErrorKind::OutOfMemory)),
        )
        .err()
        .expect("thread 5 does not start");
        // Returning at all means the five started threads left the gate; none ran the work.
        assert_eq!(err.started, 5);
        assert_eq!(err.source.kind(), io::ErrorKind::OutOfMemory);
        assert_eq!(ran.load(Ordering::Relaxed), 0);
        assert_eq!(started.load(Ordering::Relaxed), 0);
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn the_start_hook_runs_once_before_any_work() {
        let marked = AtomicU32::new(0);
        let timed = run_together_at(
            4,
            || marked.load(Ordering::Relaxed),
            || {
                marked.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();
        // Every thread saw the hook's mark: it ran before the gate opened, exactly once.
        assert!(timed.results.into_iter().all(|seen| seen == Some(1)));
    }

    #[test]
    fn prepared_states_keep_their_order_and_run_after_the_start() {
        let items: Vec<_> = (0..4).map(|id| (id, vec![id; 32])).collect();
        let timed = run_each(items, |(id, buffer)| {
            buffer.fill(*id + 1);
            (Instant::now(), *id, buffer.clone())
        })
        .unwrap();
        for (id, result) in timed.results.into_iter().enumerate() {
            let (began, seen, buffer) = result.unwrap();
            assert!(began >= timed.start);
            assert_eq!(seen, id);
            assert_eq!(buffer, vec![id + 1; 32]);
        }
    }

    #[test]
    fn state_destruction_and_joining_are_outside_the_clock() {
        struct State(std::sync::Arc<Mutex<Option<Instant>>>);
        impl Drop for State {
            fn drop(&mut self) {
                thread::sleep(Duration::from_millis(5));
                *self.0.lock().unwrap() = Some(Instant::now());
            }
        }
        let dropped = std::sync::Arc::new(Mutex::new(None));
        let timed = run_each(vec![State(dropped.clone())], |_| 42).unwrap();
        assert_eq!(timed.results, vec![Some(42)]);
        assert!(timed.start + timed.elapsed < dropped.lock().unwrap().unwrap());
    }

    #[test]
    fn a_panicking_start_hook_releases_waiting_submitters() {
        let ran = AtomicU32::new(0);
        let result = std::panic::catch_unwind(|| {
            run_together_at(
                4,
                || ran.fetch_add(1, Ordering::Relaxed),
                || panic!("start hook"),
            )
        });
        assert!(result.is_err());
        assert_eq!(ran.load(Ordering::Relaxed), 0);
    }
}
