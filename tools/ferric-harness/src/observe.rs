//! What the console says about a boot.
//!
//! The [`Observer`] reads console lines (escape sequences, carriage returns and NULs already
//! removed) one at a time and keeps the [`Facts`] the verdict is judged on: test markers,
//! crashes, loader failures, kernel `trace:` lines. It also returns [`Notice`]s for what the
//! session must act on as it happens: a boot milestone, the bootloader's resolution menu,
//! the trace kernel's host-ring announcement, a crash.
//!
//! The patterns are the ones `scripts/ferric-ci/boot-smoke.sh` uses, so a run judged here
//! and one judged by that script agree:
//!
//! - a crash is `UNHANDLED EXCEPTION`, `KERNEL PANIC`, `kernel panicked` or `panicked at`
//!   (only `UNHANDLED EXCEPTION` when the guest may reboot, as for the crash-dump image);
//! - the smoke shell is dead when ld.so cannot link `bin/ion` or init reports that
//!   `ferric-smoke.ion` failed;
//! - `[ld.so]: failed to link ` before `FERRIC_SMOKE_OK` fails a smoke run.
//!
//! Kernel log lines look like `kernel::trace::imp:INFO -- trace: ...`; the text after
//! ` -- ` is the message. A trace-mode kernel announces itself with the message
//! `trace: trace-mode kernel` and its host-visible rings with
//! `trace: host rings at phys 0x<hex>, <n> CPUs x <KiB> KiB`.

use std::collections::BTreeSet;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::markers::{self, Marker, SmokeMarker, TestOutcome};

/// The bootloader's resolution menu prompt, printed once per video output.
pub const MENU_PROMPT: &str = "Arrow keys and enter select mode";
/// The banner of the bootloader's boot-environment editor (opened with `e` in the menu).
pub const EDITOR_BANNER: &str = "Bootloader Environment Editor";
/// The message a trace-mode kernel logs once at boot.
pub const TRACE_MODE_MESSAGE: &str = "trace: trace-mode kernel";

/// A point in a boot whose host time the report records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Milestone {
    /// The harness started QEMU (the origin of every report time).
    QemuStart,
    /// QEMU began running the guest (QMP `cont` after the harness connected).
    GuestStart,
    /// The bootloader's banner.
    Bootloader,
    /// The bootloader's resolution menu.
    BootloaderMenu,
    /// The first kernel log line.
    Kernel,
    /// The first line from init.
    Init,
    /// The first `login:` prompt.
    Login,
    /// `FERRIC_SMOKE_OK`.
    SmokeOk,
    /// QEMU reported the guest shut down, or exited.
    Shutdown,
}

impl Milestone {
    /// Every milestone, in boot order.
    pub const ALL: [Milestone; 9] = [
        Milestone::QemuStart,
        Milestone::GuestStart,
        Milestone::Bootloader,
        Milestone::BootloaderMenu,
        Milestone::Kernel,
        Milestone::Init,
        Milestone::Login,
        Milestone::SmokeOk,
        Milestone::Shutdown,
    ];

    /// The milestone's name in reports.
    pub const fn key(self) -> &'static str {
        match self {
            Milestone::QemuStart => "qemu_start",
            Milestone::GuestStart => "guest_start",
            Milestone::Bootloader => "bootloader",
            Milestone::BootloaderMenu => "bootloader_menu",
            Milestone::Kernel => "kernel",
            Milestone::Init => "init",
            Milestone::Login => "login",
            Milestone::SmokeOk => "smoke_ok",
            Milestone::Shutdown => "shutdown",
        }
    }
}

/// Where a trace-mode kernel put its host-visible rings, from its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRings {
    /// Guest-physical address of the directory page.
    pub phys: u64,
    /// Number of per-CPU rings.
    pub cpus: u32,
    /// Size of each ring in KiB.
    pub kib: u64,
}

/// Something the session acts on as soon as it appears on the console.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// A milestone was reached for the first time.
    Milestone(Milestone),
    /// A result marker.
    Marker(Marker),
    /// The bootloader's resolution menu appeared; the number counts menus from 0.
    Menu(usize),
    /// The bootloader's boot-environment editor opened.
    Editor,
    /// The trace kernel announced its host-visible rings.
    HostRings(HostRings),
    /// The first crash line.
    Crash(String),
    /// The smoke shell cannot start (the first such line).
    SmokeDead(String),
}

/// What the console showed over a whole run; the verdict is judged on it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    /// `FERRIC_TEST_PASS` lines.
    pub tests_passed: usize,
    /// `FERRIC_TEST_SKIP` lines.
    pub tests_skipped: usize,
    /// Names of failed tests, in order.
    pub tests_failed: Vec<String>,
    /// `FERRIC_SMOKE_BEGIN` appeared.
    pub smoke_begin: bool,
    /// `FERRIC_SMOKE_OK` appeared.
    pub smoke_ok: bool,
    /// The first `[ld.so]: failed to link` line before `FERRIC_SMOKE_OK`.
    pub link_failure_before_smoke_ok: Option<String>,
    /// The first crash line.
    pub crash: Option<String>,
    /// The first line saying the smoke shell cannot start.
    pub smoke_dead: Option<String>,
    /// A kernel log line appeared.
    pub kernel_started: bool,
    /// Kernel lines whose message starts with `trace:`.
    pub kernel_trace_lines: usize,
    /// The first of those lines.
    pub first_kernel_trace_line: Option<String>,
    /// The kernel said `trace: trace-mode kernel`.
    pub trace_mode_kernel: bool,
    /// The trace kernel's host-visible rings.
    pub host_rings: Option<HostRings>,
    /// Bootloader resolution menus seen.
    pub menus: usize,
}

impl Facts {
    /// Whether any test or smoke marker appeared, which makes the run a test run.
    pub fn has_test_markers(&self) -> bool {
        self.smoke_begin
            || self.smoke_ok
            || self.tests_passed + self.tests_skipped + self.tests_failed.len() > 0
    }
}

/// Reads console lines and keeps the [`Facts`].
#[derive(Debug)]
pub struct Observer {
    facts: Facts,
    reached: BTreeSet<Milestone>,
    crash: Regex,
    smoke_dead: Regex,
    host_rings: Regex,
}

impl Observer {
    /// An observer for a run; `reboot` is whether the guest may reset and boot again, in
    /// which case only `UNHANDLED EXCEPTION` counts as a crash (a panic is then expected, as
    /// in the crash-dump image's first boot).
    pub fn new(reboot: bool) -> Self {
        let crash = if reboot {
            "UNHANDLED EXCEPTION"
        } else {
            "UNHANDLED EXCEPTION|KERNEL PANIC|kernel panicked|panicked at"
        };
        Self {
            facts: Facts::default(),
            reached: BTreeSet::new(),
            crash: Regex::new(crash).expect("constant pattern"),
            smoke_dead: Regex::new(
                r"\[ld\.so\]: failed to link '([^']*/)?bin/ion':|init: .*ferric-smoke\.ion.* failed with exit status",
            )
            .expect("constant pattern"),
            host_rings: Regex::new(
                r"^trace: host rings at phys 0x([0-9a-fA-F]+), (\d+) CPUs x (\d+) KiB",
            )
            .expect("constant pattern"),
        }
    }

    /// The facts so far.
    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    /// Marks a milestone the session observed itself (QEMU start, guest start, shutdown).
    /// Returns whether it was the first time.
    pub fn reach(&mut self, milestone: Milestone) -> bool {
        self.reached.insert(milestone)
    }

    /// Reads one console line and returns what the session must act on.
    pub fn line(&mut self, line: &str) -> Vec<Notice> {
        let mut notices = Vec::new();
        if line.contains("Bootloader") && line.contains("/UEFI") {
            self.milestone(Milestone::Bootloader, &mut notices);
        }
        if line.contains(MENU_PROMPT) {
            self.milestone(Milestone::BootloaderMenu, &mut notices);
            notices.push(Notice::Menu(self.facts.menus));
            self.facts.menus += 1;
        }
        if line.contains(EDITOR_BANNER) {
            notices.push(Notice::Editor);
        }
        if line.starts_with("init:") {
            self.milestone(Milestone::Init, &mut notices);
        }
        if line.contains("login:") {
            self.milestone(Milestone::Login, &mut notices);
        }
        if let Some(message) = kernel_message(line) {
            self.facts.kernel_started = true;
            self.milestone(Milestone::Kernel, &mut notices);
            self.kernel_line(line, message, &mut notices);
        } else if line.starts_with("trace:") {
            // A kernel that prints without its logger's prefix still counts.
            self.kernel_line(line, line, &mut notices);
        }

        let marker = markers::parse_line(line);
        let smoke_ok_at = match &marker {
            Some((at, Marker::Smoke(SmokeMarker::Ok))) => Some(*at),
            _ => None,
        };
        if !self.facts.smoke_ok && self.facts.link_failure_before_smoke_ok.is_none() {
            let before = &line[..smoke_ok_at.unwrap_or(line.len())];
            if before.contains("[ld.so]: failed to link ") {
                self.facts.link_failure_before_smoke_ok = Some(line.to_owned());
            }
        }
        if let Some((_, marker)) = marker {
            self.marker(&marker, &mut notices);
            notices.push(Notice::Marker(marker));
        }

        if self.facts.crash.is_none() && self.crash.is_match(line) {
            self.facts.crash = Some(line.to_owned());
            notices.push(Notice::Crash(line.to_owned()));
        }
        if self.facts.smoke_dead.is_none() && self.smoke_dead.is_match(line) {
            self.facts.smoke_dead = Some(line.to_owned());
            notices.push(Notice::SmokeDead(line.to_owned()));
        }
        notices
    }

    fn milestone(&mut self, milestone: Milestone, notices: &mut Vec<Notice>) {
        if self.reach(milestone) {
            notices.push(Notice::Milestone(milestone));
        }
    }

    fn marker(&mut self, marker: &Marker, notices: &mut Vec<Notice>) {
        match marker {
            Marker::Test(test) => match test.outcome {
                TestOutcome::Pass => self.facts.tests_passed += 1,
                TestOutcome::Skip => self.facts.tests_skipped += 1,
                TestOutcome::Fail => self.facts.tests_failed.push(test.name.clone()),
            },
            Marker::Smoke(SmokeMarker::Begin) => self.facts.smoke_begin = true,
            Marker::Smoke(SmokeMarker::Ok) => {
                self.facts.smoke_ok = true;
                self.milestone(Milestone::SmokeOk, notices);
            }
            Marker::Smoke(SmokeMarker::Other(_)) | Marker::Bench(_) => {}
        }
    }

    fn kernel_line(&mut self, line: &str, message: &str, notices: &mut Vec<Notice>) {
        if !message.starts_with("trace:") {
            return;
        }
        self.facts.kernel_trace_lines += 1;
        if self.facts.first_kernel_trace_line.is_none() {
            self.facts.first_kernel_trace_line = Some(line.to_owned());
        }
        if message.trim_end() == TRACE_MODE_MESSAGE {
            self.facts.trace_mode_kernel = true;
        }
        if self.facts.host_rings.is_none()
            && let Some(rings) = self.parse_host_rings(message)
        {
            self.facts.host_rings = Some(rings);
            notices.push(Notice::HostRings(rings));
        }
    }

    fn parse_host_rings(&self, message: &str) -> Option<HostRings> {
        let caps = self.host_rings.captures(message)?;
        Some(HostRings {
            phys: u64::from_str_radix(&caps[1], 16).ok()?,
            cpus: caps[2].parse().ok()?,
            kib: caps[3].parse().ok()?,
        })
    }
}

/// The message of a kernel log line (`kernel::<module>:<LEVEL> -- <message>`), or `None`
/// for any other line.
pub fn kernel_message(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("kernel::")?;
    let (_, message) = rest.split_once(" -- ")?;
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe(text: &str, reboot: bool) -> (Observer, Vec<Notice>) {
        let mut observer = Observer::new(reboot);
        let notices = text.lines().flat_map(|l| observer.line(l)).collect();
        (observer, notices)
    }

    #[test]
    fn milestones_in_order() {
        let text = "BdsDxe: loading Boot0002\n\
            Redox OS Bootloader 1.0.0 on x86_64/UEFI\n\
            Arrow keys and enter select mode\n\
            kernel::arch::x86_shared::start:INFO -- Ferric Oxide starting...\n\
            kernel::startup::memory:INFO -- Memory: 1971 MB\n\
            init: switchroot to /scheme/initfs /scheme/initfs/etc\n\
            redox login: ";
        let (observer, notices) = observe(text, false);
        let reached: Vec<_> = notices
            .iter()
            .filter_map(|n| match n {
                Notice::Milestone(m) => Some(*m),
                _ => None,
            })
            .collect();
        assert_eq!(
            reached,
            [
                Milestone::Bootloader,
                Milestone::BootloaderMenu,
                Milestone::Kernel,
                Milestone::Init,
                Milestone::Login
            ]
        );
        assert!(notices.contains(&Notice::Menu(0)));
        assert!(observer.facts().kernel_started);
        assert_eq!(observer.facts().kernel_trace_lines, 0);
    }

    #[test]
    fn trace_lines_and_host_rings() {
        let text = "kernel::trace::imp:INFO -- trace: trace-mode kernel\n\
            kernel::trace::imp:INFO -- trace: host rings at phys 0x7a000000, 4 CPUs x 4096 KiB\n\
            kernel::trace::imp:INFO -- trace: sched.switch on from boot (trace.boot)";
        let (observer, notices) = observe(text, false);
        let facts = observer.facts();
        assert!(facts.trace_mode_kernel);
        assert_eq!(facts.kernel_trace_lines, 3);
        let rings = HostRings {
            phys: 0x7a00_0000,
            cpus: 4,
            kib: 4096,
        };
        assert_eq!(facts.host_rings, Some(rings));
        assert!(notices.contains(&Notice::HostRings(rings)));

        let (observer, _) = observe("trace: trace-mode kernel\n", false);
        assert!(observer.facts().trace_mode_kernel);
        // A user-space line that merely mentions tracing is not a kernel trace line.
        let (observer, _) = observe("ferric-trace: trace: nothing\n", false);
        assert_eq!(observer.facts().kernel_trace_lines, 0);
    }

    #[test]
    fn crash_patterns_follow_reboot() {
        let (observer, notices) = observe("x\nKERNEL PANIC: panicked at src/a.rs:1\n", false);
        assert!(observer.facts().crash.is_some());
        assert!(notices.iter().any(|n| matches!(n, Notice::Crash(_))));
        let (observer, _) = observe("KERNEL PANIC: panicked at src/a.rs:1\n", true);
        assert!(observer.facts().crash.is_none());
        let (observer, _) = observe("UNHANDLED EXCEPTION, CPU #0\n", true);
        assert!(observer.facts().crash.is_some());
    }

    #[test]
    fn link_failures_before_and_after_the_marker() {
        let (o, _) = observe(
            "FERRIC_TEST_PASS xstate\n[ld.so]: failed to link '/usr/bin/cat': NotFound FERRIC_SMOKE_OK\n",
            false,
        );
        assert!(o.facts().link_failure_before_smoke_ok.is_some());
        let (o, _) = observe(
            "FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK [ld.so]: failed to link '/usr/bin/cat': NotFound\n",
            false,
        );
        assert!(o.facts().link_failure_before_smoke_ok.is_none());
        assert!(o.facts().smoke_ok);
    }

    #[test]
    fn smoke_dead_lines() {
        for line in [
            "[ld.so]: failed to link '/usr/bin/ion': NotFound",
            "[ld.so]: failed to link '/bin/ion': NotFound",
            "init: cd \"/\" && env -i \"ion\" \"/usr/lib/ferric-smoke.ion\" failed with exit status: 1",
        ] {
            let (o, _) = observe(line, false);
            assert!(o.facts().smoke_dead.is_some(), "{line}");
        }
        let (o, _) = observe("[ld.so]: failed to link '/usr/bin/cat': NotFound", false);
        assert!(o.facts().smoke_dead.is_none());
    }

    #[test]
    fn markers_are_counted() {
        let (o, _) = observe(
            "FERRIC_SMOKE_BEGIN\nFERRIC_TEST_PASS a\nFERRIC_TEST_SKIP b\nFERRIC_TEST_FAIL c: why\nFERRIC_SMOKE_OK\n",
            false,
        );
        let f = o.facts();
        assert!(f.smoke_begin && f.smoke_ok && f.has_test_markers());
        assert_eq!((f.tests_passed, f.tests_skipped), (1, 1));
        assert_eq!(f.tests_failed, ["c"]);
    }
}
