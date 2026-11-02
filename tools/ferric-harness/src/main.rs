//! `ferric-harness`: boot Ferric Oxide images in QEMU under a supervisor and drive them.
//!
//! See `docs/src/ferric-harness.md` and `tools/ferric-harness/README.md`.

use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use ferric_harness::bootenv::{self, BootEnv, BootEnvVia};
use ferric_harness::control::{self, Output, Request};
use ferric_harness::error::{Error, ErrorKind, chain};
use ferric_harness::input::{Button, ButtonAction, PointerAction};
use ferric_harness::machine::{
    self, Accel, Display, InputDevices, Iommu, LaunchConfig, Mode, Nic, OnCrash,
};
use ferric_harness::report::{self, Report, ScenarioReport};
use ferric_harness::rundir::{self, RunDir};
use ferric_harness::scenario::{self, Scenario};
use ferric_harness::serial::{Eol, Secrecy};
use ferric_harness::session::{Finish, Session, Since};
use ferric_harness::trace;
use ferric_harness::verdict::{Check, Outcome};

/// Boot Ferric Oxide images in QEMU under a supervisor: serial console, screenshots and
/// text, keyboard and pointer, boot environment, trace mode and measurements.
#[derive(Debug, Parser)]
#[command(name = "ferric-harness", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start QEMU and a background supervisor; returns once the guest runs.
    Up {
        /// The disk image (opened with snapshot=on; never modified).
        image: PathBuf,
        #[command(flatten)]
        options: UpOptions,
    },
    /// Boot, run a scenario (or wait for the guest to power off), bring the guest down and
    /// report, all in one process. Exits 0 when the verdict passes.
    Run {
        /// The disk image.
        image: PathBuf,
        /// Steps to run (TOML); without one the run waits for the guest to power off
        /// (time limit 300 s unless --timeout is given).
        #[arg(long)]
        scenario: Option<PathBuf>,
        #[command(flatten)]
        options: UpOptions,
    },
    /// Show the run's state (or its verdict once it has ended).
    Status {
        #[command(flatten)]
        run: RunArg,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Take a screenshot (PNG), kept in <run>/shots/.
    Shot {
        #[command(flatten)]
        run: RunArg,
        /// Also copy it here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Read the text of a text console on the screen.
    Text {
        #[command(flatten)]
        run: RunArg,
        /// Also copy the screenshot here.
        #[arg(long)]
        save: Option<PathBuf>,
        /// Print JSON (rows, size, unknown cells).
        #[arg(long)]
        json: bool,
    },
    /// Type text on the keyboard (US layout; \n is Enter).
    Type {
        #[command(flatten)]
        run: RunArg,
        /// The text.
        text: String,
    },
    /// Press key combinations in turn: ret, ctrl-alt-f2, meta_l-f1 (switch to VT 1), ...
    Key {
        #[command(flatten)]
        run: RunArg,
        /// Key combinations (QEMU qcode names joined by -).
        #[arg(required = true)]
        combos: Vec<String>,
    },
    /// Move the pointer or press a button.
    Pointer {
        #[command(flatten)]
        run: RunArg,
        #[command(subcommand)]
        action: PointerCmd,
    },
    /// Serial console input.
    Serial {
        #[command(flatten)]
        run: RunArg,
        #[command(subcommand)]
        action: SerialCmd,
    },
    /// Wait for a regular expression on the serial console (multi-line mode).
    Expect {
        #[command(flatten)]
        run: RunArg,
        /// The pattern.
        pattern: String,
        /// Seconds to wait.
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        /// Where to search from: cursor (after the previous match), start, now, or a mark.
        #[arg(long, default_value = "cursor")]
        since: Since,
    },
    /// Set a named mark (its time goes in the report; expect --since <name> starts there).
    Mark {
        #[command(flatten)]
        run: RunArg,
        /// The mark's name.
        name: String,
    },
    /// Wait until the screen satisfies a predicate: text:<s>, regex:<re>, changed,
    /// stable[:<ms>] or nonblank.
    WaitScreen {
        #[command(flatten)]
        run: RunArg,
        /// The predicate.
        predicate: String,
        /// Seconds to wait.
        #[arg(long, default_value_t = 60.0)]
        timeout: f64,
    },
    /// Start or stop the host trace reader (trace mode).
    Trace {
        #[command(flatten)]
        run: RunArg,
        #[command(subcommand)]
        action: TraceCmd,
    },
    /// Sample host thread CPU time and KVM statistics now (JSON).
    Stats {
        #[command(flatten)]
        run: RunArg,
    },
    /// Power the guest down (ACPI, then quit after a timeout) and write the report.
    /// Exits 0 when the verdict passes.
    Down {
        #[command(flatten)]
        run: RunArg,
        /// Seconds to wait for the guest to power off before quitting QEMU.
        #[arg(long)]
        timeout: Option<f64>,
    },
    /// Print the reports of finished runs, or compare two groups pair by pair.
    Report {
        /// Run directories (or report.json files).
        runs: Vec<PathBuf>,
        /// Group A of a paired comparison.
        #[arg(long, num_args = 1..)]
        a: Vec<PathBuf>,
        /// Group B, in the same order as A.
        #[arg(long, num_args = 1..)]
        b: Vec<PathBuf>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// The background supervisor `up` starts.
    #[command(hide = true)]
    Supervise {
        /// The run directory holding launch.json.
        #[arg(long)]
        run: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PointerCmd {
    /// Move to a position, in screen pixels (or axis units 0..32767 with --raw).
    Abs {
        /// Horizontal.
        x: u32,
        /// Vertical.
        y: u32,
        /// The coordinates are QEMU axis units, not pixels.
        #[arg(long)]
        raw: bool,
    },
    /// Move by a relative amount (ignored by ps2d while it uses the absolute vmmouse).
    Rel {
        /// Horizontal.
        #[arg(allow_hyphen_values = true)]
        dx: i32,
        /// Vertical.
        #[arg(allow_hyphen_values = true)]
        dy: i32,
    },
    /// Press, release or click (the default) a button.
    Button {
        /// The button.
        button: Button,
        #[command(flatten)]
        how: ButtonHow,
    },
}

#[derive(Debug, Args)]
#[group(multiple = false)]
struct ButtonHow {
    /// Press and release (the default).
    #[arg(long)]
    click: bool,
    /// Press and hold.
    #[arg(long)]
    down: bool,
    /// Release.
    #[arg(long)]
    up: bool,
}

impl ButtonHow {
    fn action(&self) -> ButtonAction {
        if self.down {
            ButtonAction::Down
        } else if self.up {
            ButtonAction::Up
        } else {
            ButtonAction::Click
        }
    }
}

#[derive(Debug, Subcommand)]
enum SerialCmd {
    /// Write text to the console, then a line ending.
    Send {
        /// The text; with `--secret`, it is read from standard input instead.
        #[arg(required_unless_present = "secret", conflicts_with = "secret")]
        text: Option<String>,
        /// The line ending.
        #[arg(long, value_enum, default_value_t = Eol::Cr)]
        eol: Eol,
        /// Read the text (a password) as one line of standard input, so it is not in the
        /// command line other users can see, and log it only as `<redacted N bytes>`.
        #[arg(long)]
        secret: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TraceCmd {
    /// Start `ferric-trace follow` into a new capture directory.
    Start,
    /// Stop it (it flushes and exits).
    Stop,
}

#[derive(Debug, Args)]
struct RunArg {
    /// The run directory; default $FERRIC_HARNESS_RUN, else build/harness/latest.
    #[arg(long, env = "FERRIC_HARNESS_RUN", global = true)]
    run: Option<PathBuf>,
}

impl RunArg {
    fn dir(&self) -> PathBuf {
        self.run
            .clone()
            .unwrap_or_else(|| rundir::harness_root(&cwd()).join(rundir::LATEST))
    }
}

#[derive(Debug, Args)]
struct UpOptions {
    /// The run directory; default build/harness/<name>.
    #[arg(long)]
    run: Option<PathBuf>,
    /// The run's name; default <image>-<date>-<time>.
    #[arg(long)]
    name: Option<String>,
    /// Normal kernel, or the trace-mode kernel with host-visible trace rings.
    #[arg(long, value_enum, default_value_t = Mode::Normal)]
    mode: Mode,
    /// A VGA display (read through QMP), or none as in CI's smoke boots.
    #[arg(long, value_enum, default_value_t = Display::Std)]
    display: Display,
    /// A boot environment entry KEY=VALUE (repeatable).
    #[arg(long = "boot-env", value_parser = bootenv::parse_assignment)]
    boot_env: Vec<(String, String)>,
    /// How the boot environment reaches the bootloader.
    #[arg(long, value_enum, default_value_t = BootEnvVia::FwCfg)]
    boot_env_via: BootEnvVia,
    /// Host-visible trace ring per CPU in KiB (trace mode; a power of two from 4 to 1048576).
    #[arg(long, default_value_t = 4096, value_parser = bootenv::parse_ring_kib)]
    trace_ring_kib: u32,
    /// The ferric-trace binary (trace mode); default PATH, then target/*/ferric-trace.
    #[arg(long)]
    ferric_trace: Option<PathBuf>,
    /// Do not start ferric-trace follow when the kernel announces its rings.
    #[arg(long)]
    no_trace_autostart: bool,
    /// vCPUs.
    #[arg(long, default_value_t = 4)]
    smp: u32,
    /// Guest RAM in MiB.
    #[arg(long, default_value_t = 2048)]
    mem: u64,
    /// Accelerator.
    #[arg(long, value_enum, default_value_t = Accel::Auto)]
    accel: Accel,
    /// CPU model (default host,migratable=off under KVM, max under TCG).
    #[arg(long)]
    cpu: Option<String>,
    /// Extra input devices.
    #[arg(long, value_enum, default_value_t = InputDevices::Ps2)]
    input: InputDevices,
    /// Network card.
    #[arg(long, value_enum, default_value_t = Nic::E1000)]
    nic: Nic,
    /// Emulated IOMMU.
    #[arg(long, value_enum, default_value_t = Iommu::None)]
    iommu: Iommu,
    /// Another disk image, attached with snapshot=on (repeatable).
    #[arg(long)]
    extra_disk: Vec<PathBuf>,
    /// Let the guest reset instead of ending the run.
    #[arg(long)]
    reboot: bool,
    /// Stop the run after this many seconds.
    #[arg(long)]
    timeout: Option<f64>,
    /// What a crash line on the console does.
    #[arg(long, value_enum, default_value_t = OnCrash::Stop)]
    on_crash: OnCrash,
    /// Seconds between stats samples (0: only at the end).
    #[arg(long, default_value_t = 5.0)]
    stats_interval: f64,
    /// UEFI firmware.
    #[arg(long, env = "OVMF_FIRMWARE")]
    firmware: Option<PathBuf>,
    /// The QEMU program.
    #[arg(long, default_value = "qemu-system-x86_64")]
    qemu: PathBuf,
    /// An argument between the program and QEMU's arguments, for a wrapper (repeatable).
    #[arg(long, allow_hyphen_values = true)]
    qemu_wrapper_arg: Vec<String>,
    /// An extra QEMU argument, appended (repeatable).
    #[arg(long, allow_hyphen_values = true)]
    qemu_arg: Vec<String>,
    /// Require the smoke protocol (FERRIC_SMOKE_OK and a test result).
    #[arg(long)]
    smoke: bool,
    /// Milliseconds between keyboard events.
    #[arg(long, default_value_t = 15)]
    key_delay_ms: u64,
    /// Seconds `down` waits for the guest to power off before quitting QEMU.
    #[arg(long, default_value_t = 20.0)]
    powerdown_timeout: f64,
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd().join(path)
    }
}

impl UpOptions {
    fn config(&self, image: &Path) -> Result<LaunchConfig, Error> {
        let image =
            fs::canonicalize(image).map_err(|_| Error::ImageMissing(image.to_path_buf()))?;
        let here = cwd();
        let repo_root = rundir::find_repo_root(&here);
        let run_dir = match (&self.run, &self.name) {
            (Some(dir), _) => absolute(dir),
            (None, name) => {
                let name = name.clone().unwrap_or_else(|| {
                    rundir::default_name(&image, time::OffsetDateTime::now_utc())
                });
                rundir::harness_root(&here).join(rundir::sanitize(&name))
            }
        };
        if run_dir.exists() && fs::read_dir(&run_dir).is_ok_and(|mut d| d.next().is_some()) {
            return Err(Error::RunDirExists(run_dir));
        }
        let mut config = LaunchConfig::new(image, run_dir);
        config.repo_root = repo_root;
        config.mode = self.mode;
        config.display = self.display;
        config.boot_env = self.boot_env.clone();
        config.boot_env_via = self.boot_env_via;
        config.trace_ring_kib = self.trace_ring_kib;
        config.trace_autostart = !self.no_trace_autostart;
        config.smp = self.smp;
        config.mem_mib = self.mem;
        config.accel = self.accel;
        config.cpu = self.cpu.clone();
        config.input = self.input;
        config.nic = self.nic;
        config.iommu = self.iommu;
        config.extra_disks = self.extra_disk.iter().map(|p| absolute(p)).collect();
        config.reboot = self.reboot;
        config.timeout_s = self.timeout;
        config.on_crash = self.on_crash;
        config.stats_interval_s = self.stats_interval;
        config.firmware = self.firmware.clone();
        config.qemu = self.qemu.clone();
        config.qemu_wrapper_args = self.qemu_wrapper_arg.clone();
        config.qemu_args = self.qemu_arg.clone();
        config.smoke = self.smoke;
        config.key_delay_ms = self.key_delay_ms;
        config.powerdown_timeout_s = self.powerdown_timeout;
        // Problems that would only show in the supervisor's log are caught here.
        BootEnv::for_run(
            &config.boot_env,
            config.mode,
            config.trace_ring_kib,
            config.display,
            config.boot_env_via,
        )?;
        machine::find_firmware(config.firmware.as_deref())?;
        if config.mode == Mode::Trace {
            let ferric_trace = trace::find_ferric_trace(
                self.ferric_trace.as_deref(),
                config.repo_root.as_deref(),
            )?;
            trace::check_follow(&ferric_trace)?;
            config.ferric_trace = Some(ferric_trace);
        }
        Ok(config)
    }
}

/// Exit status for an error: 1 for a failed wait or a gone guest, 2 for everything else.
fn exit_for(error: &Error) -> ExitCode {
    match error.kind() {
        ErrorKind::Timeout | ErrorKind::GuestExited => ExitCode::from(1),
        ErrorKind::BadRequest | ErrorKind::Other => ExitCode::from(2),
    }
}

fn verdict_exit(outcome: Option<Outcome>) -> ExitCode {
    match outcome {
        Some(Outcome::Pass) => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

/// One line of standard input, without its line ending: `serial send --secret`'s text. When
/// standard input is a terminal, its echo is off while the line is read ([`NoEcho`]), so the
/// secret is not shown or kept in the terminal's scrollback.
///
/// # Errors
///
/// [`Error::SecretInput`] when standard input cannot be read, or ends before a line (no
/// secret is sent then, rather than an empty one).
fn read_secret() -> Result<String, Error> {
    let stdin = std::io::stdin();
    let echo = NoEcho::on(&stdin);
    let mut line = String::new();
    let read = stdin.read_line(&mut line).map_err(Error::SecretInput)?;
    let quiet = echo.active();
    drop(echo);
    if quiet {
        // The newline the user typed was not echoed either.
        eprintln!();
    }
    if read == 0 {
        return Err(Error::SecretInput(std::io::Error::from(
            std::io::ErrorKind::UnexpectedEof,
        )));
    }
    let len = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(len);
    Ok(line)
}

/// Turns a terminal's echo off for as long as it lives, and back to what it was on drop. Does
/// nothing when standard input is not a terminal.
struct NoEcho<'a> {
    stdin: &'a std::io::Stdin,
    saved: Option<rustix::termios::Termios>,
}

impl<'a> NoEcho<'a> {
    fn on(stdin: &'a std::io::Stdin) -> Self {
        use rustix::termios::{LocalModes, OptionalActions, isatty, tcgetattr, tcsetattr};
        let saved = isatty(stdin)
            .then(|| tcgetattr(stdin).ok())
            .flatten()
            .filter(|saved| {
                let mut quiet = saved.clone();
                quiet.local_modes.remove(LocalModes::ECHO);
                tcsetattr(stdin, OptionalActions::Now, &quiet).is_ok()
            });
        Self { stdin, saved }
    }

    /// Whether echo was turned off (standard input is a terminal that allowed it).
    fn active(&self) -> bool {
        self.saved.is_some()
    }
}

impl Drop for NoEcho<'_> {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved
            && let Err(err) =
                rustix::termios::tcsetattr(self.stdin, rustix::termios::OptionalActions::Now, saved)
        {
            eprintln!("ferric-harness: restoring the terminal's echo: {err}");
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ferric-harness: {}", chain(&e));
            exit_for(&e)
        }
    }
}

fn dispatch(command: Cmd) -> Result<ExitCode, Error> {
    match command {
        Cmd::Up { image, options } => up(&image, &options),
        Cmd::Run {
            image,
            scenario,
            options,
        } => run(&image, scenario.as_deref(), &options),
        Cmd::Supervise { run } => supervise(&run),
        Cmd::Status { run, json } => status(&run.dir(), json),
        Cmd::Report { runs, a, b, json } => report_cmd(&runs, &a, &b, json),
        Cmd::Shot { run, out } => {
            match control::request(&run.dir(), &Request::Shot { name: None })? {
                Output::Shot(mut shot) => {
                    if let Some(out) = out {
                        shot.path = copy_shot(&shot.path, &out)?;
                    }
                    print_output(Output::Shot(shot))
                }
                other => print_output(other),
            }
        }
        Cmd::Text { run, save, json } => {
            match control::request(&run.dir(), &Request::Text { name: None })? {
                Output::Text(mut t) => {
                    if let Some(save) = save {
                        t.shot.path = copy_shot(&t.shot.path, &save)?;
                    }
                    if json {
                        println!("{}", to_json(&*t));
                    } else {
                        println!("{}", t.screen.text());
                    }
                }
                other => println!("{}", to_json(&other)),
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Type { run, text } => request_output(&run.dir(), &Request::Type { text }),
        Cmd::Key { run, combos } => request_output(&run.dir(), &Request::Key { combos }),
        Cmd::Pointer { run, action } => {
            let action = match action {
                PointerCmd::Abs { x, y, raw } => PointerAction::Abs { x, y, raw },
                PointerCmd::Rel { dx, dy } => PointerAction::Rel { dx, dy },
                PointerCmd::Button { button, how } => PointerAction::Button {
                    button,
                    action: how.action(),
                },
            };
            request_output(&run.dir(), &Request::Pointer { action })
        }
        Cmd::Serial {
            run,
            action: SerialCmd::Send { text, eol, secret },
        } => {
            let (text, secrecy) = match text {
                Some(text) if !secret => (text, Secrecy::Logged),
                _ => (read_secret()?, Secrecy::Secret),
            };
            request_output(&run.dir(), &Request::SerialSend { text, eol, secrecy })
        }
        Cmd::Expect {
            run,
            pattern,
            timeout,
            since,
        } => request_output(
            &run.dir(),
            &Request::Expect {
                pattern,
                timeout_s: timeout,
                since,
            },
        ),
        Cmd::Mark { run, name } => request_output(&run.dir(), &Request::Mark { name }),
        Cmd::WaitScreen {
            run,
            predicate,
            timeout,
        } => request_output(
            &run.dir(),
            &Request::WaitScreen {
                predicate,
                timeout_s: timeout,
            },
        ),
        Cmd::Trace { run, action } => {
            let request = match action {
                TraceCmd::Start => Request::TraceStart,
                TraceCmd::Stop => Request::TraceStop,
            };
            request_output(&run.dir(), &request)
        }
        Cmd::Stats { run } => request_output(&run.dir(), &Request::Stats),
        Cmd::Down { run, timeout } => {
            match control::request(&run.dir(), &Request::Down { timeout_s: timeout })? {
                Output::Down(down) => {
                    let outcome = down.verdict.as_ref().map(|v| v.result);
                    if let Some(verdict) = &down.verdict {
                        print_verdict(verdict);
                    }
                    println!("report: {}", down.report.display());
                    Ok(verdict_exit(outcome))
                }
                other => {
                    println!("{}", to_json(&other));
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// Copies a screenshot the supervisor wrote under `<run>/shots/` to where the user asked
/// for it, and returns that path. The client writes it, so the supervisor and QEMU never
/// write outside the run directory.
fn copy_shot(from: &Path, to: &Path) -> Result<PathBuf, Error> {
    let to = absolute(to);
    fs::copy(from, &to).map_err(|source| Error::Io {
        path: to.clone(),
        source,
    })?;
    Ok(to)
}

fn request_output(run: &Path, request: &Request) -> Result<ExitCode, Error> {
    print_output(control::request(run, request)?)
}

fn print_output(output: Output) -> Result<ExitCode, Error> {
    match output {
        Output::Done => {}
        Output::Shot(shot) => println!(
            "{} {}x{} at {:.0} ms",
            shot.path.display(),
            shot.width,
            shot.height,
            shot.t_ms
        ),
        Output::Expect(m) => println!("{}", m.matched.line),
        Output::Mark(mark) => println!(
            "{} at {:.3} ms (console offset {})",
            mark.name, mark.t_ms, mark.serial_offset
        ),
        Output::Trace(t) => match &t.exit {
            None => println!("tracing into {}", t.dir.display()),
            Some(exit) => println!("{}: follow exited {exit:?}", t.dir.display()),
        },
        other => println!("{}", to_json(&other)),
    }
    Ok(ExitCode::SUCCESS)
}

fn print_verdict(verdict: &ferric_harness::verdict::Verdict) {
    println!("verdict: {:?}", verdict.result);
    for check in &verdict.checks {
        println!(
            "  {:<9} {:<5} {}",
            check.name,
            format!("{:?}", check.status).to_lowercase(),
            check.detail
        );
    }
}

fn write_launch(config: &LaunchConfig) -> Result<RunDir, Error> {
    let run = RunDir::new(&config.run_dir);
    run.create()?;
    let path = run.launch_json();
    let text = serde_json::to_string_pretty(config).map_err(|source| Error::Json {
        path: path.clone(),
        source,
    })?;
    fs::write(&path, text).map_err(|source| Error::Io { path, source })?;
    if config
        .run_dir
        .parent()
        .is_some_and(|p| p == rundir::harness_root(&cwd()))
    {
        rundir::update_latest(&run)?;
    }
    Ok(run)
}

fn up(image: &Path, options: &UpOptions) -> Result<ExitCode, Error> {
    let config = options.config(image)?;
    let run = write_launch(&config)?;
    let log_path = run.supervisor_log();
    let log = fs::File::create(&log_path).map_err(|source| Error::Io {
        path: log_path.clone(),
        source,
    })?;
    let log_err = log.try_clone().map_err(|source| Error::Io {
        path: log_path.clone(),
        source,
    })?;
    let exe = std::env::current_exe().map_err(|source| Error::Spawn {
        program: PathBuf::from("ferric-harness"),
        source,
    })?;
    let mut command = Command::new(&exe);
    command
        .arg("supervise")
        .arg("--run")
        .arg(run.root())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        // Its own process group, so the terminal's Ctrl-C does not reach it.
        .process_group(0);
    let mut child = command.spawn().map_err(|source| Error::Spawn {
        program: exe.clone(),
        source,
    })?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            if let Ok(text) = fs::read_to_string(&log_path) {
                eprint!("{text}");
            }
            return Err(Error::SupervisorFailed(log_path));
        }
        if let Ok(Output::Status(status)) = control::request(run.root(), &Request::Status) {
            println!("run: {}", run.root().display());
            println!(
                "qemu pid {}, mode {}, display {:?}; commands take --run {} (or use build/harness/latest)",
                status.qemu_pid,
                status.mode,
                config.display,
                run.root().display()
            );
            return Ok(ExitCode::SUCCESS);
        }
        if Instant::now() >= deadline {
            return Err(Error::SupervisorTimeout(60.0));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Calls `on_signal` once when SIGINT or SIGTERM arrives.
fn watch_signals(on_signal: impl Fn() + Send + 'static) -> Result<(), Error> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&flag)).map_err(|source| Error::Io {
            path: PathBuf::from("signal handler"),
            source,
        })?;
    }
    thread::Builder::new()
        .name("signals".to_owned())
        .spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
            }
            on_signal();
        })
        .map(drop)
        .map_err(|source| Error::Spawn {
            program: PathBuf::from("signals"),
            source,
        })
}

fn supervise(run_dir: &Path) -> Result<ExitCode, Error> {
    let run = RunDir::new(run_dir);
    let path = run.launch_json();
    let text = fs::read_to_string(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let config: LaunchConfig =
        serde_json::from_str(&text).map_err(|source| Error::Json { path, source })?;
    let session = Session::launch(config)?;
    eprintln!(
        "supervisor: QEMU running; serving {}",
        run.control_sock().display()
    );
    {
        let session = session.clone();
        watch_signals(move || session.interrupt())?;
    }
    control::serve(&session, &run.control_sock())?;
    let report = session.wait_report(Some(Duration::from_secs(5)));
    Ok(verdict_exit(report.map(|r| r.verdict.result)))
}

fn run(image: &Path, scenario_path: Option<&Path>, options: &UpOptions) -> Result<ExitCode, Error> {
    let mut config = options.config(image)?;
    let scenario = scenario_path.map(Scenario::load).transpose()?;
    if scenario.is_none() && config.timeout_s.is_none() {
        config.timeout_s = Some(300.0);
    }
    let run = write_launch(&config)?;
    eprintln!("run: {}", run.root().display());
    // With a scenario, the report waits for its results: a guest that exits in the middle
    // of a step must not be judged before the step's failure is recorded.
    let finish = if scenario.is_some() {
        Finish::AfterScenario
    } else {
        Finish::OnExit
    };
    let session = Session::launch_with(config, finish)?;
    {
        let session = session.clone();
        watch_signals(move || session.interrupt())?;
    }
    // The other commands (status, shot, text, ...) can attach to a run in progress too.
    let server = {
        let session = session.clone();
        let sock = run.control_sock();
        thread::spawn(move || control::serve(&session, &sock))
    };
    if let Some(scenario) = &scenario {
        let (steps, summary) = scenario::run(&session, scenario);
        session.set_scenario(ScenarioReport {
            path: scenario.path.clone(),
            summary,
            steps,
        });
        if !session.exited() {
            session.down(None);
        }
    }
    let report = session.wait_report(None);
    // The server stops once the report exists; a failure to serve only cost attaching.
    if let Ok(Err(e)) = server.join() {
        eprintln!("ferric-harness: control socket: {}", chain(&e));
    }
    if let Some(report) = &report {
        print_verdict(&report.verdict);
    }
    println!("report: {}", run.report_json().display());
    Ok(verdict_exit(report.map(|r| r.verdict.result)))
}

/// `status --json` for a run that has ended: the keys it shares with a live run's
/// [`Status`](ferric_harness::session::Status) (`name`, `run_dir`, `running`, `verdict`)
/// with the same types, plus the verdict's checks and the report's path.
#[derive(serde::Serialize)]
struct EndedStatus<'a> {
    name: &'a str,
    run_dir: &'a Path,
    running: bool,
    verdict: Outcome,
    checks: &'a [Check],
    report: PathBuf,
}

impl<'a> EndedStatus<'a> {
    fn new(run_dir: &'a Path, report: &'a Report) -> Self {
        Self {
            name: &report.run.name,
            run_dir,
            running: false,
            verdict: report.verdict.result,
            checks: &report.verdict.checks,
            report: run_dir.join("report.json"),
        }
    }
}

fn status(run_dir: &Path, json: bool) -> Result<ExitCode, Error> {
    match control::request(run_dir, &Request::Status) {
        Ok(Output::Status(status)) => {
            if json {
                println!("{}", to_json(&*status));
            } else {
                println!(
                    "{} ({}): qemu pid {}, {}",
                    status.name,
                    status.mode,
                    status.qemu_pid,
                    if status.running { "running" } else { "exited" }
                );
                println!(
                    "  up {:.1} s; serial {} lines, {} bytes",
                    status.uptime_ms / 1000.0,
                    status.serial.lines,
                    status.serial.raw_bytes
                );
                let mut milestones: Vec<(&String, &f64)> = status.milestones.iter().collect();
                milestones.sort_by(|a, b| a.1.total_cmp(b.1));
                for (name, t) in milestones {
                    println!("  {name:<16} {t:>10.1} ms");
                }
                let (p, s, f) = status.tests;
                println!(
                    "  tests {p} passed, {s} skipped, {f} failed; kernel trace: lines {}",
                    status.kernel_trace_lines
                );
                if let Some(crash) = &status.crash {
                    println!("  crash: {crash}");
                }
                if let Some(rings) = &status.host_rings {
                    println!(
                        "  host rings at {:#x}, {} CPUs x {} KiB; follow {}",
                        rings.phys,
                        rings.cpus,
                        rings.kib,
                        if status.tracing { "running" } else { "stopped" }
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => {
            println!("{}", to_json(&other));
            Ok(ExitCode::SUCCESS)
        }
        Err(Error::NotRunning(dir)) => {
            let report = Report::load(&dir).map_err(|_| Error::NotRunning(dir.clone()))?;
            if json {
                println!("{}", to_json(&EndedStatus::new(&dir, &report)));
            } else {
                println!("{}: not running", report.run.name);
                print_verdict(&report.verdict);
            }
            Ok(verdict_exit(Some(report.verdict.result)))
        }
        Err(e) => Err(e),
    }
}

fn report_cmd(
    runs: &[PathBuf],
    a: &[PathBuf],
    b: &[PathBuf],
    json: bool,
) -> Result<ExitCode, Error> {
    let load = |paths: &[PathBuf]| -> Result<Vec<(PathBuf, Report)>, Error> {
        paths
            .iter()
            .map(|p| Report::load(p).map(|r| (p.clone(), r)))
            .collect()
    };
    let listed = load(runs)?;
    let group_a = load(a)?;
    let group_b = load(b)?;
    let paired = if a.is_empty() && b.is_empty() {
        None
    } else {
        let ra: Vec<Report> = group_a.iter().map(|(_, r)| r.clone()).collect();
        let rb: Vec<Report> = group_b.iter().map(|(_, r)| r.clone()).collect();
        Some(report::paired(&ra, &rb)?)
    };
    let all: Vec<(PathBuf, Report)> = listed
        .iter()
        .chain(&group_a)
        .chain(&group_b)
        .cloned()
        .collect();
    if json {
        let value = serde_json::json!({
            "runs": all.iter().map(|(p, r)| serde_json::json!({
                "dir": p,
                "name": r.run.name,
                "verdict": r.verdict.result,
                "metrics": report::metrics(r),
            })).collect::<Vec<_>>(),
            "paired": paired,
        });
        println!("{}", to_json(&value));
    } else {
        if !all.is_empty() {
            print!("{}", report::table(&all));
        }
        if let Some(p) = &paired {
            println!();
            print!("{}", report::paired_table(p));
        }
    }
    let failed = all.iter().any(|(_, r)| r.verdict.result == Outcome::Fail);
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_common_commands() {
        let cli = Cli::try_parse_from([
            "ferric-harness",
            "up",
            "img",
            "--mode",
            "trace",
            "--boot-env",
            "FERRIC_TRACE=sched.switch",
            "--qemu-arg",
            "-d",
            "--qemu-arg",
            "guest_errors",
            "--display",
            "none",
        ])
        .expect("parse");
        let Cmd::Up { options, .. } = cli.command else {
            panic!("not up")
        };
        assert_eq!(options.mode, Mode::Trace);
        assert_eq!(
            options.boot_env,
            [("FERRIC_TRACE".to_owned(), "sched.switch".to_owned())]
        );
        assert_eq!(options.qemu_arg, ["-d", "guest_errors"]);
        assert_eq!(options.trace_ring_kib, 4096);
        for bad in ["1", "3000", "2097152", "4M"] {
            assert!(
                Cli::try_parse_from(["ferric-harness", "up", "img", "--trace-ring-kib", bad])
                    .is_err(),
                "--trace-ring-kib {bad}"
            );
        }
        assert!(
            Cli::try_parse_from(["ferric-harness", "up", "img", "--boot-env", "nope"]).is_err()
        );
        let cli =
            Cli::try_parse_from(["ferric-harness", "pointer", "rel", "-5", "3"]).expect("parse");
        assert!(matches!(
            cli.command,
            Cmd::Pointer {
                action: PointerCmd::Rel { dx: -5, dy: 3 },
                ..
            }
        ));
        let cli = Cli::try_parse_from(["ferric-harness", "serial", "send", "root", "--run", "/r"])
            .expect("parse");
        assert!(matches!(
            cli.command,
            Cmd::Serial {
                run: RunArg { run: Some(_) },
                action: SerialCmd::Send { secret: false, .. },
            }
        ));
        // A secret comes from standard input, never the command line.
        let cli =
            Cli::try_parse_from(["ferric-harness", "serial", "send", "--secret"]).expect("parse");
        assert!(matches!(
            cli.command,
            Cmd::Serial {
                action: SerialCmd::Send {
                    secret: true,
                    text: None,
                    ..
                },
                ..
            }
        ));
        assert!(
            Cli::try_parse_from(["ferric-harness", "serial", "send", "--secret", "pw"]).is_err()
        );
        assert!(Cli::try_parse_from(["ferric-harness", "serial", "send"]).is_err());
        let cli = Cli::try_parse_from(["ferric-harness", "pointer", "button", "left", "--down"])
            .expect("parse");
        assert!(matches!(
            cli.command,
            Cmd::Pointer {
                action: PointerCmd::Button { how, .. },
                ..
            } if how.action() == ButtonAction::Down
        ));
        let cli =
            Cli::try_parse_from(["ferric-harness", "pointer", "button", "right"]).expect("parse");
        assert!(matches!(
            cli.command,
            Cmd::Pointer {
                action: PointerCmd::Button { how, .. },
                ..
            } if how.action() == ButtonAction::Click
        ));
        assert!(
            Cli::try_parse_from([
                "ferric-harness",
                "pointer",
                "button",
                "left",
                "--up",
                "--down"
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["ferric-harness", "expect", "x", "--since", "boot"])
            .expect("parse");
        assert!(matches!(
            cli.command,
            Cmd::Expect { since: Since::Mark(name), .. } if name == "boot"
        ));
        let cli =
            Cli::try_parse_from(["ferric-harness", "key", "meta_l-f1", "ret"]).expect("parse");
        assert!(matches!(cli.command, Cmd::Key { combos, .. } if combos.len() == 2));
        let cli = Cli::try_parse_from([
            "ferric-harness",
            "report",
            "--a",
            "r1",
            "r2",
            "--b",
            "r3",
            "r4",
        ])
        .expect("parse");
        assert!(matches!(cli.command, Cmd::Report { a, b, .. } if a.len() == 2 && b.len() == 2));
    }
}
