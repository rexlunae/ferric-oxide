//! Timing a benchmark: warm-up, an iteration limit and a time budget, and per-phase latency
//! histograms.
//!
//! [`measure`] runs an operation first [`Plan::warmup`] times untimed, then times it until it
//! has [`Plan::iters`] samples, or until [`Plan::budget`] has passed since the first timed
//! iteration and it has at least [`Plan::min_iters`]. The budget keeps a run that is far slower
//! than expected (a wake-up that waits for the scheduler tick every time, say) to seconds.
//!
//! An iteration can have several phases (map, touch and unmap a file): the operation calls
//! [`Laps::lap`] at the end of each, and each phase gets its own [`Histogram`]. Work it does
//! not want timed (making the next file name) goes before a [`Laps::restart`].
//!
//! Time comes from a [`Clock`]; the benchmarks use [`Monotonic`], and the tests a scripted
//! one, so everything here runs on the host.

use std::time::{Duration, Instant};

use ferric_iobench::latency::Histogram;

/// How a benchmark runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Untimed iterations before the first timed one.
    pub warmup: u32,
    /// The most timed iterations.
    pub iters: u32,
    /// The fewest timed iterations, budget or not.
    pub min_iters: u32,
    /// How long timed iterations may start for, from the first one.
    pub budget: Duration,
}

impl Plan {
    /// The plan for `--quick`: at most one warm-up, three iterations and a tenth of a second,
    /// to check that a benchmark works rather than to measure it.
    pub fn quick(self) -> Self {
        Self {
            warmup: self.warmup.min(1),
            iters: self.iters.min(3),
            min_iters: self.min_iters.min(1),
            budget: self.budget.min(Duration::from_millis(100)),
        }
    }
}

/// A source of monotonic time in nanoseconds.
pub trait Clock {
    /// Nanoseconds since some fixed point; never decreases.
    fn now_ns(&mut self) -> u64;
}

/// The system's monotonic clock ([`Instant`]).
#[derive(Clone, Copy, Debug)]
pub struct Monotonic(Instant);

impl Monotonic {
    /// A clock counting from now.
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Clock for Monotonic {
    fn now_ns(&mut self) -> u64 {
        u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// The phase times of one iteration: `N` phases, each ended by [`Laps::lap`].
pub struct Laps<'c, C, const N: usize> {
    clock: &'c mut C,
    /// When the current phase started.
    last: u64,
    laps: [u64; N],
    taken: usize,
}

impl<'c, C: Clock, const N: usize> Laps<'c, C, N> {
    fn new(clock: &'c mut C) -> Self {
        let last = clock.now_ns();
        Self {
            clock,
            last,
            laps: [0; N],
            taken: 0,
        }
    }

    /// Ends the current phase and starts the next one.
    pub fn lap(&mut self) {
        let now = self.clock.now_ns();
        if let Some(slot) = self.laps.get_mut(self.taken) {
            *slot = now.saturating_sub(self.last);
        }
        self.taken += 1;
        self.last = now;
    }

    /// Starts the current phase again from now, leaving out what ran since it started.
    pub fn restart(&mut self) {
        self.last = self.clock.now_ns();
    }
}

/// What [`measure`] recorded.
#[derive(Clone, Debug)]
pub struct Measured<const N: usize> {
    /// Each phase's times, in nanoseconds; their count is the timed iterations.
    pub phases: [Histogram; N],
}

/// Runs `op` as `plan` says and returns the times of its `N` phases.
///
/// `op` must call [`Laps::lap`] once at the end of each of its `N` phases. A phase it does
/// not end records 0, and laps past the `N`th are ignored.
///
/// # Errors
///
/// The first error `op` returns; nothing runs after it.
pub fn measure<C: Clock, E, const N: usize>(
    plan: &Plan,
    clock: &mut C,
    mut op: impl FnMut(&mut Laps<'_, C, N>) -> Result<(), E>,
) -> Result<Measured<N>, E> {
    for _ in 0..plan.warmup {
        op(&mut Laps::new(clock))?;
    }
    let budget = u64::try_from(plan.budget.as_nanos()).unwrap_or(u64::MAX);
    let start = clock.now_ns();
    let mut phases = core::array::from_fn(|_| Histogram::new());
    let mut iters = 0;
    while iters < plan.iters {
        let mut laps = Laps::new(clock);
        if iters >= plan.min_iters && laps.last.saturating_sub(start) >= budget {
            break;
        }
        op(&mut laps)?;
        debug_assert_eq!(laps.taken, N, "an iteration ends each of its phases once");
        for (histogram, &ns) in phases.iter_mut().zip(&laps.laps) {
            histogram.record(ns);
        }
        iters += 1;
    }
    Ok(Measured { phases })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that moves forward by the next step of a script on every read, and by the
    /// last step once the script runs out.
    struct Scripted {
        now: u64,
        steps: Vec<u64>,
        reads: usize,
    }

    impl Scripted {
        fn new(steps: &[u64]) -> Self {
            Self {
                now: 0,
                steps: steps.to_vec(),
                reads: 0,
            }
        }
    }

    impl Clock for Scripted {
        fn now_ns(&mut self) -> u64 {
            let step = self
                .steps
                .get(self.reads)
                .or(self.steps.last())
                .copied()
                .unwrap_or(0);
            self.reads += 1;
            self.now += step;
            self.now
        }
    }

    fn plan(warmup: u32, iters: u32, min_iters: u32, budget_ns: u64) -> Plan {
        Plan {
            warmup,
            iters,
            min_iters,
            budget: Duration::from_nanos(budget_ns),
        }
    }

    #[test]
    fn warmup_runs_untimed_and_iterations_stop_at_the_limit() {
        let mut clock = Scripted::new(&[10]);
        let mut calls = 0;
        let measured = measure(&plan(2, 5, 1, u64::MAX), &mut clock, |laps| {
            calls += 1;
            laps.lap();
            Ok::<(), ()>(())
        })
        .expect("no error");
        assert_eq!(calls, 7);
        let [phase] = &measured.phases;
        assert_eq!(phase.count(), 5);
        // Every lap is one clock step after its iteration's start.
        assert_eq!((phase.min(), phase.max(), phase.mean()), (10, 10, 10));
    }

    #[test]
    fn the_budget_stops_iterations_after_the_minimum() {
        // Each read moves the clock 100 ns: an iteration's start and its lap.
        let mut clock = Scripted::new(&[100]);
        let measured = measure::<_, _, 1>(&plan(0, 1000, 2, 450), &mut clock, |laps| {
            laps.lap();
            Ok::<(), ()>(())
        })
        .expect("no error");
        // The budget counts from 100 ns. Iterations start at 200, 400 and 600 ns; the third is
        // 500 ns in, past the 450 ns budget, so two run.
        assert_eq!(measured.phases[0].count(), 2);

        // A budget already spent still runs the minimum.
        let mut clock = Scripted::new(&[100]);
        let measured = measure::<_, _, 1>(&plan(0, 1000, 4, 0), &mut clock, |laps| {
            laps.lap();
            Ok::<(), ()>(())
        })
        .expect("no error");
        assert_eq!(measured.phases[0].count(), 4);
    }

    #[test]
    fn phases_get_their_own_times_and_restart_leaves_setup_out() {
        // Reads: start (+1), restart (+50), lap 1 (+7), lap 2 (+300).
        let mut clock = Scripted::new(&[1, 1, 50, 7, 300]);
        let measured = measure(&plan(0, 1, 1, u64::MAX), &mut clock, |laps| {
            laps.restart();
            laps.lap();
            laps.lap();
            Ok::<(), ()>(())
        })
        .expect("no error");
        let [first, second] = &measured.phases;
        assert_eq!((first.count(), first.max()), (1, 7));
        assert_eq!((second.count(), second.max()), (1, 300));
    }

    #[test]
    fn an_error_stops_the_run() {
        let mut clock = Scripted::new(&[1]);
        let mut calls = 0;
        let result = measure::<_, _, 1>(&plan(1, 10, 1, u64::MAX), &mut clock, |laps| {
            calls += 1;
            if calls == 3 {
                return Err("third call");
            }
            laps.lap();
            Ok(())
        });
        assert_eq!(result.err(), Some("third call"));
        assert_eq!(calls, 3);
    }

    #[test]
    fn quick_plans_are_short() {
        let full = plan(100, 10_000, 50, 2_000_000_000);
        assert_eq!(full.quick(), plan(1, 3, 1, 100_000_000));
        let small = plan(0, 2, 0, 1_000);
        assert_eq!(small.quick(), small);
    }

    #[test]
    fn the_monotonic_clock_does_not_go_back() {
        let mut clock = Monotonic::new();
        let first = clock.now_ns();
        assert!(clock.now_ns() >= first);
    }
}
