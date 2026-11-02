//! Pass or fail for a run.
//!
//! [`judge`] turns what the console showed ([`Facts`]) and how the run ended ([`Ending`])
//! into a list of named [`Check`]s. The run passes when no check failed; a warning is
//! reported but does not fail it. The smoke rules are those of
//! `scripts/ferric-ci/boot-smoke.sh`, and its self-test cases are ported to the tests below.
//!
//! The mode check is what makes the trace mode a separate kernel: in [`Mode::Normal`] the run
//! fails if any kernel line starts with `trace:` (a kernel built without `trace` prints
//! none); in [`Mode::Trace`] it fails unless the kernel said `trace: trace-mode kernel`.
//!
//! The trace check is what makes a trace run worth its trace. In [`Mode::Trace`] it fails
//! unless the kernel announced its host rings (`trace: host rings at phys ...`); with
//! automatic start (the default) it also fails unless `ferric-trace follow` started, and
//! every capture must have ended whole: `follow` exited with 0 on its own after SIGINT, and
//! its directory has `stats.end` and no `INCOMPLETE` marker. Without automatic start a run
//! needs no capture, but one started by hand is held to the same rule. The capture's state
//! comes in as data ([`TraceInfo`]); the session reads the files, not this module.

use serde::{Deserialize, Serialize};

use crate::machine::Mode;
use crate::observe::Facts;
use crate::report::{Capture, TraceInfo};
use crate::trace::FollowExit;

/// A check's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check held.
    Pass,
    /// The check failed; the run fails.
    Fail,
    /// Something worth reading that does not fail the run.
    Warn,
    /// The check does not apply to this run.
    Skip,
}

/// One named check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Short name (`exit`, `crash`, `mode`, ...).
    pub name: String,
    /// Result.
    pub status: CheckStatus,
    /// What was seen.
    pub detail: String,
}

/// The run's overall result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// No check failed.
    Pass,
    /// At least one check failed.
    Fail,
}

/// A run's verdict: the outcome and every check behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Pass when no check failed.
    pub result: Outcome,
    /// The checks, in a fixed order.
    pub checks: Vec<Check>,
}

impl Verdict {
    /// The failed checks.
    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| c.status == CheckStatus::Fail)
    }
}

/// Why the harness stopped the run, or that the guest ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// QEMU exited on its own (the guest powered off, reset or crashed QEMU).
    GuestExit,
    /// A `down` request or the end of a scenario.
    Down,
    /// The run's time limit passed.
    Timeout,
    /// A crash line appeared and the run was set to stop on crashes.
    Crash,
    /// The smoke shell could not start.
    SmokeDead,
    /// The harness received SIGINT or SIGTERM.
    Interrupted,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ending {
    /// Why it stopped.
    pub stop: StopReason,
    /// QEMU's exit status; `None` when a signal killed it.
    pub qemu_exit: Option<i32>,
    /// The `reason` of QEMU's last `SHUTDOWN` event, if any.
    pub shutdown_reason: Option<String>,
    /// The harness had to kill QEMU because `quit` did not end it.
    pub killed: bool,
    /// Seconds the guest had to act on the ACPI power button before the harness quit QEMU,
    /// when `down` pressed it.
    pub powerdown_wait_s: Option<f64>,
    /// The time limit, for the message when it passed.
    pub timeout_s: Option<f64>,
}

/// What a scenario's steps did, for the verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioSummary {
    /// Steps that ran.
    pub ran: usize,
    /// Steps in the scenario.
    pub total: usize,
    /// The first failed step: its number (from 1) and why.
    pub failed: Option<(usize, String)>,
}

/// What the verdict is judged against, besides the console facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expectations {
    /// Normal or trace kernel.
    pub mode: Mode,
    /// Whether the smoke protocol must complete even if no marker appears.
    pub smoke: bool,
    /// Whether the guest may reset (QEMU without `-no-reboot`).
    pub reboot: bool,
    /// Whether `ferric-trace follow` starts by itself when the rings are announced (trace
    /// mode).
    pub trace_autostart: bool,
}

/// Judges a run: `trace` is what the harness recorded of host tracing (`None` when it
/// recorded nothing, as in normal mode).
pub fn judge(
    expect: Expectations,
    facts: &Facts,
    ending: &Ending,
    trace: Option<&TraceInfo>,
    scenario: Option<&ScenarioSummary>,
) -> Verdict {
    let mut checks = vec![
        stop_check(ending),
        exit_check(ending),
        crash_check(facts),
        tests_check(facts),
        smoke_check(expect, facts),
        mode_check(expect.mode, facts),
        trace_check(expect, facts, trace),
        shutdown_check(expect.reboot, ending),
    ];
    if let Some(summary) = scenario {
        checks.push(scenario_check(summary));
    }
    let result = if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        Outcome::Fail
    } else {
        Outcome::Pass
    };
    Verdict { result, checks }
}

fn check(name: &str, status: CheckStatus, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_owned(),
        status,
        detail: detail.into(),
    }
}

fn stop_check(ending: &Ending) -> Check {
    use CheckStatus::{Fail, Pass};
    match ending.stop {
        StopReason::GuestExit => check("stop", Pass, "QEMU exited on its own"),
        StopReason::Down => check("stop", Pass, "stopped on request"),
        StopReason::Timeout => check(
            "stop",
            Fail,
            match ending.timeout_s {
                Some(s) => format!("timed out after {s}s"),
                None => "timed out".to_owned(),
            },
        ),
        StopReason::Crash => check("stop", Fail, "stopped after a crash on the console"),
        StopReason::SmokeDead => check("stop", Fail, "the smoke shell could not start"),
        StopReason::Interrupted => check("stop", Fail, "interrupted by a signal"),
    }
}

fn exit_check(ending: &Ending) -> Check {
    match ending.qemu_exit {
        Some(0) if ending.killed => check(
            "exit",
            CheckStatus::Warn,
            "QEMU did not quit and was killed",
        ),
        Some(0) => check("exit", CheckStatus::Pass, "QEMU exited with 0"),
        Some(code) => check(
            "exit",
            CheckStatus::Fail,
            format!("QEMU exited with {code}"),
        ),
        None if ending.killed => check(
            "exit",
            CheckStatus::Warn,
            "QEMU did not quit and was killed",
        ),
        None => check("exit", CheckStatus::Fail, "QEMU was killed by a signal"),
    }
}

fn crash_check(facts: &Facts) -> Check {
    match &facts.crash {
        Some(line) => check("crash", CheckStatus::Fail, line.clone()),
        None => check("crash", CheckStatus::Pass, "no crash on the console"),
    }
}

fn tests_check(facts: &Facts) -> Check {
    let summary = format!(
        "{} passed, {} skipped, {} failed",
        facts.tests_passed,
        facts.tests_skipped,
        facts.tests_failed.len()
    );
    if !facts.tests_failed.is_empty() {
        check(
            "tests",
            CheckStatus::Fail,
            format!("{summary}: {}", facts.tests_failed.join(", ")),
        )
    } else if facts.tests_passed + facts.tests_skipped == 0 {
        check("tests", CheckStatus::Skip, "no test markers")
    } else {
        check("tests", CheckStatus::Pass, summary)
    }
}

fn smoke_check(expect: Expectations, facts: &Facts) -> Check {
    use CheckStatus::{Fail, Pass, Skip};
    if !expect.smoke && !facts.has_test_markers() && facts.smoke_dead.is_none() {
        return check("smoke", Skip, "not a smoke run");
    }
    if !facts.smoke_ok
        && let Some(line) = &facts.smoke_dead
    {
        return check(
            "smoke",
            Fail,
            format!("the smoke shell could not start: {line}"),
        );
    }
    if let Some(line) = &facts.link_failure_before_smoke_ok {
        return check(
            "smoke",
            Fail,
            format!("a program failed to link before the smoke marker: {line}"),
        );
    }
    if facts.tests_passed + facts.tests_skipped + facts.tests_failed.len() == 0 {
        return check("smoke", Fail, "no test reported a result");
    }
    if !facts.smoke_ok {
        return check("smoke", Fail, "no FERRIC_SMOKE_OK");
    }
    check("smoke", Pass, "FERRIC_SMOKE_OK")
}

fn mode_check(mode: Mode, facts: &Facts) -> Check {
    use CheckStatus::{Fail, Pass};
    if !facts.kernel_started && !facts.trace_mode_kernel {
        return check("mode", Fail, "no kernel line on the console");
    }
    match mode {
        Mode::Normal => match &facts.first_kernel_trace_line {
            Some(line) => check(
                "mode",
                Fail,
                format!(
                    "normal mode, but the kernel printed {} trace: line(s), first: {line}",
                    facts.kernel_trace_lines
                ),
            ),
            None => check("mode", Pass, "normal kernel: no trace: lines"),
        },
        Mode::Trace if facts.trace_mode_kernel => check("mode", Pass, "trace: trace-mode kernel"),
        Mode::Trace => check(
            "mode",
            Fail,
            "trace mode, but the kernel never said trace: trace-mode kernel",
        ),
    }
}

fn trace_check(expect: Expectations, facts: &Facts, trace: Option<&TraceInfo>) -> Check {
    use CheckStatus::{Fail, Pass, Skip};
    if expect.mode != Mode::Trace {
        return check("trace", Skip, "normal mode");
    }
    let Some(rings) = facts.host_rings else {
        return check(
            "trace",
            Fail,
            "the kernel never announced its host rings (trace: host rings at phys ...)",
        );
    };
    let (start_failures, captures) = trace.map_or((&[][..], &[][..]), |t| {
        (&t.start_failures[..], &t.captures[..])
    });
    let mut problems: Vec<String> = start_failures
        .iter()
        .map(|why| format!("ferric-trace follow could not start: {why}"))
        .collect();
    if expect.trace_autostart && start_failures.is_empty() && captures.is_empty() {
        problems.push("ferric-trace follow never started".to_owned());
    }
    problems.extend(captures.iter().filter_map(capture_problems));
    if !problems.is_empty() {
        return check("trace", Fail, problems.join("; "));
    }
    let announced = format!(
        "host rings at {:#x}, {} CPUs x {} KiB",
        rings.phys, rings.cpus, rings.kib
    );
    match captures.len() {
        0 => check(
            "trace",
            Pass,
            format!("{announced}; no capture (automatic start off)"),
        ),
        n => check(
            "trace",
            Pass,
            format!("{announced}; {n} complete capture(s)"),
        ),
    }
}

/// What is wrong with one capture, if anything: how `follow` ended and what its directory
/// lacks.
fn capture_problems(capture: &Capture) -> Option<String> {
    let mut problems = Vec::new();
    match &capture.exit {
        None => problems.push("follow had not stopped".to_owned()),
        Some(FollowExit { killed: true, .. }) => {
            problems.push("follow ignored SIGINT and was killed".to_owned());
        }
        Some(FollowExit { code: Some(0), .. }) => {}
        Some(FollowExit {
            code: Some(code), ..
        }) => {
            problems.push(format!("follow exited with {code}"));
        }
        Some(FollowExit { code: None, .. }) => {
            problems.push("follow was killed by a signal".to_owned());
        }
    }
    match capture.files {
        None => problems.push("the capture was not checked".to_owned()),
        Some(files) => {
            if files.incomplete {
                problems.push("the capture is marked INCOMPLETE".to_owned());
            }
            if !files.stats_end {
                problems.push("the capture has no stats.end".to_owned());
            }
        }
    }
    (!problems.is_empty()).then(|| format!("{}: {}", capture.dir.display(), problems.join(", ")))
}

fn shutdown_check(reboot: bool, ending: &Ending) -> Check {
    use CheckStatus::{Fail, Pass, Warn};
    let Some(reason) = ending.shutdown_reason.as_deref() else {
        return match ending.qemu_exit {
            Some(0) => check("shutdown", Pass, "QEMU exited without a SHUTDOWN event"),
            _ => check("shutdown", Fail, "no SHUTDOWN event"),
        };
    };
    match reason {
        "guest-shutdown" => check("shutdown", Pass, "the guest powered off"),
        "guest-reset" if reboot => check("shutdown", Pass, "the guest reset"),
        "guest-reset" => check(
            "shutdown",
            Fail,
            "the guest reset (reboot or triple fault) and QEMU runs with -no-reboot",
        ),
        "guest-panic" => check("shutdown", Fail, "the guest reported a panic to QEMU"),
        "host-qmp-quit" | "host-signal" | "host-ui" => match ending.powerdown_wait_s {
            Some(wait) => check(
                "shutdown",
                Warn,
                format!(
                    "the guest did not power off within {wait}s of the ACPI power button; QEMU was told to quit"
                ),
            ),
            None => check("shutdown", Pass, format!("stopped by the host ({reason})")),
        },
        other => check("shutdown", Warn, format!("SHUTDOWN reason {other}")),
    }
}

fn scenario_check(summary: &ScenarioSummary) -> Check {
    match &summary.failed {
        Some((step, why)) => check(
            "scenario",
            CheckStatus::Fail,
            format!("step {step} of {} failed: {why}", summary.total),
        ),
        None if summary.ran < summary.total => check(
            "scenario",
            CheckStatus::Fail,
            format!("only {} of {} steps ran", summary.ran, summary.total),
        ),
        None => check(
            "scenario",
            CheckStatus::Pass,
            format!("{} steps", summary.total),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::Observer;
    use crate::trace::CaptureFiles;
    use std::path::PathBuf;

    fn facts(text: &str) -> Facts {
        let mut observer = Observer::new(false);
        for line in text.lines() {
            observer.line(line);
        }
        let mut f = observer.facts().clone();
        // The smoke cases have no kernel lines; the mode check is tested separately.
        f.kernel_started = true;
        f
    }

    fn exited(code: i32) -> Ending {
        Ending {
            stop: StopReason::GuestExit,
            qemu_exit: Some(code),
            shutdown_reason: Some("guest-shutdown".to_owned()),
            killed: false,
            powerdown_wait_s: None,
            timeout_s: None,
        }
    }

    const SMOKE: Expectations = Expectations {
        mode: Mode::Normal,
        smoke: true,
        reboot: false,
        trace_autostart: true,
    };

    fn verdict(text: &str, ending: &Ending) -> Verdict {
        judge(SMOKE, &facts(text), ending, None, None)
    }

    fn failed(v: &Verdict) -> Vec<String> {
        v.failures().map(|c| c.name.clone()).collect()
    }

    // The cases of scripts/ferric-ci/test-boot-smoke.sh.
    #[test]
    fn boot_smoke_cases() {
        let ok = exited(0);
        assert_eq!(
            verdict("FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK", &ok).result,
            Outcome::Pass
        );
        assert_eq!(
            verdict("FERRIC_TEST_SKIP xstate\nFERRIC_SMOKE_OK", &ok).result,
            Outcome::Pass
        );
        assert_eq!(
            failed(&verdict("FERRIC_SMOKE_OK", &ok)),
            ["smoke"],
            "missing result"
        );
        assert_eq!(
            failed(&verdict("FERRIC_TEST_FAIL xstate\nFERRIC_SMOKE_OK", &ok)),
            ["tests"]
        );
        assert_eq!(
            failed(&verdict(
                "FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK\nKERNEL PANIC",
                &ok
            )),
            ["crash"]
        );
        assert_eq!(
            failed(&verdict("FERRIC_TEST_PASS xstate", &ok)),
            ["smoke"],
            "missing marker"
        );
        assert_eq!(
            failed(&verdict(
                "FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK",
                &exited(1)
            )),
            ["exit"]
        );
        let timeout = Ending {
            stop: StopReason::Timeout,
            timeout_s: Some(1.0),
            ..exited(0)
        };
        let v = verdict("FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK", &timeout);
        assert_eq!(failed(&v), ["stop"]);
        assert_eq!(v.checks[0].detail, "timed out after 1s");
    }

    #[test]
    fn loader_failures() {
        let ok = exited(0);
        let unlinked = "FERRIC_TEST_PASS iommu-dma\n[ld.so]: failed to link '/usr/bin/ion': NotFound\n\
            init: cd \"/\" && env -i \"ion\" \"/usr/lib/ferric-smoke.ion\" failed with exit status: 1";
        let v = verdict(unlinked, &ok);
        assert!(failed(&v).contains(&"smoke".to_owned()));
        assert!(
            v.checks
                .iter()
                .any(|c| c.detail.starts_with("the smoke shell could not start"))
        );
        // After the marker the same message says nothing about the run.
        assert_eq!(
            verdict(
                "FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK\n[ld.so]: failed to link '/usr/bin/ion': NotFound",
                &ok
            )
            .result,
            Outcome::Pass
        );
        // A later marker cannot erase a loader failure observed before completion.
        let v = verdict(
            "[ld.so]: failed to link '/usr/bin/ion': NotFound\nFERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK",
            &ok,
        );
        assert_eq!(failed(&v), ["smoke"]);
        let v = verdict(
            "FERRIC_SMOKE_BEGIN\n[ld.so]: failed to link '/usr/bin/cat': NotFound\nFERRIC_TEST_SKIP profile\nFERRIC_SMOKE_OK",
            &ok,
        );
        assert!(
            v.checks
                .iter()
                .any(|c| c.detail.starts_with("a program failed to link"))
        );
        assert_eq!(
            verdict(
                "FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK [ld.so]: failed to link '/usr/bin/cat': NotFound",
                &ok
            )
            .result,
            Outcome::Pass
        );
    }

    #[test]
    fn mode_checks() {
        let ok = exited(0);
        let normal = Expectations {
            mode: Mode::Normal,
            smoke: false,
            reboot: false,
            trace_autostart: true,
        };
        let trace = Expectations {
            mode: Mode::Trace,
            ..normal
        };
        let plain = facts("kernel::a:INFO -- hello\n");
        let traced =
            facts("kernel::a:INFO -- hello\nkernel::trace::imp:INFO -- trace: trace-mode kernel\n");
        assert_eq!(judge(normal, &plain, &ok, None, None).result, Outcome::Pass);
        assert_eq!(failed(&judge(normal, &traced, &ok, None, None)), ["mode"]);
        let mode = |v: Verdict| {
            v.checks
                .into_iter()
                .find(|c| c.name == "mode")
                .map(|c| c.status)
        };
        assert_eq!(
            mode(judge(trace, &plain, &ok, None, None)),
            Some(CheckStatus::Fail)
        );
        assert_eq!(
            mode(judge(trace, &traced, &ok, None, None)),
            Some(CheckStatus::Pass)
        );
        let silent = Facts::default();
        assert_eq!(failed(&judge(normal, &silent, &ok, None, None)), ["mode"]);
    }

    fn capture(name: &str, code: Option<i32>, killed: bool, files: CaptureFiles) -> Capture {
        Capture {
            dir: PathBuf::from(format!("/run/{name}")),
            started_ms: 1.0,
            exit: Some(FollowExit { code, killed }),
            files: Some(files),
        }
    }

    const WHOLE: CaptureFiles = CaptureFiles {
        incomplete: false,
        stats_end: true,
    };

    fn traced(captures: Vec<Capture>, start_failures: &[&str]) -> TraceInfo {
        TraceInfo {
            host_rings: None,
            guest_ram: None,
            captures,
            start_failures: start_failures.iter().map(|&s| s.to_owned()).collect(),
        }
    }

    #[test]
    fn trace_checks() {
        let ok = exited(0);
        let auto = Expectations {
            mode: Mode::Trace,
            smoke: false,
            reboot: false,
            trace_autostart: true,
        };
        let manual = Expectations {
            trace_autostart: false,
            ..auto
        };
        let kernel = "kernel::trace::imp:INFO -- trace: trace-mode kernel\n";
        let no_rings = facts(kernel);
        let rings = facts(&format!(
            "{kernel}kernel::trace::imp:INFO -- trace: host rings at phys 0x7a000000, 4 CPUs x 4096 KiB\n"
        ));
        let trace = |expect, facts: &Facts, info: Option<&TraceInfo>| {
            judge(expect, facts, &ok, info, None)
                .checks
                .into_iter()
                .find(|c| c.name == "trace")
                .expect("a trace check")
        };

        // The kernel refused the ring size: no announcement, no capture.
        let c = trace(auto, &no_rings, Some(&traced(Vec::new(), &[])));
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.detail.contains("never announced its host rings"),
            "{}",
            c.detail
        );
        assert_eq!(trace(manual, &no_rings, None).status, CheckStatus::Fail);

        let whole = traced(vec![capture("trace", Some(0), false, WHOLE)], &[]);
        let c = trace(auto, &rings, Some(&whole));
        assert_eq!(c.status, CheckStatus::Pass, "{}", c.detail);
        assert_eq!(
            c.detail,
            "host rings at 0x7a000000, 4 CPUs x 4096 KiB; 1 complete capture(s)"
        );
        assert_eq!(
            judge(auto, &rings, &ok, Some(&whole), None).result,
            Outcome::Pass
        );

        let c = trace(auto, &rings, Some(&traced(Vec::new(), &[])));
        assert_eq!(
            (c.status, c.detail.as_str()),
            (CheckStatus::Fail, "ferric-trace follow never started")
        );
        let c = trace(
            auto,
            &rings,
            Some(&traced(Vec::new(), &["cannot run ferric-trace: not found"])),
        );
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(
            c.detail,
            "ferric-trace follow could not start: cannot run ferric-trace: not found"
        );

        let failed_follow = traced(
            vec![capture(
                "trace",
                Some(1),
                false,
                CaptureFiles {
                    incomplete: true,
                    stats_end: false,
                },
            )],
            &[],
        );
        let c = trace(auto, &rings, Some(&failed_follow));
        assert_eq!(c.status, CheckStatus::Fail);
        assert_eq!(
            c.detail,
            "/run/trace: follow exited with 1, the capture is marked INCOMPLETE, the capture has no stats.end"
        );
        let killed = traced(vec![capture("trace", None, true, WHOLE)], &[]);
        assert!(
            trace(auto, &rings, Some(&killed))
                .detail
                .contains("ignored SIGINT and was killed")
        );
        let signalled = traced(vec![capture("trace", None, false, WHOLE)], &[]);
        assert!(
            trace(auto, &rings, Some(&signalled))
                .detail
                .contains("killed by a signal")
        );
        // A later capture does not make up for an earlier broken one.
        let second = traced(
            vec![
                capture("trace", Some(0), false, WHOLE),
                capture(
                    "trace.2",
                    Some(0),
                    false,
                    CaptureFiles {
                        incomplete: false,
                        stats_end: false,
                    },
                ),
            ],
            &[],
        );
        assert_eq!(
            trace(auto, &rings, Some(&second)).detail,
            "/run/trace.2: the capture has no stats.end"
        );

        // Without automatic start the announcement is enough, but a capture taken by hand
        // must still be whole.
        let c = trace(manual, &rings, Some(&traced(Vec::new(), &[])));
        assert_eq!(c.status, CheckStatus::Pass, "{}", c.detail);
        assert!(c.detail.ends_with("no capture (automatic start off)"));
        assert_eq!(
            trace(manual, &rings, Some(&failed_follow)).status,
            CheckStatus::Fail
        );

        let normal = Expectations {
            mode: Mode::Normal,
            ..auto
        };
        assert_eq!(trace(normal, &no_rings, None).status, CheckStatus::Skip);
    }

    #[test]
    fn shutdown_reasons() {
        let normal = Expectations {
            mode: Mode::Normal,
            smoke: false,
            reboot: false,
            trace_autostart: true,
        };
        let f = facts("kernel::a:INFO -- hello\n");
        let reset = Ending {
            shutdown_reason: Some("guest-reset".to_owned()),
            ..exited(0)
        };
        assert_eq!(failed(&judge(normal, &f, &reset, None, None)), ["shutdown"]);
        let reboot = Expectations {
            reboot: true,
            ..normal
        };
        assert_eq!(judge(reboot, &f, &reset, None, None).result, Outcome::Pass);
        let quit = Ending {
            stop: StopReason::Down,
            shutdown_reason: Some("host-qmp-quit".to_owned()),
            powerdown_wait_s: Some(20.0),
            ..exited(0)
        };
        let v = judge(normal, &f, &quit, None, None);
        assert_eq!(v.result, Outcome::Pass);
        assert!(v.checks.iter().any(|c| c.status == CheckStatus::Warn));
    }

    #[test]
    fn scenario_results() {
        let normal = Expectations {
            mode: Mode::Normal,
            smoke: false,
            reboot: false,
            trace_autostart: true,
        };
        let f = facts("kernel::a:INFO -- hello\n");
        let bad = ScenarioSummary {
            ran: 2,
            total: 3,
            failed: Some((2, "expect timed out".to_owned())),
        };
        let v = judge(normal, &f, &exited(0), None, Some(&bad));
        assert_eq!(failed(&v), ["scenario"]);
        let good = ScenarioSummary {
            ran: 3,
            total: 3,
            failed: None,
        };
        assert_eq!(
            judge(normal, &f, &exited(0), None, Some(&good)).result,
            Outcome::Pass
        );
    }
}
