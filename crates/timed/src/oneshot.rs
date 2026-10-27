//! Programming a per-CPU one-shot timer for an absolute deadline.
//!
//! Deadlines are absolute TSC readings (the clocksource), so a deadline means the same
//! instant whichever backend fires it. Two local APIC timer modes back a deadline (Intel
//! SDM, Vol. 3A, section 11.5.4):
//!
//! - [`Backend::TscDeadline`]: the timer fires when the TSC reaches the value written to
//!   `IA32_TSC_DEADLINE`. Available when CPUID.01H:ECX\[24\] is set. No conversion and no
//!   interval limit; writing a deadline already in the past fires at once.
//! - [`Backend::Countdown`]: the timer counts down a 32-bit initial count at a rate the
//!   kernel calibrated against the TSC. The interval is limited to `u32::MAX` counts, so a
//!   distant deadline is armed as the longest countdown and re-armed when that ends early.
//!
//! [`arm`] computes what to program; [`on_fire`] tells the interrupt handler whether the
//! deadline has really passed.

use crate::rate::TicksPerSecond;

/// How a CPU's one-shot timer is driven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// The local APIC timer in TSC-deadline mode.
    TscDeadline,
    /// The local APIC timer as a one-shot countdown at `timer_hz` (the bus or crystal
    /// clock after the divide configuration).
    Countdown {
        /// The countdown's calibrated rate.
        timer_hz: TicksPerSecond,
    },
}

impl Backend {
    /// A short name for logs and the `sched.timer_arm` trace record: 1 for TSC-deadline, 2 for
    /// a countdown.
    pub const fn code(self) -> u64 {
        match self {
            Self::TscDeadline => 1,
            Self::Countdown { .. } => 2,
        }
    }

    /// `"tsc-deadline"` or `"lapic-countdown"`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::TscDeadline => "tsc-deadline",
            Self::Countdown { .. } => "lapic-countdown",
        }
    }
}

/// What to write to the timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Program {
    /// Write this TSC value to `IA32_TSC_DEADLINE`.
    TscDeadline(u64),
    /// Write this initial count (never zero, which would stop the timer).
    Countdown(u32),
}

impl Program {
    /// The value written, widened, for the `sched.timer_arm` trace record.
    pub const fn value(self) -> u64 {
        match self {
            Self::TscDeadline(value) => value,
            Self::Countdown(count) => count as u64,
        }
    }
}

/// The result of [`arm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Armed {
    /// What to write.
    pub program: Program,
    /// The deadline was at or before `now + min_lead`, so the timer is set to fire as soon
    /// as the hardware allows.
    pub past: bool,
    /// The interval did not fit the countdown, which will fire before the deadline; the
    /// handler sees [`Fired::Early`] and arms again.
    pub clamped: bool,
}

/// Computes the timer value for `deadline` (a TSC reading) at TSC time `now`.
///
/// `min_lead` is the least TSC interval the caller allows between programming and firing:
/// a deadline closer than that (or already past) is moved to `now + min_lead`, so the
/// interrupt still comes, a little late, instead of being lost or fired before the write
/// lands.
pub fn arm(backend: Backend, tsc: TicksPerSecond, now: u64, deadline: u64, min_lead: u64) -> Armed {
    let earliest = now.saturating_add(min_lead);
    let past = deadline <= earliest;
    let deadline = deadline.max(earliest);
    match backend {
        Backend::TscDeadline => Armed {
            program: Program::TscDeadline(deadline),
            past,
            clamped: false,
        },
        Backend::Countdown { timer_hz } => {
            let count = tsc.convert_ceil(deadline - now, timer_hz).max(1);
            let clamped = count > u64::from(u32::MAX);
            Armed {
                program: Program::Countdown(u32::try_from(count).unwrap_or(u32::MAX)),
                past,
                clamped,
            }
        }
    }
}

/// What a timer interrupt means for the deadline it was armed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fired {
    /// The deadline has passed, by this many TSC ticks.
    Due {
        /// How late the interrupt was handled.
        late: u64,
    },
    /// The deadline is still this many TSC ticks away: a clamped countdown ended, or the
    /// countdown's calibration ran fast. Arm again for the same deadline.
    Early {
        /// The time left.
        remaining: u64,
    },
}

/// Classifies a timer interrupt handled at TSC time `now` for `deadline`.
pub const fn on_fire(now: u64, deadline: u64) -> Fired {
    if now >= deadline {
        Fired::Due {
            late: now - deadline,
        }
    } else {
        Fired::Early {
            remaining: deadline - now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hz(v: u64) -> TicksPerSecond {
        TicksPerSecond::new(v).unwrap()
    }

    fn tsc() -> TicksPerSecond {
        hz(3_000_000_000)
    }

    #[test]
    fn tsc_deadline_writes_the_deadline_itself() {
        let armed = arm(Backend::TscDeadline, tsc(), 1_000, 50_000, 100);
        assert_eq!(
            armed,
            Armed {
                program: Program::TscDeadline(50_000),
                past: false,
                clamped: false
            }
        );
    }

    #[test]
    fn a_past_or_too_close_deadline_fires_after_the_minimum_lead() {
        for deadline in [0, 999, 1_000, 1_050, 1_100] {
            let armed = arm(Backend::TscDeadline, tsc(), 1_000, deadline, 100);
            assert_eq!(
                armed.program,
                Program::TscDeadline(1_100),
                "deadline {deadline}"
            );
            assert!(armed.past);
        }
        let countdown = Backend::Countdown {
            timer_hz: hz(1_000_000_000),
        };
        let armed = arm(countdown, tsc(), 1_000, 0, 0);
        assert_eq!(
            armed.program,
            Program::Countdown(1),
            "a zero count would stop the timer"
        );
        assert!(armed.past);
    }

    #[test]
    fn a_countdown_is_converted_and_rounded_up() {
        let countdown = Backend::Countdown {
            timer_hz: hz(1_000_000_000),
        };
        // 3,000,001 TSC ticks at 3 GHz is 1,000,000.33 ns: 1,000,001 counts at 1 GHz.
        let armed = arm(countdown, tsc(), 0, 3_000_001, 0);
        assert_eq!(armed.program, Program::Countdown(1_000_001));
        assert!(!armed.past && !armed.clamped);
    }

    #[test]
    fn a_distant_countdown_is_clamped_and_reports_it() {
        let countdown = Backend::Countdown {
            timer_hz: hz(1_000_000_000),
        };
        // An hour at 3 GHz is far past u32::MAX counts at 1 GHz (4.29 s).
        let armed = arm(countdown, tsc(), 0, 3_000_000_000 * 3600, 0);
        assert_eq!(armed.program, Program::Countdown(u32::MAX));
        assert!(armed.clamped);
    }

    #[test]
    fn deadlines_near_the_end_of_time_saturate() {
        let armed = arm(Backend::TscDeadline, tsc(), u64::MAX - 5, u64::MAX, 100);
        assert_eq!(armed.program, Program::TscDeadline(u64::MAX));
        assert!(armed.past);
    }

    #[test]
    fn an_interrupt_is_due_or_early() {
        assert_eq!(on_fire(1_000, 900), Fired::Due { late: 100 });
        assert_eq!(on_fire(900, 900), Fired::Due { late: 0 });
        assert_eq!(on_fire(800, 900), Fired::Early { remaining: 100 });
    }

    #[test]
    fn backend_codes_and_names_are_stable() {
        assert_eq!(Backend::TscDeadline.code(), 1);
        assert_eq!(Backend::Countdown { timer_hz: hz(1) }.code(), 2);
        assert_eq!(Backend::TscDeadline.name(), "tsc-deadline");
        assert_eq!(Program::Countdown(7).value(), 7);
    }
}
