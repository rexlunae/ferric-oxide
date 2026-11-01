//! Measured-time accounting for Ferric's best-effort EEVDF scheduler.
//!
//! The kernel owns runnable-context selection, switching and synchronization.
//! This crate owns real remaining slices and weighted virtual time, without
//! hardware ticks, allocation or callbacks. Virtual deadlines express fairness,
//! never real-time reservations. The separation follows the published
//! [Fuchsia scheduler design](https://fuchsia.dev/fuchsia-src/concepts/kernel/kernel_scheduling)
//! and Ferric's [scheduler plan](https://github.com/rexlunae/ferric-oxide/issues/187);
//! it is not an implementation of Fuchsia's scheduling policy.
//!
//! # Examples
//!
//! ```
//! use core::time::Duration;
//! use ferric_sched_core::{FairState, ScheduleReason, VirtualTime, Weight};
//!
//! let weight = Weight::for_priority(20).expect("normal priority exists");
//! let mut task = FairState::new();
//! task.activate(VirtualTime::ZERO, weight);
//! let before = task.remaining();
//! task.account(Duration::from_millis(1), weight, ScheduleReason::Wake);
//! assert_eq!(task.remaining(), before - Duration::from_millis(1));
//! task.account(Duration::ZERO, weight, ScheduleReason::Yield);
//! assert!(task.deadline() > task.runtime());
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use core::{num::NonZeroU64, time::Duration};

/// Requested real execution time per best-effort slice.
///
/// Preserves the former scheduler's intended 20.25 ms slice independently of
/// the actual timer interrupt interval. This is not an IRQ/preemption bound.
pub const BASE_SLICE: Duration = Duration::from_nanos(20_250_000);

const NORMAL_WEIGHT: u128 = 1024;
const PRIORITY_WEIGHTS: [u64; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

/// Why the scheduler is reconsidering its current timeslice owner.
///
/// Only an explicit [`Self::Yield`] forfeits unconsumed service. A short wake,
/// timer interval or blocked operation is never inferred to be a yield.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ScheduleReason {
    /// Periodic tick or a future one-shot preemption event.
    Preempt,
    /// Readiness changed, including a targeted wake IPI.
    Wake,
    /// The running context explicitly requested `sched_yield`.
    Yield,
    /// The running context blocked or terminated.
    Block,
    /// Idle selection, contention or another internal reconsideration.
    Reconsider,
}

/// Nonzero best-effort scheduling weight, separate from physical CPU capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Weight(NonZeroU64);

impl Weight {
    /// Returns the inherited weight for a priority in `0..40`.
    ///
    /// Priority 20 is normal. Out-of-range priorities return `None`.
    pub fn for_priority(priority: usize) -> Option<Self> {
        PRIORITY_WEIGHTS
            .get(priority)
            .and_then(|&weight| NonZeroU64::new(weight))
            .map(Self)
    }

    /// Returns this weight for aggregate runnable-weight accounting.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Weighted virtual nanoseconds, with a wider range than real clock readings.
///
/// One real nanosecond at normal weight advances one virtual nanosecond.
/// Arithmetic saturates rather than wrapping scheduling order.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct VirtualTime(u128);

impl VirtualTime {
    /// The origin of a virtual timeline.
    pub const ZERO: Self = Self(0);
    /// The largest representable virtual time, useful as a selection sentinel.
    pub const MAX: Self = Self(u128::MAX);

    /// Constructs a virtual time from weighted nanoseconds.
    pub const fn new(nanos: u128) -> Self {
        Self(nanos)
    }

    /// Returns weighted virtual nanoseconds (not a wall-clock deadline).
    pub const fn get(self) -> u128 {
        self.0
    }
}

// Carries division residue across observations with unchanged weight. Changing
// the denominator discards less than one virtual ns, never a whole delta.
#[derive(Clone, Copy, Debug)]
struct Divider {
    denominator: NonZeroU64,
    remainder: u128,
}

impl Divider {
    const fn new() -> Self {
        Self {
            denominator: NonZeroU64::MIN,
            remainder: 0,
        }
    }

    fn charge(&mut self, elapsed: Duration, denominator: NonZeroU64) -> u128 {
        if self.denominator != denominator {
            self.denominator = denominator;
            self.remainder = 0;
        }
        // Duration's maximum fits in 94 bits; multiplication by 1024 and an
        // at-most-u64 remainder cannot overflow u128.
        let numerator = elapsed.as_nanos() * NORMAL_WEIGHT + self.remainder;
        let denominator = u128::from(denominator.get());
        self.remainder = numerator % denominator;
        numerator / denominator
    }
}

/// Per-CPU virtual clock, charged before changing the runnable population.
#[derive(Clone, Copy, Debug)]
pub struct FairClock {
    time: VirtualTime,
    divider: Divider,
}

impl Default for FairClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FairClock {
    /// Creates a virtual clock at the origin.
    pub const fn new() -> Self {
        Self {
            time: VirtualTime::ZERO,
            divider: Divider::new(),
        }
    }

    /// Returns the current virtual time.
    pub const fn time(&self) -> VirtualTime {
        self.time
    }

    /// Accounts elapsed non-idle execution at the preceding runnable weight.
    ///
    /// A zero runnable weight means no service was charged. Idle callers must
    /// pass zero elapsed time, even if runnable work arrived during idleness.
    pub fn account(&mut self, elapsed: Duration, total_weight: u64) {
        if let Some(weight) = NonZeroU64::new(total_weight) {
            self.time.0 = self
                .time
                .0
                .saturating_add(self.divider.charge(elapsed, weight));
        }
    }

    /// Advances to the first eligible virtual time when no task is eligible.
    ///
    /// Never moves backward. Fractional time from before the jump is discarded.
    pub fn advance_to(&mut self, time: VirtualTime) {
        if time > self.time {
            self.time = time;
            self.divider.remainder = 0;
        }
    }
}

/// Per-timeslice-owner fair accounting, including borrowed scheme execution.
///
/// Account every non-idle interval, even when its owner has just blocked. The
/// caller deactivates a blocked owner after charging it. A wakeup activates a
/// fresh slice; ordinary timer/readiness events preserve the unconsumed slice.
#[derive(Clone, Copy, Debug)]
pub struct FairState {
    runtime: VirtualTime,
    deadline: VirtualTime,
    remaining: Duration,
    divider: Divider,
}

impl Default for FairState {
    fn default() -> Self {
        Self::new()
    }
}

impl FairState {
    /// Creates inactive accounting at the virtual origin.
    pub const fn new() -> Self {
        Self {
            runtime: VirtualTime::ZERO,
            deadline: VirtualTime::ZERO,
            remaining: Duration::ZERO,
            divider: Divider::new(),
        }
    }

    /// Returns the owner's weighted execution time.
    pub const fn runtime(&self) -> VirtualTime {
        self.runtime
    }

    /// Returns its fairness deadline on the CPU's virtual timeline.
    pub const fn deadline(&self) -> VirtualTime {
        self.deadline
    }

    /// Returns unconsumed real execution time in this slice.
    pub const fn remaining(&self) -> Duration {
        self.remaining
    }

    /// Starts a full slice, clamping a waking owner's runtime to the CPU clock.
    pub fn activate(&mut self, floor: VirtualTime, weight: Weight) {
        self.runtime = self.runtime.max(floor);
        self.renew(weight);
    }

    /// Charges measured execution, renewing a completed or yielded slice.
    ///
    /// Overruns charge the entire interval. Only `Yield` additionally charges
    /// unused service; no inference depends on interval length. This method
    /// also charges a blocked owner; call [`Self::deactivate`] afterward.
    pub fn account(&mut self, elapsed: Duration, weight: Weight, reason: ScheduleReason) {
        self.runtime.0 = self
            .runtime
            .0
            .saturating_add(self.divider.charge(elapsed, weight.0));
        self.remaining = self.remaining.saturating_sub(elapsed);
        if reason == ScheduleReason::Yield {
            self.runtime.0 = self
                .runtime
                .0
                .saturating_add(self.divider.charge(self.remaining, weight.0));
            self.remaining = Duration::ZERO;
        }
        if self.remaining.is_zero() {
            self.renew(weight);
        }
    }

    /// Ends a blocked owner's slice after its last execution is accounted.
    pub fn deactivate(&mut self) {
        self.remaining = Duration::ZERO;
    }

    /// Moves accounting between CPU clocks while preserving signed virtual lag.
    ///
    /// Remaining real service is unchanged. A negative translated runtime is
    /// clamped to the origin; large positive values saturate instead of wrap.
    pub fn migrate(&mut self, source: VirtualTime, destination: VirtualTime, weight: Weight) {
        self.runtime = VirtualTime(if self.runtime >= source {
            destination.0.saturating_add(self.runtime.0 - source.0)
        } else {
            destination.0.saturating_sub(source.0 - self.runtime.0)
        });
        self.set_deadline(weight);
    }

    fn renew(&mut self, weight: Weight) {
        self.remaining = BASE_SLICE;
        self.set_deadline(weight);
    }

    fn set_deadline(&mut self, weight: Weight) {
        let virtual_slice = self.remaining.as_nanos() * NORMAL_WEIGHT / u128::from(weight.get());
        self.deadline = VirtualTime(self.runtime.0.saturating_add(virtual_slice));
    }
}

#[cfg(test)]
mod tests;
