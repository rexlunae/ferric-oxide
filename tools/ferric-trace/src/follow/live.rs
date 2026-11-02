//! The `--live` view of `ferric-trace follow`: one line a second with each event's records
//! so far and its rate over the last second, and the records lost.
//!
//! ```text
//! live t_s=12.0 records=48211 rate=4031 lost=0 syscall.enter=20012/1673 syscall.exit=20011/1673 sched.switch=812/71
//! ```
//!
//! `t_s` counts seconds since the follower found the rings, `records` the records read so
//! far and `rate` those of the last interval per second. `lost` is the follower's estimate of
//! the records the producer overwrote before it could read them. Each `<event>=<total>/<rate>`
//! pair is an event with records in the last interval, the busiest first; records of events
//! the catalog does not know are counted as `unknown`.

use ferric_trace::EventId;
use ferric_trace::kernel::CATALOG;

/// Nanoseconds between two lines.
pub const LIVE_INTERVAL_NS: u64 = 1_000_000_000;

/// Per-event counts for the `--live` lines.
#[derive(Debug)]
pub struct LiveCounts {
    /// Records so far, by event index; the last slot is for unknown events.
    totals: Vec<u64>,
    /// Records since the last line, by event index.
    interval: Vec<u64>,
    /// When the follower started, in host nanoseconds.
    start_ns: u64,
    /// When the last line was made (or the follower started).
    last_ns: u64,
}

impl LiveCounts {
    /// Counts from host time `now_ns` on.
    pub fn new(now_ns: u64) -> Self {
        Self {
            totals: vec![0; CATALOG.len() + 1],
            interval: vec![0; CATALOG.len() + 1],
            start_ns: now_ns,
            last_ns: now_ns,
        }
    }

    /// Counts one record of `event`.
    pub fn record(&mut self, event: EventId) {
        let index = CATALOG
            .get(event)
            .map_or(CATALOG.len(), |event| event.id().index());
        if let (Some(total), Some(interval)) =
            (self.totals.get_mut(index), self.interval.get_mut(index))
        {
            *total += 1;
            *interval += 1;
        }
    }

    /// The line for the interval up to `now_ns` (with `lost` records lost so far) once
    /// [`LIVE_INTERVAL_NS`] has passed since the last one, and starts the next interval;
    /// `None` before.
    pub fn line(&mut self, now_ns: u64, lost: u64) -> Option<String> {
        let elapsed_ns = now_ns.checked_sub(self.last_ns)?;
        (elapsed_ns >= LIVE_INTERVAL_NS).then(|| self.take_line(now_ns, elapsed_ns, lost))
    }

    fn take_line(&mut self, now_ns: u64, elapsed_ns: u64, lost: u64) -> String {
        let per_s = |count: u64| (count as f64 * 1e9 / elapsed_ns as f64).round() as u64;
        let total: u64 = self.totals.iter().sum();
        let recent: u64 = self.interval.iter().sum();
        let mut line = format!(
            "live t_s={:.1} records={total} rate={} lost={lost}",
            now_ns.saturating_sub(self.start_ns) as f64 / 1e9,
            per_s(recent),
        );
        let mut busy: Vec<(usize, u64)> = self
            .interval
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, count)| count > 0)
            .collect();
        busy.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (index, count) in busy {
            let name = CATALOG
                .events()
                .get(index)
                .map_or("unknown", |event| event.name());
            let total = self.totals.get(index).copied().unwrap_or(0);
            line.push_str(&format!(" {name}={total}/{}", per_s(count)));
        }
        self.interval.fill(0);
        self.last_ns = now_ns;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferric_trace::kernel::{SCHED_SWITCH, SYSCALL_ENTER, SYSCALL_EXIT};

    #[test]
    fn a_line_a_second_busiest_first() {
        let mut live = LiveCounts::new(5_000_000_000);
        for _ in 0..3 {
            live.record(SYSCALL_ENTER.id());
            live.record(SYSCALL_EXIT.id());
        }
        live.record(SCHED_SWITCH.id());
        live.record(EventId::new(u16::MAX));
        assert_eq!(live.line(5_900_000_000, 0), None, "not a second yet");
        assert_eq!(
            live.line(7_000_000_000, 4).as_deref(),
            Some(
                "live t_s=2.0 records=8 rate=4 lost=4 syscall.enter=3/2 syscall.exit=3/2 \
                 sched.switch=1/1 unknown=1/1"
            )
        );
        live.record(SCHED_SWITCH.id());
        assert_eq!(
            live.line(8_000_000_000, 4).as_deref(),
            Some("live t_s=3.0 records=9 rate=1 lost=4 sched.switch=2/1")
        );
        assert_eq!(
            live.line(9_000_000_000, 4).as_deref(),
            Some("live t_s=4.0 records=9 rate=0 lost=4")
        );
    }

    #[test]
    fn a_clock_that_goes_back_makes_no_line() {
        let mut live = LiveCounts::new(5_000_000_000);
        assert_eq!(live.line(1, 0), None);
    }
}
