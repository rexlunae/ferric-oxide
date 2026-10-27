//! Measures the TSC frequency against the operating system's monotonic clock.
//!
//! The kernel's trace clock is the invariant TSC when the CPU has one, and the kernel does
//! not know that counter's rate, so a capture's TSC timestamps need the frequency from
//! somewhere else (`--tsc-mhz`). [`measure`] gets it on the machine being traced: it reads the
//! counter and [`Instant`] (the monotonic clock, which the kernel keeps from a source with a
//! known rate: the KVM clock, the HPET or the PIT) on both sides of a sleep, and divides the
//! cycles by the elapsed nanoseconds. The two clock reads bracket the two counter reads, so the
//! measured interval is never shorter than the cycles counted; over the default one-second
//! window the error from the reads themselves is a few parts per million.
//!
//! The counter is a parameter so the arithmetic can be tested without a TSC; the
//! `ferric-trace tsc-mhz` command passes the CPU's.

use std::time::{Duration, Instant};

use crate::perfetto::{TscMhz, TscMhzError};

/// The window [`measure`] sleeps for unless told otherwise.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(1);

/// Why a TSC frequency could not be measured.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TscRateError {
    /// The monotonic clock did not advance, so there is no interval to divide by.
    #[error("the monotonic clock did not advance during the measurement")]
    NoElapsedTime,
    /// The counter went backwards or did not move.
    #[error("the counter did not advance during the measurement ({start} to {end})")]
    CounterStalled {
        /// The first reading.
        start: u64,
        /// The second reading.
        end: u64,
    },
    /// The quotient is not a frequency [`TscMhz`] accepts.
    #[error("the measured rate is not a usable TSC frequency")]
    Rate(#[from] TscMhzError),
}

/// The frequency of a counter that advanced by `cycles` in `elapsed`.
///
/// # Errors
///
/// [`TscRateError::NoElapsedTime`] for a zero `elapsed`, and [`TscRateError::Rate`] when the
/// quotient is below [`TscMhz::MIN`].
pub fn rate(cycles: u64, elapsed: Duration) -> Result<TscMhz, TscRateError> {
    let ns = elapsed.as_nanos();
    if ns == 0 {
        return Err(TscRateError::NoElapsedTime);
    }
    // Cycles per nanosecond times 1000 is cycles per microsecond, i.e. MHz.
    Ok(TscMhz::new(cycles as f64 * 1000.0 / ns as f64)?)
}

/// Measures the frequency of `counter` by sleeping for `window` between two readings of it,
/// each inside a reading of the monotonic clock.
///
/// # Errors
///
/// [`TscRateError::CounterStalled`] when the counter did not advance, and the errors of
/// [`rate`].
///
/// # Examples
///
/// ```
/// use std::time::{Duration, Instant};
///
/// // A stand-in counter that runs at exactly 1000 MHz (one count per nanosecond).
/// let origin = Instant::now();
/// let counter = || origin.elapsed().as_nanos() as u64;
/// let mhz = ferric_trace_cli::tsc::measure(counter, Duration::from_millis(20)).unwrap();
/// assert!((mhz.get() - 1000.0).abs() < 50.0);
/// ```
pub fn measure(mut counter: impl FnMut() -> u64, window: Duration) -> Result<TscMhz, TscRateError> {
    let started = Instant::now();
    let start = counter();
    std::thread::sleep(window);
    let end = counter();
    let elapsed = started.elapsed();
    match end.checked_sub(start) {
        Some(cycles) if cycles > 0 => rate(cycles, elapsed),
        _ => Err(TscRateError::CounterStalled { start, end }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_is_cycles_per_microsecond() {
        let mhz = rate(4_292_000_000, Duration::from_secs(1)).unwrap();
        assert_eq!(mhz.get(), 4292.0);
        let mhz = rate(1_500, Duration::from_nanos(1_000)).unwrap();
        assert_eq!(mhz.get(), 1500.0);
    }

    #[test]
    fn rate_refuses_an_empty_interval() {
        assert!(matches!(
            rate(1, Duration::ZERO),
            Err(TscRateError::NoElapsedTime)
        ));
    }

    #[test]
    fn rate_refuses_a_counter_slower_than_the_minimum() {
        assert!(matches!(
            rate(1, Duration::from_secs(1)),
            Err(TscRateError::Rate(_))
        ));
    }

    #[test]
    fn measure_finds_a_known_rate() {
        // A counter at 3000 MHz, read off the same clock the measurement uses.
        let origin = Instant::now();
        let counter = || origin.elapsed().as_nanos() as u64 * 3;
        let mhz = measure(counter, Duration::from_millis(50)).unwrap();
        // The bracketing clock reads can only make the interval longer, so the result is at
        // most the true rate and close to it.
        assert!(mhz.get() <= 3000.0, "{}", mhz.get());
        assert!(mhz.get() > 2900.0, "{}", mhz.get());
    }

    #[test]
    fn measure_refuses_a_stalled_counter() {
        let err = measure(|| 7, Duration::from_millis(1)).unwrap_err();
        assert!(matches!(
            err,
            TscRateError::CounterStalled { start: 7, end: 7 }
        ));
        let mut readings = [10, 5].into_iter();
        let err = measure(|| readings.next().unwrap(), Duration::from_millis(1)).unwrap_err();
        assert!(matches!(
            err,
            TscRateError::CounterStalled { start: 10, end: 5 }
        ));
    }
}
