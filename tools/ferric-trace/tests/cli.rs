//! The `ferric-trace` binary on a capture directory.

use std::fs;
use std::process::Command;

use ferric_trace::Event;
use ferric_trace::kernel::{
    SCHED_SWITCH, SCHEME_REQUEST, SCHEME_RESPONSE, SYSCALL_ENTER, SYSCALL_EXIT,
};
use ferric_trace::record::{MAX_RECORD_WORDS, encode};

fn bytes(records: &[(&Event, u64, &[u64])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (event, ts, args) in records {
        let mut words = [0u64; MAX_RECORD_WORDS];
        let n = encode(&mut words, event.id(), *ts, args).expect("valid record");
        out.extend(words[..n].iter().flat_map(|w| w.to_le_bytes()));
    }
    out
}

fn capture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::write(
        dir.path().join("cpu0"),
        bytes(&[
            (&SCHED_SWITCH, 1_000, &[0, 3, 1, 7]),
            (&SYSCALL_ENTER, 2_000, &[0x2100_0004, 5, 0, 32, 0]),
            (&SCHEME_REQUEST, 2_500, &[2, 0, 6, 3, 1, 32]),
            (&SYSCALL_EXIT, 6_000, &[0x2100_0004, 32]),
        ]),
    )
    .expect("write");
    fs::write(
        dir.path().join("cpu1"),
        bytes(&[(&SCHEME_RESPONSE, 4_000, &[2, 0, 32, 9])]),
    )
    .expect("write");
    dir
}

fn ferric_trace() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ferric-trace"))
}

#[test]
fn exports_perfetto_json() {
    let dir = capture();
    let out = dir.path().join("trace.json");
    let status = ferric_trace()
        .args(["perfetto", "--tsc-mhz", "1000", "-o"])
        .arg(&out)
        .arg(dir.path())
        .status()
        .expect("runs");
    assert!(status.success());
    let doc: serde_json::Value =
        serde_json::from_slice(&fs::read(&out).expect("written")).expect("JSON");
    let events = doc["traceEvents"].as_array().expect("events");
    let count = |ph: &str| events.iter().filter(|e| e["ph"] == ph).count();
    // One CPU slice, one syscall slice and its end, one scheme request and its response.
    assert_eq!(count("B"), 2);
    assert_eq!(count("E"), 2);
    assert_eq!(count("b"), 1);
    assert_eq!(count("e"), 1);
    let syscall = events
        .iter()
        .find(|e| e["ph"] == "B" && e["cat"] == "syscall")
        .expect("the syscall");
    assert_eq!(syscall["ts"], 1.0);
    assert_eq!(
        (syscall["pid"].as_u64(), syscall["tid"].as_u64()),
        (Some(3), Some(7))
    );
}

#[test]
fn prints_text() {
    let dir = capture();
    let output = ferric_trace()
        .arg("text")
        .arg(dir.path())
        .output()
        .expect("runs");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("utf-8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5);
    assert_eq!(
        lines[0],
        "[000]            0: sched.switch prev_pid=0x0 next_pid=0x3 prev_id=0x1 next_id=0x7"
    );
    assert!(lines[3].starts_with("[001]         3000: scheme.response"));
}

#[test]
fn reports_bad_input_with_its_cause() {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::write(dir.path().join("cpu0"), [1u8, 2, 3]).expect("write");
    let output = ferric_trace()
        .arg("text")
        .arg(dir.path())
        .output()
        .expect("runs");
    assert!(!output.status.success());
    let err = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        err.starts_with("ferric-trace: CPU 0: record 0 does not decode: "),
        "{err}"
    );
}

#[test]
fn refuses_a_bad_tsc_frequency() {
    let dir = capture();
    for bad in ["0", "-5", "inf", "NaN", "fast", "5e-324", "0.5"] {
        let output = ferric_trace()
            .arg("perfetto")
            .arg(format!("--tsc-mhz={bad}"))
            .arg(dir.path())
            .output()
            .expect("runs");
        assert!(!output.status.success(), "{bad}");
        assert!(output.stdout.is_empty(), "{bad}: wrote output");
        let err = String::from_utf8(output.stderr).expect("utf-8");
        assert!(err.contains("--tsc-mhz"), "{bad}: {err}");
    }
}

#[test]
fn captures_a_scheme_directory() {
    let scheme = tempfile::tempdir().expect("temp dir");
    fs::write(scheme.path().join("stats"), "cpu 0 pending 0 lost 0\n").expect("write");
    fs::create_dir(scheme.path().join("cpu")).expect("mkdir");
    fs::write(
        scheme.path().join("cpu/0"),
        bytes(&[(&SYSCALL_EXIT, 5, &[1, 2])]),
    )
    .expect("write");
    let out = scheme.path().join("saved");
    let output = ferric_trace()
        .args(["capture", "--scheme"])
        .arg(scheme.path())
        .arg(&out)
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        text.starts_with("cpu 0: 32 bytes, 0 records lost\n"),
        "{text}"
    );
    let printed = ferric_trace().arg("text").arg(&out).output().expect("runs");
    assert!(printed.status.success());
    assert_eq!(String::from_utf8_lossy(&printed.stdout).lines().count(), 1);

    let missing = ferric_trace()
        .args(["capture", "--scheme"])
        .arg(scheme.path().join("nothing"))
        .arg(&out)
        .output()
        .expect("runs");
    assert!(!missing.status.success());
    let err = String::from_utf8(missing.stderr).expect("utf-8");
    assert!(err.starts_with("ferric-trace: capture failed: "), "{err}");
}
