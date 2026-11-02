//! Host timestamps.
//!
//! Every serial line, QMP event and sample carries a [`Stamp`]: the host's
//! `CLOCK_MONOTONIC` time, which `ferric-trace follow` also writes to its `host-clock` file so
//! the two can be lined up, and the wall-clock time for people reading a log later.

use std::time::{SystemTime, UNIX_EPOCH};

use rustix::time::{ClockId, clock_gettime};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// A host time: monotonic and wall clock, both in nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Stamp {
    /// `CLOCK_MONOTONIC` in nanoseconds.
    pub mono_ns: u64,
    /// Nanoseconds since the Unix epoch.
    pub wall_ns: u64,
}

impl Stamp {
    /// The current time.
    pub fn now() -> Self {
        let mono = clock_gettime(ClockId::Monotonic);
        let mono_ns = u64::try_from(mono.tv_sec).unwrap_or(0) * 1_000_000_000
            + u64::try_from(mono.tv_nsec).unwrap_or(0);
        let wall_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        Self { mono_ns, wall_ns }
    }

    /// Milliseconds from `origin` to this stamp on the monotonic clock (negative when this
    /// stamp is earlier).
    pub fn ms_since(&self, origin: Stamp) -> f64 {
        (self.mono_ns as f64 - origin.mono_ns as f64) / 1e6
    }

    /// The wall-clock time as RFC 3339 (UTC), or the raw nanoseconds when out of range.
    pub fn wall_rfc3339(&self) -> String {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(self.wall_ns))
            .ok()
            .and_then(|t| t.format(&Rfc3339).ok())
            .unwrap_or_else(|| format!("{}ns", self.wall_ns))
    }
}

/// Rounds milliseconds to microsecond precision, for reports.
pub fn round_ms(ms: f64) -> f64 {
    (ms * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_and_formatted() {
        let a = Stamp::now();
        let b = Stamp::now();
        assert!(b.mono_ns >= a.mono_ns);
        assert!(b.ms_since(a) >= 0.0);
        let s = Stamp {
            mono_ns: 0,
            wall_ns: 1_700_000_000_123_000_000,
        };
        assert_eq!(s.wall_rfc3339(), "2023-11-14T22:13:20.123Z");
        assert_eq!(round_ms(1.234_567_8), 1.235);
    }
}
