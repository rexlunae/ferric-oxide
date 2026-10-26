//! The boot check of a CPU's one-shot timer.
//!
//! With `kernel.oneshot_timer` on, each CPU arms its timer [`SHOTS`] times in a row, each
//! interrupt arming the next, for the intervals in [`INTERVALS_NS`]. [`SelfTest`] keeps
//! the count and how late the shots were. The first interrupt only starts the series: it
//! was armed during boot, before interrupts were enabled, so its lateness measures the boot
//! and not the timer.
//!
//! The result is a report, not a pass/fail gate on lateness: a virtual machine's timer
//! interrupts are late by however long the host takes to run the vCPU. What must hold is
//! that every shot fires and none fires before its deadline unannounced
//! ([`Verdict::complete`]).

/// The intervals the series cycles through, in nanoseconds: 50 µs to 5 ms.
pub const INTERVALS_NS: [u64; 6] = [50_000, 100_000, 250_000, 500_000, 1_000_000, 5_000_000];

/// How many shots the series has after the starting one.
pub const SHOTS: u32 = 24;

/// The state of one CPU's series.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelfTest {
    started: bool,
    fired: u32,
    early: u32,
    late_min_ns: u64,
    late_max_ns: u64,
    late_sum_ns: u64,
}

/// What [`SelfTest::record`] asks the handler to do next.
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

impl SelfTest {
    /// A series that has not started.
    pub const fn new() -> Self {
        Self {
            started: false,
            fired: 0,
            early: 0,
            late_min_ns: u64::MAX,
            late_max_ns: 0,
            late_sum_ns: 0,
        }
    }

    /// The interval of the starting shot, armed during boot.
    pub const fn first_interval() -> u64 {
        INTERVALS_NS[0]
    }

    /// Records an early interrupt that the handler re-armed for the same deadline.
    pub const fn record_early(&mut self) {
        self.early += 1;
    }

    /// Records a shot that fired `late_ns` after its deadline and says what to do next.
    pub fn record(&mut self, late_ns: u64) -> Next {
        if !self.started {
            self.started = true;
        } else {
            self.fired += 1;
            self.late_min_ns = self.late_min_ns.min(late_ns);
            self.late_max_ns = self.late_max_ns.max(late_ns);
            self.late_sum_ns = self.late_sum_ns.saturating_add(late_ns);
        }
        if self.fired >= SHOTS {
            Next::Done
        } else {
            Next::Arm(INTERVALS_NS[self.fired as usize % INTERVALS_NS.len()])
        }
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
    fn the_series_skips_the_boot_shot_and_counts_the_rest() {
        let mut test = SelfTest::new();
        assert_eq!(test.record(9_999_999), Next::Arm(INTERVALS_NS[0]));
        let mut armed = std::vec::Vec::new();
        for i in 0..SHOTS {
            match test.record(u64::from(i) * 10) {
                Next::Arm(ns) => armed.push(ns),
                Next::Done => assert_eq!(i, SHOTS - 1),
            }
        }
        assert_eq!(armed.len() as u32, SHOTS - 1);
        assert_eq!(armed[0], INTERVALS_NS[1]);
        let verdict = test.verdict();
        assert!(verdict.complete());
        assert_eq!(verdict.late_min_ns, 0);
        assert_eq!(verdict.late_max_ns, u64::from(SHOTS - 1) * 10);
        assert_eq!(verdict.late_avg_ns, u64::from(SHOTS - 1) * 10 / 2);
        assert_eq!(test.record(0), Next::Done);
    }

    #[test]
    fn an_unstarted_series_reports_nothing_fired() {
        let mut test = SelfTest::new();
        test.record_early();
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
