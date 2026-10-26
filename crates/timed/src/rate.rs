//! Converting between monotonic nanoseconds and a clock source's ticks.
//!
//! A clock source (the invariant TSC, an HPET counter, a PIT countdown) ticks
//! at a rate the architecture measures and validates ([#186]); timers are
//! programmed in the source's ticks and deadlines are kept in nanoseconds, so
//! the two conversions below are the only places rates appear. They use
//! 128-bit intermediates, so they cannot overflow or wrap for any rate a
//! source can have (a counter of ~5 GHz ticks converts from a 64-bit nanosecond
//! deadline into ~2^64 × 5 ticks: the 128-bit product holds it, and a >64-bit
//! result saturates to "never").
//!
//! The rate is `ticks_per_second`, measured (never assumed) by the
//! architecture code, which also logs the calibration and its error.

/// A clock source's tick rate, in ticks per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TicksPerSecond(u64);

/// A count of a clock source's ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticks(u64);

/// Why a rate or a conversion was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateError {
    /// The rate is zero: nothing can be converted at it.
    Zero,
}

impl core::fmt::Display for RateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Zero => f.write_str("a clock rate of zero"),
        }
    }
}

impl core::error::Error for RateError {}

impl core::fmt::Display for TicksPerSecond {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} Hz", self.0)
    }
}

impl TicksPerSecond {
    /// The rate `ticks_per_second`.
    ///
    /// # Errors
    ///
    /// [`RateError::Zero`] when the rate is zero.
    pub const fn new(ticks_per_second: u64) -> Result<Self, RateError> {
        if ticks_per_second == 0 {
            Err(RateError::Zero)
        } else {
            Ok(Self(ticks_per_second))
        }
    }

    /// The rate in hertz.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// How many ticks `nanos` nanoseconds of this source are: the deadline the
    /// one-shot timer is programmed with (rounded down; the timer fires no
    /// earlier than asked). A deadline beyond the counter's range saturates
    /// to [`Ticks::MAX`], whose caller treats as "far, far away".
    pub const fn ticks_for_nanos(self, nanos: u64) -> Ticks {
        let ticks = (nanos as u128 * self.0 as u128) / crate::NANOS_PER_SEC as u128;
        Ticks(if ticks > u64::MAX as u128 {
            u64::MAX
        } else {
            ticks as u64
        })
    }

    /// The fewest ticks that last at least `nanos` nanoseconds (rounded up, saturating
    /// at [`Ticks::MAX`]): the interval a one-shot timer is programmed with, so it never
    /// fires before the deadline it stands for.
    pub const fn ticks_for_nanos_ceil(self, nanos: u64) -> Ticks {
        let ticks = (nanos as u128 * self.0 as u128).div_ceil(crate::NANOS_PER_SEC as u128);
        Ticks(if ticks > u64::MAX as u128 {
            u64::MAX
        } else {
            ticks as u64
        })
    }

    /// `ticks` of this source as ticks of a source running at `other`, rounded up and
    /// saturating: a TSC interval as a local APIC countdown that is not shorter.
    pub const fn convert_ceil(self, ticks: u64, other: TicksPerSecond) -> u64 {
        let out = (ticks as u128 * other.0 as u128).div_ceil(self.0 as u128);
        if out > u64::MAX as u128 {
            u64::MAX
        } else {
            out as u64
        }
    }

    /// How many nanoseconds `ticks` of this source are: a timestamps or
    /// elapsed-span conversion (rounded down).
    pub const fn nanos_for_ticks(self, ticks: u64) -> u64 {
        ((ticks as u128 * crate::NANOS_PER_SEC as u128) / self.0 as u128) as u64
    }
}

impl Ticks {
    /// The counter's maximum (the "never" conversion result).
    pub const MAX: Self = Self(u64::MAX);

    /// The raw tick count.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GHZ: TicksPerSecond = match TicksPerSecond::new(4_292_000_000) {
        Ok(rate) => rate,
        Err(_) => unreachable!(),
    };

    #[test]
    fn a_zero_rate_is_refused() {
        assert_eq!(TicksPerSecond::new(0), Err(RateError::Zero));
    }

    #[test]
    fn ticks_for_nanos_rounds_down_and_saturates() {
        // 4.292 GHz: one nanosecond is about 4.292 ticks.
        assert_eq!(GHZ.ticks_for_nanos(0), Ticks(0));
        assert_eq!(GHZ.ticks_for_nanos(1), Ticks(4));
        assert_eq!(GHZ.ticks_for_nanos(1_000_000_000), Ticks(4_292_000_000));
        assert_eq!(GHZ.ticks_for_nanos(u64::MAX), Ticks::MAX);
    }

    #[test]
    fn nanos_for_ticks_rounds_down() {
        assert_eq!(GHZ.nanos_for_ticks(0), 0);
        assert_eq!(
            GHZ.nanos_for_ticks(4),
            0,
            "a sub-nanosecond span rounds to zero"
        );
        assert_eq!(
            GHZ.nanos_for_ticks(4_292),
            1_000,
            "4 292 ticks are one microsecond"
        );
        assert_eq!(GHZ.nanos_for_ticks(4_292_000_000), 1_000_000_000);
        let max = ((u64::MAX as u128 * crate::NANOS_PER_SEC as u128) / 4_292_000_000u128) as u64;
        assert_eq!(GHZ.nanos_for_ticks(u64::MAX), max);
    }

    #[test]
    fn ticks_for_nanos_ceil_is_the_fewest_ticks_that_are_not_early() {
        let rate = TicksPerSecond::new(2_500_000_000).unwrap();
        for ns in [0, 1, 2, 3, 999, 1_000, 12_345_678, 1 << 40] {
            let ticks = rate.ticks_for_nanos_ceil(ns).get();
            let exact_ns = ticks as u128 * crate::NANOS_PER_SEC as u128 / 2_500_000_000;
            assert!(exact_ns >= ns as u128, "{ns} ns -> {ticks} ticks is early");
            if ticks > 0 {
                let shorter = (ticks - 1) as u128 * crate::NANOS_PER_SEC as u128 / 2_500_000_000;
                assert!(
                    shorter < ns as u128,
                    "{ns} ns -> {ticks} ticks is not the fewest"
                );
            }
        }
        assert_eq!(GHZ.ticks_for_nanos_ceil(u64::MAX), Ticks::MAX);
    }

    #[test]
    fn a_tsc_interval_becomes_an_apic_count_that_is_not_shorter() {
        let tsc = TicksPerSecond::new(3_000_000_000).unwrap();
        let apic = TicksPerSecond::new(1_000_000_000).unwrap();
        assert_eq!(tsc.convert_ceil(3, apic), 1);
        assert_eq!(tsc.convert_ceil(4, apic), 2);
        assert_eq!(tsc.convert_ceil(0, apic), 0);
        let max = TicksPerSecond::new(u64::MAX).unwrap();
        assert_eq!(apic.convert_ceil(u64::MAX, max), u64::MAX);
    }

    #[test]
    fn rates_display_in_hertz() {
        assert_eq!(std::format!("{GHZ}"), "4292000000 Hz");
    }

    #[test]
    fn conversions_round_trip_at_the_second_granularity() {
        for secs in [1u64, 60, 3_600, 86_400] {
            let nanos = secs * crate::NANOS_PER_SEC;
            let ticks = GHZ.ticks_for_nanos(nanos);
            assert_eq!(GHZ.nanos_for_ticks(ticks.get()), nanos);
        }
    }
}
