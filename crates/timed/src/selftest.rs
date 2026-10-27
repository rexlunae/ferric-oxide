//! The lateness series of the boot one-shot timer selftest.
//!
//! With `kernel.time.one_shot` on, each CPU arms its timer [`SHOTS`] times in a row for
//! the intervals in [`INTERVALS_NS`], waiting for each to fire before arming the next.
//! [`SelfTest`] picks each interval and keeps the count and how late the shots were.
//!
//! The result is a report, not a pass/fail gate on lateness: a virtual machine's timer
//! interrupts are late by however long the host takes to run the vCPU. What must hold is
//! that every shot fires ([`Verdict::complete`]).

/// The intervals the series cycles through, in nanoseconds: 50 µs to 5 ms.
pub const INTERVALS_NS: [u64; 6] = [50_000, 100_000, 250_000, 500_000, 1_000_000, 5_000_000];

/// How many shots the series has.
pub const SHOTS: u32 = 24;

/// The state of one CPU's series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelfTest {
    fired: u32,
    early: u32,
    late_min_ns: u64,
    late_max_ns: u64,
    late_sum_ns: u64,
}

/// What [`SelfTest::next`] asks the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Arm the next shot this many nanoseconds from now.
    Arm(u64),
    /// The series is over; report [`SelfTest::verdict`].
    Done,
}

/// The summary of a finished (or abandoned) series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Shots that fired at or after their deadline.
    pub fired: u32,
    /// Shots expected.
    pub expected: u32,
    /// Interrupts that came before their deadline (each was re-armed).
    pub early: u32,
    /// The least lateness, in nanoseconds.
    pub late_min_ns: u64,
    /// The mean lateness, in nanoseconds.
    pub late_avg_ns: u64,
    /// The greatest lateness, in nanoseconds.
    pub late_max_ns: u64,
}

impl Verdict {
    /// Whether every expected shot fired.
    pub const fn complete(&self) -> bool {
        self.fired == self.expected
    }
}

impl Default for SelfTest {
    fn default() -> Self {
        Self::new()
    }
}

impl SelfTest {
    /// A series with no shots yet.
    pub const fn new() -> Self {
        Self {
            fired: 0,
            early: 0,
            late_min_ns: u64::MAX,
            late_max_ns: 0,
            late_sum_ns: 0,
        }
    }

    /// The interval of the next shot, or [`Next::Done`] once [`SHOTS`] have fired.
    pub const fn next(&self) -> Next {
        if self.fired >= SHOTS {
            Next::Done
        } else {
            Next::Arm(INTERVALS_NS[self.fired as usize % INTERVALS_NS.len()])
        }
    }

    /// Records early interrupts that were re-armed for the same deadline.
    pub const fn record_early(&mut self, count: u32) {
        self.early = self.early.saturating_add(count);
    }

    /// Records a shot that fired `late_ns` after its deadline.
    pub fn record(&mut self, late_ns: u64) {
        self.fired = self.fired.saturating_add(1);
        self.late_min_ns = self.late_min_ns.min(late_ns);
        self.late_max_ns = self.late_max_ns.max(late_ns);
        self.late_sum_ns = self.late_sum_ns.saturating_add(late_ns);
    }

    /// The summary so far.
    pub const fn verdict(&self) -> Verdict {
        Verdict {
            fired: self.fired,
            expected: SHOTS,
            early: self.early,
            late_min_ns: if self.fired == 0 { 0 } else { self.late_min_ns },
            late_avg_ns: if self.fired == 0 {
                0
            } else {
                self.late_sum_ns / self.fired as u64
            },
            late_max_ns: self.late_max_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_series_cycles_the_intervals_and_summarizes() {
        let mut test = SelfTest::new();
        let mut armed = 0;
        while let Next::Arm(ns) = test.next() {
            assert_eq!(ns, INTERVALS_NS[armed % INTERVALS_NS.len()]);
            test.record(armed as u64 * 10);
            armed += 1;
        }
        assert_eq!(armed as u32, SHOTS);
        let verdict = test.verdict();
        assert!(verdict.complete());
        assert_eq!(verdict.late_min_ns, 0);
        assert_eq!(verdict.late_max_ns, u64::from(SHOTS - 1) * 10);
        assert_eq!(verdict.late_avg_ns, u64::from(SHOTS - 1) * 10 / 2);
    }

    #[test]
    fn an_empty_series_reports_nothing_fired() {
        let mut test = SelfTest::new();
        test.record_early(1);
        let verdict = test.verdict();
        assert!(!verdict.complete());
        assert_eq!(
            (
                verdict.fired,
                verdict.early,
                verdict.late_min_ns,
                verdict.late_avg_ns
            ),
            (0, 1, 0, 0)
        );
    }
}
