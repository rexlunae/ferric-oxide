//! The guest's result lines on the serial console.
//!
//! Test programs and the smoke script print three kinds of marker (see
//! `config/ferric-smoke.toml` and `recipes/tests/*`):
//!
//! - `FERRIC_TEST_PASS|SKIP|FAIL <name>[: detail]`: one test's outcome.
//! - `FERRIC_SMOKE_BEGIN` and `FERRIC_SMOKE_OK`: the smoke script started and finished.
//! - `FERRIC_BENCH <name> [label...] key=value...` and `FERRIC_BENCH_SAMPLE ...`: benchmark
//!   results (`crates/iobench/src/report.rs` formats most of them). Values are numbers
//!   (`iops=15042`, `mib_s=58.7`) or words (`policy=main`, `ab=pass`).
//!
//! A marker may share a line with other console output (a login prompt, say), so it is
//! found wherever it starts, as `scripts/ferric-ci/boot-smoke.sh` does, and runs to the end
//! of the line.
//!
//! A benchmark line names what it measured first and the figures after it, so the words
//! before the first number (`syscall out`, `policy=main`) identify the series. [`BenchRecord::series`]
//! joins them into a key such as `iobench-randread-4k-qd1[policy=main]`, which `report`
//! pairs across runs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A test's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    /// `FERRIC_TEST_PASS`.
    Pass,
    /// `FERRIC_TEST_SKIP`.
    Skip,
    /// `FERRIC_TEST_FAIL`.
    Fail,
}

/// A `FERRIC_TEST_*` line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestMarker {
    /// The outcome.
    pub outcome: TestOutcome,
    /// The test's name: the first word, without a trailing `:`.
    pub name: String,
    /// Everything after the name, if anything.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}

/// A `FERRIC_SMOKE_*` line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmokeMarker {
    /// `FERRIC_SMOKE_BEGIN`: the smoke script started.
    Begin,
    /// `FERRIC_SMOKE_OK`: the smoke script finished.
    Ok,
    /// Another `FERRIC_SMOKE_<WORD>`.
    Other(String),
}

/// A `FERRIC_BENCH` or `FERRIC_BENCH_SAMPLE` line.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchRecord {
    /// `true` for `FERRIC_BENCH_SAMPLE` (one round), `false` for `FERRIC_BENCH` (a result).
    pub sample: bool,
    /// The benchmark's name, the first word.
    pub name: String,
    /// Bare words before the first `key=value` field (`out` in `syscall out min_ns=...`).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub labels: Vec<String>,
    /// Word-valued fields printed before the first number; they identify the series.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub keys: BTreeMap<String, String>,
    /// Numeric fields.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub values: BTreeMap<String, f64>,
    /// Word-valued fields printed after a number (`ab=pass`): results, not identity.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub text: BTreeMap<String, String>,
}

impl BenchRecord {
    /// The series this record belongs to: the name, then its labels and identifying
    /// fields in brackets, as in `syscall[out]` or `iobench-randread-4k-qd1[policy=main]`.
    pub fn series(&self) -> String {
        let mut parts: Vec<String> = self.labels.clone();
        parts.extend(self.keys.iter().map(|(k, v)| format!("{k}={v}")));
        if parts.is_empty() {
            self.name.clone()
        } else {
            format!("{}[{}]", self.name, parts.join(" "))
        }
    }

    fn parse(sample: bool, rest: &str) -> Option<Self> {
        let mut words = rest.split_whitespace();
        let name = words.next()?.to_owned();
        let mut record = Self {
            sample,
            name,
            ..Self::default()
        };
        let mut seen_number = false;
        for word in words {
            match word.split_once('=') {
                None if !seen_number && record.keys.is_empty() => {
                    record.labels.push(word.to_owned());
                }
                // A bare word among the figures carries no field name; it is not a result
                // anyone can compare, so it is dropped.
                None => {}
                Some((key, value)) => match parse_number(value) {
                    Some(number) => {
                        seen_number = true;
                        record.values.insert(key.to_owned(), number);
                    }
                    None if seen_number => {
                        record.text.insert(key.to_owned(), value.to_owned());
                    }
                    None => {
                        record.keys.insert(key.to_owned(), value.to_owned());
                    }
                },
            }
        }
        Some(record)
    }
}

/// A finite decimal number, as the guest prints them. Words that Rust would also parse as
/// numbers (`inf`, `nan`) are words here.
fn parse_number(value: &str) -> Option<f64> {
    let plausible = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
        && value.bytes().any(|b| b.is_ascii_digit());
    plausible
        .then(|| value.parse::<f64>().ok())
        .flatten()
        .filter(|v| v.is_finite())
}

/// A marker found on a console line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Marker {
    /// A `FERRIC_TEST_*` line.
    Test(TestMarker),
    /// A `FERRIC_SMOKE_*` line.
    Smoke(SmokeMarker),
    /// A `FERRIC_BENCH` or `FERRIC_BENCH_SAMPLE` line.
    Bench(BenchRecord),
}

/// Finds the first marker on `line` (already stripped of escape sequences and carriage
/// returns) and returns the byte offset where it starts with the parsed marker.
pub fn parse_line(line: &str) -> Option<(usize, Marker)> {
    let mut search = 0;
    while let Some(found) = line[search..].find("FERRIC_") {
        let at = search + found;
        if let Some(marker) = parse_at(&line[at..]) {
            return Some((at, marker));
        }
        search = at + "FERRIC_".len();
    }
    None
}

fn parse_at(text: &str) -> Option<Marker> {
    if let Some(rest) = text.strip_prefix("FERRIC_BENCH_SAMPLE ") {
        return BenchRecord::parse(true, rest).map(Marker::Bench);
    }
    if let Some(rest) = text.strip_prefix("FERRIC_BENCH ") {
        return BenchRecord::parse(false, rest).map(Marker::Bench);
    }
    if let Some(rest) = text.strip_prefix("FERRIC_TEST_") {
        let (kind, rest) = split_word(rest);
        let outcome = match kind {
            "PASS" => TestOutcome::Pass,
            "SKIP" => TestOutcome::Skip,
            "FAIL" => TestOutcome::Fail,
            _ => return None,
        };
        let rest = rest.trim();
        let (name, detail) = split_word(rest);
        let detail = detail.trim();
        return Some(Marker::Test(TestMarker {
            outcome,
            name: name.trim_end_matches(':').to_owned(),
            detail: (!detail.is_empty()).then(|| detail.to_owned()),
        }));
    }
    if let Some(rest) = text.strip_prefix("FERRIC_SMOKE_") {
        let (kind, _) = split_word(rest);
        if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
            return None;
        }
        return Some(Marker::Smoke(match kind {
            "BEGIN" => SmokeMarker::Begin,
            "OK" => SmokeMarker::Ok,
            other => SmokeMarker::Other(other.to_owned()),
        }));
    }
    None
}

/// Splits off the first word (up to whitespace).
fn split_word(text: &str) -> (&str, &str) {
    match text.find(char::is_whitespace) {
        Some(end) => (&text[..end], &text[end..]),
        None => (text, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench(line: &str) -> BenchRecord {
        match parse_line(line) {
            Some((_, Marker::Bench(b))) => b,
            other => panic!("not a bench line: {other:?}"),
        }
    }

    #[test]
    fn test_markers() {
        assert_eq!(
            parse_line("FERRIC_TEST_PASS aslr-address-space: sparse 32 GiB reservation"),
            Some((
                0,
                Marker::Test(TestMarker {
                    outcome: TestOutcome::Pass,
                    name: "aslr-address-space".into(),
                    detail: Some("sparse 32 GiB reservation".into()),
                })
            ))
        );
        assert_eq!(
            parse_line("FERRIC_TEST_SKIP xstate-signal Standard"),
            Some((
                0,
                Marker::Test(TestMarker {
                    outcome: TestOutcome::Skip,
                    name: "xstate-signal".into(),
                    detail: Some("Standard".into()),
                })
            ))
        );
        assert_eq!(
            parse_line("redox login: FERRIC_TEST_FAIL dd-pipe"),
            Some((
                13,
                Marker::Test(TestMarker {
                    outcome: TestOutcome::Fail,
                    name: "dd-pipe".into(),
                    detail: None,
                })
            ))
        );
        assert_eq!(parse_line("FERRIC_TEST_MAYBE x"), None);
        assert_eq!(parse_line("nothing here"), None);
    }

    #[test]
    fn smoke_markers() {
        assert_eq!(
            parse_line("FERRIC_SMOKE_BEGIN"),
            Some((0, Marker::Smoke(SmokeMarker::Begin)))
        );
        assert_eq!(
            parse_line("x FERRIC_SMOKE_OK [ld.so]: failed"),
            Some((2, Marker::Smoke(SmokeMarker::Ok)))
        );
        assert_eq!(
            parse_line("FERRIC_SMOKE_ABORTED"),
            Some((0, Marker::Smoke(SmokeMarker::Other("ABORTED".into()))))
        );
        assert_eq!(parse_line("FERRIC_SMOKE_ok"), None);
    }

    #[test]
    fn bench_lines_and_series() {
        let b = bench(
            "FERRIC_BENCH iobench-randread-4k-qd1 policy=main rounds=3 ios=3072 iops=15042 \
             mib_s=58.7 lat_p50_ns=63744",
        );
        assert!(!b.sample);
        assert_eq!(b.series(), "iobench-randread-4k-qd1[policy=main]");
        assert_eq!(b.values["mib_s"], 58.7);
        assert_eq!(b.values["iops"], 15042.0);

        let b = bench("FERRIC_BENCH syscall out min_ns=320 median_ns=331");
        assert_eq!(b.labels, ["out"]);
        assert_eq!(b.series(), "syscall[out]");

        let b = bench(
            "FERRIC_BENCH nvme-read-4k-t1 bytes=12582912 copy_mib_s=58.1 ab_ratio=1006 ab=pass",
        );
        assert_eq!(b.series(), "nvme-read-4k-t1");
        assert_eq!(b.text["ab"], "pass");

        let b = bench("FERRIC_BENCH_SAMPLE nvme-read-4k-t1 round=0 single_iops=15700");
        assert!(b.sample);
        assert_eq!(b.values["round"], 0.0);

        let b = bench("FERRIC_BENCH tlb sched_yield_ns=597 note=inf");
        assert_eq!(b.series(), "tlb");
        assert_eq!(b.text["note"], "inf");
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number("-1.5e3"), Some(-1500.0));
        assert_eq!(parse_number("nan"), None);
        assert_eq!(parse_number("0x10"), None);
        assert_eq!(parse_number("."), None);
        assert_eq!(parse_number(""), None);
    }
}
