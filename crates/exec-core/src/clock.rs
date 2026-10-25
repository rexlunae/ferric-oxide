//! Monotonic time for the executor: a [`Clock`] trait so host tests inject a fake
//! clock and the kernel later passes its validated monotonic clock ([#186]).
//!
//! The executor never reads the clock on its own: it only converts `now()` into
//! due deadlines when the worker enters it, so nothing here polls time. Elapsed
//! time and CPU budgets are separate quantities: a fake or wall clock answers
//! "what time is it", never "how long did this task run" ([#180]).

use core::sync::atomic::{AtomicU64, Ordering};

/// A source of monotonic nanoseconds.
///
/// The value must be monotonic (never go backwards) and comparable across the
/// executor's workers (initially one worker per executor, so this is trivially
/// true; [#186] defines the cross-CPU guarantee for the kernel clock).
pub trait Clock: Send + Sync {
    /// The current time in nanoseconds since an arbitrary, fixed epoch.
    fn now(&self) -> u64;
}

/// A host-test [`Clock`] the tests wind forward by hand.
///
/// `now` starts at zero and only advances, so the monotonic contract holds.
/// [`advance`](FakeClock::advance) and [`set`](FakeClock::set) model long
/// tick-free gaps, wake storms and deadline reordering deterministically.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: AtomicU64,
}

impl FakeClock {
    /// A clock at time zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances the clock by `ns` nanoseconds.
    pub fn advance(&self, ns: u64) {
        self.set(self.now.load(Ordering::SeqCst).saturating_add(ns));
    }

    /// Sets the clock to exactly `ns` nanoseconds, which must not go backwards.
    ///
    /// # Panics
    ///
    /// When `ns` is before the current time: the contract says a clock never goes
    /// backwards, so the test would be winding time in a way the executor's
    /// deadline logic does not have to handle.
    pub fn set(&self, ns: u64) {
        assert!(
            ns >= self.now.load(Ordering::SeqCst),
            "a monotonic clock cannot go backwards to {ns}"
        );
        self.now.store(ns, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fake_clock_advances_and_never_goes_back() {
        let clock = FakeClock::new();
        assert_eq!(clock.now(), 0);
        clock.advance(1_000);
        assert_eq!(clock.now(), 1_000);
        clock.set(5_000);
        assert_eq!(clock.now(), 5_000);
    }

    #[test]
    #[should_panic(expected = "cannot go backwards")]
    fn a_fake_clock_refuses_to_go_back() {
        let clock = FakeClock::new();
        clock.advance(100);
        clock.set(50);
    }
}
