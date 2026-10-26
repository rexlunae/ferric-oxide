//! Measuring a counter's rate against a reference counter of known period.
//!
//! The kernel reads the counter it wants to calibrate (the TSC, or the local APIC timer's
//! current count) and a reference (the HPET main counter, whose period the HPET reports in
//! femtoseconds, or the PIT at 1.193182 MHz) at the start and end of a busy-wait window,
//! with interrupts off. [`Sample`] is one such pair of readings; [`estimate`] (reference of
//! known period) or [`estimate_against`] (reference of known rate) turns two of them into a
//! rate, and [`calibrate`] combines several windows, taking the median and
//! refusing a set whose estimates disagree by more than the caller's bound.
//!
//! References narrower than 64 bits wrap during a window (a 32-bit HPET at 14.3 MHz
//! every five minutes, the PIT's channel 0 every period it was programmed for);
//! [`WrappingCounter`] extends them, provided it is read at least once per wrap.

use core::{fmt, num::NonZeroU64};

use crate::rate::TicksPerSecond;

/// Femtoseconds in a second.
const FS_PER_SEC: u128 = 1_000_000_000_000_000;

/// The shortest window [`estimate`] accepts: one millisecond, in femtoseconds. Shorter
/// windows leave a one-tick reference reading error above one part in 1000 for a
/// 1.19 MHz PIT.
pub const MIN_WINDOW_FS: u128 = 1_000_000_000_000;

/// Readings of the counter being calibrated and of the reference, taken together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The counter being calibrated, in its own ticks. Must count up.
    pub counter: u64,
    /// The reference, in its ticks, already extended past any wrap.
    pub reference: u64,
}

/// Why a calibration failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CalibrationError {
    /// The reference did not advance across the window (it is stopped or not present).
    ReferenceStalled,
    /// The counter being calibrated did not advance across the window.
    CounterStalled,
    /// The reference period is zero.
    NoReferencePeriod,
    /// The window was shorter than [`MIN_WINDOW_FS`].
    WindowTooShort {
        /// The window's length in femtoseconds.
        window_fs: u128,
    },
    /// No windows were given.
    NoWindows,
    /// The windows' estimates disagree by more than the allowed spread.
    Inconsistent {
        /// `(max - min) / median` of the estimates, in parts per million.
        spread_ppm: u64,
    },
    /// The estimated rate does not fit in 64 bits, or is zero.
    OutOfRange,
}

impl fmt::Display for CalibrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReferenceStalled => f.write_str("the reference counter did not advance"),
            Self::CounterStalled => f.write_str("the calibrated counter did not advance"),
            Self::NoReferencePeriod => f.write_str("the reference counter has no period"),
            Self::WindowTooShort { window_fs } => {
                write!(f, "the window was {window_fs} fs, below the 1 ms minimum")
            }
            Self::NoWindows => f.write_str("no calibration windows"),
            Self::Inconsistent { spread_ppm } => {
                write!(f, "the windows disagree by {spread_ppm} ppm")
            }
            Self::OutOfRange => f.write_str("the estimated rate is out of range"),
        }
    }
}

impl core::error::Error for CalibrationError {}

/// The rate of the counter over the window from `start` to `end`, against a reference
/// whose tick lasts `reference_period_fs` femtoseconds.
///
/// # Errors
///
/// [`CalibrationError::NoReferencePeriod`] for a zero period,
/// [`CalibrationError::ReferenceStalled`] or [`CalibrationError::CounterStalled`] when a
/// counter did not move forward, [`CalibrationError::WindowTooShort`] for a window under
/// [`MIN_WINDOW_FS`], and [`CalibrationError::OutOfRange`] for a rate that is zero or
/// above `u64::MAX`.
pub fn estimate(
    start: Sample,
    end: Sample,
    reference_period_fs: u64,
) -> Result<TicksPerSecond, CalibrationError> {
    if reference_period_fs == 0 {
        return Err(CalibrationError::NoReferencePeriod);
    }
    let reference = end
        .reference
        .checked_sub(start.reference)
        .filter(|&d| d > 0)
        .ok_or(CalibrationError::ReferenceStalled)?;
    let counter = end
        .counter
        .checked_sub(start.counter)
        .filter(|&d| d > 0)
        .ok_or(CalibrationError::CounterStalled)?;
    let window_fs = u128::from(reference) * u128::from(reference_period_fs);
    if window_fs < MIN_WINDOW_FS {
        return Err(CalibrationError::WindowTooShort { window_fs });
    }
    let hz = u128::from(counter) * FS_PER_SEC / window_fs;
    u64::try_from(hz)
        .ok()
        .and_then(|hz| TicksPerSecond::new(hz).ok())
        .ok_or(CalibrationError::OutOfRange)
}

/// The rate of the counter over the window from `start` to `end`, against a reference
/// counter whose rate is already known (the calibrated TSC, when measuring a local APIC
/// timer).
///
/// # Errors
///
/// As [`estimate`], with the window's length taken from `reference`'s rate.
pub fn estimate_against(
    start: Sample,
    end: Sample,
    reference: TicksPerSecond,
) -> Result<TicksPerSecond, CalibrationError> {
    let reference_ticks = end
        .reference
        .checked_sub(start.reference)
        .filter(|&d| d > 0)
        .ok_or(CalibrationError::ReferenceStalled)?;
    let counter = end
        .counter
        .checked_sub(start.counter)
        .filter(|&d| d > 0)
        .ok_or(CalibrationError::CounterStalled)?;
    let window_fs = u128::from(reference_ticks) * FS_PER_SEC / u128::from(reference.get());
    if window_fs < MIN_WINDOW_FS {
        return Err(CalibrationError::WindowTooShort { window_fs });
    }
    let hz = u128::from(counter) * u128::from(reference.get()) / u128::from(reference_ticks);
    u64::try_from(hz)
        .ok()
        .and_then(|hz| TicksPerSecond::new(hz).ok())
        .ok_or(CalibrationError::OutOfRange)
}

/// The result of [`calibrate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibrated {
    /// The median of the windows' estimates.
    pub hz: TicksPerSecond,
    /// `(max - min) / median` of the estimates, in parts per million.
    pub spread_ppm: u64,
    /// How many windows were combined.
    pub windows: usize,
}

/// Combines per-window estimates into one rate: their median, provided they agree within
/// `max_spread_ppm` parts per million.
///
/// `estimates` is sorted in place. An even count takes the lower middle value, so the
/// result is always one of the measured rates.
///
/// # Errors
///
/// [`CalibrationError::NoWindows`] for an empty slice and
/// [`CalibrationError::Inconsistent`] when the spread is above `max_spread_ppm`.
pub fn calibrate(
    estimates: &mut [TicksPerSecond],
    max_spread_ppm: u64,
) -> Result<Calibrated, CalibrationError> {
    estimates.sort_unstable();
    let (Some(min), Some(max)) = (estimates.first(), estimates.last()) else {
        return Err(CalibrationError::NoWindows);
    };
    let median = estimates[(estimates.len() - 1) / 2];
    let spread = u128::from(max.get() - min.get()) * 1_000_000 / u128::from(median.get());
    let spread_ppm = u64::try_from(spread).unwrap_or(u64::MAX);
    if spread_ppm > max_spread_ppm {
        return Err(CalibrationError::Inconsistent { spread_ppm });
    }
    Ok(Calibrated {
        hz: median,
        spread_ppm,
        windows: estimates.len(),
    })
}

/// Extends a counter that counts up modulo `modulus` into a 64-bit count.
///
/// Each [`update`](Self::update) adds the distance from the previous reading, taken modulo
/// `modulus`, so the extended count is right as long as the counter is read at least once
/// per `modulus` ticks. A 32-bit HPET uses `1 << 32`; the PIT's elapsed-since-reload count
/// uses its channel 0 divisor; a 64-bit counter uses [`full_width`](Self::full_width).
#[derive(Clone, Copy, Debug)]
pub struct WrappingCounter {
    /// `None` for a counter that wraps at 2^64.
    modulus: Option<NonZeroU64>,
    last: u64,
    total: u64,
}

impl WrappingCounter {
    /// Starts extending a counter that wraps at `modulus`, whose first reading is `first`.
    /// Readings at or above `modulus` are taken modulo it.
    pub const fn new(modulus: NonZeroU64, first: u64) -> Self {
        Self {
            modulus: Some(modulus),
            last: first % modulus.get(),
            total: 0,
        }
    }

    /// Starts extending a 64-bit counter whose first reading is `first`.
    pub const fn full_width(first: u64) -> Self {
        Self {
            modulus: None,
            last: first,
            total: 0,
        }
    }

    /// Takes a new reading and returns the ticks counted since the first one.
    pub const fn update(&mut self, raw: u64) -> u64 {
        let (raw, delta) = match self.modulus {
            None => (raw, raw.wrapping_sub(self.last)),
            Some(modulus) => {
                let raw = raw % modulus.get();
                let delta = if raw >= self.last {
                    raw - self.last
                } else {
                    modulus.get() - self.last + raw
                };
                (raw, delta)
            }
        };
        self.last = raw;
        self.total = self.total.saturating_add(delta);
        self.total
    }

    /// The ticks counted since the first reading, as of the last [`update`](Self::update).
    pub const fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The HPET in QEMU and on most chipsets: 14.31818 MHz, a 69.84 ns tick.
    const HPET_FS: u64 = 69_841_279;

    fn hz(v: u64) -> TicksPerSecond {
        TicksPerSecond::new(v).unwrap()
    }

    fn sample(counter: u64, reference: u64) -> Sample {
        Sample { counter, reference }
    }

    #[test]
    fn a_ten_millisecond_window_against_the_hpet_gives_the_rate() {
        // 10 ms of HPET ticks and of a 3 GHz counter.
        let reference = 143_182;
        let counter = 30_000_000;
        let got = estimate(sample(5, 7), sample(5 + counter, 7 + reference), HPET_FS).unwrap();
        // One HPET tick in 143,182 bounds the error at about 7 ppm.
        assert!(got.get().abs_diff(3_000_000_000) < 30_000, "{got}");
    }

    #[test]
    fn the_pit_period_works_as_a_reference() {
        let pit_fs = 838_095_110;
        // 20 ms of PIT ticks and of a 2.4 GHz counter.
        let reference = 23_864;
        let got = estimate(sample(0, 0), sample(48_000_000, reference), pit_fs).unwrap();
        assert!(got.get().abs_diff(2_400_000_000) < 240_000, "{got}");
    }

    #[test]
    fn stalled_counters_and_bad_windows_are_errors() {
        assert_eq!(
            estimate(sample(0, 10), sample(100, 10), HPET_FS),
            Err(CalibrationError::ReferenceStalled)
        );
        assert_eq!(
            estimate(sample(0, 10), sample(100, 9), HPET_FS),
            Err(CalibrationError::ReferenceStalled)
        );
        assert_eq!(
            estimate(sample(100, 0), sample(100, 200_000), HPET_FS),
            Err(CalibrationError::CounterStalled)
        );
        assert_eq!(
            estimate(sample(0, 0), sample(100, 200_000), 0),
            Err(CalibrationError::NoReferencePeriod)
        );
        assert!(matches!(
            estimate(sample(0, 0), sample(1_000, 10), HPET_FS),
            Err(CalibrationError::WindowTooShort { .. })
        ));
    }

    #[test]
    fn a_counter_is_measured_against_a_calibrated_one() {
        let tsc = hz(3_000_000_000);
        // 2 ms of TSC and of a 1 GHz APIC timer.
        let got = estimate_against(sample(0, 100), sample(2_000_000, 6_000_100), tsc).unwrap();
        assert_eq!(got, hz(1_000_000_000));
        assert!(matches!(
            estimate_against(sample(0, 0), sample(10, 3_000), tsc),
            Err(CalibrationError::WindowTooShort { .. })
        ));
        assert_eq!(
            estimate_against(sample(5, 0), sample(5, 6_000_000), tsc),
            Err(CalibrationError::CounterStalled)
        );
    }

    #[test]
    fn calibrate_takes_the_median_and_reports_the_spread() {
        let mut estimates = [hz(3_000_000_300), hz(2_999_999_700), hz(3_000_000_000)];
        let got = calibrate(&mut estimates, 10).unwrap();
        assert_eq!(got.hz, hz(3_000_000_000));
        assert_eq!(got.windows, 3);
        assert_eq!(got.spread_ppm, 0);

        let mut even = [hz(10), hz(20), hz(11), hz(19)];
        let got = calibrate(&mut even, u64::MAX).unwrap();
        assert_eq!(got.hz, hz(11));
    }

    #[test]
    fn calibrate_refuses_windows_that_disagree() {
        // One window hit by an SMI or a vCPU preemption: 1% slow.
        let mut estimates = [hz(3_000_000_000), hz(2_970_000_000), hz(3_000_000_100)];
        assert_eq!(
            calibrate(&mut estimates, 500),
            Err(CalibrationError::Inconsistent { spread_ppm: 10_000 })
        );
        assert_eq!(calibrate(&mut [], 500), Err(CalibrationError::NoWindows));
    }

    #[test]
    fn a_wrapping_counter_extends_across_wraps() {
        let modulus = |m: u64| NonZeroU64::new(m).unwrap();
        let mut pit = WrappingCounter::new(modulus(4847), 4000);
        assert_eq!(pit.update(4500), 500);
        // Wrapped past the divisor: 347 to the end, then 100.
        assert_eq!(pit.update(100), 947);
        assert_eq!(pit.update(100), 947);
        assert_eq!(pit.update(4846), 947 + 4746);
        assert_eq!(pit.total(), 947 + 4746);

        let mut hpet32 = WrappingCounter::new(modulus(1 << 32), u64::from(u32::MAX) - 1);
        assert_eq!(hpet32.update(3), 5);

        let mut hpet64 = WrappingCounter::full_width(u64::MAX - 1);
        assert_eq!(hpet64.update(1), 3);
    }

    #[test]
    fn a_reading_at_the_modulus_counts_as_zero() {
        // The PIT's elapsed count reads the divisor itself when the counter is caught at 0.
        let mut pit = WrappingCounter::new(NonZeroU64::new(4847).unwrap(), 4847);
        assert_eq!(pit.update(10), 10);
        assert_eq!(pit.update(4847), 4847);
    }
}
