//! Publishing one monotonic clock from per-CPU readings, and handing a primary
//! clock source over to a fallback when the primary steps backward.
//!
//! The kernel reads its clock source on every CPU and returns the result as
//! the system's monotonic time, so two properties must hold across CPUs:
//!
//! - [`Watermark`] is the latest time any CPU has returned. Publishing a
//!   reading is one atomic `fetch_max`, so a CPU whose reading is older than
//!   another CPU's (it was delayed between reading the source and publishing)
//!   can never lower the published time; it returns the newer value instead.
//! - [`Handover`] records, once and for all, that the primary source (the KVM
//!   paravirtual clock) is no longer trusted, and the offset that carries the
//!   last published time onto the fallback source's scale. After a handover
//!   every read uses the fallback, whatever the primary later reports: a
//!   primary that re-based to a lower epoch would otherwise hold the clock at
//!   the watermark until it caught up again.
//!
//! [`read`] combines the two; the architecture code supplies the readers.

use core::sync::atomic::{AtomicU64, Ordering};

/// The latest monotonic time returned on any CPU, in nanoseconds.
#[derive(Debug, Default)]
pub struct Watermark(AtomicU64);

impl Watermark {
    /// A watermark at time 0.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// The latest published time.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Publishes `reading` and returns the time to report: `reading`, or the
    /// later time another reader already published.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_timed::monotonic::Watermark;
    ///
    /// let clock = Watermark::new();
    /// assert_eq!(clock.publish(103), 103);
    /// // A reader delayed with an older reading gets the newer time.
    /// assert_eq!(clock.publish(102), 103);
    /// assert_eq!(clock.get(), 103);
    /// ```
    pub fn publish(&self, reading: u64) -> u64 {
        self.0.fetch_max(reading, Ordering::AcqRel).max(reading)
    }
}

/// Whether the primary source was handed over to the fallback, and the offset
/// that maps fallback time onto the published scale.
///
/// The state is one word: 0 while the primary is in use, else the offset plus
/// one, so an offset of 0 (the fallback already ahead of the published time)
/// still records the handover. An offset of `u64::MAX` (585 years of
/// difference between the two sources) is stored as `u64::MAX - 1`.
#[derive(Debug, Default)]
pub struct Handover(AtomicU64);

impl Handover {
    /// The primary source in use, with no handover.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// The offset added to fallback readings, or `None` while the primary is
    /// still in use.
    pub fn offset(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            encoded => Some(encoded - 1),
        }
    }

    /// Hands the primary over to the fallback, if no reader did yet, so that
    /// `fallback_now + offset` continues from `published`, and returns the
    /// offset in force. When several readers race, the first one's offset
    /// stands and every reader gets it.
    pub fn hand_over(&self, published: u64, fallback_now: u64) -> u64 {
        let encoded = published.saturating_sub(fallback_now).saturating_add(1);
        match self
            .0
            .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => encoded - 1,
            Err(current) => current - 1,
        }
    }
}

/// Reads the clock: the primary source until it steps backward (`primary`
/// returns `None`), then the fallback plus the handover offset for good, and
/// publishes the result on `watermark`, so the returned time never decreases
/// across calls on any CPU.
///
/// # Examples
///
/// ```
/// use ferric_timed::monotonic::{read, Handover, Watermark};
///
/// let (clock, handover) = (Watermark::new(), Handover::new());
/// assert_eq!(read(&clock, &handover, || Some(12_000), || 500), 12_000);
/// // The primary re-bases: from now on the fallback carries the clock.
/// assert_eq!(read(&clock, &handover, || None, || 600), 12_000);
/// assert_eq!(read(&clock, &handover, || Some(2_000), || 700), 12_100);
/// ```
pub fn read(
    watermark: &Watermark,
    handover: &Handover,
    primary: impl FnOnce() -> Option<u64>,
    fallback: impl FnOnce() -> u64,
) -> u64 {
    if let Some(offset) = handover.offset() {
        return watermark.publish(fallback().saturating_add(offset));
    }
    match primary() {
        Some(now) => watermark.publish(now),
        None => {
            let fallback_now = fallback();
            let offset = handover.hand_over(watermark.get(), fallback_now);
            watermark.publish(fallback_now.saturating_add(offset))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::vec::Vec;

    #[test]
    fn a_delayed_older_reading_never_lowers_the_published_time() {
        let clock = Watermark::new();
        // CPUs A and B both saw 100; B publishes 103 first, then A its 102.
        assert_eq!(clock.publish(100), 100);
        assert_eq!(clock.publish(103), 103);
        assert_eq!(clock.publish(102), 103);
        assert_eq!(clock.get(), 103);
    }

    #[test]
    fn concurrent_publishers_never_observe_a_regression() {
        let clock = Arc::new(Watermark::new());
        let threads: Vec<_> = (0..8u64)
            .map(|cpu| {
                let clock = Arc::clone(&clock);
                thread::spawn(move || {
                    let mut last = 0;
                    for step in 0..20_000u64 {
                        // Each CPU's readings run at a different skew from the others.
                        let now = clock.publish(step * 8 + (cpu * 5) % 13);
                        assert!(now >= last, "cpu {cpu}: {now} < {last}");
                        last = now;
                    }
                    last
                })
            })
            .collect();
        let finals: Vec<u64> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let max = finals.iter().copied().max().unwrap();
        assert_eq!(clock.get(), max);
        assert!(clock.get() >= 19_999 * 8);
    }

    #[test]
    fn a_rebase_hands_over_for_good_even_when_the_primary_reads_again() {
        let (clock, handover) = (Watermark::new(), Handover::new());
        // The KVM clock ran to 12 s; the fallback (TSC) is at 5 s on its own scale.
        assert_eq!(read(&clock, &handover, || Some(12_000), || 5_000), 12_000);
        // The KVM clock re-bases to 2 s: the reader reports the step once.
        assert_eq!(read(&clock, &handover, || None, || 5_010), 12_000);
        assert_eq!(handover.offset(), Some(12_000 - 5_010));
        // Later KVM readings are valid on their own new epoch (below the old one), but
        // the fallback keeps carrying the clock, so it advances instead of freezing.
        let mut last = 12_000;
        for (kvm, tsc) in [(2_001, 5_020), (2_500, 5_500), (9_000, 12_000)] {
            let now = read(&clock, &handover, || Some(kvm), || tsc);
            assert_eq!(now, tsc + 12_000 - 5_010);
            assert!(now > last);
            last = now;
        }
        // The primary is not even consulted after the handover.
        let now = read(
            &clock,
            &handover,
            || panic!("primary read after handover"),
            || 13_000,
        );
        assert_eq!(now, 13_000 + 12_000 - 5_010);
    }

    #[test]
    fn a_fallback_already_ahead_still_records_the_handover() {
        let (clock, handover) = (Watermark::new(), Handover::new());
        assert_eq!(read(&clock, &handover, || Some(1_000), || 0), 1_000);
        // The fallback is ahead of the published time: the offset is 0, which used to
        // read as "no handover" and let the primary back in.
        assert_eq!(read(&clock, &handover, || None, || 4_000), 4_000);
        assert_eq!(handover.offset(), Some(0));
        assert_eq!(read(&clock, &handover, || Some(1_001), || 4_100), 4_100);
    }

    #[test]
    fn racing_handovers_agree_on_the_first_offset() {
        let handover = Handover::new();
        assert_eq!(handover.hand_over(10_000, 3_000), 7_000);
        // A second CPU computed its own offset from a later fallback reading.
        assert_eq!(handover.hand_over(10_000, 3_500), 7_000);
        assert_eq!(handover.offset(), Some(7_000));
    }

    #[test]
    fn concurrent_reads_across_a_handover_never_regress() {
        let clock = Arc::new(Watermark::new());
        let handover = Arc::new(Handover::new());
        let threads: Vec<_> = (0..4u64)
            .map(|cpu| {
                let (clock, handover) = (Arc::clone(&clock), Arc::clone(&handover));
                thread::spawn(move || {
                    let mut last = 0;
                    for step in 0..10_000u64 {
                        // The primary runs at 1_000_000 + step until step 5_000, where it
                        // re-bases to step (and reports the step back once); the fallback
                        // runs on its own scale at 10 + step.
                        let primary = move || match step {
                            0..5_000 => Some(1_000_000 + step),
                            5_000 => None,
                            _ => Some(step + cpu),
                        };
                        let now = read(&clock, &handover, primary, move || 10 + step);
                        assert!(now >= last, "cpu {cpu}: {now} < {last}");
                        last = now;
                    }
                    last
                })
            })
            .collect();
        for t in threads {
            // Every CPU's clock kept advancing past the old epoch after the handover.
            assert!(t.join().unwrap() > 1_000_000 + 4_000);
        }
        assert!(handover.offset().is_some());
    }
}
