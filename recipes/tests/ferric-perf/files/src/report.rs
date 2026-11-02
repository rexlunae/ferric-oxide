//! The `FERRIC_BENCH` line of a measurement.
//!
//! One line per measured phase, as space-separated `key=value` fields after the benchmark's
//! name, like the other benchmarks' lines, so a script or `ferric-harness` matches them with a
//! regular expression:
//!
//! ```text
//! FERRIC_BENCH perf-pipe-throughput iters=50 mean_ns=1234567 median_ns=1230000 p99_ns=1400000 min_ns=1200000 max_ns=1500000 bytes=4194304 mib_s=3240.1
//! ```
//!
//! - `iters` is the timed iterations: fewer than planned when the time budget ran out.
//! - `mean_ns` is exact; `median_ns` and `p99_ns` are percentiles of `ferric_iobench`'s
//!   fio-layout [`Histogram`] (the middle of the bucket the rank falls in, within 1/64 of the
//!   true value, as fio and `ferric-trace counts` report them); `min_ns` and `max_ns` are exact.
//!   The rank is the nearest rank, rounded up: the median of an even count is the lower
//!   middle sample, and with fewer than 100 samples `p99_ns` is the slowest one, `max_ns`.
//! - A line can add integer fields of its own ([`Line::extra`]): `perf-clock` adds `res_ns`,
//!   the clock's resolution.
//! - A phase that moves data adds `bytes`, per iteration, and `mib_s`, the bytes of an
//!   iteration over its mean time in MiB per second with one decimal.

use std::fmt;

use ferric_iobench::latency::Histogram;

/// The prefix of every benchmark name in this program's lines.
pub const PREFIX: &str = "perf-";

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// One `FERRIC_BENCH` line.
#[derive(Clone, Copy, Debug)]
pub struct Line<'a> {
    /// The measurement's name, without [`PREFIX`].
    pub name: &'a str,
    /// The phase's times.
    pub latency: &'a Histogram,
    /// Bytes each iteration moves, for a throughput.
    pub bytes: Option<u64>,
    /// More `key=value` fields, printed after `max_ns`.
    pub extra: &'a [(&'static str, u64)],
}

impl Line<'_> {
    /// Tenths of a MiB per second: [`Line::bytes`] over the mean time, rounded down; `None`
    /// without bytes, 0 when the mean is 0.
    pub fn mib_s_tenths(&self) -> Option<u64> {
        let bytes = self.bytes?;
        let mean = u128::from(self.latency.mean());
        if mean == 0 {
            return Some(0);
        }
        let tenths = (u128::from(bytes) * NANOS_PER_SEC * 10 / mean) >> 20;
        Some(u64::try_from(tenths).unwrap_or(u64::MAX))
    }
}

impl fmt::Display for Line<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lat = self.latency;
        write!(
            f,
            "FERRIC_BENCH {PREFIX}{} iters={} mean_ns={} median_ns={} p99_ns={} min_ns={} \
             max_ns={}",
            self.name,
            lat.count(),
            lat.mean(),
            lat.percentile(500_000),
            lat.percentile(990_000),
            lat.min(),
            lat.max(),
        )?;
        for (key, value) in self.extra {
            write!(f, " {key}={value}")?;
        }
        if let (Some(bytes), Some(tenths)) = (self.bytes, self.mib_s_tenths()) {
            write!(f, " bytes={bytes} mib_s={}.{}", tenths / 10, tenths % 10)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histogram(samples: &[u64]) -> Histogram {
        let mut h = Histogram::new();
        for &ns in samples {
            h.record(ns);
        }
        h
    }

    #[test]
    fn a_latency_line() {
        // 1..=100 µs: the median and p99 fall in the 50 and 99 µs buckets.
        let samples: Vec<u64> = (1..=100).map(|us| us * 1000).collect();
        let latency = histogram(&samples);
        let line = Line {
            name: "spawn-posix-spawn-static",
            latency: &latency,
            bytes: None,
            extra: &[],
        };
        assert_eq!(
            line.to_string(),
            format!(
                "FERRIC_BENCH perf-spawn-posix-spawn-static iters=100 mean_ns=50500 \
                 median_ns={} p99_ns={} min_ns=1000 max_ns=100000",
                latency.percentile(500_000),
                latency.percentile(990_000)
            )
        );
        let close = |got: u64, want: u64| got.abs_diff(want) * 64 <= want;
        assert!(close(latency.percentile(500_000), 50_000));
        assert!(close(latency.percentile(990_000), 99_000));
        assert_eq!(line.mib_s_tenths(), None);
    }

    #[test]
    fn a_throughput_line() {
        // 4 MiB in 1 ms each time is 4000 MiB/s.
        let latency = histogram(&[1_000_000, 1_000_000]);
        let line = Line {
            name: "pipe-throughput",
            latency: &latency,
            bytes: Some(4 << 20),
            extra: &[],
        };
        assert_eq!(line.mib_s_tenths(), Some(40_000));
        assert_eq!(
            line.to_string(),
            "FERRIC_BENCH perf-pipe-throughput iters=2 mean_ns=1000000 median_ns=1000000 \
             p99_ns=1000000 min_ns=1000000 max_ns=1000000 bytes=4194304 mib_s=4000.0"
        );
        // 70 bytes in 3 µs: 22.25 MiB/s, rounded down to a tenth.
        let latency = histogram(&[3000]);
        let line = Line {
            name: "write-null",
            latency: &latency,
            bytes: Some(70),
            extra: &[],
        };
        assert_eq!(line.mib_s_tenths(), Some(222));
    }

    #[test]
    fn extra_fields_follow_the_distribution() {
        let latency = histogram(&[40, 60]);
        let line = Line {
            name: "clock",
            latency: &latency,
            bytes: None,
            extra: &[("res_ns", 1)],
        };
        assert_eq!(
            line.to_string(),
            "FERRIC_BENCH perf-clock iters=2 mean_ns=50 median_ns=40 p99_ns=60 min_ns=40 \
             max_ns=60 res_ns=1"
        );
    }

    #[test]
    fn few_samples_take_the_nearest_rank() {
        // Ten samples: the median is the fifth, and p99 is the slowest.
        let latency = histogram(&(1..=10).collect::<Vec<u64>>());
        assert_eq!(latency.percentile(500_000), 5);
        assert_eq!(latency.percentile(990_000), 10);
    }

    #[test]
    fn an_empty_or_instant_phase_reads_zero() {
        let empty = Histogram::new();
        let line = Line {
            name: "clock",
            latency: &empty,
            bytes: Some(1),
            extra: &[],
        };
        assert_eq!(line.mib_s_tenths(), Some(0));
        assert_eq!(
            line.to_string(),
            "FERRIC_BENCH perf-clock iters=0 mean_ns=0 median_ns=0 p99_ns=0 min_ns=0 max_ns=0 \
             bytes=1 mib_s=0.0"
        );
    }
}
