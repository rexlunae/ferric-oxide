//! A job's results: IOPS, bandwidth and latency, and their `FERRIC_BENCH` fields.
//!
//! The figures mean what fio's do for the same job:
//!
//! - IOPS is reads completed over the wall-clock time from the first submission to the last
//!   completion, across every submitter (fio's `--group_reporting` `IOPS=`).
//! - Bandwidth is bytes over the same time, in MiB/s (fio's `BW=` in `MiB/s`, with
//!   `kb_base=1024`).
//! - Latency is per read, from just before the read is issued to just after it returns
//!   (fio's `clat`; with a synchronous engine fio's submission latency is near zero and `lat`
//!   and `clat` agree). Percentiles come from the [`Histogram`].
//!
//! [`Summary`]'s `Display` prints them as space-separated `key=value` fields, integers only,
//! so a CI script can match them with a regular expression:
//!
//! `ios=1024 iops=51200 mib_s=200.0 lat_min_ns=… lat_mean_ns=… lat_p50_ns=… lat_p99_ns=…
//! lat_p999_ns=… lat_max_ns=…`

use core::fmt;

use crate::latency::Histogram;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// What one run of a job measured.
#[derive(Clone, Copy, Debug)]
pub struct Summary<'a> {
    /// Bytes per read.
    pub bs: u32,
    /// Wall-clock time of the run, first submission to last completion, in nanoseconds.
    pub elapsed_ns: u64,
    /// The latency of every read, merged across submitters. Its count is the reads done.
    pub latency: &'a Histogram,
}

impl Summary<'_> {
    /// Reads completed.
    pub const fn ios(&self) -> u64 {
        self.latency.count()
    }

    /// Reads per second, rounded down (0 for a run that took no time).
    pub fn iops(&self) -> u64 {
        per_second(u128::from(self.ios()), self.elapsed_ns)
    }

    /// Bandwidth in tenths of a MiB per second, rounded down (0 for a run that took no time).
    pub fn mib_s_tenths(&self) -> u64 {
        let bytes = u128::from(self.ios()) * u128::from(self.bs);
        per_second(bytes * 10, self.elapsed_ns) >> 20
    }
}

/// `amount` per second over `elapsed_ns`, rounded down and saturating.
fn per_second(amount: u128, elapsed_ns: u64) -> u64 {
    match elapsed_ns {
        0 => 0,
        ns => u64::try_from(amount * NANOS_PER_SEC / u128::from(ns)).unwrap_or(u64::MAX),
    }
}

impl fmt::Display for Summary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lat = self.latency;
        let mib = self.mib_s_tenths();
        write!(
            f,
            "ios={} iops={} mib_s={}.{} lat_min_ns={} lat_mean_ns={} lat_p50_ns={} \
             lat_p99_ns={} lat_p999_ns={} lat_max_ns={}",
            self.ios(),
            self.iops(),
            mib / 10,
            mib % 10,
            lat.min(),
            lat.mean(),
            lat.percentile(500_000),
            lat.percentile(990_000),
            lat.percentile(999_000),
            lat.max(),
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::*;

    #[test]
    fn figures_follow_the_counts_and_time() {
        let mut latency = Histogram::new();
        for _ in 0..1000 {
            latency.record(20_000);
        }
        // 1000 reads of 4 KiB in 50 ms.
        let summary = Summary {
            bs: 4096,
            elapsed_ns: 50_000_000,
            latency: &latency,
        };
        assert_eq!(summary.iops(), 20_000);
        // 4,096,000 bytes in 0.05 s is 78.125 MiB/s.
        assert_eq!(summary.mib_s_tenths(), 781);
        assert_eq!(
            summary.to_string(),
            "ios=1000 iops=20000 mib_s=78.1 lat_min_ns=20000 lat_mean_ns=20000 \
             lat_p50_ns=20000 lat_p99_ns=20000 lat_p999_ns=20000 lat_max_ns=20000"
        );
    }

    #[test]
    fn a_run_that_took_no_time_reads_zero() {
        let latency = Histogram::new();
        let summary = Summary {
            bs: 4096,
            elapsed_ns: 0,
            latency: &latency,
        };
        assert_eq!(
            (summary.ios(), summary.iops(), summary.mib_s_tenths()),
            (0, 0, 0)
        );
    }
}
