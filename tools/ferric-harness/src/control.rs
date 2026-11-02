//! The supervisor's control socket.
//!
//! `ferric-harness up` leaves a supervisor running that owns the [`Session`]; the other
//! commands (`status`, `shot`, `type`, `expect`, `down`, ...) reach it through
//! `<run>/control.sock`. Each connection carries one request and one reply, each a line of
//! JSON: a [`Request`] (tagged by `cmd`) and a [`Reply`] (`{"ok": ...}` or
//! `{"err": {"kind": ..., "message": ...}}`). Connections are served on their own threads,
//! so a long `expect` does not hold up a `shot`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, chain};
use crate::input::PointerAction;
use crate::report::Mark;
use crate::serial::Eol;
use crate::session::{
    DownResult, ExpectResult, Session, ShotInfo, Since, Status, TextShot, TraceStatus,
};
use crate::stats::Sample;

/// A request to the supervisor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// The run's state.
    Status,
    /// A screenshot.
    Shot {
        /// Where to write it (absolute); default under `<run>/shots/`.
        out: Option<PathBuf>,
    },
    /// The text on the screen.
    Text {
        /// Where to keep the screenshot (absolute).
        save: Option<PathBuf>,
    },
    /// Type text on the keyboard.
    Type {
        /// The text.
        text: String,
    },
    /// Press key combinations.
    Key {
        /// The combinations, pressed in turn.
        combos: Vec<String>,
    },
    /// Pointer input.
    Pointer {
        /// What to do.
        action: PointerAction,
    },
    /// Write to the serial console.
    SerialSend {
        /// The text.
        text: String,
        /// The line ending appended.
        eol: Eol,
    },
    /// Wait for a regular expression on the serial console.
    Expect {
        /// The pattern.
        pattern: String,
        /// Seconds to wait.
        timeout_s: f64,
        /// Where to start.
        since: Since,
    },
    /// Set a named mark.
    Mark {
        /// Its name.
        name: String,
    },
    /// Wait for a screen predicate.
    WaitScreen {
        /// The predicate.
        predicate: String,
        /// Seconds to wait.
        timeout_s: f64,
    },
    /// Start `ferric-trace follow`.
    TraceStart,
    /// Stop `ferric-trace follow`.
    TraceStop,
    /// Take a stats sample.
    Stats,
    /// End the run.
    Down {
        /// Seconds to wait for the guest to power off before quitting QEMU.
        timeout_s: Option<f64>,
    },
}

/// A successful request's result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Output {
    /// `status`.
    Status(Box<Status>),
    /// `shot` and `wait-screen`.
    Shot(ShotInfo),
    /// `text`.
    Text(Box<TextShot>),
    /// Requests with nothing to return.
    Done,
    /// `expect`.
    Expect(ExpectResult),
    /// `mark`.
    Mark(Mark),
    /// `trace start` and `trace stop`.
    Trace(TraceStatus),
    /// `stats`; `None` once QEMU has gone.
    Stats(Option<Box<Sample>>),
    /// `down`.
    Down(Box<DownResult>),
}

/// The error half of a [`Reply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    /// Its class.
    pub kind: ErrorKind,
    /// The message with its source chain.
    pub message: String,
}

/// A reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reply {
    /// Success.
    Ok(Output),
    /// Failure.
    Err(RemoteError),
}

impl From<Result<Output, Error>> for Reply {
    fn from(result: Result<Output, Error>) -> Self {
        match result {
            Ok(output) => Reply::Ok(output),
            Err(e) => Reply::Err(RemoteError {
                kind: e.kind(),
                message: chain(&e),
            }),
        }
    }
}

fn seconds(s: f64) -> Duration {
    Duration::from_secs_f64(s.max(0.0))
}

/// Carries out `request` on `session`.
///
/// # Errors
///
/// Whatever the session method returns.
pub fn handle(session: &Session, request: Request) -> Result<Output, Error> {
    Ok(match request {
        Request::Status => Output::Status(Box::new(session.status())),
        Request::Shot { out } => Output::Shot(session.shot(out.as_deref())?),
        Request::Text { save } => Output::Text(Box::new(session.text(save.as_deref())?)),
        Request::Type { text } => {
            session.type_text(&text)?;
            Output::Done
        }
        Request::Key { combos } => {
            session.key(&combos)?;
            Output::Done
        }
        Request::Pointer { action } => {
            session.pointer(action)?;
            Output::Done
        }
        Request::SerialSend { text, eol } => {
            session.serial_send(&text, eol)?;
            Output::Done
        }
        Request::Expect {
            pattern,
            timeout_s,
            since,
        } => Output::Expect(session.expect(&pattern, &since, seconds(timeout_s))?),
        Request::Mark { name } => Output::Mark(session.mark(&name)),
        Request::WaitScreen {
            predicate,
            timeout_s,
        } => Output::Shot(session.wait_screen(&predicate, seconds(timeout_s))?),
        Request::TraceStart => Output::Trace(session.trace_start()?),
        Request::TraceStop => Output::Trace(session.trace_stop()?),
        Request::Stats => Output::Stats(session.stats().map(Box::new)),
        Request::Down { timeout_s } => Output::Down(Box::new(session.down(timeout_s.map(seconds)))),
    })
}

/// How long [`serve`] lets requests in flight finish once the report is written (a `down`
/// waiting for it must still get its reply).
const DRAIN: Duration = Duration::from_secs(10);

/// Requests being served; the count drops when the guard does.
#[derive(Clone, Debug, Default)]
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn enter(&self) -> InFlightGuard {
        self.0.fetch_add(1, Ordering::AcqRel);
        InFlightGuard(Arc::clone(&self.0))
    }

    fn idle(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }
}

struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Serves requests for `session` on `path` until the session's report is written and the
/// requests in flight have been answered; then removes the socket.
///
/// # Errors
///
/// [`Error::Io`] when the socket cannot be bound.
pub fn serve(session: &Session, path: &Path) -> Result<(), Error> {
    let listener = UnixListener::bind(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let done = Arc::new(AtomicBool::new(false));
    let in_flight = InFlight::default();
    {
        let session = session.clone();
        let done = Arc::clone(&done);
        let in_flight = in_flight.clone();
        let path = path.to_path_buf();
        thread::Builder::new()
            .name("control".to_owned())
            .spawn(move || accept_loop(&listener, &session, &done, &in_flight))
            .map_err(|source| Error::Io { path, source })?;
    }
    session.wait_report(None);
    done.store(true, Ordering::Release);
    // Wake the accept loop so it sees `done`; a refused connection is fine too.
    UnixStream::connect(path).ok();
    let deadline = Instant::now() + DRAIN;
    while !in_flight.idle() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_file(path).ok();
    Ok(())
}

fn accept_loop(
    listener: &UnixListener,
    session: &Session,
    done: &AtomicBool,
    in_flight: &InFlight,
) {
    for stream in listener.incoming() {
        if done.load(Ordering::Acquire) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let session = session.clone();
        let guard = in_flight.enter();
        thread::spawn(move || {
            serve_one(stream, &session);
            drop(guard);
        });
    }
}

fn serve_one(stream: UnixStream, session: &Session) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut line = String::new();
    if BufReader::new(read_half).read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let reply: Reply = match serde_json::from_str::<Request>(&line) {
        Ok(request) => handle(session, request).into(),
        Err(e) => Reply::Err(RemoteError {
            kind: ErrorKind::BadRequest,
            message: format!("malformed request: {e}"),
        }),
    };
    let mut stream = stream;
    if let Ok(mut text) = serde_json::to_string(&reply) {
        text.push('\n');
        // The client may have gone; nobody is left to tell.
        stream.write_all(text.as_bytes()).ok();
    }
}

/// Sends `request` to the supervisor of the run in `run_dir` and returns its output.
///
/// # Errors
///
/// [`Error::NotRunning`] when nothing answers, [`Error::NoReply`] or [`Error::Protocol`]
/// for a broken exchange, and [`Error::Remote`] for an error the supervisor reports.
pub fn request(run_dir: &Path, request: &Request) -> Result<Output, Error> {
    let path = run_dir.join("control.sock");
    let mut stream =
        UnixStream::connect(&path).map_err(|_| Error::NotRunning(run_dir.to_path_buf()))?;
    let mut text = serde_json::to_string(request).map_err(Error::Protocol)?;
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|source| Error::Io { path, source })?;
    if line.is_empty() {
        return Err(Error::NoReply);
    }
    match serde_json::from_str(&line).map_err(Error::Protocol)? {
        Reply::Ok(output) => Ok(output),
        Reply::Err(RemoteError { kind, message }) => Err(Error::Remote { kind, message }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Button, ButtonAction};

    #[test]
    fn wire_format() {
        let r = Request::Expect {
            pattern: "login:".into(),
            timeout_s: 30.0,
            since: Since::Mark("boot".into()),
        };
        let text = serde_json::to_string(&r).expect("encode");
        assert_eq!(
            text,
            r#"{"cmd":"expect","pattern":"login:","timeout_s":30.0,"since":{"mark":"boot"}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&text).expect("decode"), r);
        let p = Request::Pointer {
            action: PointerAction::Button {
                button: Button::Left,
                action: ButtonAction::Click,
            },
        };
        let text = serde_json::to_string(&p).expect("encode");
        assert_eq!(serde_json::from_str::<Request>(&text).expect("decode"), p);
        assert_eq!(
            serde_json::to_string(&Request::TraceStart).expect("encode"),
            r#"{"cmd":"trace-start"}"#
        );
        let reply: Reply = Err::<Output, _>(Error::GuestExited).into();
        assert_eq!(
            serde_json::to_string(&reply).expect("encode"),
            r#"{"err":{"kind":"guest-exited","message":"the guest has exited"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::Ok(Output::Done)).expect("encode"),
            r#"{"ok":"done"}"#
        );
    }

    #[test]
    fn no_supervisor() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            request(dir.path(), &Request::Status),
            Err(Error::NotRunning(_))
        ));
    }
}
