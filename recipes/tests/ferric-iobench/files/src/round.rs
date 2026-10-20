//! Running a round's submitters together, timed from when they are all ready.

use std::sync::Barrier;
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

/// Runs `work` on `threads` threads at once. The threads are started first and wait at a
/// barrier; once all of them are there the clock starts, and a second barrier lets them go,
/// so thread start-up is not timed, as fio starts its jobs before its clock. The clock stops
/// at the last thread's end.
pub fn run_together<T, F>(threads: u32, work: F) -> Timed<T>
where
    T: Send,
    F: Fn() -> T + Sync,
{
    let n = usize::try_from(threads).unwrap_or(usize::MAX);
    let ready = Barrier::new(n.saturating_add(1));
    let go = Barrier::new(n.saturating_add(1));
    thread::scope(|scope| {
        let workers: Vec<_> = (0..n)
            .map(|_| {
                scope.spawn(|| {
                    ready.wait();
                    go.wait();
                    let result = work();
                    (result, Instant::now())
                })
            })
            .collect();
        ready.wait();
        let start = Instant::now();
        go.wait();
        let mut end = start;
        let results = workers
            .into_iter()
            .map(|worker| {
                let (result, done) = worker.join().ok()?;
                end = end.max(done);
                Some(result)
            })
            .collect();
        Timed {
            start,
            elapsed: end - start,
            results,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_starts_once_every_thread_is_ready() {
        // Each thread notes when its work began and how long it took.
        let timed = run_together(16, || {
            let began = Instant::now();
            thread::sleep(Duration::from_millis(5));
            (began, began.elapsed())
        });
        assert_eq!(timed.results.len(), 16);
        for (began, took) in timed.results.into_iter().map(Option::unwrap) {
            // No work started before the clock, and the round covers all of it.
            assert!(began >= timed.start);
            assert!(timed.elapsed >= took);
        }
    }

    #[test]
    fn a_panicking_thread_is_reported() {
        let timed = run_together(2, || panic!("submitter"));
        assert!(timed.results.iter().all(Option::<()>::is_none));
    }
}
