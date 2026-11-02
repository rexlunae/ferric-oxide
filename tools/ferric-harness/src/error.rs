//! The harness's error type.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::bootenv::BootEnvError;
use crate::input::InputError;
use crate::qmp::QmpError;
use crate::rundir::NotFileName;
use crate::scenario::ScenarioError;
use crate::screen::ScreenError;

/// Everything that can go wrong in the harness.
#[derive(Debug, Error)]
pub enum Error {
    /// `serial send --secret` could not read its text from standard input.
    #[error("reading the secret from standard input")]
    SecretInput(#[source] std::io::Error),
    /// A file or directory operation failed.
    #[error("{path}")]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// A program could not be started.
    #[error("cannot run {program}")]
    Spawn {
        /// The program.
        program: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// A JSON file could not be read or written.
    #[error("{path}: invalid JSON")]
    Json {
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: serde_json::Error,
    },
    /// The image to boot does not exist.
    #[error("image {0} does not exist")]
    ImageMissing(PathBuf),
    /// A Unix socket path is longer than the kernel accepts.
    #[error("socket path {path} is longer than {max} bytes; use a shorter --run directory")]
    SocketPathTooLong {
        /// The path.
        path: PathBuf,
        /// The limit.
        max: usize,
    },
    /// No UEFI firmware was found.
    #[error("no UEFI firmware found (set --firmware or OVMF_FIRMWARE); searched {searched:?}")]
    NoFirmware {
        /// Paths tried.
        searched: Vec<PathBuf>,
    },
    /// `--accel kvm` without a usable `/dev/kvm`.
    #[error("--accel kvm, but /dev/kvm cannot be opened read-write")]
    KvmUnavailable,
    /// `--boot-env-via keys` on a run without a display.
    #[error(
        "--boot-env-via keys needs --display std: without a display the bootloader shows no menu to type into"
    )]
    BootEnvKeysHeadless,
    /// A malformed boot environment.
    #[error(transparent)]
    BootEnv(#[from] BootEnvError),
    /// Trace mode without a `ferric-trace` binary.
    #[error(
        "--mode trace needs `ferric-trace` (its `follow` subcommand reads the host rings); pass --ferric-trace or build it (cargo build -p ferric-trace-cli). Searched {searched:?}"
    )]
    FerricTraceNotFound {
        /// Places searched.
        searched: Vec<PathBuf>,
    },
    /// A `ferric-trace` without the `follow` subcommand that reads host rings.
    #[error(
        "{program} has no `follow` subcommand (`follow --help` exited with {code:?}); trace mode needs a ferric-trace that reads host-visible rings"
    )]
    FollowUnsupported {
        /// The binary.
        program: PathBuf,
        /// Its exit code.
        code: Option<i32>,
    },
    /// The trace-mode RAM file already exists.
    #[error(
        "{0} exists: another run with this name is active, or an earlier one left it behind (remove it)"
    )]
    ShmExists(PathBuf),
    /// QEMU exited before the harness could connect to it.
    #[error("QEMU exited during start-up with {code:?}; see {log}")]
    QemuStartFailed {
        /// Its exit code.
        code: Option<i32>,
        /// Its log.
        log: PathBuf,
    },
    /// A socket QEMU should create never appeared.
    #[error("cannot connect to {path}")]
    Connect {
        /// The socket.
        path: PathBuf,
        /// The last error.
        #[source]
        source: io::Error,
    },
    /// QMP failed.
    #[error(transparent)]
    Qmp(#[from] QmpError),
    /// A screenshot or screen predicate failed.
    #[error(transparent)]
    Screen(#[from] ScreenError),
    /// Input that cannot be sent.
    #[error(transparent)]
    Input(#[from] InputError),
    /// A scenario file is invalid.
    #[error(transparent)]
    Scenario(#[from] ScenarioError),
    /// A regular expression is invalid.
    #[error("invalid regular expression")]
    Regex(#[from] regex::Error),
    /// `expect` timed out.
    #[error("no match for {pattern:?} within {seconds}s; the console ends with:\n{tail}")]
    ExpectTimeout {
        /// The pattern.
        pattern: String,
        /// Seconds waited.
        seconds: f64,
        /// The end of the console text.
        tail: String,
    },
    /// `wait-screen` timed out.
    #[error("the screen did not satisfy {predicate:?} within {seconds}s")]
    ScreenTimeout {
        /// The predicate.
        predicate: String,
        /// Seconds waited.
        seconds: f64,
    },
    /// The guest is gone (QEMU exited).
    #[error("the guest has exited")]
    GuestExited,
    /// A scenario's `text` step did not find its substring.
    #[error("the screen's text does not contain {0:?}")]
    TextNotOnScreen(String),
    /// A scenario's `wait-exit` step timed out.
    #[error("QEMU did not exit within {0}s")]
    ExitTimeout(f64),
    /// The run directory already holds a run.
    #[error("{0} already holds a run; pick another --run or --name")]
    RunDirExists(PathBuf),
    /// An `expect --since` mark that was never set.
    #[error("no mark named {0:?}")]
    UnknownMark(String),
    /// A trace command in a normal-mode run.
    #[error("tracing needs a run started with --mode trace")]
    TraceNotEnabled,
    /// `trace start` before the kernel announced its rings.
    #[error("the kernel has not announced its host trace rings yet")]
    TraceNotReady,
    /// `trace start` while `follow` runs.
    #[error("ferric-trace follow is already running")]
    TraceRunning,
    /// `trace stop` without a running `follow`.
    #[error("ferric-trace follow is not running")]
    TraceNotRunning,
    /// No supervisor answers in the run directory.
    #[error("no harness is running in {0} (its control socket does not answer)")]
    NotRunning(PathBuf),
    /// The supervisor exited during start-up.
    #[error("the supervisor exited during start-up; see {0}")]
    SupervisorFailed(PathBuf),
    /// The supervisor did not become ready in time.
    #[error("the supervisor did not answer within {0}s")]
    SupervisorTimeout(f64),
    /// The control protocol broke (a malformed or missing reply).
    #[error("control socket protocol error")]
    Protocol(#[source] serde_json::Error),
    /// A screenshot name that would put the file outside `<run>/shots/`.
    #[error("invalid screenshot name")]
    ShotName(#[from] NotFileName),
    /// A control client that runs as another user than the supervisor, or whose user
    /// could not be read.
    #[error("only uid {owner} may use this run's control socket")]
    PeerRefused {
        /// The supervisor's effective user ID.
        owner: u32,
    },
    /// A control client that did not send its whole request in time.
    #[error("no complete request within {seconds}s of connecting")]
    RequestTimeout {
        /// The limit, in seconds.
        seconds: f64,
    },
    /// A control request larger than the supervisor reads.
    #[error("the request is larger than {max} bytes")]
    RequestTooLarge {
        /// The limit, in bytes.
        max: usize,
    },
    /// A control connection beyond the number served at once.
    #[error("the supervisor is already serving {limit} connections; try again")]
    ControlBusy {
        /// The number served at once.
        limit: usize,
    },
    /// Reading a control request failed.
    #[error("cannot read the control request")]
    ControlRead(#[source] io::Error),
    /// The control socket closed before replying.
    #[error("the supervisor closed the connection without replying")]
    NoReply,
    /// An error the supervisor reported.
    #[error("{message}")]
    Remote {
        /// Its kind.
        kind: ErrorKind,
        /// Its message, with the source chain.
        message: String,
    },
    /// `report --a ... --b ...` with groups of different sizes.
    #[error("--a has {a} runs and --b has {b}; a paired comparison needs the same number")]
    UnpairedGroups {
        /// Runs in A.
        a: usize,
        /// Runs in B.
        b: usize,
    },
    /// A run directory without `report.json`.
    #[error("{0} has no report.json (is the run still going?)")]
    NoReport(PathBuf),
}

/// Broad classes of [`enum@Error`], carried over the control socket so the client can choose an
/// exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    /// Something timed out (`expect`, `wait-screen`).
    Timeout,
    /// The guest has exited.
    GuestExited,
    /// The request was invalid.
    BadRequest,
    /// Anything else.
    Other,
}

impl Error {
    /// This error's [`ErrorKind`].
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::ExpectTimeout { .. }
            | Error::ScreenTimeout { .. }
            | Error::ExitTimeout(_)
            | Error::TextNotOnScreen(_) => ErrorKind::Timeout,
            Error::GuestExited | Error::Qmp(QmpError::Disconnected) => ErrorKind::GuestExited,
            Error::Input(_)
            | Error::Regex(_)
            | Error::UnknownMark(_)
            | Error::TraceNotEnabled
            | Error::TraceNotReady
            | Error::TraceRunning
            | Error::TraceNotRunning
            | Error::PeerRefused { .. }
            | Error::RequestTimeout { .. }
            | Error::RequestTooLarge { .. }
            | Error::ShotName(_) => ErrorKind::BadRequest,
            Error::Screen(ScreenError::Predicate(_) | ScreenError::Regex(_)) => {
                ErrorKind::BadRequest
            }
            Error::Remote { kind, .. } => *kind,
            _ => ErrorKind::Other,
        }
    }
}

/// The error and its sources, joined with `: `, for people to read.
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chains_and_kinds() {
        let e = Error::Io {
            path: PathBuf::from("/x"),
            source: io::Error::new(io::ErrorKind::NotFound, "gone"),
        };
        assert_eq!(chain(&e), "/x: gone");
        assert_eq!(e.kind(), ErrorKind::Other);
        assert_eq!(Error::GuestExited.kind(), ErrorKind::GuestExited);
        let t = Error::ExpectTimeout {
            pattern: "x".into(),
            seconds: 1.0,
            tail: String::new(),
        };
        assert_eq!(t.kind(), ErrorKind::Timeout);
    }
}
