//! Running a round's submitters together, timed from when they are all ready.

use std::fmt;
use std::io;
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// What [`run_together`] measured.
pub struct Timed<T> {
    /// When the clock started: every thread had started and was ready.
    #[cfg_attr(not(test), allow(dead_code))]
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
    pub started: u32,
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
    ready: u32,
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
    fn wait_ready(&self, threads: u32) {
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
    run_with(threads, work, |_| None)
}

/// [`run_together`], with `fail` able to refuse to start thread `i` (for the tests).
fn run_with<T, F>(
    threads: u32,
    work: F,
    fail: impl Fn(u32) -> Option<io::Error>,
) -> Result<Timed<T>, SpawnError>
where
    T: Send,
    F: Fn() -> T + Sync,
{
    let gate = StartGate::default();
    thread::scope(|scope| {
        let mut workers = Vec::new();
        for i in 0..threads {
            let spawned = match fail(i) {
                Some(err) => Err(err),
                None => thread::Builder::new().spawn_scoped(scope, || {
                    if !gate.arrive() {
                        return None;
                    }
                    let result = work();
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
    fn a_panicking_thread_is_reported() {
        let timed = run_together(2, || panic!("submitter")).unwrap();
        assert!(timed.results.iter().all(Option::<()>::is_none));
    }

    #[test]
    fn a_thread_that_cannot_start_releases_the_others() {
        let ran = AtomicU32::new(0);
        let err = run_with(
            8,
            || ran.fetch_add(1, Ordering::Relaxed),
            |i| (i == 5).then(|| io::Error::from(io::ErrorKind::OutOfMemory)),
        )
        .err()
        .expect("thread 5 does not start");
        // Returning at all means the five started threads left the gate; none ran the work.
        assert_eq!(err.started, 5);
        assert_eq!(err.source.kind(), io::ErrorKind::OutOfMemory);
        assert_eq!(ran.load(Ordering::Relaxed), 0);
        assert!(!err.to_string().is_empty());
    }
}
