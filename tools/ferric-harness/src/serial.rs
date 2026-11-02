//! The guest's serial console.
//!
//! QEMU serves COM1 on a Unix socket. [`SerialConsole::start`] reads it on a thread and:
//!
//! - appends the raw bytes to `serial.log`;
//! - removes terminal escape sequences, carriage returns, NULs and other control bytes
//!   ([`Cleaner`]) and keeps the clean text in memory for [`SerialConsole::expect`];
//! - splits the clean text into lines stamped with the host time their first byte arrived
//!   and writes them to `serial.jsonl`. A line that stays unfinished for
//!   [`PARTIAL_AFTER`] (a `login:` prompt waiting for input) is written as a partial line, so
//!   prompts are seen without waiting for a newline;
//! - hands each line to a callback (the session's [`crate::observe::Observer`]).
//!
//! [`SerialConsole::send`] writes to the console (COM1 receive), which reaches the kernel's
//! debug scheme and the serial getty.
//!
//! Offsets into the clean text are absolute (they count every clean byte since boot), so a
//! mark taken earlier stays meaningful. The text kept in memory is capped at
//! [`MAX_TEXT`]; older text is dropped from the front and remains in the log files.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use regex::bytes::Regex;
use serde::Serialize;

use crate::clock::{Stamp, round_ms};

/// How long an unfinished line waits before it is reported as a partial line.
pub const PARTIAL_AFTER: Duration = Duration::from_millis(250);
/// Clean console text kept in memory.
pub const MAX_TEXT: usize = 64 << 20;
/// How far back a new search re-reads text already searched, so a match that spans the
/// arrival of new bytes is still found.
const RESCAN: u64 = 64 << 10;

/// Removes terminal escape sequences and control bytes from a byte stream, keeping
/// newlines, tabs and UTF-8 text. Sequences split across reads are handled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleaner {
    state: CleanState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CleanState {
    #[default]
    Ground,
    Escape,
    /// `ESC` plus a character-set or line-attribute introducer; one more byte follows.
    EscapeArgument,
    Csi,
    /// An operating-system command, ended by BEL or `ESC \\` (the `ESC` ends it through
    /// the escape state).
    Osc,
}

impl Cleaner {
    /// Cleans `input`, appending the kept bytes to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        use CleanState::*;
        for &b in input {
            self.state = match (self.state, b) {
                (_, 0x1b) => Escape,
                (Ground, b'\n' | b'\t') => {
                    out.push(b);
                    Ground
                }
                (Ground, 0..=0x1f | 0x7f) => Ground,
                (Ground, _) => {
                    out.push(b);
                    Ground
                }
                (Escape, b'[') => Csi,
                (Escape, b']') => Osc,
                (Escape, b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' | b'%') => {
                    EscapeArgument
                }
                (Escape | EscapeArgument, _) => Ground,
                (Csi, 0x40..=0x7e) => Ground,
                // A broken sequence must not swallow the line break after it.
                (Csi, b'\n') => {
                    out.push(b);
                    Ground
                }
                (Csi, _) => Csi,
                (Osc, 0x07) => Ground,
                (Osc, _) => Osc,
            };
        }
    }
}

/// A console line with the host time its first byte arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The line without its newline.
    pub text: String,
    /// Arrival of its first byte.
    pub stamp: Stamp,
    /// `true` when the line was reported before its newline arrived.
    pub partial: bool,
}

/// Splits clean text into [`Line`]s.
#[derive(Debug, Default)]
pub struct LineSplitter {
    current: Vec<u8>,
    started: Option<(Stamp, Instant)>,
}

impl LineSplitter {
    /// Adds clean bytes that arrived at `stamp`, returning the lines they complete.
    pub fn feed(&mut self, clean: &[u8], stamp: Stamp, now: Instant) -> Vec<Line> {
        let mut lines = Vec::new();
        for &b in clean {
            if self.started.is_none() {
                self.started = Some((stamp, now));
            }
            if b == b'\n' {
                lines.push(self.take(false));
            } else {
                self.current.push(b);
            }
        }
        lines
    }

    /// Reports the unfinished line as partial once it has waited [`PARTIAL_AFTER`].
    pub fn flush_idle(&mut self, now: Instant) -> Option<Line> {
        let (_, since) = self.started?;
        (!self.current.is_empty() && now.duration_since(since) >= PARTIAL_AFTER)
            .then(|| self.take(true))
    }

    /// Reports whatever is left, at the end of the stream.
    pub fn finish(&mut self) -> Option<Line> {
        (!self.current.is_empty()).then(|| self.take(true))
    }

    fn take(&mut self, partial: bool) -> Line {
        let (stamp, _) = self
            .started
            .take()
            .unwrap_or((Stamp::now(), Instant::now()));
        let text = String::from_utf8_lossy(&self.current).into_owned();
        self.current.clear();
        Line {
            text,
            stamp,
            partial,
        }
    }
}

/// A match of [`SerialConsole::expect`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ExpectMatch {
    /// Absolute offset of the match in the clean console text.
    pub start: u64,
    /// Absolute offset just past the match.
    pub end: u64,
    /// The matched text.
    pub text: String,
    /// The whole line the match ends on (as far as it has arrived).
    pub line: String,
}

/// Why [`SerialConsole::expect`] returned without a match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpectFailure {
    /// The time ran out.
    Timeout,
    /// The console closed (QEMU exited) and the text does not match.
    Closed,
}

#[derive(Debug, Default)]
struct Text {
    bytes: Vec<u8>,
    base: u64,
    closed: bool,
    raw_bytes: u64,
    lines: u64,
}

impl Text {
    fn end(&self) -> u64 {
        self.base + self.bytes.len() as u64
    }

    fn append(&mut self, clean: &[u8]) {
        self.bytes.extend_from_slice(clean);
        if self.bytes.len() > MAX_TEXT {
            let drop = self.bytes.len() - MAX_TEXT / 2;
            self.bytes.drain(..drop);
            self.base += drop as u64;
        }
    }

    fn slice_from(&self, offset: u64) -> (u64, &[u8]) {
        let start = offset.clamp(self.base, self.end());
        let index = usize::try_from(start - self.base).unwrap_or(self.bytes.len());
        (start, &self.bytes[index..])
    }
}

/// The line ending [`SerialConsole::send`] callers append.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum Eol {
    /// Carriage return, what a terminal sends for Enter.
    #[default]
    Cr,
    /// Line feed.
    Lf,
    /// Both.
    Crlf,
    /// Nothing.
    None,
}

impl Eol {
    /// The bytes of the line ending.
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Eol::Cr => b"\r",
            Eol::Lf => b"\n",
            Eol::Crlf => b"\r\n",
            Eol::None => b"",
        }
    }
}

/// Counters for status and reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct SerialCounts {
    /// Raw bytes received.
    pub raw_bytes: u64,
    /// Clean bytes kept.
    pub clean_bytes: u64,
    /// Lines (complete and partial).
    pub lines: u64,
}

/// Receives each console line on the reader thread.
pub type LineSink = Box<dyn FnMut(&Line) + Send>;

/// The serial console of a running guest.
#[derive(Debug)]
pub struct SerialConsole {
    text: Mutex<Text>,
    changed: Condvar,
    writer: Mutex<UnixStream>,
    input_log: Mutex<File>,
    origin: Stamp,
}

impl SerialConsole {
    /// Starts reading `stream`: raw bytes to `raw_log`, stamped lines to `jsonl` and to
    /// `sink`, text sent with [`send`](Self::send) to `input_log`. Times in `jsonl` are
    /// relative to `origin` (QEMU's start).
    ///
    /// # Errors
    ///
    /// I/O errors from duplicating the socket or starting the thread.
    pub fn start(
        stream: UnixStream,
        raw_log: File,
        jsonl: File,
        input_log: File,
        origin: Stamp,
        sink: LineSink,
    ) -> io::Result<Arc<Self>> {
        let console = Arc::new(Self {
            text: Mutex::default(),
            changed: Condvar::new(),
            writer: Mutex::new(stream.try_clone()?),
            input_log: Mutex::new(input_log),
            origin,
        });
        let reader = Arc::clone(&console);
        thread::Builder::new()
            .name("serial-reader".to_owned())
            .spawn(move || reader.read_loop(stream, raw_log, jsonl, sink))?;
        Ok(console)
    }

    fn lock(&self) -> MutexGuard<'_, Text> {
        self.text.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_loop(
        &self,
        mut stream: UnixStream,
        mut raw: File,
        mut jsonl: File,
        mut sink: LineSink,
    ) {
        // A short read timeout lets an unfinished line be reported while the guest waits.
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .ok();
        let mut cleaner = Cleaner::default();
        let mut splitter = LineSplitter::default();
        let mut buf = vec![0u8; 64 << 10];
        let mut clean = Vec::with_capacity(buf.len());
        let emit = |line: Line, jsonl: &mut File, sink: &mut LineSink| {
            let record = serde_json::json!({
                "t_ms": round_ms(line.stamp.ms_since(self.origin)),
                "mono_ns": line.stamp.mono_ns,
                "wall_ns": line.stamp.wall_ns,
                "text": line.text,
                "partial": line.partial,
            });
            // The log is evidence, not control flow: a full disk must not stop the guest.
            writeln!(jsonl, "{record}").ok();
            self.lock().lines += 1;
            sink(&line);
        };
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let stamp = Stamp::now();
                    raw.write_all(&buf[..n]).ok();
                    clean.clear();
                    cleaner.feed(&buf[..n], &mut clean);
                    {
                        let mut text = self.lock();
                        text.raw_bytes += n as u64;
                        text.append(&clean);
                    }
                    self.changed.notify_all();
                    for line in splitter.feed(&clean, stamp, Instant::now()) {
                        emit(line, &mut jsonl, &mut sink);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if let Some(line) = splitter.flush_idle(Instant::now()) {
                        emit(line, &mut jsonl, &mut sink);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if let Some(line) = splitter.finish() {
            emit(line, &mut jsonl, &mut sink);
        }
        raw.flush().ok();
        jsonl.flush().ok();
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// The absolute offset of the end of the text received so far.
    pub fn offset(&self) -> u64 {
        self.lock().end()
    }

    /// Whether the console has closed.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Counters so far.
    pub fn counts(&self) -> SerialCounts {
        let text = self.lock();
        SerialCounts {
            raw_bytes: text.raw_bytes,
            clean_bytes: text.end(),
            lines: text.lines,
        }
    }

    /// The last `max` bytes of clean text.
    pub fn tail(&self, max: usize) -> String {
        let text = self.lock();
        let from = text.bytes.len().saturating_sub(max);
        String::from_utf8_lossy(&text.bytes[from..]).into_owned()
    }

    /// Waits until `pattern` matches the clean text from `since` on, or `timeout` passes.
    ///
    /// # Errors
    ///
    /// [`ExpectFailure::Timeout`] when the time runs out and [`ExpectFailure::Closed`] when
    /// the console closes without a match.
    pub fn expect(
        &self,
        pattern: &Regex,
        since: u64,
        timeout: Duration,
    ) -> Result<ExpectMatch, ExpectFailure> {
        let deadline = Instant::now() + timeout;
        let mut text = self.lock();
        let mut scanned = since;
        loop {
            let from = since.max(scanned.saturating_sub(RESCAN));
            let (start, bytes) = text.slice_from(from);
            if let Some(m) = pattern.find(bytes) {
                let line_start = bytes[..m.end()]
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(0, |i| i + 1);
                let line_end = bytes[m.end()..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |i| m.end() + i);
                return Ok(ExpectMatch {
                    start: start + m.start() as u64,
                    end: start + m.end() as u64,
                    text: String::from_utf8_lossy(m.as_bytes()).into_owned(),
                    line: String::from_utf8_lossy(&bytes[line_start..line_end]).into_owned(),
                });
            }
            scanned = text.end();
            if text.closed {
                return Err(ExpectFailure::Closed);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(ExpectFailure::Timeout);
            }
            text = self
                .changed
                .wait_timeout(text, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Writes `bytes` to the console and logs them, stamped, to `serial-input.jsonl`.
    ///
    /// # Errors
    ///
    /// The socket's write error (QEMU has exited, say).
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let stamp = Stamp::now();
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_all(bytes)?;
        let record = serde_json::json!({
            "t_ms": round_ms(stamp.ms_since(self.origin)),
            "mono_ns": stamp.mono_ns,
            "text": String::from_utf8_lossy(bytes),
        });
        writeln!(
            self.input_log
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            "{record}"
        )
        .ok();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(chunks: &[&[u8]]) -> String {
        let mut cleaner = Cleaner::default();
        let mut out = Vec::new();
        for chunk in chunks {
            cleaner.feed(chunk, &mut out);
        }
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn strips_escapes_across_chunks() {
        assert_eq!(
            clean(&[b"\x1b[2J\x1b[001;001HBdsDxe: loading\r\n"]),
            "BdsDxe: loading\n"
        );
        assert_eq!(
            clean(&[b"\x1b[3m\x1b[38;5;1", b"5m2026\x1b[m\x1b[39m ok\x00\r\n"]),
            "2026 ok\n"
        );
        assert_eq!(
            clean(&[b"\x1b]0;title\x07a\x1b]2;t\x1b\\b\x1b(Bc\x1b7d"]),
            "abcd"
        );
        assert_eq!(
            clean(&[b"\x1b[?1000l\x1b[?1l\x1b[Jredox login: "]),
            "redox login: "
        );
        assert_eq!(clean(&["αβγ\t⏎".as_bytes()]), "αβγ\t⏎");
        // A broken sequence does not eat the next line.
        assert_eq!(clean(&[b"\x1b[12\nnext"]), "\nnext");
    }

    #[test]
    fn splits_lines_and_reports_prompts() {
        let mut s = LineSplitter::default();
        let t0 = Instant::now();
        let st = Stamp::now();
        let lines = s.feed(b"one\ntw", st, t0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "one");
        assert!(!lines[0].partial);
        assert!(s.flush_idle(t0 + Duration::from_millis(10)).is_none());
        let lines = s.feed(b"o\nlogin: ", st, t0);
        assert_eq!(lines[0].text, "two");
        let partial = s
            .flush_idle(t0 + PARTIAL_AFTER + Duration::from_millis(1))
            .expect("partial");
        assert_eq!((partial.text.as_str(), partial.partial), ("login: ", true));
        assert!(s.finish().is_none());
        s.feed(b"tail", st, t0);
        assert_eq!(s.finish().map(|l| l.text).as_deref(), Some("tail"));
    }

    #[test]
    fn console_end_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut qemu, harness) = UnixStream::pair().expect("pair");
        let file = |name: &str| File::create(dir.path().join(name)).expect("create");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);
        let console = SerialConsole::start(
            harness,
            file("serial.log"),
            file("serial.jsonl"),
            file("input.jsonl"),
            Stamp::now(),
            Box::new(move |line: &Line| seen.lock().expect("lock").push(line.clone())),
        )
        .expect("start");

        qemu.write_all(b"\x1b[1mredox login:\x1b[0m ")
            .expect("write");
        let re = Regex::new("login: $").expect("regex");
        let m = console
            .expect(&re, 0, Duration::from_secs(5))
            .expect("prompt without a newline");
        assert_eq!((m.start, m.end), (6, 13));
        assert_eq!(m.line, "redox login: ");
        // The prompt is reported as a partial line once the console has been idle.
        let deadline = Instant::now() + Duration::from_secs(5);
        while lines.lock().expect("lock").is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        console.send(b"root\r").expect("send");
        let mut got = [0u8; 5];
        qemu.read_exact(&mut got).expect("read");
        assert_eq!(&got, b"root\r");

        let mark = console.offset();
        qemu.write_all(b"root\r\nPassword: \r\n# ok\r\n")
            .expect("write");
        let re = Regex::new("(?m)^# (\\w+)$").expect("regex");
        let m = console
            .expect(&re, mark, Duration::from_secs(5))
            .expect("match");
        assert_eq!(m.text, "# ok");
        // Text before the mark is not searched.
        let re = Regex::new("login").expect("regex");
        assert_eq!(
            console.expect(&re, mark, Duration::from_millis(50)),
            Err(ExpectFailure::Timeout)
        );
        drop(qemu);
        assert_eq!(
            console.expect(&re, m.end, Duration::from_secs(5)),
            Err(ExpectFailure::Closed)
        );
        assert!(console.is_closed());
        let texts: Vec<String> = lines
            .lock()
            .expect("lock")
            .iter()
            .map(|l| l.text.clone())
            .collect();
        assert_eq!(texts, ["redox login: ", "root", "Password: ", "# ok"]);
        let counts = console.counts();
        assert_eq!(counts.lines, 4);
        let raw = std::fs::read(dir.path().join("serial.log")).expect("raw");
        assert!(raw.starts_with(b"\x1b[1mredox"));
        let jsonl = std::fs::read_to_string(dir.path().join("serial.jsonl")).expect("jsonl");
        assert!(
            jsonl
                .lines()
                .next()
                .expect("line")
                .contains("\"partial\":true")
        );
        let input = std::fs::read_to_string(dir.path().join("input.jsonl")).expect("input");
        assert!(input.contains("root\\r"));
    }
}
