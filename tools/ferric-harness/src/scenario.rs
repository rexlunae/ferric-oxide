//! Scripted runs: `ferric-harness run <image> --scenario <file.toml>`.
//!
//! A scenario is a TOML file with a list of steps, run in order against a fresh guest;
//! the first failing step ends the scenario, and the run is then brought down and judged
//! (the verdict's `scenario` check fails). Each step is a table with one action key and an
//! optional `timeout` in seconds (default: the file's `timeout`, else 60):
//!
//! ```toml
//! description = "log in on the serial console"
//! timeout = 120
//!
//! [[step]]
//! expect = "login: $"           # regular expression on the console (multi-line)
//! timeout = 300
//! [[step]]
//! send = "root"                 # serial input; `eol` = "cr" (default), "lf", "crlf", "none"
//! [[step]]
//! expect = "assword"
//! [[step]]
//! send = "password"
//! [[step]]
//! type = "echo hi\n"            # keyboard (US layout)
//! [[step]]
//! key = ["meta_l-f1"]           # key combinations, a string or a list
//! [[step]]
//! pointer = { abs = [640, 400] }              # pixels; `raw = true` for 0..0x7fff
//! [[step]]
//! pointer = { button = "left", action = "click" }
//! [[step]]
//! shot = "desktop.png"          # screenshot file name in <run>/shots; `true` names it
//! [[step]]
//! text = "vt1.txt"              # read the screen into <run>/vt1.txt (a plain file name); `true` only logs it
//! contains = "login"            # optional: fail unless the text contains this
//! [[step]]
//! wait = "text:redox login"     # a wait-screen predicate
//! [[step]]
//! sleep = 2.5                   # may not exceed the step's timeout
//! [[step]]
//! mark = "workload"
//! [[step]]
//! trace-start = true
//! [[step]]
//! trace-stop = true
//! [[step]]
//! wait-exit = true              # wait for the guest to power off by itself
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use thiserror::Error;

use crate::error::{Error, chain};
use crate::input::{self, Button, ButtonAction, InputError, PointerAction};
use crate::report::StepResult;
use crate::rundir::{self, NotFileName};
use crate::screen::{ScreenError, ScreenPredicate};
use crate::serial::Eol;
use crate::session::{Session, Since};
use crate::verdict::ScenarioSummary;

/// A scenario file that cannot be used.
#[derive(Debug, Error)]
pub enum ScenarioError {
    /// The file cannot be read.
    #[error("cannot read scenario {path}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML for a scenario.
    #[error("invalid scenario {path}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: toml::de::Error,
    },
    /// A step with no action.
    #[error(
        "step {0} has no action (expect, send, type, key, pointer, shot, text, wait, sleep, mark, trace-start, trace-stop, wait-exit)"
    )]
    NoAction(usize),
    /// A step with several actions.
    #[error("step {step} has more than one action: {actions:?}")]
    ManyActions {
        /// The step number.
        step: usize,
        /// The actions it names.
        actions: Vec<&'static str>,
    },
    /// A step whose action is malformed.
    #[error("step {step}")]
    Invalid {
        /// The step number.
        step: usize,
        /// What is wrong.
        #[source]
        problem: StepProblem,
    },
}

/// What is wrong with a malformed scenario step.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StepProblem {
    /// An `expect` pattern that is not a regular expression.
    #[error("invalid regular expression")]
    Regex(#[from] regex::Error),
    /// A `type` text or `key` combination the keyboard cannot send.
    #[error(transparent)]
    Input(#[from] InputError),
    /// A `wait` predicate that does not parse.
    #[error(transparent)]
    Predicate(#[from] ScreenError),
    /// A `shot` or `text` file name that is not a plain file name, which could put the file
    /// outside the run directory.
    #[error("{action}: {name}")]
    FileName {
        /// The step's action, `shot` or `text`.
        action: &'static str,
        /// The name refused.
        #[source]
        name: NotFileName,
    },
    /// A `pointer` table without exactly one of `abs`, `rel` and `button`.
    #[error("pointer needs exactly one of abs, rel or button")]
    PointerShape,
    /// `shot = false` or `text = false`, a step that would do nothing.
    #[error("{action} = false does nothing; remove the step")]
    DoesNothing {
        /// The action's key.
        action: &'static str,
    },
    /// A `sleep` or `timeout` outside 0 to 86400 seconds.
    #[error("{field} = {seconds} is outside 0 to 86400 seconds")]
    OutOfRange {
        /// The key.
        field: &'static str,
        /// Its value.
        seconds: f64,
    },
    /// A `sleep` longer than the step's time limit, which would let the step overrun it.
    #[error("sleep = {sleep} is longer than the step's timeout ({timeout} s)")]
    SleepPastTimeout {
        /// The sleep in seconds.
        sleep: f64,
        /// The step's effective timeout in seconds (its own, else the scenario's).
        timeout: f64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FileOrFlag {
    Flag(bool),
    File(String),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PointerSpec {
    abs: Option<[u32; 2]>,
    rel: Option<[i32; 2]>,
    button: Option<Button>,
    action: Option<ButtonAction>,
    #[serde(default)]
    raw: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct StepSpec {
    expect: Option<String>,
    since: Option<String>,
    send: Option<String>,
    eol: Option<Eol>,
    #[serde(rename = "type")]
    type_text: Option<String>,
    key: Option<OneOrMany>,
    pointer: Option<PointerSpec>,
    shot: Option<FileOrFlag>,
    text: Option<FileOrFlag>,
    contains: Option<String>,
    wait: Option<String>,
    sleep: Option<f64>,
    mark: Option<String>,
    trace_start: Option<bool>,
    trace_stop: Option<bool>,
    wait_exit: Option<bool>,
    timeout: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioFile {
    description: Option<String>,
    timeout: Option<f64>,
    #[serde(default)]
    step: Vec<StepSpec>,
}

/// One step's action.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Wait for a pattern on the console.
    Expect {
        /// The pattern.
        pattern: String,
        /// Where to start.
        since: Since,
    },
    /// Write to the console.
    Send {
        /// The text.
        text: String,
        /// The line ending.
        eol: Eol,
    },
    /// Type on the keyboard.
    Type(String),
    /// Press key combinations.
    Key(Vec<String>),
    /// Pointer input.
    Pointer(PointerAction),
    /// Take a screenshot (to the file of this name in `<run>/shots/`, or named
    /// automatically).
    Shot(Option<String>),
    /// Read the screen's text, optionally saving it and requiring a substring.
    Text {
        /// Where to save the text, under the run directory.
        save: Option<PathBuf>,
        /// A substring that must appear.
        contains: Option<String>,
    },
    /// Wait for a screen predicate.
    Wait(String),
    /// Sleep.
    Sleep(Duration),
    /// Set a mark.
    Mark(String),
    /// Start `ferric-trace follow`.
    TraceStart,
    /// Stop `ferric-trace follow`.
    TraceStop,
    /// Wait for QEMU to exit.
    WaitExit,
}

impl Action {
    /// A short description for logs and the report.
    pub fn describe(&self) -> String {
        match self {
            Action::Expect { pattern, .. } => format!("expect {pattern:?}"),
            Action::Send { text, .. } => format!("send {text:?}"),
            Action::Type(text) => format!("type {text:?}"),
            Action::Key(keys) => format!("key {}", keys.join(" ")),
            Action::Pointer(p) => format!("pointer {p:?}"),
            Action::Shot(name) => format!("shot {}", name.as_deref().unwrap_or("")),
            Action::Text { save, .. } => format!(
                "text {}",
                save.as_deref()
                    .map_or("".into(), |p| p.display().to_string())
            ),
            Action::Wait(p) => format!("wait {p}"),
            Action::Sleep(d) => format!("sleep {}s", d.as_secs_f64()),
            Action::Mark(name) => format!("mark {name}"),
            Action::TraceStart => "trace-start".to_owned(),
            Action::TraceStop => "trace-stop".to_owned(),
            Action::WaitExit => "wait-exit".to_owned(),
        }
    }
}

/// A step: an action and how long it may take.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// What to do.
    pub action: Action,
    /// Its time limit.
    pub timeout: Duration,
}

/// A parsed scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    /// The file it came from.
    pub path: PathBuf,
    /// Its description.
    pub description: Option<String>,
    /// Its steps.
    pub steps: Vec<Step>,
}

impl Scenario {
    /// Reads and checks a scenario file: every pattern, predicate, key and character is
    /// validated before the guest boots.
    ///
    /// # Errors
    ///
    /// Any [`ScenarioError`].
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        let text = fs::read_to_string(path).map_err(|source| ScenarioError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(path, &text)
    }

    /// Parses scenario text (`path` is only for messages).
    ///
    /// # Errors
    ///
    /// Any [`ScenarioError`].
    pub fn parse(path: &Path, text: &str) -> Result<Self, ScenarioError> {
        let file: ScenarioFile = toml::from_str(text).map_err(|source| ScenarioError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        let default_timeout = file.timeout.unwrap_or(60.0);
        let steps = file
            .step
            .into_iter()
            .enumerate()
            .map(|(i, spec)| convert(i + 1, spec, default_timeout))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            path: path.to_path_buf(),
            description: file.description,
            steps,
        })
    }
}

fn convert(step: usize, spec: StepSpec, default_timeout: f64) -> Result<Step, ScenarioError> {
    let invalid = |problem: StepProblem| ScenarioError::Invalid { step, problem };
    let mut actions: Vec<(&'static str, Action)> = Vec::new();
    if let Some(pattern) = spec.expect {
        regex::bytes::Regex::new(&pattern).map_err(|e| invalid(e.into()))?;
        let since = match spec.since.as_deref().map(str::parse) {
            None => Since::Cursor,
            Some(Ok(since)) => since,
        };
        actions.push(("expect", Action::Expect { pattern, since }));
    }
    if let Some(text) = spec.send {
        actions.push((
            "send",
            Action::Send {
                text,
                eol: spec.eol.unwrap_or_default(),
            },
        ));
    }
    if let Some(text) = spec.type_text {
        input::type_events(&text).map_err(|e| invalid(e.into()))?;
        actions.push(("type", Action::Type(text)));
    }
    if let Some(keys) = spec.key {
        let keys = match keys {
            OneOrMany::One(k) => vec![k],
            OneOrMany::Many(ks) => ks,
        };
        for combo in &keys {
            input::parse_combo(combo).map_err(|e| invalid(e.into()))?;
        }
        actions.push(("key", Action::Key(keys)));
    }
    if let Some(p) = spec.pointer {
        actions.push(("pointer", Action::Pointer(pointer(p).map_err(invalid)?)));
    }
    if let Some(shot) = spec.shot {
        let name = file_or_flag(shot, "shot").map_err(invalid)?;
        if let Some(name) = &name {
            rundir::file_name(name).map_err(|name| {
                invalid(StepProblem::FileName {
                    action: "shot",
                    name,
                })
            })?;
        }
        actions.push(("shot", Action::Shot(name)));
    }
    if let Some(text) = spec.text {
        let save = file_or_flag(text, "text").map_err(invalid)?;
        if let Some(name) = &save {
            rundir::file_name(name).map_err(|name| {
                invalid(StepProblem::FileName {
                    action: "text",
                    name,
                })
            })?;
        }
        let save = save.map(PathBuf::from);
        actions.push((
            "text",
            Action::Text {
                save,
                contains: spec.contains,
            },
        ));
    }
    if let Some(predicate) = spec.wait {
        predicate
            .parse::<ScreenPredicate>()
            .map_err(|e| invalid(e.into()))?;
        actions.push(("wait", Action::Wait(predicate)));
    }
    if let Some(seconds) = spec.sleep {
        if !(0.0..=86_400.0).contains(&seconds) {
            return Err(invalid(StepProblem::OutOfRange {
                field: "sleep",
                seconds,
            }));
        }
        actions.push(("sleep", Action::Sleep(Duration::from_secs_f64(seconds))));
    }
    if let Some(name) = spec.mark {
        actions.push(("mark", Action::Mark(name)));
    }
    if spec.trace_start == Some(true) {
        actions.push(("trace-start", Action::TraceStart));
    }
    if spec.trace_stop == Some(true) {
        actions.push(("trace-stop", Action::TraceStop));
    }
    if spec.wait_exit == Some(true) {
        actions.push(("wait-exit", Action::WaitExit));
    }
    let timeout = spec.timeout.unwrap_or(default_timeout);
    if !(0.0..=86_400.0).contains(&timeout) {
        return Err(invalid(StepProblem::OutOfRange {
            field: "timeout",
            seconds: timeout,
        }));
    }
    if let Some(sleep) = spec.sleep
        && sleep > timeout
    {
        return Err(invalid(StepProblem::SleepPastTimeout { sleep, timeout }));
    }
    match actions.len() {
        0 => Err(ScenarioError::NoAction(step)),
        1 => Ok(Step {
            action: actions.remove(0).1,
            timeout: Duration::from_secs_f64(timeout),
        }),
        _ => Err(ScenarioError::ManyActions {
            step,
            actions: actions.iter().map(|(name, _)| *name).collect(),
        }),
    }
}

/// `true` or a file name; `false` would be a step that does nothing.
fn file_or_flag(value: FileOrFlag, action: &'static str) -> Result<Option<String>, StepProblem> {
    match value {
        FileOrFlag::File(path) => Ok(Some(path)),
        FileOrFlag::Flag(true) => Ok(None),
        FileOrFlag::Flag(false) => Err(StepProblem::DoesNothing { action }),
    }
}

fn pointer(p: PointerSpec) -> Result<PointerAction, StepProblem> {
    match (p.abs, p.rel, p.button) {
        (Some([x, y]), None, None) => Ok(PointerAction::Abs { x, y, raw: p.raw }),
        (None, Some([dx, dy]), None) => Ok(PointerAction::Rel { dx, dy }),
        (None, None, Some(button)) => Ok(PointerAction::Button {
            button,
            action: p.action.unwrap_or_default(),
        }),
        _ => Err(StepProblem::PointerShape),
    }
}

/// Runs the steps against `session`, stopping at the first failure, and returns each
/// step's result with the summary the verdict judges.
pub fn run(session: &Session, scenario: &Scenario) -> (Vec<StepResult>, ScenarioSummary) {
    let mut results = Vec::new();
    let mut failed = None;
    for (i, step) in scenario.steps.iter().enumerate() {
        let started_ms = session.now_ms();
        let start = Instant::now();
        let outcome = execute(session, step);
        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        let (ok, detail) = match outcome {
            Ok(detail) => (true, detail),
            Err(e) => (false, chain(&e)),
        };
        eprintln!(
            "step {}/{} {} {}: {}",
            i + 1,
            scenario.steps.len(),
            if ok { "ok" } else { "FAILED" },
            step.action.describe(),
            detail.lines().next().unwrap_or("")
        );
        results.push(StepResult {
            step: i + 1,
            action: step.action.describe(),
            ok,
            started_ms,
            duration_ms,
            detail: detail.clone(),
        });
        if !ok {
            failed = Some((i + 1, detail));
            break;
        }
    }
    let summary = ScenarioSummary {
        ran: results.len(),
        total: scenario.steps.len(),
        failed,
    };
    (results, summary)
}

fn execute(session: &Session, step: &Step) -> Result<String, Error> {
    let run = session.run();
    match &step.action {
        Action::Expect { pattern, since } => {
            let m = session.expect(pattern, since, step.timeout)?;
            Ok(format!("matched {:?} at {:.0} ms", m.matched.text, m.t_ms))
        }
        Action::Send { text, eol } => {
            session.serial_send(text, *eol)?;
            Ok(String::new())
        }
        Action::Type(text) => {
            session.type_text(text)?;
            Ok(String::new())
        }
        Action::Key(keys) => {
            session.key(keys)?;
            Ok(String::new())
        }
        Action::Pointer(action) => {
            session.pointer(*action)?;
            Ok(String::new())
        }
        Action::Shot(name) => {
            let shot = session.shot(name.as_deref())?;
            Ok(format!(
                "{} ({}x{})",
                shot.path.display(),
                shot.width,
                shot.height
            ))
        }
        Action::Text { save, contains } => {
            let text = session.text(None)?;
            let body = text.screen.text();
            if let Some(file) = save {
                let path = run.root().join(file);
                fs::write(&path, &body).map_err(|source| Error::Io { path, source })?;
            }
            if let Some(needle) = contains
                && !body.contains(needle.as_str())
            {
                return Err(Error::TextNotOnScreen(needle.clone()));
            }
            Ok(format!(
                "{} rows, {} unknown cells; {}",
                text.screen.rows,
                text.screen.unknown,
                body.lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
            ))
        }
        Action::Wait(predicate) => {
            let shot = session.wait_screen(predicate, step.timeout)?;
            Ok(format!("held at {:.0} ms", shot.t_ms))
        }
        Action::Sleep(d) => {
            std::thread::sleep(*d);
            Ok(String::new())
        }
        Action::Mark(name) => {
            let mark = session.mark(name);
            Ok(format!("at {:.0} ms", mark.t_ms))
        }
        Action::TraceStart => {
            let t = session.trace_start()?;
            Ok(t.dir.display().to_string())
        }
        Action::TraceStop => {
            let t = session.trace_stop()?;
            Ok(format!("{} {:?}", t.dir.display(), t.exit))
        }
        Action::WaitExit => {
            if session.wait_exit(Some(step.timeout)) {
                Ok("QEMU exited".to_owned())
            } else {
                Err(Error::ExitTimeout(step.timeout.as_secs_f64()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Scenario, ScenarioError> {
        Scenario::parse(Path::new("t.toml"), text)
    }

    #[test]
    fn every_action() {
        let s = parse(
            r#"
description = "all"
timeout = 30
[[step]]
expect = "login: $"
timeout = 300
[[step]]
send = "root"
[[step]]
send = "x"
eol = "none"
[[step]]
type = "Echo hi\n"
[[step]]
key = "meta_l-f1"
[[step]]
key = ["ctrl-c", "ret"]
[[step]]
pointer = { abs = [640, 400] }
[[step]]
pointer = { rel = [-5, 3] }
[[step]]
pointer = { button = "right", action = "down" }
[[step]]
shot = "a.png"
[[step]]
shot = true
[[step]]
text = "vt.txt"
contains = "login"
[[step]]
wait = "stable:500"
[[step]]
sleep = 0.5
[[step]]
mark = "m"
[[step]]
trace-start = true
[[step]]
trace-stop = true
[[step]]
wait-exit = true
[[step]]
expect = "x"
since = "m"
"#,
        )
        .expect("parse");
        assert_eq!(s.description.as_deref(), Some("all"));
        assert_eq!(s.steps.len(), 19);
        assert_eq!(s.steps[0].timeout, Duration::from_secs(300));
        assert_eq!(s.steps[1].timeout, Duration::from_secs(30));
        assert_eq!(
            s.steps[2].action,
            Action::Send {
                text: "x".into(),
                eol: Eol::None
            }
        );
        assert_eq!(
            s.steps[6].action,
            Action::Pointer(PointerAction::Abs {
                x: 640,
                y: 400,
                raw: false
            })
        );
        assert_eq!(
            s.steps[8].action,
            Action::Pointer(PointerAction::Button {
                button: Button::Right,
                action: ButtonAction::Down
            })
        );
        assert_eq!(s.steps[10].action, Action::Shot(None));
        assert_eq!(
            s.steps[18].action,
            Action::Expect {
                pattern: "x".into(),
                since: Since::Mark("m".into())
            }
        );
        assert_eq!(s.steps[13].action.describe(), "sleep 0.5s");
    }

    #[test]
    fn shipped_scenarios_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
        let mut count = 0;
        for entry in fs::read_dir(&dir).expect("scenarios directory") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|e| e == "toml") {
                let scenario = Scenario::load(&path)
                    .unwrap_or_else(|e| panic!("{}: {}", path.display(), chain(&e)));
                assert!(!scenario.steps.is_empty(), "{}", path.display());
                count += 1;
            }
        }
        assert!(count >= 2, "found {count} scenarios in {}", dir.display());
    }

    #[test]
    fn rejects_bad_steps() {
        assert!(matches!(
            parse("[[step]]\ntimeout = 3\n"),
            Err(ScenarioError::NoAction(1))
        ));
        assert!(matches!(
            parse("[[step]]\nexpect = \"a\"\nsend = \"b\"\n"),
            Err(ScenarioError::ManyActions { step: 1, .. })
        ));
        assert!(matches!(
            parse("[[step]]\nexpect = \"(\"\n"),
            Err(ScenarioError::Invalid {
                step: 1,
                problem: StepProblem::Regex(_)
            })
        ));
        assert!(matches!(
            parse("[[step]]\nkey = \"hyper-x\"\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::Input(InputError::Key(_)),
                ..
            })
        ));
        assert!(matches!(
            parse("[[step]]\ntype = \"é\"\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::Input(InputError::Char('é')),
                ..
            })
        ));
        assert!(matches!(
            parse("[[step]]\nwait = \"sparkly\"\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::Predicate(ScreenError::Predicate(_)),
                ..
            })
        ));
        assert!(matches!(
            parse("[[step]]\npointer = { abs = [1, 2], rel = [1, 2] }\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::PointerShape,
                ..
            })
        ));
        assert!(matches!(
            parse("[[step]]\nbogus = 1\n"),
            Err(ScenarioError::Parse { .. })
        ));
        assert!(matches!(
            parse("[[step]]\nshot = false\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::DoesNothing { action: "shot" },
                ..
            })
        ));
        assert!(matches!(
            parse("[[step]]\nshot = \"../../escape.png\"\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::FileName { action: "shot", .. },
                ..
            })
        ));
        for escape in ["../outside.txt", "/tmp/outside.txt", "sub/dir.txt", ".."] {
            let step = format!("[[step]]\ntext = \"{escape}\"\n");
            assert!(
                matches!(
                    parse(&step),
                    Err(ScenarioError::Invalid {
                        problem: StepProblem::FileName { action: "text", .. },
                        ..
                    })
                ),
                "{escape}"
            );
        }
        assert!(matches!(
            parse("[[step]]\nsleep = -1\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::OutOfRange { field: "sleep", .. },
                ..
            })
        ));
    }

    #[test]
    fn rejects_sleep_past_timeout() {
        assert!(matches!(
            parse("[[step]]\nsleep = 5\ntimeout = 2\n"),
            Err(ScenarioError::Invalid {
                step: 1,
                problem: StepProblem::SleepPastTimeout {
                    sleep: 5.0,
                    timeout: 2.0
                },
            })
        ));
        assert!(matches!(
            parse("timeout = 1.5\n[[step]]\nmark = \"m\"\n[[step]]\nsleep = 2\n"),
            Err(ScenarioError::Invalid {
                step: 2,
                problem: StepProblem::SleepPastTimeout {
                    sleep: 2.0,
                    timeout: 1.5
                },
            })
        ));
        assert!(matches!(
            parse("[[step]]\nsleep = 61\n"),
            Err(ScenarioError::Invalid {
                problem: StepProblem::SleepPastTimeout { timeout: 60.0, .. },
                ..
            })
        ));
        let ok = parse("timeout = 2\n[[step]]\nsleep = 2\n[[step]]\nsleep = 90\ntimeout = 90\n")
            .expect("sleeps within their timeouts");
        assert_eq!(ok.steps[1].action, Action::Sleep(Duration::from_secs(90)));
    }
}
