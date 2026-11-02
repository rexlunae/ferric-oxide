//! A QMP (QEMU Machine Protocol) client.
//!
//! QMP is newline-delimited JSON over a socket: QEMU greets, the client negotiates with
//! `qmp_capabilities`, then sends `{"execute": ..., "arguments": ..., "id": ...}` commands
//! and gets `{"return": ...}` or `{"error": {"class": ..., "desc": ...}}` replies carrying
//! the same `id`. Asynchronous `{"event": ...}` messages can arrive at any time, between
//! replies.
//!
//! [`QmpClient`] owns one connection. A reader thread routes each reply to the call waiting
//! for its `id` and hands every event to an [`EventSink`] with the host time it arrived, so
//! commands may be sent from several threads at once and no event is lost while a command
//! waits.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::clock::Stamp;

/// A QMP failure.
#[derive(Debug, Error)]
pub enum QmpError {
    /// The socket could not be connected before the deadline.
    #[error("cannot connect to QMP socket {path}")]
    Connect {
        /// The socket.
        path: PathBuf,
        /// The last connection error.
        #[source]
        source: io::Error,
    },
    /// Reading or writing the socket failed.
    #[error("QMP connection I/O")]
    Io(#[from] io::Error),
    /// QEMU sent something that is not JSON.
    #[error("QMP sent malformed JSON")]
    Json(#[from] serde_json::Error),
    /// The first message was not a QMP greeting.
    #[error("the QMP socket did not send a greeting")]
    BadGreeting,
    /// The connection closed (QEMU exited).
    #[error("the QMP connection closed")]
    Disconnected,
    /// No reply in time.
    #[error("QMP command {command} got no reply within {seconds}s")]
    Timeout {
        /// The command.
        command: String,
        /// The time waited.
        seconds: f64,
    },
    /// QEMU answered with an error.
    #[error("QMP command {command} failed: {class}: {desc}")]
    Command {
        /// The command.
        command: String,
        /// QMP's error class (`GenericError`, `CommandNotFound`, ...).
        class: String,
        /// QEMU's description.
        desc: String,
    },
}

/// An asynchronous QMP event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QmpEvent {
    /// The event name (`SHUTDOWN`, `RESET`, `STOP`, ...).
    pub event: String,
    /// Its data, or `null`.
    #[serde(default)]
    pub data: Value,
    /// When the harness received it.
    pub stamp: Stamp,
}

/// Receives every event, on the reader thread.
pub type EventSink = Box<dyn Fn(QmpEvent) + Send + Sync>;

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, QmpError>>>>>;

/// One QMP connection.
pub struct QmpClient {
    writer: Mutex<UnixStream>,
    pending: Pending,
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
    greeting: Value,
}

impl std::fmt::Debug for QmpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmpClient")
            .field("greeting", &self.greeting)
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl QmpClient {
    /// Connects to the socket at `path`, retrying until `timeout` (QEMU creates it shortly
    /// after starting), reads the greeting and negotiates capabilities.
    ///
    /// # Errors
    ///
    /// [`QmpError::Connect`] if the socket never accepts, [`QmpError::BadGreeting`] if the
    /// peer is not QMP, and I/O, JSON or command errors from the negotiation.
    pub fn connect(path: &Path, timeout: Duration, sink: EventSink) -> Result<Self, QmpError> {
        let deadline = Instant::now() + timeout;
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(stream) => break stream,
                Err(source) if Instant::now() >= deadline => {
                    return Err(QmpError::Connect {
                        path: path.to_path_buf(),
                        source,
                    });
                }
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        };
        stream.set_read_timeout(Some(timeout))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let greeting: Value = serde_json::from_str(&read_line(&mut reader)?)?;
        if greeting.get("QMP").is_none() {
            return Err(QmpError::BadGreeting);
        }
        let mut writer = stream.try_clone()?;
        send(&mut writer, &json!({"execute": "qmp_capabilities"}))?;
        let reply: Value = serde_json::from_str(&read_line(&mut reader)?)?;
        check_reply("qmp_capabilities", reply)?;
        stream.set_read_timeout(None)?;

        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        {
            let pending = Arc::clone(&pending);
            let closed = Arc::clone(&closed);
            thread::Builder::new()
                .name("qmp-reader".to_owned())
                .spawn(move || read_loop(reader, &pending, &closed, &sink))?;
        }
        Ok(Self {
            writer: Mutex::new(writer),
            pending,
            closed,
            next_id: AtomicU64::new(1),
            greeting,
        })
    }

    /// QEMU's version from the greeting, as `major.minor.micro`.
    pub fn version(&self) -> Option<String> {
        let v = self.greeting.get("QMP")?.get("version")?.get("qemu")?;
        Some(format!(
            "{}.{}.{}",
            v.get("major")?,
            v.get("minor")?,
            v.get("micro")?
        ))
    }

    /// Whether the connection has closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Runs `command` with `arguments` and returns its `return` value.
    ///
    /// # Errors
    ///
    /// [`QmpError::Command`] when QEMU reports an error, [`QmpError::Timeout`] when no reply
    /// comes within `timeout`, [`QmpError::Disconnected`] when the connection is closed, and
    /// [`QmpError::Io`] when the command cannot be written.
    pub fn execute(
        &self,
        command: &str,
        arguments: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, QmpError> {
        if self.is_closed() {
            return Err(QmpError::Disconnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        let mut message = json!({"execute": command, "id": id});
        if let Some(arguments) = arguments {
            message["arguments"] = arguments;
        }
        let sent = send(
            &mut self.writer.lock().unwrap_or_else(PoisonError::into_inner),
            &message,
        );
        if let Err(e) = sent {
            self.forget(id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(reply) => reply.and_then(|r| check_reply(command, r)),
            Err(RecvTimeoutError::Timeout) => {
                self.forget(id);
                Err(QmpError::Timeout {
                    command: command.to_owned(),
                    seconds: timeout.as_secs_f64(),
                })
            }
            Err(RecvTimeoutError::Disconnected) => Err(QmpError::Disconnected),
        }
    }

    fn forget(&self, id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<String, QmpError> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(QmpError::Disconnected);
    }
    Ok(line)
}

fn send(writer: &mut UnixStream, message: &Value) -> Result<(), QmpError> {
    let mut text = serde_json::to_vec(message)?;
    text.push(b'\n');
    writer.write_all(&text)?;
    Ok(())
}

/// Turns a reply into its `return` value or a [`QmpError::Command`].
fn check_reply(command: &str, reply: Value) -> Result<Value, QmpError> {
    if let Some(error) = reply.get("error") {
        let field = |name: &str| {
            error
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        return Err(QmpError::Command {
            command: command.to_owned(),
            class: field("class"),
            desc: field("desc"),
        });
    }
    Ok(reply.get("return").cloned().unwrap_or(Value::Null))
}

fn read_loop(mut reader: impl BufRead, pending: &Pending, closed: &AtomicBool, sink: &EventSink) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let stamp = Stamp::now();
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(event) = message.get("event").and_then(Value::as_str) {
            sink(QmpEvent {
                event: event.to_owned(),
                data: message.get("data").cloned().unwrap_or(Value::Null),
                stamp,
            });
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let waiter = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some(waiter) = waiter {
            // The caller may have timed out and gone; nothing else wants the reply.
            waiter.send(Ok(message)).ok();
        }
    }
    closed.store(true, Ordering::Release);
    for (_, waiter) in pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
    {
        waiter.send(Err(QmpError::Disconnected)).ok();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A QMP server on a thread: greets, negotiates, then answers each command with
    /// `respond` and can push events first.
    pub(crate) fn fake_server(
        path: &Path,
        respond: impl Fn(&Value) -> Vec<Value> + Send + 'static,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).expect("bind");
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            writeln!(
                writer,
                r#"{{"QMP": {{"version": {{"qemu": {{"micro": 1, "minor": 2, "major": 10}}, "package": ""}}, "capabilities": ["oob"]}}}}"#
            )
            .expect("greet");
            let mut line = String::new();
            reader.read_line(&mut line).expect("caps");
            assert!(line.contains("qmp_capabilities"));
            writeln!(writer, r#"{{"return": {{}}}}"#).expect("caps reply");
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let command: Value = serde_json::from_str(&line).expect("json");
                for reply in respond(&command) {
                    let mut text = serde_json::to_string(&reply).expect("encode");
                    text.push_str("\r\n");
                    if writer.write_all(text.as_bytes()).is_err() {
                        return;
                    }
                }
                if command["execute"] == "quit" {
                    return;
                }
            }
        })
    }

    #[test]
    fn commands_events_and_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("qmp.sock");
        let server = fake_server(&path, |command| {
            let id = command["id"].clone();
            match command["execute"].as_str() {
                Some("query-status") => vec![
                    // An event arrives between the command and its reply.
                    json!({"event": "RESUME", "data": {}, "timestamp": {"seconds": 1, "microseconds": 2}}),
                    json!({"return": {"status": "running", "running": true}, "id": id}),
                ],
                Some("quit") => vec![
                    json!({"return": {}, "id": id}),
                    json!({"event": "SHUTDOWN", "data": {"guest": false, "reason": "host-qmp-quit"}}),
                ],
                _ => vec![json!({"error": {"class": "CommandNotFound", "desc": "nope"}, "id": id})],
            }
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let client = QmpClient::connect(
            &path,
            Duration::from_secs(5),
            Box::new(move |e| sink_events.lock().expect("lock").push(e)),
        )
        .expect("connect");
        assert_eq!(client.version().as_deref(), Some("10.2.1"));
        let status = client
            .execute("query-status", None, Duration::from_secs(5))
            .expect("status");
        assert_eq!(status["status"], "running");
        match client.execute("bogus", Some(json!({"a": 1})), Duration::from_secs(5)) {
            Err(QmpError::Command { class, desc, .. }) => {
                assert_eq!((class.as_str(), desc.as_str()), ("CommandNotFound", "nope"));
            }
            other => panic!("expected a command error, got {other:?}"),
        }
        client
            .execute("quit", None, Duration::from_secs(5))
            .expect("quit");
        server.join().expect("server");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.is_closed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(client.is_closed());
        assert!(matches!(
            client.execute("query-status", None, Duration::from_secs(1)),
            Err(QmpError::Disconnected)
        ));
        let names: Vec<String> = events
            .lock()
            .expect("lock")
            .iter()
            .map(|e| e.event.clone())
            .collect();
        assert_eq!(names, ["RESUME", "SHUTDOWN"]);
        assert_eq!(
            events.lock().expect("lock")[1].data["reason"],
            "host-qmp-quit"
        );
    }

    #[test]
    fn timeouts_and_bad_peers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("qmp.sock");
        let _server = fake_server(&path, |_| Vec::new());
        let client =
            QmpClient::connect(&path, Duration::from_secs(5), Box::new(|_| {})).expect("connect");
        assert!(matches!(
            client.execute("query-status", None, Duration::from_millis(100)),
            Err(QmpError::Timeout { .. })
        ));

        let missing = dir.path().join("missing.sock");
        assert!(matches!(
            QmpClient::connect(&missing, Duration::from_millis(100), Box::new(|_| {})),
            Err(QmpError::Connect { .. })
        ));

        let not_qmp = dir.path().join("other.sock");
        let listener = UnixListener::bind(&not_qmp).expect("bind");
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            writeln!(stream, r#"{{"hello": 1}}"#).expect("write");
        });
        assert!(matches!(
            QmpClient::connect(&not_qmp, Duration::from_secs(5), Box::new(|_| {})),
            Err(QmpError::BadGreeting)
        ));
        peer.join().expect("peer");
    }
}
