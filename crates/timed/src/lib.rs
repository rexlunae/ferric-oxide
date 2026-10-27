//! Monotonic nanosecond time and deadline arithmetic for the scheduler
//! ([#186] and the tickless issue track).
//!
//! The kernel's scheduling clock is a monotonic nanosecond counter; this crate
//! holds the portable arithmetic around it so it is host-tested, while the
//! clock source itself stays in the architecture code:
//!
//! - [`Nanos`] and [`Duration`] are newtype nanoseconds with checked,
//!   saturating and wrapping arithmetic. The counters are `u64`; a 64-bit
//!   nanosecond counter wraps after about 584 years, so [`Nanos::elapsed_since`]
//!   uses wrapping subtraction like a hardware counter, and deadlines are
//!   compared by subtraction, not by raw ordering.
//! - [`rate::Ticks`] converts between monotonic nanoseconds and a source's
//!   ticks (`ticks_per_second`), with 128-bit intermediates so the conversion
//!   cannot overflow or wrap for the rates a clock source can have.
//! - [`ArmedDeadline`] is the per-CPU "what have we armed" cell of the
//!   one-shot rearm protocol: arming a later deadline is a no-op, arming an
//!   earlier one says "the hardware must be reprogrammed", and a disarm plus
//!   an explicit due recheck never drops an already-due deadline.
//! - [`calibrate`](mod@calibrate) measures a counter's rate (the TSC, a local APIC
//!   timer) against a reference (the HPET, the PIT, the KVM clock) over several windows,
//!   extending narrow references past their wrap ([`calibrate::WrappingCounter`]) and
//!   refusing windows that disagree.
//! - [`oneshot`] turns a nanosecond interval into what a local APIC timer is programmed
//!   with (a TSC-deadline value, or a one-shot countdown at a calibrated rate) and
//!   classifies a timer interrupt as due or early.
//! - [`selftest`] is the per-CPU lateness series of the boot selftest.
//! - [`monotonic`] publishes the clock across CPUs without regressing
//!   ([`monotonic::Watermark`]) and hands the KVM clock over to a fallback for
//!   good once it steps backward ([`monotonic::Handover`]).
//!
//! The shared deadline service (#187) and the async executor binding (#181)
//! build their earliest-deadline queues on the same types.
//!
//! Nothing here reads hardware or a clock: no `now()` exists in this crate.
//!
//! [#186]: https://github.com/rexlunae/ferric-oxide/issues/186
//! [#187]: https://github.com/rexlunae/ferric-oxide/issues/187
//! [#181]: https://github.com/rexlunae/ferric-oxide/issues/181

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod calibrate;
pub mod monotonic;
pub mod oneshot;
pub mod rate;
pub mod selftest;

use core::cmp::Ordering;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// One nanosecond.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Monotonic nanoseconds, from a clock source the architecture selected and
/// validated ([#186]): the same scale as the kernel's `time::monotonic`.
///
/// A 64-bit counter of nanoseconds wraps after ~584 years; every comparison
/// in this file is by subtraction, as a hardware counter would be read, so a
/// wrap does not reorder deadlines that span it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Nanos(u64);

impl Nanos {
    /// A time `0` nanoseconds after the epoch.
    pub const ZERO: Self = Self(0);

    /// The time `nanos` nanoseconds after the epoch.
    pub const fn new(nanos: u64) -> Self {
        Self(nanos)
    }

    /// The raw nanosecond count.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Adds `duration`, saturating at the counter's maximum; a deadline that
    /// would overflow must be treated as "never" by the caller, which is what
    /// [`Deadline::from_nanos`] documents.
    pub const fn saturating_add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration.0))
    }

    /// Whether `self` is at or past `deadline`, comparing by subtraction so a
    /// wrap of the counter cannot reorder the two.
    pub const fn at_or_past(self, deadline: Nanos) -> bool {
        self.0.wrapping_sub(deadline.0) >> 63 == 0
    }

    /// The time between `earlier` and `self`, by wrapping subtraction: valid
    /// across a counter wrap, like `Instant::duration_since` on a hardware
    /// counter. The answer is only meaningful when `earlier` really was
    /// earlier (or the counter wrapped between the two).
    pub const fn elapsed_since(self, earlier: Nanos) -> Duration {
        Duration(self.0.wrapping_sub(earlier.0))
    }
}

impl Ord for Nanos {
    /// Orders two times by subtraction, so a counter wrap cannot reorder
    /// them: `PartialOrd` and `Ord` agree with [`Nanos::at_or_past`].
    fn cmp(&self, other: &Self) -> Ordering {
        if self.0 == other.0 {
            Ordering::Equal
        } else if other.0.wrapping_sub(self.0) >> 63 == 0 {
            Ordering::Less // `other` is at most half a counter after `self`
        } else {
            Ordering::Greater
        }
    }
}

impl PartialOrd for Nanos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Nanos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ns", self.0)
    }
}

/// A span of monotonic nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Duration(u64);

impl Duration {
    /// No time at all.
    pub const ZERO: Self = Self(0);
    /// One second.
    pub const SEC: Self = Self(NANOS_PER_SEC);

    /// A span of `nanos` nanoseconds.
    pub const fn new(nanos: u64) -> Self {
        Self(nanos)
    }

    /// The raw nanosecond count.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The span in whole seconds (rounded down).
    pub const fn secs(self) -> u64 {
        self.0 / NANOS_PER_SEC
    }

    /// Adds two spans, saturating.
    pub const fn saturating_add(self, other: Duration) -> Duration {
        Duration(self.0.saturating_add(other.0))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ns", self.0)
    }
}

/// A point where something may run: a nanosecond deadline, or never.
///
/// A deadline is *relative to the scheduler's monotonic clock*, never to
/// realtime: a wall-clock offset change must not shift a deadline ([#186]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deadline {
    /// Run at or after this instant.
    At(Nanos),
    /// No deadline: a task that only this waits for has nothing to wake it
    /// but a wake or a cancel.
    Never,
}

impl Deadline {
    /// At `nanos`.
    pub const fn from_nanos(nanos: u64) -> Self {
        Self::At(Nanos::new(nanos))
    }

    /// In `duration` from `now`, saturating (an overflowing deadline becomes
    /// [`Deadline::Never`], the "far enough that it does not matter" case).
    pub const fn after(now: Nanos, duration: Duration) -> Self {
        match now.0.checked_add(duration.0) {
            Some(nanos) => Self::At(Nanos::new(nanos)),
            None => Self::Never,
        }
    }

    /// Whether `now` is at or past this deadline (never is not due).
    pub const fn due(self, now: Nanos) -> bool {
        match self {
            Self::At(deadline) => now.at_or_past(deadline),
            Self::Never => false,
        }
    }

    /// The deadline's instant, if it has one.
    pub const fn nanos(self) -> Option<Nanos> {
        match self {
            Self::At(nanos) => Some(nanos),
            Self::Never => None,
        }
    }
}

impl Ord for Deadline {
    /// Earlier deadlines order first; [`Deadline::Never`] orders last.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Never, Self::Never) => Ordering::Equal,
            (Self::Never, Self::At(_)) => Ordering::Greater,
            (Self::At(_), Self::Never) => Ordering::Less,
            (Self::At(a), Self::At(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Deadline {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The per-CPU "what have we armed" cell of the one-shot rearm protocol
/// ([#186], [#187]): which deadline (if any) the hardware currently has.
///
/// The owner (the CPU's timer path) arms and disarms; the rearm protocol
/// decides from the answer whether the hardware must be reprogrammed:
///
/// - [`arm`](ArmedDeadline::arm) with a deadline *earlier* than the armed one
///   returns true ("the hardware must be reprogrammed now"); an equal or
///   later deadline is already covered.
/// - A fire, a cancel and an earlier rearm do not race here: the cell is
///   plain `u64` state owned by one CPU (the architecture code serializes the
///   timer and the rearm), so the armed value is the reason the next
///   interrupt may arrive. The kernel's per-CPU locking discipline makes a
///   cross-CPU read unnecessary.
///
/// The cell is `u64` nanoseconds (`0` = disarmed) so interrupt contexts can
/// read and write it cheaply; the arithmetic stays in this crate.
#[derive(Debug, Default)]
pub struct ArmedDeadline {
    armed: AtomicU64,
}

impl ArmedDeadline {
    /// A disarmed cell.
    pub const fn new() -> Self {
        Self {
            armed: AtomicU64::new(0),
        }
    }

    /// The armed deadline, or `None` when disarmed.
    pub fn armed(&self) -> Option<Nanos> {
        let raw = self.armed.load(AtomicOrdering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(Nanos::new(raw))
        }
    }

    /// Arms `deadline` if it is earlier than what is armed (or the cell is
    /// disarmed), returning whether the hardware must be reprogrammed (the
    /// deadline is earlier than the armed one). An equal or later deadline
    /// keeps the armed one: the earliest event governs.
    pub fn arm(&self, deadline: Nanos) -> bool {
        let mut armed = self.armed.load(AtomicOrdering::Acquire);
        loop {
            // 0 means disarmed; otherwise the armed deadline is kept when it
            // is at or before the new one (compared by subtraction, so a
            // counter wrap cannot reorder them).
            let covered = armed != 0 && deadline.at_or_past(Nanos::new(armed));
            if covered {
                return false;
            }
            match self.armed.compare_exchange_weak(
                armed,
                deadline.get(),
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => armed = actual,
            }
        }
    }

    /// Disarms the cell, returning whether it was armed (the caller then
    /// cancels the hardware event). A due deadline must be fired before it is
    /// disarmed: [`due`](ArmedDeadline::due) makes that check explicit.
    pub fn disarm(&self) -> bool {
        self.armed.swap(0, AtomicOrdering::AcqRel) != 0
    }

    /// Whether the armed deadline (if any) is at or past `now`: the
    /// "recheck time after programming" step, so an event that came due while
    /// it was being programmed is never discarded.
    pub fn due(&self, now: Nanos) -> bool {
        match self.armed() {
            Some(deadline) => now.at_or_past(deadline),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanos_order_by_subtraction() {
        assert!(Nanos::new(10) < Nanos::new(20));
        assert!(Nanos::new(20) > Nanos::new(10));
        assert_eq!(Nanos::new(7), Nanos::new(7));
        // A wrap: 10 ns before the wrap and 5 ns after compare correctly.
        let just_before_wrap = Nanos::new(u64::MAX - 10);
        let just_after_wrap = Nanos::new(5);
        assert!(just_before_wrap < just_after_wrap);
        assert_eq!(
            just_after_wrap.elapsed_since(just_before_wrap),
            Duration::new(16)
        );
    }

    #[test]
    fn at_or_past_across_the_wrap() {
        assert!(Nanos::new(30).at_or_past(Nanos::new(20)));
        assert!(!Nanos::new(20).at_or_past(Nanos::new(30)));
        assert!(Nanos::new(5).at_or_past(Nanos::new(u64::MAX - 10)));
        assert!(!Nanos::new(u64::MAX - 10).at_or_past(Nanos::new(5)));
        assert!(Nanos::new(20).at_or_past(Nanos::new(20)));
    }

    #[test]
    fn deadline_due_and_ordering() {
        let now = Nanos::new(1_000_000);
        let past = Deadline::from_nanos(1_000);
        let future = Deadline::after(now, Duration::new(500));
        assert!(past.due(now));
        assert!(!future.due(now));
        assert!(future.due(Nanos::new(2_000_000)));
        assert!(!Deadline::Never.due(now));
        assert!(past < future);
        assert!(Deadline::Never > future);
    }

    #[test]
    fn overflow_becomes_never() {
        let now = Nanos::new(u64::MAX - 100);
        assert_eq!(
            Deadline::after(now, Duration::new(200)),
            Deadline::Never,
            "a deadline past the counter's range is never"
        );
        assert_eq!(
            Deadline::after(now, Duration::new(50)),
            Deadline::At(Nanos::new(u64::MAX - 50))
        );
    }

    #[test]
    fn armed_deadline_keeps_the_earliest() {
        let cell = ArmedDeadline::new();
        assert_eq!(cell.armed(), None);
        assert!(!cell.due(Nanos::new(1_000_000)));
        assert!(cell.arm(Nanos::new(2_000)), "first arm reprograms");
        assert_eq!(cell.armed(), Some(Nanos::new(2_000)));
        assert!(
            !cell.arm(Nanos::new(3_000)),
            "a later deadline is covered by the armed one"
        );
        assert!(
            cell.arm(Nanos::new(1_000)),
            "an earlier deadline reprograms"
        );
        assert_eq!(cell.armed(), Some(Nanos::new(1_000)));
        assert!(cell.due(Nanos::new(1_000)), "due exactly at the deadline");
        assert!(cell.due(Nanos::new(1_500)));
        assert!(!cell.due(Nanos::new(999)));
        assert!(cell.disarm());
        assert_eq!(cell.armed(), None);
        assert!(!cell.disarm(), "second disarm is a no-op");
    }
}
