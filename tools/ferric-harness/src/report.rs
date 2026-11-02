//! `report.json` and the `report` command.
//!
//! Every run ends by writing a [`Report`] to `<run>/report.json`: the environment (commit,
//! QEMU version and command line, host CPU, image), the options, the verdict and every check
//! behind it, the boot milestones (host milliseconds since QEMU started), the test markers
//! and benchmark records with their times, the marks, the serial counters, the stats
//! summary (per-vCPU thread CPU time and KVM counters), trace captures and scenario steps.
//!
//! [`metrics`] flattens a report into named numbers: `milestone.<name>`, `bench.<series>.
//! <field>` (the first `FERRIC_BENCH` line of each series in the run; `_SAMPLE` lines are
//! rounds, not results), `stats.*` and `kvm.<counter>` (summed over vCPUs). [`table`] prints
//! runs side by side, and [`paired`] compares two groups of runs the way
//! `scripts/ferric-ci/check-trace-overhead.sh` does: the Nth run of A and of B are a pair
//! (two boots run back to back, which see about the same host load), each pair's percentage
//! difference is computed, and the median of those differences is the result, so one
//! unlucky pair cannot decide it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::machine::LaunchConfig;
use crate::markers::{BenchRecord, Marker};
use crate::observe::{Facts, HostRings};
use crate::serial::SerialCounts;
use crate::stats::StatsSummary;
use crate::trace::FollowExit;
use crate::verdict::{Ending, Outcome, ScenarioSummary, Verdict};

/// The report format version.
pub const SCHEMA: u32 = 1;

/// Everything a run produced, as written to `report.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// [`SCHEMA`].
    pub schema: u32,
    /// The run.
    pub run: RunInfo,
    /// Where it ran.
    pub environment: Environment,
    /// Its options.
    pub options: LaunchConfig,
    /// Pass or fail, and why.
    pub verdict: Verdict,
    /// How it ended.
    pub ending: Ending,
    /// What the console showed.
    pub facts: Facts,
    /// Milestone name to milliseconds since QEMU started.
    pub milestones: BTreeMap<String, f64>,
    /// Test and smoke markers.
    pub markers: Vec<TimedMarker>,
    /// Benchmark records.
    pub bench: Vec<TimedBench>,
    /// Marks set during the run.
    pub marks: Vec<Mark>,
    /// Serial counters.
    pub serial: SerialCounts,
    /// QMP events by name.
    pub qmp_events: BTreeMap<String, usize>,
    /// Host CPU and KVM statistics.
    pub stats: StatsSummary,
    /// Host trace captures (trace mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceInfo>,
    /// Scenario steps (`run --scenario`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<ScenarioReport>,
}

/// The run itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    /// Its name.
    pub name: String,
    /// Its directory.
    pub dir: PathBuf,
    /// When QEMU started (RFC 3339, UTC).
    pub started: String,
    /// QEMU's start on the host's `CLOCK_MONOTONIC`, in ns, for lining up with
    /// `ferric-trace follow`'s `host-clock` file.
    pub qemu_start_mono_ns: u64,
    /// From QEMU's start to the end of the run, in ms.
    pub duration_ms: f64,
}

/// Where a run ran.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    /// The checkout's commit.
    pub commit: Option<String>,
    /// Whether tracked files had uncommitted changes.
    pub dirty: Option<bool>,
    /// QEMU.
    pub qemu: QemuInfo,
    /// The host.
    pub host: HostInfo,
    /// The image.
    pub image: ImageInfo,
}

/// QEMU as run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QemuInfo {
    /// The program.
    pub program: PathBuf,
    /// Its version, from the QMP greeting.
    pub version: Option<String>,
    /// `kvm` or `tcg`.
    pub accel: String,
    /// The CPU model.
    pub cpu: String,
    /// The full command line.
    pub args: Vec<String>,
}

/// The host machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    /// The CPU model name.
    pub cpu_model: Option<String>,
    /// Logical CPUs.
    pub cpus: usize,
    /// The kernel release.
    pub kernel: Option<String>,
}

/// The booted image.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// Its path.
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// Its modification time (RFC 3339).
    pub modified: Option<String>,
}

/// A marker with its time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimedMarker {
    /// Milliseconds since QEMU started.
    pub t_ms: f64,
    /// The marker.
    #[serde(flatten)]
    pub marker: Marker,
}

/// A benchmark record with its time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimedBench {
    /// Milliseconds since QEMU started.
    pub t_ms: f64,
    /// The record.
    #[serde(flatten)]
    pub record: BenchRecord,
}

/// A named point in a run, set with `mark`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    /// Its name.
    pub name: String,
    /// Milliseconds since QEMU started.
    pub t_ms: f64,
    /// The console offset when it was set (where `expect --since <name>` starts).
    pub serial_offset: u64,
}

/// Host trace captures.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceInfo {
    /// The rings the kernel announced.
    pub host_rings: Option<HostRings>,
    /// The guest-RAM file `follow` read.
    pub guest_ram: Option<PathBuf>,
    /// Each `follow` run.
    pub captures: Vec<Capture>,
}

/// One `ferric-trace follow` run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    /// The capture directory.
    pub dir: PathBuf,
    /// When it started, ms since QEMU started.
    pub started_ms: f64,
    /// How it ended (absent while running).
    pub exit: Option<FollowExit>,
}

/// One scenario step's result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    /// The step number, from 1.
    pub step: usize,
    /// What it did (`expect "login:"`, `key ret`, ...).
    pub action: String,
    /// Whether it succeeded.
    pub ok: bool,
    /// Start, ms since QEMU started.
    pub started_ms: f64,
    /// How long it took, in ms.
    pub duration_ms: f64,
    /// What it produced or why it failed.
    pub detail: String,
}

/// A scenario's steps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScenarioReport {
    /// The scenario file.
    pub path: PathBuf,
    /// The verdict's view of it.
    pub summary: ScenarioSummary,
    /// Every step that ran.
    pub steps: Vec<StepResult>,
}

impl Report {
    /// Writes the report to `path` (pretty JSON).
    ///
    /// # Errors
    ///
    /// [`Error::Json`] or [`Error::Io`].
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        let text = serde_json::to_string_pretty(self).map_err(|source| Error::Json {
            path: path.to_path_buf(),
            source,
        })?;
        fs::write(path, text + "\n").map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Reads `<dir>/report.json` (or `dir` itself when it is a file).
    ///
    /// # Errors
    ///
    /// [`Error::NoReport`], [`Error::Io`] or [`Error::Json`].
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let path = if dir.is_file() {
            dir.to_path_buf()
        } else {
            dir.join("report.json")
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NoReport(dir.to_path_buf()));
            }
            Err(source) => return Err(Error::Io { path, source }),
        };
        serde_json::from_str(&text).map_err(|source| Error::Json { path, source })
    }
}

/// A report's numbers by name (see the [module docs](self)).
pub fn metrics(report: &Report) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for (name, ms) in &report.milestones {
        out.insert(format!("milestone.{name}"), *ms);
    }
    for bench in report.bench.iter().filter(|b| !b.record.sample) {
        let series = bench.record.series();
        for (field, value) in &bench.record.values {
            out.entry(format!("bench.{series}.{field}"))
                .or_insert(*value);
        }
    }
    let stats = &report.stats;
    if stats.samples > 0 {
        out.insert("stats.qemu_cpu_ms".to_owned(), stats.qemu_cpu_ms);
        let vcpu: f64 = stats.vcpus.iter().filter_map(|v| v.cpu_ms).sum();
        out.insert("stats.vcpu_cpu_ms".to_owned(), vcpu);
        let mut kvm: BTreeMap<String, f64> = BTreeMap::new();
        for v in &stats.vcpus {
            for (k, d) in &v.kvm {
                *kvm.entry(format!("kvm.{k}")).or_default() += d;
            }
        }
        out.extend(kvm);
    }
    out.insert(
        "serial.raw_bytes".to_owned(),
        report.serial.raw_bytes as f64,
    );
    out
}

/// The median of `values` (the mean of the middle two for an even count).
pub fn median(values: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = values.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(v[n / 2]),
        _ => Some((v[n / 2 - 1] + v[n / 2]) / 2.0),
    }
}

/// One metric compared across paired runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairedRow {
    /// The metric.
    pub metric: String,
    /// Median over A's runs.
    pub median_a: f64,
    /// Median over B's runs.
    pub median_b: f64,
    /// Each pair's difference of B from A in percent, in pair order (`None` where A is 0).
    pub pair_pct: Vec<Option<f64>>,
    /// The median of the pairs' differences, in percent.
    pub median_pct: Option<f64>,
}

/// A paired comparison of two groups.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Paired {
    /// Number of pairs.
    pub pairs: usize,
    /// Metrics present in every run of both groups.
    pub rows: Vec<PairedRow>,
    /// Metrics missing from some run, which are not judged on fewer runs.
    pub incomplete: Vec<String>,
}

/// Compares group B against group A pair by pair.
///
/// # Errors
///
/// [`Error::UnpairedGroups`] when the groups differ in size.
pub fn paired(a: &[Report], b: &[Report]) -> Result<Paired, Error> {
    if a.len() != b.len() {
        return Err(Error::UnpairedGroups {
            a: a.len(),
            b: b.len(),
        });
    }
    let ma: Vec<_> = a.iter().map(metrics).collect();
    let mb: Vec<_> = b.iter().map(metrics).collect();
    let mut names: Vec<&String> = ma.iter().chain(&mb).flat_map(BTreeMap::keys).collect();
    names.sort();
    names.dedup();
    let mut rows = Vec::new();
    let mut incomplete = Vec::new();
    for name in names {
        let values = |group: &[BTreeMap<String, f64>]| -> Option<Vec<f64>> {
            group.iter().map(|m| m.get(name).copied()).collect()
        };
        let (Some(va), Some(vb)) = (values(&ma), values(&mb)) else {
            incomplete.push(name.clone());
            continue;
        };
        let pair_pct: Vec<Option<f64>> = va
            .iter()
            .zip(&vb)
            .map(|(x, y)| (*x != 0.0).then(|| (y / x - 1.0) * 100.0))
            .collect();
        let all: Option<Vec<f64>> = pair_pct.iter().copied().collect();
        rows.push(PairedRow {
            metric: name.clone(),
            median_a: median(&va).unwrap_or(f64::NAN),
            median_b: median(&vb).unwrap_or(f64::NAN),
            median_pct: all.and_then(|v| median(&v)),
            pair_pct,
        });
    }
    Ok(Paired {
        pairs: a.len(),
        rows,
        incomplete,
    })
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

/// The runs as a text table: verdict, milestones, test counts, failures, then each metric's
/// value (one run) or median, minimum and maximum (several).
pub fn table(reports: &[(PathBuf, Report)]) -> String {
    let mut out = String::new();
    let milestones = ["kernel", "init", "login", "smoke_ok", "shutdown"];
    write!(out, "{:<34} {:<7}", "run", "verdict").ok();
    for m in milestones {
        write!(out, " {:>11}", format!("{m}_ms")).ok();
    }
    writeln!(out, " {:>11}", "tests p/s/f").ok();
    for (_, r) in reports {
        let verdict = match r.verdict.result {
            Outcome::Pass => "pass",
            Outcome::Fail => "FAIL",
        };
        write!(out, "{:<34} {:<7}", r.run.name, verdict).ok();
        for m in milestones {
            let cell = r
                .milestones
                .get(m)
                .map_or_else(|| "-".to_owned(), |v| format!("{v:.1}"));
            write!(out, " {cell:>11}").ok();
        }
        let f = &r.facts;
        writeln!(
            out,
            " {:>11}",
            format!(
                "{}/{}/{}",
                f.tests_passed,
                f.tests_skipped,
                f.tests_failed.len()
            )
        )
        .ok();
    }
    for (_, r) in reports {
        for c in r.verdict.failures() {
            writeln!(out, "  {}: {} failed: {}", r.run.name, c.name, c.detail).ok();
        }
    }
    let all: Vec<BTreeMap<String, f64>> = reports.iter().map(|(_, r)| metrics(r)).collect();
    let mut names: Vec<&String> = all.iter().flat_map(BTreeMap::keys).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return out;
    }
    writeln!(out).ok();
    if reports.len() == 1 {
        writeln!(out, "{:<60} {:>14}", "metric", "value").ok();
        for name in names {
            writeln!(out, "{name:<60} {:>14}", fmt_num(all[0][name])).ok();
        }
    } else {
        writeln!(
            out,
            "{:<60} {:>4} {:>14} {:>14} {:>14}",
            "metric", "n", "median", "min", "max"
        )
        .ok();
        for name in names {
            let values: Vec<f64> = all.iter().filter_map(|m| m.get(name).copied()).collect();
            let min = values.iter().copied().fold(f64::INFINITY, f64::min);
            let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            writeln!(
                out,
                "{name:<60} {:>4} {:>14} {:>14} {:>14}",
                values.len(),
                fmt_num(median(&values).unwrap_or(f64::NAN)),
                fmt_num(min),
                fmt_num(max)
            )
            .ok();
        }
    }
    out
}

/// A paired comparison as a text table.
pub fn paired_table(p: &Paired) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "B against A over {} pairs (median of each pair's difference)",
        p.pairs
    )
    .ok();
    writeln!(
        out,
        "{:<60} {:>14} {:>14} {:>9}  pairs",
        "metric", "A median", "B median", "paired"
    )
    .ok();
    for row in &p.rows {
        let pct = row
            .median_pct
            .map_or_else(|| "-".to_owned(), |v| format!("{v:+.2}%"));
        let pairs: Vec<String> = row
            .pair_pct
            .iter()
            .map(|v| v.map_or_else(|| "-".to_owned(), |v| format!("{v:+.2}")))
            .collect();
        writeln!(
            out,
            "{:<60} {:>14} {:>14} {:>9}  {}",
            row.metric,
            fmt_num(row.median_a),
            fmt_num(row.median_b),
            pct,
            pairs.join(" ")
        )
        .ok();
    }
    if !p.incomplete.is_empty() {
        writeln!(
            out,
            "not in every run (not compared): {}",
            p.incomplete.join(", ")
        )
        .ok();
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::markers::{TestMarker, TestOutcome};
    use crate::stats::VcpuSummary;
    use crate::verdict::{Check, CheckStatus, StopReason};

    pub(crate) fn sample_report(name: &str, login_ms: f64, iops: f64) -> Report {
        let mut values = BTreeMap::new();
        values.insert("iops".to_owned(), iops);
        let mut keys = BTreeMap::new();
        keys.insert("policy".to_owned(), "main".to_owned());
        let mut milestones = BTreeMap::new();
        milestones.insert("kernel".to_owned(), 900.0);
        milestones.insert("login".to_owned(), login_ms);
        let mut kvm = BTreeMap::new();
        kvm.insert("exits".to_owned(), 1000.0);
        Report {
            schema: SCHEMA,
            run: RunInfo {
                name: name.to_owned(),
                dir: PathBuf::from("/runs").join(name),
                started: "2026-11-01T00:00:00Z".to_owned(),
                qemu_start_mono_ns: 1,
                duration_ms: 5000.0,
            },
            environment: Environment::default(),
            options: LaunchConfig::new("/img".into(), PathBuf::from("/runs").join(name)),
            verdict: Verdict {
                result: Outcome::Pass,
                checks: vec![Check {
                    name: "exit".to_owned(),
                    status: CheckStatus::Pass,
                    detail: "QEMU exited with 0".to_owned(),
                }],
            },
            ending: Ending {
                stop: StopReason::GuestExit,
                qemu_exit: Some(0),
                shutdown_reason: Some("guest-shutdown".to_owned()),
                killed: false,
                powerdown_wait_s: None,
                timeout_s: None,
            },
            facts: Facts::default(),
            milestones,
            markers: vec![TimedMarker {
                t_ms: 1000.0,
                marker: Marker::Test(TestMarker {
                    outcome: TestOutcome::Pass,
                    name: "dd-pipe".to_owned(),
                    detail: None,
                }),
            }],
            bench: vec![
                TimedBench {
                    t_ms: 2000.0,
                    record: BenchRecord {
                        sample: false,
                        name: "iobench-randread-4k-qd1".to_owned(),
                        keys: keys.clone(),
                        values: values.clone(),
                        ..BenchRecord::default()
                    },
                },
                TimedBench {
                    t_ms: 2100.0,
                    record: BenchRecord {
                        sample: true,
                        name: "iobench-randread-4k-qd1".to_owned(),
                        keys,
                        values,
                        ..BenchRecord::default()
                    },
                },
            ],
            marks: Vec::new(),
            serial: SerialCounts::default(),
            qmp_events: BTreeMap::new(),
            stats: StatsSummary {
                samples: 2,
                span_ms: 1000.0,
                qemu_cpu_ms: 50.0,
                vcpus: vec![VcpuSummary {
                    index: 0,
                    thread_id: 1,
                    cpu_ms: Some(40.0),
                    user_ms: Some(30.0),
                    system_ms: Some(10.0),
                    kvm,
                }],
                vm: BTreeMap::new(),
                threads: Vec::new(),
            },
            trace: None,
            scenario: None,
        }
    }

    #[test]
    fn round_trip_and_fixture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = sample_report("a", 4000.0, 15000.0);
        report
            .write(&dir.path().join("report.json"))
            .expect("write");
        let back = Report::load(dir.path()).expect("load");
        assert_eq!(back, report);
        let json = fs::read_to_string(dir.path().join("report.json")).expect("read");
        // The flattened marker and record keep the documented shape.
        assert!(json.contains("\"test\": {"));
        assert!(json.contains("\"name\": \"iobench-randread-4k-qd1\""));
        assert!(matches!(
            Report::load(&dir.path().join("missing")),
            Err(Error::NoReport(_))
        ));
    }

    #[test]
    fn metric_names() {
        let m = metrics(&sample_report("a", 4000.0, 15000.0));
        assert_eq!(m["milestone.login"], 4000.0);
        assert_eq!(
            m["bench.iobench-randread-4k-qd1[policy=main].iops"],
            15000.0
        );
        assert_eq!(m["stats.vcpu_cpu_ms"], 40.0);
        assert_eq!(m["kvm.exits"], 1000.0);
    }

    #[test]
    fn medians() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }

    #[test]
    fn paired_comparison() {
        let a = [
            sample_report("a1", 1000.0, 100.0),
            sample_report("a2", 2000.0, 100.0),
            sample_report("a3", 1000.0, 100.0),
        ];
        let b = [
            sample_report("b1", 1100.0, 110.0),
            sample_report("b2", 1800.0, 90.0),
            sample_report("b3", 1050.0, 105.0),
        ];
        let p = paired(&a, &b).expect("paired");
        assert_eq!(p.pairs, 3);
        let login = p
            .rows
            .iter()
            .find(|r| r.metric == "milestone.login")
            .expect("login");
        // Pairs: +10%, -10%, +5%; the median is +5%, while the medians of the groups
        // (1000 and 1100) would say +10%.
        assert_eq!(login.median_a, 1000.0);
        assert_eq!(login.median_b, 1100.0);
        let pct = login.median_pct.expect("pct");
        assert!((pct - 5.0).abs() < 1e-9, "{pct}");
        let text = paired_table(&p);
        assert!(text.contains("milestone.login"));
        assert!(text.contains("+5.00%"));

        let mut missing = sample_report("b4", 1.0, 1.0);
        missing.milestones.remove("login");
        let p = paired(&a[..1], &[missing]).expect("paired");
        assert_eq!(p.incomplete, ["milestone.login"]);
        assert!(matches!(
            paired(&a, &b[..1]),
            Err(Error::UnpairedGroups { a: 3, b: 1 })
        ));
    }

    #[test]
    fn tables() {
        let one = vec![(PathBuf::from("/r/a"), sample_report("a", 4000.0, 15000.0))];
        let text = table(&one);
        assert!(text.contains("a "));
        assert!(text.contains("4000.0"));
        assert!(text.contains("bench.iobench-randread-4k-qd1[policy=main].iops"));
        let mut bad = sample_report("b", 5000.0, 14000.0);
        bad.verdict.result = Outcome::Fail;
        bad.verdict.checks[0].status = CheckStatus::Fail;
        let two = vec![one[0].clone(), (PathBuf::from("/r/b"), bad)];
        let text = table(&two);
        assert!(text.contains("FAIL"));
        assert!(text.contains("b: exit failed"));
        assert!(text.contains("median"));
    }
}
