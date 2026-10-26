//! The counts view: one summary line per event in a capture.
//!
//! For every event with records, it prints how many records there were, on each CPU and
//! merged across all of them, and the rate over the time from the capture's first record to
//! its last. With `--gap <event>`, it also prints the gaps between consecutive records of
//! that event on the same CPU (the delay from one record of the kind to the next), with the
//! fio-style percentiles of `ferric_iobench`'s `Histogram`, in nanoseconds — the same
//! distribution code the benchmark jobs report with.
//!
//! The scheduler baseline uses it to turn a capture taken around a workload window into
//! measured numbers: `sched.tick` and `sched.ipi_pit` count the periodic timer interrupts
//! and their jitter, `sched.switch` counts context switches, and `sched.ipi_wake` counts
//! cross-CPU wakeups. The lines are integer-only `key=value` fields, so a script can match
//! them with a regular expression, like the `FERRIC_BENCH` lines:
//!
//! ```text
//! counts sched.tick cpu=0 count=2440 rate_per_s=243
//! counts sched.tick cpu=all count=2440 rate_per_s=243 gap_ns_min=4050000 gap_ns_mean=4053000
//!         gap_ns_p50=4052000 gap_ns_p99=4073000 gap_ns_p999=4100000 gap_ns_max=12340000
//! ```
//!
//! One line per CPU, then a merged `cpu=all` line per event. The merged line carries the
//! events' same-CPU gaps merged across CPUs when `--gap` asked for them. When the input is a
//! capture directory and it holds the drain's `stats.start` and `stats.end` snapshots, each
//! CPU line also carries `lost_before` (records dropped to the `stats.start` snapshot) and
//! `lost_during` (dropped while draining), so a window that overflowed its ring reports the
//! loss instead of silently undercounting.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ferric_iobench::latency::Histogram;
use ferric_trace::abi::StatsLine;

use crate::capture::Capture;
use crate::perfetto::{TickRate, TscMhz};

/// One event's counts and, when asked for, its same-CPU gaps.
struct EventCounts {
    /// Records per CPU, in CPU order.
    per_cpu: BTreeMap<u32, u64>,
    /// Records in all.
    total: u64,
    /// First and last records' timestamps, in ticks of the capture's clock.
    first: u64,
    last: u64,
    /// Nanosecond gaps per CPU, when `--gap` named this event.
    gaps: BTreeMap<u32, Histogram>,
    /// Timestamp of each CPU's previous record of this event, in ticks.
    previous: BTreeMap<u32, u64>,
}

impl EventCounts {
    fn new() -> Self {
        Self {
            per_cpu: BTreeMap::new(),
            total: 0,
            first: u64::MAX,
            last: 0,
            gaps: BTreeMap::new(),
            previous: BTreeMap::new(),
        }
    }

    /// Records one `entry`, and the gap from the CPU's previous record of the same event
    /// when gaps are being collected.
    fn record(&mut self, entry: &crate::capture::Entry, rate: TickRate, gaps_on: bool) {
        let cpu = entry.cpu;
        *self.per_cpu.entry(cpu).or_insert(0) += 1;
        self.total += 1;
        self.first = self.first.min(entry.timestamp);
        self.last = self.last.max(entry.timestamp);
        if gaps_on {
            if let Some(&previous) = self.previous.get(&cpu) {
                self.gaps
                    .entry(cpu)
                    .or_default()
                    .record(ticks_to_ns(entry.timestamp.saturating_sub(previous), rate));
            }
            self.previous.insert(cpu, entry.timestamp);
        }
    }

    /// The event's rate over `elapsed_ns`, per second, rounded down.
    fn rate(count: u64, elapsed_ns: u64) -> u64 {
        match elapsed_ns {
            0 => 0,
            ns => u64::try_from(u128::from(count) * 1_000_000_000 / u128::from(ns))
                .unwrap_or(u64::MAX),
        }
    }

    /// The gaps of all CPUs, merged.
    fn merged_gaps(&self) -> Histogram {
        let mut merged = Histogram::new();
        for gaps in self.gaps.values() {
            merged.merge(gaps);
        }
        merged
    }
}

/// Ticks of a capture's clock, as nanoseconds.
fn ticks_to_ns(ticks: u64, rate: TickRate) -> u64 {
    (ticks as f64 / rate.get() * 1000.0).round() as u64
}

/// The gap fields of `gaps`, as `key=value` fields.
fn gap_fields(gaps: &Histogram) -> String {
    format!(
        "gap_ns_min={} gap_ns_mean={} gap_ns_p50={} gap_ns_p99={} gap_ns_p999={} gap_ns_max={}",
        gaps.min(),
        gaps.mean(),
        gaps.percentile(500_000),
        gaps.percentile(990_000),
        gaps.percentile(999_000),
        gaps.max(),
    )
}

/// Each CPU's `(lost_before, lost_during)` from the `stats.start` and `stats.end` snapshots
/// of the capture directories in `inputs`; empty when a directory has none.
pub fn losses(inputs: &[PathBuf]) -> BTreeMap<u32, (u64, u64)> {
    fn snapshot(path: &Path) -> Option<BTreeMap<u32, u64>> {
        let text = fs::read_to_string(path).ok()?;
        let mut by_cpu = BTreeMap::new();
        for line in text.lines() {
            if let Ok(stats) = StatsLine::parse(line) {
                by_cpu.insert(stats.cpu, stats.lost);
            }
        }
        Some(by_cpu)
    }
    let mut losses = BTreeMap::new();
    for input in inputs {
        if !input.is_dir() {
            continue;
        }
        let (Some(before), Some(after)) = (
            snapshot(&input.join("stats.start")),
            snapshot(&input.join("stats.end")),
        ) else {
            continue;
        };
        for (cpu, &lost) in &after {
            let lost_before = before.get(cpu).copied().unwrap_or(0);
            losses.insert(*cpu, (lost_before, lost.saturating_sub(lost_before)));
        }
    }
    losses
}

/// Writes the counts view of `capture` to `out`: one line per event and CPU, then one
/// merged line per event, as documented in the module. `losses` carries each CPU's
/// `(lost_before, lost_during)` from the capture directories' `stats` snapshots (see
/// [`losses`]) and is printed on the CPU's lines when present.
///
/// # Errors
///
/// The error of a failed write.
pub fn write(
    capture: &Capture,
    gaps: &[&str],
    losses: &BTreeMap<u32, (u64, u64)>,
    tsc_mhz: TscMhz,
    out: &mut impl Write,
) -> io::Result<()> {
    let rate = TickRate::for_clock(capture.clock(), tsc_mhz);
    let mut counts: BTreeMap<&'static str, EventCounts> = BTreeMap::new();
    let mut first = u64::MAX;
    let mut last = 0;
    for entry in capture.entries() {
        let gaps_on = gaps.iter().any(|name| *name == entry.event.name());
        counts
            .entry(entry.event.name())
            .or_insert_with(EventCounts::new)
            .record(entry, rate, gaps_on);
        first = first.min(entry.timestamp);
        last = last.max(entry.timestamp);
    }
    let elapsed_ns = ticks_to_ns(last.saturating_sub(first), rate);
    for (name, counts) in &counts {
        for (&cpu, &count) in &counts.per_cpu {
            write!(
                out,
                "counts {name} cpu={cpu} count={count} rate_per_s={}",
                EventCounts::rate(count, elapsed_ns)
            )?;
            if let Some(gaps) = counts.gaps.get(&cpu) {
                write!(out, " {}", gap_fields(gaps))?;
            }
            if let Some(&(lost_before, lost_during)) = losses.get(&cpu) {
                write!(out, " lost_before={lost_before} lost_during={lost_during}")?;
            }
            writeln!(out)?;
        }
        let mut line = format!(
            "counts {name} cpu=all count={} rate_per_s={}",
            counts.total,
            EventCounts::rate(counts.total, elapsed_ns)
        );
        if !counts.gaps.is_empty() {
            line.push(' ');
            line.push_str(&gap_fields(&counts.merged_gaps()));
        }
        writeln!(out, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferric_trace::EventId;
    use ferric_trace::clock::ClockLine;
    use ferric_trace::kernel::{CATALOG, SCHED_IPI_WAKE, SCHED_SWITCH, SCHED_TICK};

    use crate::capture::tests::bytes as record_bytes;
    use crate::capture::{Capture, CpuCapture};

    /// A capture of `records` on `cpu`, with the clock given.
    fn capture(clock: Option<ClockLine>, cpus: &[(&[u8], u32)]) -> Capture {
        let cpus: Vec<CpuCapture> = cpus
            .iter()
            .map(|(bytes, cpu)| CpuCapture::decode(*cpu, bytes).expect("valid capture"))
            .collect();
        Capture::from_cpus(cpus).with_clock(clock)
    }

    /// A clock whose tick is one nanosecond.
    fn clock() -> ClockLine {
        ClockLine::parse("clock tsc period_fs 1000000\n").expect("clock line")
    }

    #[test]
    fn catalog_has_the_baseline_events() {
        assert_eq!(
            CATALOG.find("sched.tick").map(|e| e.id()),
            Some(SCHED_TICK.id())
        );
        assert_eq!(
            CATALOG.find("sched.ipi_pit").map(|e| e.id()),
            SCHED_TICK.id().get().checked_add(1).map(EventId::new)
        );
        assert_eq!(
            CATALOG.find("sched.ipi_wake").map(|e| e.id()),
            Some(SCHED_IPI_WAKE.id())
        );
        assert_eq!(
            CATALOG.find("sched.switch").map(|e| e.id()),
            Some(SCHED_SWITCH.id())
        );
    }

    #[test]
    fn counts_rates_and_gaps() {
        // CPU 0: switches at 1 s and 4 s (gap 3 s), a tick at 2 s.
        let cpu0 = record_bytes(&[
            (&SCHED_SWITCH, 1_000_000_000, &[1, 2, 1, 2]),
            (&SCHED_TICK, 2_000_000_000, &[]),
            (&SCHED_SWITCH, 4_000_000_000, &[2, 1, 2, 1]),
        ]);
        // CPU 1: one tick at 3 s.
        let cpu1 = record_bytes(&[(&SCHED_TICK, 3_000_000_000, &[])]);
        let capture = capture(Some(clock()), &[(&cpu0, 0), (&cpu1, 1)]);
        let mut out = Vec::new();
        write(
            &capture,
            &["sched.tick", "sched.switch"],
            &BTreeMap::new(),
            TscMhz::new(1000.0).expect("mhz"),
            &mut out,
        )
        .expect("write");
        let text = String::from_utf8(out).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        // Two switches in the capture's 3 s, one tick per CPU in it.
        assert_eq!(
            lines,
            vec![
                "counts sched.switch cpu=0 count=2 rate_per_s=0 gap_ns_min=3000000000 \
                 gap_ns_mean=3000000000 gap_ns_p50=3000000000 gap_ns_p99=3000000000 \
                 gap_ns_p999=3000000000 gap_ns_max=3000000000",
                "counts sched.switch cpu=all count=2 rate_per_s=0 gap_ns_min=3000000000 \
                 gap_ns_mean=3000000000 gap_ns_p50=3000000000 gap_ns_p99=3000000000 \
                 gap_ns_p999=3000000000 gap_ns_max=3000000000",
                "counts sched.tick cpu=0 count=1 rate_per_s=0",
                "counts sched.tick cpu=1 count=1 rate_per_s=0",
                "counts sched.tick cpu=all count=2 rate_per_s=0",
            ]
        );
    }

    #[test]
    fn per_cpu_rates_use_each_cpu_count_and_merged_rates_use_the_total() {
        let cpu0 = record_bytes(&[
            (&SCHED_TICK, 0, &[]),
            (&SCHED_TICK, 500_000_000, &[]),
            (&SCHED_TICK, 1_000_000_000, &[]),
        ]);
        let cpu1 = record_bytes(&[(&SCHED_TICK, 500_000_000, &[])]);
        let capture = capture(Some(clock()), &[(&cpu0, 0), (&cpu1, 1)]);
        let mut out = Vec::new();
        write(
            &capture,
            &[],
            &BTreeMap::new(),
            TscMhz::new(1000.0).unwrap(),
            &mut out,
        )
        .unwrap();
        let output = String::from_utf8(out).unwrap();
        assert_eq!(
            output,
            "counts sched.tick cpu=0 count=3 rate_per_s=3\ncounts sched.tick cpu=1 count=1 rate_per_s=1\ncounts sched.tick cpu=all count=4 rate_per_s=4\n"
        );
    }

    #[test]
    fn rates_over_the_capture() {
        // Two switches in the capture's two seconds: one per second, and an ipi_wake.
        let cpu0 = record_bytes(&[
            (&SCHED_SWITCH, 0, &[1, 2, 1, 2]),
            (&SCHED_IPI_WAKE, 1_000_000_000, &[2]),
            (&SCHED_SWITCH, 2_000_000_000, &[2, 1, 2, 1]),
        ]);
        let capture = capture(Some(clock()), &[(&cpu0, 0)]);
        let mut out = Vec::new();
        write(
            &capture,
            &[],
            &BTreeMap::new(),
            TscMhz::new(1000.0).expect("mhz"),
            &mut out,
        )
        .expect("write");
        let text = String::from_utf8(out).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "counts sched.ipi_wake cpu=0 count=1 rate_per_s=0",
                "counts sched.ipi_wake cpu=all count=1 rate_per_s=0",
                "counts sched.switch cpu=0 count=2 rate_per_s=1",
                "counts sched.switch cpu=all count=2 rate_per_s=1",
            ]
        );
    }

    #[test]
    fn losses_come_from_the_capture_snapshots() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("stats.start"),
            "cpu 0 pending 0 lost 5\ncpu 1 pending 0 lost 1\n",
        )
        .expect("write");
        fs::write(
            dir.path().join("stats.end"),
            "cpu 0 pending 0 lost 7\ncpu 1 pending 0 lost 1\n",
        )
        .expect("write");
        let losses = losses(&[dir.path().to_path_buf()]);
        assert_eq!(losses.get(&0), Some(&(5, 2)));
        assert_eq!(losses.get(&1), Some(&(1, 0)));
        assert_eq!(losses.len(), 2);
    }

    #[test]
    fn clock_falls_back_to_the_given_tsc() {
        // No clock line: the rates assume the given TSC frequency. With a 1 GHz host clock
        // the 2e9-tick capture is two seconds, so one switch per second.
        let cpu0 = record_bytes(&[
            (&SCHED_SWITCH, 0, &[1, 2, 1, 2]),
            (&SCHED_SWITCH, 2_000_000_000, &[2, 1, 2, 1]),
        ]);
        let capture = capture(None, &[(&cpu0, 0)]);
        let mut out = Vec::new();
        write(
            &capture,
            &[],
            &BTreeMap::new(),
            TscMhz::new(1000.0).expect("mhz"),
            &mut out,
        )
        .expect("write");
        assert!(
            String::from_utf8(out)
                .expect("utf-8")
                .contains("counts sched.switch cpu=all count=2 rate_per_s=1")
        );
    }
}
