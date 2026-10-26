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
    fn conversions_round_trip_at_the_second_granularity() {
        for secs in [1u64, 60, 3_600, 86_400] {
            let nanos = secs * crate::NANOS_PER_SEC;
            let ticks = GHZ.ticks_for_nanos(nanos);
            assert_eq!(GHZ.nanos_for_ticks(ticks.get()), nanos);
        }
    }
}
