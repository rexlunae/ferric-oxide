//! Request-phase timing for nvmed (the `perf` Cargo feature).
//!
//! With the `perf` feature, the driver times each I/O request's phases with the CPU cycle
//! counter (`rdtsc` on x86-64; an `Instant`-based nanosecond counter elsewhere), so the
//! instrumented driver still makes the same system calls as the unmodified one:
//!
//! - **service**: a request's whole time in `namespace_read`/`namespace_write`, from entry
//!   to return, on whatever thread serves it (the scheme's main thread, or a per-CPU worker
//!   answering a job or a direct request);
//! - **device**: the sum over the request's commands of the time between the submission
//!   queue's doorbell and the completion being observed, which includes the controller's
//!   service time and the interrupt-to-worker wake-up latency.
//!
//! The difference (service minus device) is the driver's own work: queue-pair and lock
//! waits, bounce-buffer copies and executor wake-up latency not charged to the device.
//! Which thread served what is left to the kernel trace (`sched.switch`, `irq.deliver`):
//! this module keeps one global counter set per phase.
//!
//! The counters reset at each report. The scheme loop reports at most once every
//! [`REPORT_EVERY`] as `nvmed: perf: ...` lines, one per phase with the count, mean, minimum
//! and maximum in microseconds. The ticks are converted with a one-time calibration against
//! the monotonic clock ([`calibrate`]); they are only for comparing phases in one boot, not a
//! documented time source.

use std::cell::RefCell;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Report at most this often, and the first report no earlier than one interval.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// The phases a request is split into.
#[derive(Clone, Copy)]
pub enum Phase {
    /// `namespace_read` entry to return.
    ReadService,
    /// The summed submission-to-completion times of a read's commands.
    ReadDevice,
    /// `namespace_write` entry to return.
    WriteService,
    /// The summed submission-to-completion times of a write's commands.
    WriteDevice,
}

impl Phase {
    /// The index of this phase in the accumulator arrays.
    const fn index(self) -> usize {
        match self {
            Self::ReadService => 0,
            Self::ReadDevice => 1,
            Self::WriteService => 2,
            Self::WriteDevice => 3,
        }
    }
}

/// One phase's accumulator.
#[derive(Clone, Copy)]
struct Counts {
    count: u64,
    sum_ticks: u64,
    min_ticks: u64,
    max_ticks: u64,
}

impl Counts {
    fn record(&mut self, dt_ticks: u64) {
        self.count += 1;
        self.sum_ticks += dt_ticks;
        self.min_ticks = self.min_ticks.min(dt_ticks);
        self.max_ticks = self.max_ticks.max(dt_ticks);
    }
}

/// The accumulators, reset when the report takes them.
static STATS: Mutex<Option<[Counts; 4]>> = Mutex::new(None);

/// The last report, in ticks, which also gates how often the report runs.
static LAST_REPORT: Mutex<Option<u64>> = Mutex::new(None);

/// Ticks per nanosecond, set by [`calibrate`] before the first [`record`].
static NS_PER_TICK: Mutex<f64> = Mutex::new(0.0);

thread_local! {
    /// This thread's ticks at the start of its current request, if one is being timed.
    static REQUEST_START: RefCell<Option<u64>> = const { RefCell::new(None) };
}

/// Reads the counter: `rdtsc` on x86-64, a nanosecond counter elsewhere.
#[cfg(target_arch = "x86_64")]
pub fn ticks() -> u64 {
    // SAFETY: rdtsc has no memory side effects and is available on every x86-64 CPU the
    // kernel or QEMU exposes.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Reads the counter: `rdtsc` on x86-64, a nanosecond counter elsewhere.
#[cfg(not(target_arch = "x86_64"))]
pub fn ticks() -> u64 {
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    u64_from_u128(ORIGIN.get_or_init(Instant::now).elapsed().as_nanos())
}

/// Measures the counter rate once: ticks per nanosecond, from two samples around a short
/// sleep. Every later [`ns`] uses this rate. Called once at driver start.
pub fn calibrate() {
    let (a, instant) = { let a = ticks(); (a, Instant::now()) };
    std::thread::sleep(Duration::from_millis(100));
    let (b, elapsed) = { let b = ticks(); (b, instant.elapsed()) };
    let delta = b.saturating_sub(a);
    let elapsed_ns = u64_from_u128(elapsed.as_nanos());
    let mut rate = NS_PER_TICK.lock().unwrap_or_else(PoisonError::into_inner);
    *rate = if delta == 0 || elapsed_ns == 0 {
        1.0
    } else {
        elapsed_ns as f64 / delta as f64
    };
    // Gives the driver its phase counter set.
    *STATS.lock().unwrap_or_else(PoisonError::into_inner) = Some(Counts::array());
}

impl Counts {
    /// An all-zero counter set.
    const fn array() -> [Counts; 4] {
        [Counts { count: 0, sum_ticks: 0, min_ticks: 0, max_ticks: 0 }; 4]
    }
}

/// `n` as `u64`, saturated, for the small durations this module measures.
fn u64_from_u128(n: u128) -> u64 {
    if n > u64::MAX as u128 {
        u64::MAX
    } else {
        n as u64
    }
}

/// Converts a tick delta to nanoseconds with the calibrated rate (1:1 before [`calibrate`]).
fn ns(dt_ticks: u64) -> u64 {
    let rate = NS_PER_TICK.lock().unwrap_or_else(PoisonError::into_inner);
    (dt_ticks as f64 * *rate).round() as u64
}

/// Starts timing the current thread's request. Nested starts keep the outer one: the inner
/// path is part of the outer service time.
pub fn start_request() {
    if !STATS.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
        return;
    }
    REQUEST_START.with(|start| {
        let mut start = start.borrow_mut();
        if start.is_none() {
            *start = Some(ticks());
        }
    });
}

/// Ends the current thread's request, recording `phase`. Does nothing when no request
/// started here, or when the feature is not compiled in.
pub fn end_request(phase: Phase) {
    let started = REQUEST_START.with(|start| start.borrow_mut().take());
    let Some(start) = started else {
        return;
    };
    record(phase, ticks().saturating_sub(start));
}

/// Records a completed command's device time (submission to completion) in `phase`'s bucket.
pub fn record_device(phase: Phase, dt_ticks: u64) {
    record(phase, dt_ticks);
}

/// Adds `dt_ticks` to `phase`'s counter set, if any exists.
fn record(phase: Phase, dt_ticks: u64) {
    let mut stats = STATS.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(stats) = stats.as_mut() else {
        return;
    };
    stats[phase.index()].record(dt_ticks);
}

/// The report labels, by [`Phase::index`].
const PHASE_LABELS: [&'static str; 4] = [
    "read.service",
    "read.device",
    "write.service",
    "write.device",
];

/// Logs the summary when the last one is at least [`REPORT_EVERY`] old, then resets the
/// counters. Called from the scheme loop, so the report cadence follows the driver's wake-ups.
pub fn maybe_report() {
    let mut report_at = LAST_REPORT.lock().unwrap_or_else(PoisonError::into_inner);
    let now = ticks();
    let interval_ns = u64_from_u128(REPORT_EVERY.as_nanos());
    let due = match *report_at {
        None => true,
        Some(last) => ns(now.saturating_sub(last)) >= interval_ns,
    };
    if !due {
        return;
    }
    *report_at = Some(now);
    let stats = STATS.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(stats) = stats.as_ref() else {
        return;
    };
    for (phase, counts) in stats.iter().zip(PHASE_LABELS) {
        if phase.count == 0 {
            continue;
        }
        let mean = phase.sum_ticks / phase.count;
        log::info!(
            "nvmed: perf: {} n={} mean={}.{:03} min={}.{:03} max={}.{:03} us",
            counts,
            phase.count,
            ns(mean) / 1000,
            ns(mean) % 1000,
            ns(phase.min_ticks) / 1000,
            ns(phase.min_ticks) % 1000,
            ns(phase.max_ticks) / 1000,
            ns(phase.max_ticks) % 1000,
        );
    }
}