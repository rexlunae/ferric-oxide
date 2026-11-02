//! The `ferric-perf` binary, run on the host with `--quick` and host paths for its targets.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};

/// A scratch directory under Cargo's per-target temporary directory, not the system's.
fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{name}-{}", std::process::id()))
}

/// `ferric-perf --quick` with host stand-ins for its Redox paths, and `args`.
fn ferric_perf(dir: &PathBuf, args: &[&str]) -> Output {
    let helper = env!("CARGO_BIN_EXE_ferric-perf-true");
    Command::new(env!("CARGO_BIN_EXE_ferric-perf"))
        .args(["--quick", "--static-true", helper, "--dynamic-true", helper])
        .args([
            "--ion",
            "/bin/sh",
            "--log",
            "/dev/null",
            "--null",
            "/dev/null",
        ])
        .args(["--mmap-mib", "1", "--dir"])
        .arg(dir)
        .args(args)
        .output()
        .expect("runs")
}

/// The `key=value` fields of each `FERRIC_BENCH` line in `stdout`, by benchmark name.
fn bench_lines(stdout: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("FERRIC_BENCH "))
        .map(|line| {
            let mut words = line.split(' ');
            let name = words.next().expect("a name").to_owned();
            let fields = words
                .map(|field| {
                    let (key, value) = field.split_once('=').expect("key=value");
                    (key.to_owned(), value.to_owned())
                })
                .collect();
            (name, fields)
        })
        .collect()
}

#[test]
fn a_quick_run_prints_every_line_and_passes() {
    let dir = scratch("all");
    let output = ferric_perf(&dir, &[]);
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(output.status.success(), "{stdout}");
    assert_eq!(stdout.lines().last(), Some("FERRIC_TEST_PASS perf"));
    let lines = bench_lines(&stdout);
    let expected = [
        "clock",
        "spawn-fork-exec-static",
        "spawn-fork-exec-dynamic",
        "spawn-posix-spawn-static",
        "spawn-posix-spawn-dynamic",
        "spawn-ion",
        "pingpong-thread",
        "pingpong-process",
        "pipe-rtt-1b",
        "pipe-throughput",
        "write-null",
        "write-log",
        "file-create",
        "file-stat",
        "file-unlink",
        "mmap-map",
        "mmap-touch",
        "mmap-unmap",
    ];
    let names: Vec<&str> = lines.keys().map(String::as_str).collect();
    let mut want: Vec<String> = expected.iter().map(|n| format!("perf-{n}")).collect();
    want.sort();
    assert_eq!(names, want);
    for (name, fields) in &lines {
        for key in [
            "iters",
            "mean_ns",
            "median_ns",
            "p99_ns",
            "min_ns",
            "max_ns",
        ] {
            let value: u64 = fields[key]
                .parse()
                .unwrap_or_else(|_| panic!("{name} {key}"));
            if key == "iters" {
                assert!(
                    (1..=3).contains(&value) || name == "perf-file-unlink",
                    "{name}"
                );
            }
        }
        let min: u64 = fields["min_ns"].parse().expect("integer");
        let max: u64 = fields["max_ns"].parse().expect("integer");
        assert!(min <= max, "{name}");
    }
    // Throughput lines carry their bytes per iteration.
    assert_eq!(lines["perf-pipe-throughput"]["bytes"], "4194304");
    assert_eq!(lines["perf-write-log"]["bytes"], "70");
    assert_eq!(lines["perf-mmap-touch"]["bytes"], "1048576");
    assert!(!lines["perf-mmap-unmap"].contains_key("bytes"));
    // Unlink removes the warm-up's file too, and then the scratch directories are gone.
    let created: u64 = lines["perf-file-create"]["iters"].parse().expect("integer");
    assert_eq!(
        lines["perf-file-unlink"]["iters"],
        (created + 1).to_string()
    );
    assert!(
        std::fs::read_dir(&dir)
            .expect("the scratch root stays")
            .next()
            .is_none()
    );
    std::fs::remove_dir(&dir).expect("cleanup");
}

#[test]
fn a_failing_benchmark_fails_the_run_but_not_the_others() {
    let dir = scratch("missing");
    let output = ferric_perf(
        &dir,
        &[
            "--static-true",
            "/nonexistent/true",
            "clock",
            "spawn-posix-spawn-static",
        ],
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(lines[0].starts_with("FERRIC_BENCH perf-clock iters="));
    assert_eq!(
        lines[1],
        "FERRIC_TEST_FAIL perf-spawn-posix-spawn-static: cannot stat /nonexistent/true: No such \
         file or directory (os error 2)"
    );
    assert_eq!(
        lines[2],
        "FERRIC_TEST_FAIL perf: 1 of 2 benchmark groups failed"
    );
}

#[test]
fn filters_list_and_refuse_unknown_names() {
    let dir = scratch("list");
    let output = ferric_perf(&dir, &["--list", "file-stat", "spawn-ion"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf-8"),
        "spawn-ion: spawn-ion\nfile: file-create file-stat file-unlink\n"
    );
    let output = ferric_perf(&dir, &["--list", "fork"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no benchmark name starts with \"fork\"")
    );
}
