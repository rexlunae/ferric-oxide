//! Counter frequencies and tick/nanosecond conversion.

use core::{fmt, num::NonZeroU64};

/// Nanoseconds in a second.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;

/// The rate of a counter in ticks per second. Never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hz(NonZeroU64);

impl Hz {
    /// The rate `hz` ticks per second, or `None` for zero.
    pub const fn new(hz: u64) -> Option<Self> {
        match NonZeroU64::new(hz) {
            Some(hz) => Some(Self(hz)),
            None => None,
        }
    }

    /// The rate in ticks per second.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for Hz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz", self.get())
    }
}

/// Converts ticks of a counter running at a fixed [`Hz`] to nanoseconds and back.
///
/// [`ticks_to_ns`](Self::ticks_to_ns) is the hot path (every clock read) and uses a
/// precomputed fixed-point multiplier: `ns = ticks * mult >> shift`, with the largest
/// `shift` that keeps `mult` below 2^63, so one 64x64 multiply and a shift replace a
/// division. `mult` is rounded up, so the result is never below the exact value rounded
/// down, and exceeds it by at most `ticks / 2^shift` nanoseconds: whole seconds convert
/// exactly, and for counters between 1 MHz and 10 GHz the excess stays under a
/// nanosecond per day. [`ns_to_ticks_ceil`](Self::ns_to_ticks_ceil) runs when a timer
/// is armed, is exact, and rounds up so a deadline never fires early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    hz: Hz,
    mult: u64,
    shift: u32,
}

impl Scale {
    /// The conversion for a counter running at `hz`.
    pub const fn new(hz: Hz) -> Self {
        let mut shift = 63;
        loop {
            let mult = ((NANOS_PER_SEC as u128) << shift).div_ceil(hz.get() as u128);
            if mult < (1 << 63) || shift == 0 {
                return Self {
                    hz,
                    mult: mult as u64,
                    shift,
                };
            }
            shift -= 1;
        }
    }

    /// The counter's rate.
    pub const fn hz(&self) -> Hz {
        self.hz
    }

    /// `ticks` in nanoseconds (see the type's docs for the rounding).
    pub const fn ticks_to_ns(&self, ticks: u64) -> u64 {
        let ns = (ticks as u128 * self.mult as u128) >> self.shift;
        if ns > u64::MAX as u128 {
            u64::MAX
        } else {
            ns as u64
        }
    }

    /// The fewest ticks that last at least `ns` nanoseconds, saturating at `u64::MAX`.
    pub const fn ns_to_ticks_ceil(&self, ns: u64) -> u64 {
        let ticks = (ns as u128 * self.hz.get() as u128).div_ceil(NANOS_PER_SEC as u128);
        if ticks > u64::MAX as u128 {
            u64::MAX
        } else {
            ticks as u64
        }
    }

    /// `ticks` of this counter as ticks of a counter running at `other`, rounded up.
    ///
    /// Used to turn a TSC interval into a local APIC timer count: rounding up keeps a
    /// one-shot countdown from expiring before the TSC deadline it stands for.
    pub const fn convert_ceil(&self, ticks: u64, other: Hz) -> u64 {
        let out = (ticks as u128 * other.get() as u128).div_ceil(self.hz.get() as u128);
        if out > u64::MAX as u128 {
            u64::MAX
        } else {
            out as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hz(v: u64) -> Hz {
        Hz::new(v).unwrap()
    }

    #[test]
    fn zero_is_not_a_rate() {
        assert!(Hz::new(0).is_none());
    }

    #[test]
    fn whole_seconds_convert_exactly() {
        for rate in [
            1_000_000,
            1_193_182,
            14_318_180,
            100_000_000,
            2_399_999_999,
            4_292_000_000,
            10_000_000_000,
        ] {
            let scale = Scale::new(hz(rate));
            let one_sec = scale.ticks_to_ns(rate);
            assert_eq!(one_sec, NANOS_PER_SEC, "{rate} Hz");
        }
    }

    #[test]
    fn conversion_error_is_under_a_nanosecond_over_a_day() {
        let rate = 3_700_123_457;
        let scale = Scale::new(hz(rate));
        let day = rate * 86_400;
        let exact = (day as u128 * NANOS_PER_SEC as u128 / rate as u128) as u64;
        let got = scale.ticks_to_ns(day);
        assert!(got >= exact);
        assert!(got - exact <= 1, "drift {} ns over a day", got - exact);
    }

    #[test]
    fn ns_to_ticks_rounds_up_and_round_trips_without_coming_early() {
        let scale = Scale::new(hz(2_500_000_000));
        for ns in [0, 1, 2, 3, 999, 1_000, 12_345_678, 1 << 40] {
            let ticks = scale.ns_to_ticks_ceil(ns);
            let exact_ns = ticks as u128 * NANOS_PER_SEC as u128 / 2_500_000_000;
            assert!(exact_ns >= ns as u128, "{ns} ns -> {ticks} ticks is early");
            if ticks > 0 {
                let shorter = (ticks - 1) as u128 * NANOS_PER_SEC as u128 / 2_500_000_000;
                assert!(
                    shorter < ns as u128,
                    "{ns} ns -> {ticks} ticks is not the fewest"
                );
            }
        }
    }

    #[test]
    fn huge_values_saturate_instead_of_wrapping() {
        let slow = Scale::new(hz(1));
        assert_eq!(slow.ticks_to_ns(u64::MAX), u64::MAX);
        let fast = Scale::new(hz(10_000_000_000));
        assert_eq!(fast.ns_to_ticks_ceil(u64::MAX), u64::MAX);
        assert_eq!(fast.convert_ceil(u64::MAX, hz(u64::MAX)), u64::MAX);
    }

    #[test]
    fn a_tsc_interval_becomes_an_apic_count_that_is_not_shorter() {
        let tsc = Scale::new(hz(3_000_000_000));
        let apic = hz(1_000_000_000);
        assert_eq!(tsc.convert_ceil(3, apic), 1);
        assert_eq!(tsc.convert_ceil(4, apic), 2);
        assert_eq!(tsc.convert_ceil(0, apic), 0);
    }
}
