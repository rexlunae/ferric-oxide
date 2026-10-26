//! Request-owned NVMe service and command timing (`drivers.nvmed_perf`).
//!
//! Buffered and DMA requests publish one paired sample, including partial errors.
//! Command time starts only once an SQ slot is available, and includes doorbell
//! batching, controller service and completion-to-task wakeup. It is not a pure
//! controller-time measurement. A separate reporting thread drains fixed windows
//! even when the scheme is idle, and once more when the queue policy changes, so a
//! window never mixes two policies; each line names the policy it measured.
//!
//! With per-CPU workers it also times the two hand-offs between the scheme's thread and a
//! worker: `handoff`, from queueing a request until its worker starts it (the worker's
//! wake-up, usually a cross-CPU one), and `return`, from a worker settling a job until the
//! scheme's thread resumes the job's future (the report pipe's wake-up).

use std::io;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ferric_blkqueue::timing::{Counts, Request, Window};

const REPORT_EVERY: Duration = Duration::from_secs(1);
static STATS: Mutex<Window> = Mutex::new(Window::new());
static NS_PER_TICK: Mutex<f64> = Mutex::new(1.0);
/// The hand-off phases, indexed by [`Hop`].
static HOPS: Mutex<[Counts; 2]> = Mutex::new([Counts::EMPTY; 2]);
/// The queue policy the current window measures.
static POLICY: Mutex<&'static str> = Mutex::new("default");

/// A hand-off between the scheme's thread and a per-CPU worker.
#[derive(Clone, Copy, Debug)]
pub enum Hop {
    /// From queueing a request for a worker until the worker starts it.
    Handoff,
    /// From a worker settling a job until the scheme's thread resumes its future.
    Return,
}

/// Records one hand-off that began at `start` ([`ticks`]) and ends now.
pub fn hop(hop: Hop, start: u64) {
    let elapsed = ticks().saturating_sub(start);
    HOPS.lock().unwrap_or_else(PoisonError::into_inner)[hop as usize].record(elapsed);
}

/// Ends the current window and starts one for `policy`, so each window measures one policy.
pub fn policy_changed(policy: &'static str) {
    report();
    *POLICY.lock().unwrap_or_else(PoisonError::into_inner) = policy;
}

/// Reads a CPU counter on x86-64; other architectures use the monotonic clock.
#[cfg(target_arch = "x86_64")]
pub fn ticks() -> u64 {
    // SAFETY: RDTSC and LFENCE are available on the supported x86-64 baseline.
    unsafe {
        core::arch::x86_64::_mm_lfence();
        let value = core::arch::x86_64::_rdtsc();
        core::arch::x86_64::_mm_lfence();
        value
    }
}

/// Reads the monotonic clock in nanoseconds on non-x86 architectures.
#[cfg(not(target_arch = "x86_64"))]
pub fn ticks() -> u64 {
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    u64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Calibrates tick conversion and starts an independent periodic reporter.
///
/// # Errors
/// Returns the OS error if the reporting thread cannot be created.
pub fn start_reporting() -> io::Result<()> {
    let start = ticks();
    let instant = Instant::now();
    std::thread::sleep(Duration::from_millis(100));
    let delta = ticks().saturating_sub(start);
    if delta != 0 {
        *NS_PER_TICK.lock().unwrap_or_else(PoisonError::into_inner) =
            instant.elapsed().as_nanos() as f64 / delta as f64;
    }
    std::thread::Builder::new()
        .name("nvmed-perf".into())
        .spawn(|| loop {
            std::thread::sleep(REPORT_EVERY);
            report();
        })?;
    Ok(())
}

/// A measurement that belongs to this request's future, not its execution thread.
/// Dropping it on return, error or cancellation publishes its paired phases.
pub struct RequestTiming {
    request: Option<Request>,
    write: bool,
}

impl RequestTiming {
    /// Starts one read or write request.
    pub fn new(write: bool) -> Self {
        Self {
            request: Some(Request::new(ticks())),
            write,
        }
    }

    /// Adds one accepted command's elapsed time to this request.
    pub fn command(&mut self, start: u64) {
        if let Some(request) = &mut self.request {
            request.command(start, ticks());
        }
    }
}

impl Drop for RequestTiming {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            STATS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(self.write, request.finish(ticks()));
        }
    }
}

fn report() {
    // Release the recording lock before converting or logging the snapshot.
    let snapshot = STATS.lock().unwrap_or_else(PoisonError::into_inner).take();
    let hops = core::mem::replace(
        &mut *HOPS.lock().unwrap_or_else(PoisonError::into_inner),
        [Counts::EMPTY; 2],
    );
    let policy = *POLICY.lock().unwrap_or_else(PoisonError::into_inner);
    let rate = *NS_PER_TICK.lock().unwrap_or_else(PoisonError::into_inner);
    let ns = |ticks: u64| (ticks as f64 * rate).round() as u64;
    for (counts, phase) in snapshot.into_iter().chain(hops).zip([
        "read.service",
        "read.device",
        "write.service",
        "write.device",
        "handoff",
        "return",
    ]) {
        if counts.count == 0 {
            continue;
        }
        let mean = ns(counts.sum / counts.count);
        let min = ns(counts.min);
        let max = ns(counts.max);
        log::info!(
            "nvmed: perf: policy={policy} {phase} n={} mean={}.{:03} min={}.{:03} max={}.{:03} us",
            counts.count,
            mean / 1000,
            mean % 1000,
            min / 1000,
            min % 1000,
            max / 1000,
            max % 1000,
        );
    }
}
