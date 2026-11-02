//! A running guest: QEMU, its sockets, and the threads that watch it.
//!
//! [`Session::launch`] prepares the run directory, starts QEMU paused, connects to its QMP
//! and serial sockets, and resumes the guest. From then on:
//!
//! - the serial reader stamps console lines and the QMP reader stamps events; both feed one
//!   monitor thread, which applies them to the [`Observer`], records milestones and
//!   markers, answers the bootloader's resolution menu, starts `ferric-trace follow` when a
//!   trace kernel announces its rings, and stops the run on a time limit or crash;
//! - a sampler thread records host thread CPU time and KVM statistics every few seconds;
//! - the methods ([`Session::shot`], [`Session::type_text`], [`Session::expect`], ...) are
//!   what the supervisor's control socket and `run --scenario` call.
//!
//! When QEMU exits (the guest powered off, or [`Session::down`] or a stop ended it) and both
//! sockets have closed, the monitor stops `follow`, removes the shared RAM file, judges the
//! run and writes `report.json`; [`Session::wait_report`] returns it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use regex::bytes::RegexBuilder;
use rustix::process::{Pid, Signal, kill_process};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bootenv::{BootEnv, BootEnvVia};
use crate::clock::{Stamp, round_ms};
use crate::error::Error;
use crate::input::{self, ButtonAction, InputEvent, PointerAction, PointerEvent, Qcode};
use crate::machine::{self, HostFacts, LaunchConfig, Mode, OnCrash, QemuPlan};
use crate::markers::Marker;
use crate::observe::{self, HostRings, Milestone, Notice, Observer};
use crate::qmp::{QmpClient, QmpEvent};
use crate::report::{
    Capture, Environment, HostInfo, ImageInfo, Mark, QemuInfo, Report, RunInfo, SCHEMA,
    ScenarioReport, TimedBench, TimedMarker, TraceInfo,
};
use crate::rundir::{self, RunDir};
use crate::screen::{self, Image, PredicateState, ScreenPredicate, TextScreen};
use crate::serial::{Eol, ExpectFailure, ExpectMatch, Line, SerialConsole, SerialCounts};
use crate::stats::{self, Sample, VcpuThread};
use crate::trace::{self, Follow};
use crate::verdict::{self, Ending, Expectations, Outcome, StopReason};

/// How long the harness waits for QEMU's sockets to appear.
const STARTUP: Duration = Duration::from_secs(15);
/// Timeout for ordinary QMP commands.
const QMP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long QEMU gets to exit after `quit` before it is killed.
const QUIT_GRACE: Duration = Duration::from_secs(10);
/// How long `follow` gets to flush after SIGINT.
const FOLLOW_GRACE: Duration = Duration::from_secs(10);
/// How long a crash line is left to finish before the run stops (the backtrace follows it).
const CRASH_GRACE: Duration = Duration::from_secs(2);
/// How long a dead smoke shell waits for `FERRIC_SMOKE_OK`, as boot-smoke does.
const SMOKE_DEAD_GRACE: Duration = Duration::from_secs(1);
/// The relative path of the netq test's TFTP fixture in a checkout.
const TFTP_FIXTURE: &str = "recipes/tests/ferric-netq-test/files/fixtures/ferric-netq.txt";

/// Where `expect` starts searching.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Since {
    /// Just past the previous match (the start of the console before any).
    #[default]
    Cursor,
    /// The start of the console.
    Start,
    /// The current end of the console: only new text matches.
    Now,
    /// A named mark.
    Mark(String),
}

impl std::str::FromStr for Since {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "cursor" => Since::Cursor,
            "start" => Since::Start,
            "now" => Since::Now,
            name => Since::Mark(name.to_owned()),
        })
    }
}

/// A screenshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShotInfo {
    /// The PNG file.
    pub path: PathBuf,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Milliseconds since QEMU started.
    pub t_ms: f64,
}

/// The text read from the screen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextShot {
    /// The screenshot it was read from.
    pub shot: ShotInfo,
    /// The text.
    pub screen: TextScreen,
}

/// An `expect` result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpectResult {
    /// The match.
    #[serde(flatten)]
    pub matched: ExpectMatch,
    /// Milliseconds since QEMU started, when the match was found.
    pub t_ms: f64,
}

/// A `trace start` or `trace stop` result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceStatus {
    /// The capture directory.
    pub dir: PathBuf,
    /// How `follow` ended (`trace stop`).
    pub exit: Option<trace::FollowExit>,
}

/// What `status` reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The run's name.
    pub name: String,
    /// The run directory.
    pub run_dir: PathBuf,
    /// The image.
    pub image: PathBuf,
    /// Normal or trace.
    pub mode: Mode,
    /// QEMU's process ID.
    pub qemu_pid: u32,
    /// Whether QEMU is still running.
    pub running: bool,
    /// Milliseconds since QEMU started.
    pub uptime_ms: f64,
    /// Milestones reached, in ms since QEMU started.
    pub milestones: BTreeMap<String, f64>,
    /// Tests passed, skipped and failed so far.
    pub tests: (usize, usize, usize),
    /// The first crash line, if any.
    pub crash: Option<String>,
    /// Kernel `trace:` lines so far.
    pub kernel_trace_lines: usize,
    /// Serial counters.
    pub serial: SerialCounts,
    /// The trace kernel's rings, once announced.
    pub host_rings: Option<HostRings>,
    /// Whether `ferric-trace follow` is running.
    pub tracing: bool,
    /// Why the run is stopping, once it is.
    pub stop: Option<StopReason>,
    /// The verdict, once the run has ended.
    pub verdict: Option<Outcome>,
    /// Where the next `expect` starts by default.
    pub expect_cursor: u64,
}

/// What `down` reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownResult {
    /// The verdict.
    pub verdict: Option<verdict::Verdict>,
    /// The report file.
    pub report: PathBuf,
}

enum Feed {
    Line(Line),
    Event(QmpEvent),
    Exited(Option<i32>),
}

/// Removes the trace-mode RAM file when dropped.
#[derive(Debug)]
struct ShmFile(PathBuf);

impl ShmFile {
    fn remove(&self) {
        // Absent already is the goal state.
        fs::remove_file(&self.0).ok();
    }
}

impl Drop for ShmFile {
    fn drop(&mut self) {
        self.remove();
    }
}

#[derive(Debug, Default)]
struct State {
    milestones: BTreeMap<Milestone, f64>,
    markers: Vec<TimedMarker>,
    bench: Vec<TimedBench>,
    marks: Vec<Mark>,
    cursor: u64,
    qmp_events: BTreeMap<String, usize>,
    shutdown_reason: Option<String>,
    exit: Option<Option<i32>>,
    stop: Option<StopReason>,
    killed: bool,
    powerdown_wait_s: Option<f64>,
    guest_started: Option<Instant>,
    crash_deadline: Option<Instant>,
    smoke_dead_deadline: Option<Instant>,
    follow: Option<Follow>,
    captures: Vec<Capture>,
    shots: u32,
    scenario: Option<ScenarioReport>,
    report: Option<Report>,
}

#[derive(Debug)]
struct Sampler {
    cpus: Option<Vec<VcpuThread>>,
    file: File,
    count: usize,
    first: Option<Sample>,
    /// The last sample with KVM counters (QEMU stops answering them while it exits).
    counted: Option<Sample>,
    last: Option<Sample>,
}

#[derive(Debug)]
struct Inner {
    config: LaunchConfig,
    run: RunDir,
    origin: Stamp,
    pid: u32,
    plan: QemuPlan,
    boot_env: BootEnv,
    ferric_trace: Option<PathBuf>,
    qmp: QmpClient,
    serial: Arc<SerialConsole>,
    observer: Mutex<Observer>,
    state: Mutex<State>,
    changed: Condvar,
    sampler: Mutex<Sampler>,
    shm: Option<ShmFile>,
}

/// A running guest (cheap to clone; every clone is the same guest).
#[derive(Clone, Debug)]
pub struct Session {
    inner: Arc<Inner>,
}

fn create(path: PathBuf) -> Result<File, Error> {
    File::create(&path).map_err(|source| Error::Io { path, source })
}

/// Waits for QEMU to create the socket at `path`, failing early if QEMU exits.
fn wait_for_socket(path: &Path, child: &mut Child, log: &Path) -> Result<(), Error> {
    let deadline = Instant::now() + STARTUP;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Error::QemuStartFailed {
                code: status.code(),
                log: log.to_path_buf(),
            });
        }
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Connect {
                path: path.to_path_buf(),
                source: std::io::Error::from(std::io::ErrorKind::TimedOut),
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn connect_serial(path: &Path) -> Result<UnixStream, Error> {
    let deadline = Instant::now() + STARTUP;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(source) if Instant::now() >= deadline => {
                return Err(Error::Connect {
                    path: path.to_path_buf(),
                    source,
                });
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

impl Session {
    /// Starts a guest as `config` says (see the [module docs](self)).
    ///
    /// # Errors
    ///
    /// Configuration errors (missing image or firmware, bad boot environment, KVM or
    /// `ferric-trace` unavailable, a leftover RAM file), I/O errors in the run directory,
    /// and start-up failures (QEMU exits or its sockets never answer).
    pub fn launch(config: LaunchConfig) -> Result<Self, Error> {
        if !config.image.is_file() {
            return Err(Error::ImageMissing(config.image.clone()));
        }
        let run = RunDir::new(&config.run_dir);
        run.create()?;
        for sock in [run.qmp_sock(), run.serial_sock(), run.control_sock()] {
            rundir::check_socket_path(&sock)?;
            // A socket left by an earlier run would make QEMU's bind fail.
            fs::remove_file(&sock).ok();
        }
        let boot_env = BootEnv::for_run(
            &config.boot_env,
            config.mode,
            config.trace_ring_kib,
            config.display,
            config.boot_env_via,
        )?;
        let with_fw_cfg = config.boot_env_via == BootEnvVia::FwCfg && !boot_env.is_empty();
        if with_fw_cfg {
            let path = run.boot_env();
            fs::write(&path, boot_env.render()).map_err(|source| Error::Io { path, source })?;
        }
        if let Some(root) = &config.repo_root {
            let fixture = root.join(TFTP_FIXTURE);
            if let Some(name) = fixture.file_name() {
                // Without the fixture only the netq test's TFTP step is affected.
                fs::copy(&fixture, run.tftp_dir().join(name)).ok();
            }
        }
        let host = HostFacts {
            kvm: machine::kvm_usable(),
            firmware: machine::find_firmware(config.firmware.as_deref())?,
        };
        let plan = machine::plan(&config, &run, &host, with_fw_cfg)?;
        let ferric_trace = match config.mode {
            Mode::Trace => Some(trace::find_ferric_trace(
                config.ferric_trace.as_deref(),
                config.repo_root.as_deref(),
            )?),
            Mode::Normal => None,
        };
        if let Some(shm) = &plan.shm
            && shm.exists()
        {
            return Err(Error::ShmExists(shm.clone()));
        }
        let mut cmdline = plan.program.to_string_lossy().into_owned();
        for arg in &plan.args {
            cmdline.push('\n');
            cmdline.push_str(&arg.to_string_lossy());
        }
        let args_path = run.qemu_args();
        fs::write(&args_path, cmdline + "\n").map_err(|source| Error::Io {
            path: args_path,
            source,
        })?;

        let qemu_log = create(run.qemu_log())?;
        let qemu_err = qemu_log.try_clone().map_err(|source| Error::Io {
            path: run.qemu_log(),
            source,
        })?;
        let serial_log = create(run.serial_log())?;
        let serial_jsonl = create(run.serial_jsonl())?;
        let serial_input = create(run.serial_input_jsonl())?;
        let events = create(run.qmp_events())?;
        let stats_file = create(run.stats_jsonl())?;

        let origin = Stamp::now();
        // Its own process group: a terminal's Ctrl-C reaches the harness, which stops QEMU
        // cleanly and still writes the report.
        let mut child = Command::new(&plan.program)
            .args(&plan.args)
            .env("TMPDIR", run.tmp_dir())
            .stdin(Stdio::null())
            .stdout(qemu_log)
            .stderr(qemu_err)
            .process_group(0)
            .spawn()
            .map_err(|source| Error::Spawn {
                program: plan.program.clone(),
                source,
            })?;
        let shm = plan.shm.clone().map(ShmFile);
        let (tx, rx) = mpsc::channel();
        let connect = || -> Result<(QmpClient, Arc<SerialConsole>), Error> {
            wait_for_socket(&run.qmp_sock(), &mut child, &run.qemu_log())?;
            let tx_qmp = tx.clone();
            let qmp = QmpClient::connect(
                &run.qmp_sock(),
                STARTUP,
                Box::new(move |e| {
                    // The monitor is gone only once the run is over.
                    tx_qmp.send(Feed::Event(e)).ok();
                }),
            )?;
            wait_for_socket(&run.serial_sock(), &mut child, &run.qemu_log())?;
            let stream = connect_serial(&run.serial_sock())?;
            let tx_serial = tx.clone();
            let serial = SerialConsole::start(
                stream,
                serial_log,
                serial_jsonl,
                serial_input,
                origin,
                Box::new(move |line: &Line| {
                    tx_serial.send(Feed::Line(line.clone())).ok();
                }),
            )
            .map_err(|source| Error::Io {
                path: run.serial_sock(),
                source,
            })?;
            Ok((qmp, serial))
        };
        let (qmp, serial) = match connect() {
            Ok(connected) => connected,
            Err(e) => {
                child.kill().ok();
                child.wait().ok();
                return Err(e);
            }
        };
        let pid = child.id();
        let inner = Arc::new(Inner {
            observer: Mutex::new(Observer::new(config.reboot)),
            config,
            run,
            origin,
            pid,
            plan,
            boot_env,
            ferric_trace,
            qmp,
            serial,
            state: Mutex::default(),
            changed: Condvar::new(),
            sampler: Mutex::new(Sampler {
                cpus: None,
                file: stats_file,
                count: 0,
                first: None,
                counted: None,
                last: None,
            }),
            shm,
        });
        {
            let reached = inner.observer().reach(Milestone::QemuStart);
            if reached {
                inner.state().milestones.insert(Milestone::QemuStart, 0.0);
            }
        }
        thread::Builder::new()
            .name("qemu-waiter".to_owned())
            .spawn(move || {
                let code = child.wait().ok().and_then(|s| s.code());
                tx.send(Feed::Exited(code)).ok();
            })
            .map_err(|source| Error::Spawn {
                program: PathBuf::from("qemu-waiter"),
                source,
            })?;
        spawn_named("monitor", {
            let inner = Arc::clone(&inner);
            move || inner.monitor(rx, events)
        })?;
        if let Err(e) = inner.qmp.execute("cont", None, QMP_TIMEOUT) {
            inner.kill();
            return Err(e.into());
        }
        inner.milestone(Milestone::GuestStart, Stamp::now());
        inner.state().guest_started = Some(Instant::now());
        spawn_named("sampler", {
            let inner = Arc::clone(&inner);
            move || inner.sample_loop()
        })?;
        Ok(Self { inner })
    }

    /// The run directory.
    pub fn run(&self) -> &RunDir {
        &self.inner.run
    }

    /// The configuration.
    pub fn config(&self) -> &LaunchConfig {
        &self.inner.config
    }

    /// Milliseconds since QEMU started.
    pub fn now_ms(&self) -> f64 {
        round_ms(Stamp::now().ms_since(self.inner.origin))
    }

    /// The current state of the run.
    pub fn status(&self) -> Status {
        let inner = &self.inner;
        let facts = inner.observer().facts().clone();
        let mut state = inner.state();
        let tracing = state.follow.as_mut().is_some_and(Follow::running);
        Status {
            name: inner.run.name(),
            run_dir: inner.run.root().to_path_buf(),
            image: inner.config.image.clone(),
            mode: inner.config.mode,
            qemu_pid: inner.pid,
            running: state.exit.is_none(),
            uptime_ms: round_ms(Stamp::now().ms_since(inner.origin)),
            milestones: milestone_map(&state.milestones),
            tests: (
                facts.tests_passed,
                facts.tests_skipped,
                facts.tests_failed.len(),
            ),
            crash: facts.crash,
            kernel_trace_lines: facts.kernel_trace_lines,
            serial: inner.serial.counts(),
            host_rings: facts.host_rings,
            tracing,
            stop: state.stop,
            verdict: state.report.as_ref().map(|r| r.verdict.result),
            expect_cursor: state.cursor,
        }
    }

    /// Takes a screenshot into `out` (absolute), or `<run>/shots/shot-<n>.png`.
    ///
    /// # Errors
    ///
    /// QMP errors (the guest exited, say) and errors reading the PNG back.
    pub fn shot(&self, out: Option<&Path>) -> Result<ShotInfo, Error> {
        let path = match out {
            Some(path) => path.to_path_buf(),
            None => {
                let n = {
                    let mut state = self.inner.state();
                    state.shots += 1;
                    state.shots
                };
                self.inner
                    .run
                    .shots_dir()
                    .join(format!("shot-{n:03}-{:.0}ms.png", self.now_ms()))
            }
        };
        self.inner.screendump(&path).map(|(info, _)| info)
    }

    /// Reads the text on the screen (see [`screen::read_text`]); the screenshot is kept at
    /// `save` or under `<run>/shots/`.
    ///
    /// # Errors
    ///
    /// As [`shot`](Self::shot).
    pub fn text(&self, save: Option<&Path>) -> Result<TextShot, Error> {
        let path = match save {
            Some(path) => path.to_path_buf(),
            None => {
                let n = {
                    let mut state = self.inner.state();
                    state.shots += 1;
                    state.shots
                };
                self.inner
                    .run
                    .shots_dir()
                    .join(format!("text-{n:03}-{:.0}ms.png", self.now_ms()))
            }
        };
        let (shot, image) = self.inner.screendump(&path)?;
        Ok(TextShot {
            shot,
            screen: screen::read_text(&image),
        })
    }

    /// Types `text` on the keyboard (US layout).
    ///
    /// # Errors
    ///
    /// [`Error::Input`] for characters without a key, before anything is sent, and QMP
    /// errors.
    pub fn type_text(&self, text: &str) -> Result<(), Error> {
        let events = input::type_events(text)?;
        self.inner.send_events(&events, self.inner.key_delay())
    }

    /// Presses each key combination in turn (`ctrl-alt-f2`, `ret`, `meta_l-f1`).
    ///
    /// # Errors
    ///
    /// [`Error::Input`] for unknown key names, before anything is sent, and QMP errors.
    pub fn key(&self, combos: &[String]) -> Result<(), Error> {
        let parsed = combos
            .iter()
            .map(|c| input::parse_combo(c))
            .collect::<Result<Vec<_>, _>>()?;
        for keys in parsed {
            self.inner
                .send_events(&input::combo_events(&keys), self.inner.key_delay())?;
        }
        Ok(())
    }

    /// Moves the pointer or presses a button.
    ///
    /// # Errors
    ///
    /// QMP errors, and screenshot errors when pixel coordinates need the screen size.
    pub fn pointer(&self, action: PointerAction) -> Result<(), Error> {
        let event = |p| InputEvent::Pointer(p);
        match action {
            PointerAction::Abs { x, y, raw } => {
                let (x, y) = if raw {
                    (x, y)
                } else {
                    let probe = self.inner.run.tmp_dir().join("size.png");
                    let (shot, _) = self.inner.screendump(&probe)?;
                    (
                        input::pixel_to_abs(x, shot.width),
                        input::pixel_to_abs(y, shot.height),
                    )
                };
                self.inner
                    .send_events(&[event(PointerEvent::Abs { x, y })], Duration::ZERO)
            }
            PointerAction::Rel { dx, dy } => self
                .inner
                .send_events(&[event(PointerEvent::Rel { dx, dy })], Duration::ZERO),
            PointerAction::Button { button, action } => {
                let press = |down| event(PointerEvent::Button { button, down });
                let events: Vec<InputEvent> = match action {
                    ButtonAction::Click => vec![press(true), press(false)],
                    ButtonAction::Down => vec![press(true)],
                    ButtonAction::Up => vec![press(false)],
                };
                self.inner.send_events(&events, Duration::from_millis(60))
            }
        }
    }

    /// Writes `text` and `eol` to the serial console.
    ///
    /// # Errors
    ///
    /// [`Error::GuestExited`] when the console is closed.
    pub fn serial_send(&self, text: &str, eol: Eol) -> Result<(), Error> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.extend_from_slice(eol.bytes());
        self.inner
            .serial
            .send(&bytes)
            .map_err(|_| Error::GuestExited)
    }

    /// Waits until `pattern` (a regular expression, multi-line) matches the console from
    /// `since` on; the match moves the cursor for the next `expect`.
    ///
    /// # Errors
    ///
    /// [`Error::Regex`], [`Error::UnknownMark`], [`Error::ExpectTimeout`] (with the end of
    /// the console) and [`Error::GuestExited`].
    pub fn expect(
        &self,
        pattern: &str,
        since: &Since,
        timeout: Duration,
    ) -> Result<ExpectResult, Error> {
        let re = RegexBuilder::new(pattern).multi_line(true).build()?;
        let start = {
            let state = self.inner.state();
            match since {
                Since::Cursor => state.cursor,
                Since::Start => 0,
                Since::Now => self.inner.serial.offset(),
                Since::Mark(name) => state
                    .marks
                    .iter()
                    .rev()
                    .find(|m| &m.name == name)
                    .map(|m| m.serial_offset)
                    .ok_or_else(|| Error::UnknownMark(name.clone()))?,
            }
        };
        match self.inner.serial.expect(&re, start, timeout) {
            Ok(matched) => {
                {
                    let mut state = self.inner.state();
                    state.cursor = state.cursor.max(matched.end);
                }
                Ok(ExpectResult {
                    matched,
                    t_ms: self.now_ms(),
                })
            }
            Err(ExpectFailure::Timeout) => Err(Error::ExpectTimeout {
                pattern: pattern.to_owned(),
                seconds: timeout.as_secs_f64(),
                tail: self.inner.serial.tail(1500),
            }),
            Err(ExpectFailure::Closed) => Err(Error::GuestExited),
        }
    }

    /// Records a named mark: its time goes in the report, and `expect --since <name>`
    /// searches from the console position it records.
    pub fn mark(&self, name: &str) -> Mark {
        let mark = Mark {
            name: name.to_owned(),
            t_ms: self.now_ms(),
            serial_offset: self.inner.serial.offset(),
        };
        self.inner.state().marks.push(mark.clone());
        mark
    }

    /// Takes screenshots until `predicate` holds (see [`ScreenPredicate`]).
    ///
    /// # Errors
    ///
    /// [`Error::Screen`] for a bad predicate, [`Error::ScreenTimeout`], and screenshot
    /// errors.
    pub fn wait_screen(&self, predicate: &str, timeout: Duration) -> Result<ShotInfo, Error> {
        let parsed: ScreenPredicate = predicate.parse()?;
        let mut check = PredicateState::new(parsed);
        let deadline = Instant::now() + timeout;
        let probe = self.inner.run.tmp_dir().join("wait.png");
        loop {
            let (shot, image) = self.inner.screendump(&probe)?;
            if check.check(image, Instant::now()) {
                return Ok(shot);
            }
            if Instant::now() >= deadline {
                return Err(Error::ScreenTimeout {
                    predicate: predicate.to_owned(),
                    seconds: timeout.as_secs_f64(),
                });
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    /// Starts `ferric-trace follow` (trace mode, after the kernel announced its rings).
    ///
    /// # Errors
    ///
    /// [`Error::TraceNotEnabled`], [`Error::TraceNotReady`], [`Error::TraceRunning`], and
    /// errors starting the program.
    pub fn trace_start(&self) -> Result<TraceStatus, Error> {
        let rings = self.inner.observer().facts().host_rings;
        self.inner.start_follow(rings)
    }

    /// Stops `ferric-trace follow` (it flushes and exits).
    ///
    /// # Errors
    ///
    /// [`Error::TraceNotEnabled`] or [`Error::TraceNotRunning`].
    pub fn trace_stop(&self) -> Result<TraceStatus, Error> {
        if self.inner.config.mode != Mode::Trace {
            return Err(Error::TraceNotEnabled);
        }
        let follow = self
            .inner
            .state()
            .follow
            .take()
            .ok_or(Error::TraceNotRunning)?;
        let dir = follow.out.clone();
        let exit = follow.stop(FOLLOW_GRACE);
        self.inner.record_follow_exit(&dir, exit.clone());
        Ok(TraceStatus {
            dir,
            exit: Some(exit),
        })
    }

    /// Takes a stats sample now (also recorded in `stats.jsonl`).
    pub fn stats(&self) -> Option<Sample> {
        self.inner.sample()
    }

    /// Ends the run: presses the ACPI power button, waits `timeout` (or the configured
    /// time) for the guest to power off, then quits QEMU, and waits for the report.
    pub fn down(&self, timeout: Option<Duration>) -> DownResult {
        let wait = timeout
            .unwrap_or_else(|| Duration::from_secs_f64(self.inner.config.powerdown_timeout_s));
        self.inner.down(wait, StopReason::Down);
        self.wait_report(Some(Duration::from_secs(60)));
        DownResult {
            verdict: self
                .inner
                .state()
                .report
                .as_ref()
                .map(|r| r.verdict.clone()),
            report: self.inner.run.report_json(),
        }
    }

    /// Stops the run because the harness was interrupted.
    pub fn interrupt(&self) {
        self.inner.request_stop(StopReason::Interrupted);
    }

    /// Stops the run because its time ran out.
    pub fn time_out(&self) {
        self.inner.request_stop(StopReason::Timeout);
    }

    /// Whether QEMU has exited.
    pub fn exited(&self) -> bool {
        self.inner.state().exit.is_some()
    }

    /// Waits until QEMU has exited, or `timeout` passes; returns whether it exited.
    pub fn wait_exit(&self, timeout: Option<Duration>) -> bool {
        self.inner.wait_until(timeout, |s| s.exit.is_some())
    }

    /// Records the scenario's results for the report (call before the run ends).
    pub fn set_scenario(&self, scenario: ScenarioReport) {
        self.inner.state().scenario = Some(scenario);
    }

    /// Waits for the report (written once QEMU has exited), up to `timeout`.
    pub fn wait_report(&self, timeout: Option<Duration>) -> Option<Report> {
        self.inner.wait_until(timeout, |s| s.report.is_some());
        self.inner.state().report.clone()
    }
}

fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), Error> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .map(drop)
        .map_err(|source| Error::Spawn {
            program: PathBuf::from(name),
            source,
        })
}

fn milestone_map(milestones: &BTreeMap<Milestone, f64>) -> BTreeMap<String, f64> {
    milestones
        .iter()
        .map(|(m, t)| (m.key().to_owned(), *t))
        .collect()
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn observer(&self) -> MutexGuard<'_, Observer> {
        self.observer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ms(&self, stamp: Stamp) -> f64 {
        round_ms(stamp.ms_since(self.origin))
    }

    fn key_delay(&self) -> Duration {
        Duration::from_millis(self.config.key_delay_ms)
    }

    fn log(&self, message: &str) {
        eprintln!(
            "[{:>10.3}s] {message}",
            Stamp::now().ms_since(self.origin) / 1000.0
        );
    }

    fn wait_until(&self, timeout: Option<Duration>, done: impl Fn(&State) -> bool) -> bool {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut state = self.state();
        loop {
            if done(&state) {
                return true;
            }
            let wait = match deadline {
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return false,
                },
                None => Duration::from_secs(1),
            };
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn milestone(&self, milestone: Milestone, stamp: Stamp) {
        if self.observer().reach(milestone) {
            let t = self.ms(stamp);
            self.state().milestones.entry(milestone).or_insert(t);
        }
    }

    fn monitor(self: Arc<Self>, rx: Receiver<Feed>, mut events: File) {
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Feed::Line(line)) => self.on_line(&line),
                Ok(Feed::Event(event)) => self.on_event(&event, &mut events),
                Ok(Feed::Exited(code)) => self.on_exit(code),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.check_deadlines();
        }
        events.flush().ok();
        self.finish();
    }

    fn on_line(self: &Arc<Self>, line: &Line) {
        let notices = self.observer().line(&line.text);
        if notices.is_empty() {
            return;
        }
        let t = self.ms(line.stamp);
        let mut start_rings = None;
        {
            let mut state = self.state();
            for notice in notices {
                match notice {
                    Notice::Milestone(m) => {
                        state.milestones.entry(m).or_insert(t);
                    }
                    Notice::Marker(Marker::Bench(record)) => {
                        state.bench.push(TimedBench { t_ms: t, record });
                    }
                    Notice::Marker(marker) => state.markers.push(TimedMarker { t_ms: t, marker }),
                    Notice::Menu(index) => {
                        let inner = Arc::clone(self);
                        let offset = self.serial.offset();
                        // Typing takes a while; the monitor must keep reading meanwhile.
                        spawn_named("menu", move || inner.answer_menu(index, offset)).ok();
                    }
                    Notice::Editor => self.log("bootloader: environment editor open"),
                    Notice::HostRings(rings) => {
                        self.log(&format!(
                            "trace: host rings at {:#x}, {} CPUs x {} KiB",
                            rings.phys, rings.cpus, rings.kib
                        ));
                        if self.config.mode == Mode::Trace && self.config.trace_autostart {
                            start_rings = Some(rings);
                        }
                    }
                    Notice::Crash(text) => {
                        self.log(&format!("crash on the console: {text}"));
                        if self.config.on_crash == OnCrash::Stop {
                            state
                                .crash_deadline
                                .get_or_insert(Instant::now() + CRASH_GRACE);
                        }
                    }
                    Notice::SmokeDead(text) => {
                        self.log(&format!("the smoke shell cannot start: {text}"));
                        state
                            .smoke_dead_deadline
                            .get_or_insert(Instant::now() + SMOKE_DEAD_GRACE);
                    }
                }
            }
        }
        if let Some(rings) = start_rings
            && let Err(e) = self.start_follow(Some(rings))
        {
            self.log(&format!(
                "cannot start ferric-trace follow: {}",
                crate::error::chain(&e)
            ));
        }
    }

    fn on_event(self: &Arc<Self>, event: &QmpEvent, events: &mut File) {
        let record = json!({
            "t_ms": self.ms(event.stamp),
            "mono_ns": event.stamp.mono_ns,
            "event": event.event,
            "data": event.data,
        });
        writeln!(events, "{record}").ok();
        let shutdown = event.event == "SHUTDOWN";
        {
            let mut state = self.state();
            *state.qmp_events.entry(event.event.clone()).or_default() += 1;
            if shutdown {
                state.shutdown_reason = event
                    .data
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
        if shutdown {
            self.milestone(Milestone::Shutdown, event.stamp);
            // The last chance to read QEMU's threads before it exits.
            let inner = Arc::clone(self);
            spawn_named("final-sample", move || {
                inner.sample();
            })
            .ok();
        }
    }

    fn on_exit(&self, code: Option<i32>) {
        self.milestone(Milestone::Shutdown, Stamp::now());
        self.log(&format!("QEMU exited with {code:?}"));
        self.state().exit = Some(code);
        self.changed.notify_all();
    }

    fn check_deadlines(self: &Arc<Self>) {
        let now = Instant::now();
        let reason =
            {
                let mut state = self.state();
                if state.exit.is_some() || state.stop.is_some() {
                    return;
                }
                let timed_out = self.config.timeout_s.zip(state.guest_started).is_some_and(
                    |(limit, started)| now.duration_since(started).as_secs_f64() >= limit,
                );
                if timed_out {
                    Some(StopReason::Timeout)
                } else if state.crash_deadline.is_some_and(|d| now >= d) {
                    Some(StopReason::Crash)
                } else if state.smoke_dead_deadline.is_some_and(|d| now >= d) {
                    if self.observer().facts().smoke_ok {
                        state.smoke_dead_deadline = None;
                        None
                    } else {
                        Some(StopReason::SmokeDead)
                    }
                } else {
                    None
                }
            };
        if let Some(reason) = reason {
            self.request_stop(reason);
        }
    }

    /// Starts stopping the run for `reason` without waiting: a final sample, then `quit`.
    fn request_stop(self: &Arc<Self>, reason: StopReason) {
        {
            let mut state = self.state();
            if state.exit.is_some() || state.stop.is_some() {
                return;
            }
            state.stop = Some(reason);
        }
        self.log(&format!("stopping: {reason:?}"));
        let inner = Arc::clone(self);
        spawn_named("stop", move || {
            inner.sample();
            inner.quit();
        })
        .ok();
    }

    fn down(&self, wait: Duration, reason: StopReason) {
        {
            let mut state = self.state();
            if state.exit.is_some() {
                return;
            }
            state.stop.get_or_insert(reason);
        }
        self.sample();
        self.log("down: ACPI power button");
        if self
            .qmp
            .execute("system_powerdown", None, QMP_TIMEOUT)
            .is_ok()
            && self.wait_until(Some(wait), |s| s.exit.is_some())
        {
            return;
        }
        self.state().powerdown_wait_s = Some(wait.as_secs_f64());
        self.log(&format!(
            "down: the guest did not power off within {}s; quitting QEMU",
            wait.as_secs_f64()
        ));
        self.quit();
    }

    /// Asks QEMU to quit and kills it if it does not.
    fn quit(&self) {
        // QEMU may already be gone; the wait below covers both cases.
        self.qmp.execute("quit", None, QMP_TIMEOUT).ok();
        if !self.wait_until(Some(QUIT_GRACE), |s| s.exit.is_some()) {
            self.kill();
        }
    }

    fn kill(&self) {
        self.log("killing QEMU");
        self.state().killed = true;
        if let Some(pid) = Pid::from_raw(i32::try_from(self.pid).unwrap_or(0)) {
            kill_process(pid, Signal::KILL).ok();
        }
    }

    fn answer_menu(&self, index: usize, offset: u64) {
        // The menu reads keys once it has drawn the mode list.
        thread::sleep(Duration::from_millis(300));
        let typed = if index == 0 && self.config.boot_env_via == BootEnvVia::Keys {
            self.boot_env.typed_lines()
        } else {
            Vec::new()
        };
        // The firmware's keyboard driver polls; give it more time than the guest's.
        let delay = self.key_delay().max(Duration::from_millis(30));
        let result = if typed.is_empty() {
            self.send_events(&input::combo_events(&[Qcode::RET]), delay)
        } else {
            self.type_boot_env(&typed, offset, delay)
        };
        match result {
            Ok(()) if typed.is_empty() => self.log(&format!("bootloader: answered menu {index}")),
            Ok(()) => self.log(&format!(
                "bootloader: typed {} boot environment line(s)",
                typed.len()
            )),
            Err(e) => self.log(&format!(
                "bootloader: cannot answer menu {index}: {}",
                crate::error::chain(&e)
            )),
        }
    }

    fn type_boot_env(&self, lines: &[String], offset: u64, delay: Duration) -> Result<(), Error> {
        let e = input::key_name("e")?;
        self.send_events(&input::combo_events(&[e]), delay)?;
        let banner = RegexBuilder::new(&regex::escape(observe::EDITOR_BANNER)).build()?;
        if self
            .serial
            .expect(&banner, offset, Duration::from_secs(5))
            .is_err()
        {
            self.log("bootloader: no editor banner on the console; typing anyway");
        }
        thread::sleep(Duration::from_millis(200));
        for line in lines {
            self.send_events(&input::type_events(line)?, delay)?;
            self.send_events(&input::combo_events(&[Qcode::RET]), delay)?;
        }
        // Enter on the empty line boots.
        self.send_events(&input::combo_events(&[Qcode::RET]), delay)
    }

    fn send_events(&self, events: &[InputEvent], delay: Duration) -> Result<(), Error> {
        for event in events {
            self.qmp.execute(
                "input-send-event",
                Some(json!({"events": event.to_qmp()})),
                QMP_TIMEOUT,
            )?;
            if !delay.is_zero() {
                thread::sleep(delay);
            }
        }
        Ok(())
    }

    fn screendump(&self, path: &Path) -> Result<(ShotInfo, Image), Error> {
        self.qmp.execute(
            "screendump",
            Some(json!({"filename": path, "format": "png"})),
            QMP_TIMEOUT,
        )?;
        let t_ms = round_ms(Stamp::now().ms_since(self.origin));
        let bytes = fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let image = Image::from_png(&bytes)?;
        Ok((
            ShotInfo {
                path: path.to_path_buf(),
                width: image.width,
                height: image.height,
                t_ms,
            },
            image,
        ))
    }

    fn start_follow(&self, rings: Option<HostRings>) -> Result<TraceStatus, Error> {
        if self.config.mode != Mode::Trace {
            return Err(Error::TraceNotEnabled);
        }
        let rings = rings.ok_or(Error::TraceNotReady)?;
        let (Some(binary), Some(ram)) = (&self.ferric_trace, &self.plan.shm) else {
            return Err(Error::TraceNotEnabled);
        };
        let mut state = self.state();
        if state.follow.as_mut().is_some_and(Follow::running) {
            return Err(Error::TraceRunning);
        }
        let dir = self.run.trace_dir(state.captures.len() + 1);
        let follow = Follow::spawn(
            binary,
            ram,
            rings,
            self.config.mem_mib,
            &dir,
            &self.run.trace_follow_log(),
        )?;
        self.log(&format!(
            "trace: ferric-trace follow writing {}",
            dir.display()
        ));
        state.captures.push(Capture {
            dir: dir.clone(),
            started_ms: round_ms(Stamp::now().ms_since(self.origin)),
            exit: None,
        });
        state.follow = Some(follow);
        Ok(TraceStatus { dir, exit: None })
    }

    fn record_follow_exit(&self, dir: &Path, exit: trace::FollowExit) {
        if let Some(capture) = self
            .state()
            .captures
            .iter_mut()
            .rev()
            .find(|c| c.dir == dir)
        {
            capture.exit = Some(exit);
        }
    }

    fn sample_loop(self: Arc<Self>) {
        if self.config.stats_interval_s <= 0.0 {
            return;
        }
        let interval = Duration::from_secs_f64(self.config.stats_interval_s);
        loop {
            let stopped = self.wait_until(Some(interval), |s| s.exit.is_some() || s.stop.is_some());
            if stopped {
                return;
            }
            self.sample();
        }
    }

    /// Samples QEMU's threads and KVM statistics, records the sample and returns it; `None`
    /// once QEMU has gone.
    fn sample(&self) -> Option<Sample> {
        let threads = stats::read_threads(self.pid).ok()?;
        let short = Duration::from_secs(2);
        let cpus = {
            let cached = self
                .sampler
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cpus
                .clone();
            match cached {
                Some(cpus) => cpus,
                None => {
                    let cpus = self
                        .qmp
                        .execute("query-cpus-fast", None, short)
                        .map(|v| stats::parse_cpus(&v))
                        .unwrap_or_default();
                    if !cpus.is_empty() {
                        self.sampler
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .cpus = Some(cpus.clone());
                    }
                    cpus
                }
            }
        };
        let query = |target: &str| {
            self.qmp
                .execute("query-stats", Some(json!({"target": target})), short)
                .map(|v| stats::parse_stats(&v))
                .unwrap_or_default()
        };
        let vcpu_stats = query("vcpu");
        let vm_stats = query("vm");
        let t_ms = round_ms(Stamp::now().ms_since(self.origin));
        let sample = Sample::assemble(t_ms, threads, &cpus, &vcpu_stats, &vm_stats);
        let mut sampler = self.sampler.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(text) = serde_json::to_string(&sample) {
            writeln!(sampler.file, "{text}").ok();
        }
        sampler.count += 1;
        sampler.first.get_or_insert_with(|| sample.clone());
        if sample.vcpus.iter().any(|v| !v.kvm.is_empty()) {
            sampler.counted = Some(sample.clone());
        }
        sampler.last = Some(sample.clone());
        Some(sample)
    }

    fn finish(&self) {
        let follow = self.state().follow.take();
        if let Some(follow) = follow {
            let dir = follow.out.clone();
            let exit = follow.stop(FOLLOW_GRACE);
            self.log(&format!("trace: follow stopped: {exit:?}"));
            self.record_follow_exit(&dir, exit);
        }
        if let Some(shm) = &self.shm {
            shm.remove();
        }
        let report = self.build_report();
        if let Err(e) = report.write(&self.run.report_json()) {
            self.log(&format!(
                "cannot write the report: {}",
                crate::error::chain(&e)
            ));
        }
        let failures: Vec<String> = report
            .verdict
            .failures()
            .map(|c| format!("{}: {}", c.name, c.detail))
            .collect();
        self.log(&format!(
            "verdict: {:?}{}",
            report.verdict.result,
            if failures.is_empty() {
                String::new()
            } else {
                format!(" ({})", failures.join("; "))
            }
        ));
        for sock in [self.run.qmp_sock(), self.run.serial_sock()] {
            fs::remove_file(sock).ok();
        }
        self.state().report = Some(report);
        self.changed.notify_all();
    }

    fn build_report(&self) -> Report {
        let facts = self.observer().facts().clone();
        let (kept, count) = {
            let sampler = self.sampler.lock().unwrap_or_else(PoisonError::into_inner);
            let mut kept: Vec<Sample> = [&sampler.first, &sampler.counted, &sampler.last]
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            kept.dedup();
            (kept, sampler.count)
        };
        let mut stats = stats::summarize(&kept);
        stats.samples = count;
        let state = self.state();
        let ending = Ending {
            stop: state.stop.unwrap_or(StopReason::GuestExit),
            qemu_exit: state.exit.flatten(),
            shutdown_reason: state.shutdown_reason.clone(),
            killed: state.killed,
            powerdown_wait_s: state.powerdown_wait_s,
            timeout_s: self.config.timeout_s,
        };
        let scenario_summary = state.scenario.as_ref().map(|s| s.summary.clone());
        let verdict = verdict::judge(
            Expectations {
                mode: self.config.mode,
                smoke: self.config.smoke,
                reboot: self.config.reboot,
            },
            &facts,
            &ending,
            scenario_summary.as_ref(),
        );
        let trace = (self.config.mode == Mode::Trace).then(|| TraceInfo {
            host_rings: facts.host_rings,
            guest_ram: self.plan.shm.clone(),
            captures: state.captures.clone(),
        });
        Report {
            schema: SCHEMA,
            run: RunInfo {
                name: self.run.name(),
                dir: self.run.root().to_path_buf(),
                started: self.origin.wall_rfc3339(),
                qemu_start_mono_ns: self.origin.mono_ns,
                duration_ms: round_ms(Stamp::now().ms_since(self.origin)),
            },
            environment: self.environment(),
            options: self.config.clone(),
            verdict,
            ending,
            facts,
            milestones: milestone_map(&state.milestones),
            markers: state.markers.clone(),
            bench: state.bench.clone(),
            marks: state.marks.clone(),
            serial: self.serial.counts(),
            qmp_events: state.qmp_events.clone(),
            stats,
            trace,
            scenario: state.scenario.clone(),
        }
    }

    fn environment(&self) -> Environment {
        let git = |args: &[&str]| -> Option<String> {
            let root = self.config.repo_root.as_ref()?;
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        let cpu_model = fs::read_to_string("/proc/cpuinfo").ok().and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
        });
        let image_meta = fs::metadata(&self.config.image).ok();
        Environment {
            commit: git(&["rev-parse", "HEAD"]),
            dirty: git(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty()),
            qemu: QemuInfo {
                program: self.plan.program.clone(),
                version: self.qmp.version(),
                accel: format!("{:?}", self.plan.accel).to_lowercase(),
                cpu: self.plan.cpu.clone(),
                args: self
                    .plan
                    .args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            },
            host: HostInfo {
                cpu_model,
                cpus: thread::available_parallelism().map_or(0, usize::from),
                kernel: fs::read_to_string("/proc/sys/kernel/osrelease")
                    .ok()
                    .map(|s| s.trim().to_owned()),
            },
            image: ImageInfo {
                path: self.config.image.clone(),
                size: image_meta.as_ref().map_or(0, fs::Metadata::len),
                modified: image_meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| {
                        Stamp {
                            mono_ns: 0,
                            wall_ns: u64::try_from(d.as_nanos()).unwrap_or(0),
                        }
                        .wall_rfc3339()
                    }),
            },
        }
    }
}
