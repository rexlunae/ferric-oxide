//! Checks that a process exits while another of its threads is busy (#200).
//!
//! Exit force-kills every other thread of the process (procmgr writes `ForceKill` to each),
//! then waits for them all to die. A thread that was running when it was marked has no wait
//! to be woken from; the kernel must stop it at its next wait instead of letting it block
//! there for good, or the process never finishes exiting and its parent waits forever.
//!
//! The test runs itself as a child many times, in two kinds. In both, the main thread starts
//! its work only once the second thread has made one pass through its wait, so the two
//! overlap:
//!
//! - `pipe`: a second thread reads a pipe one byte at a time while the main thread fills it,
//!   then the main thread exits. The reader is still draining the pipe when the kill comes,
//!   and its next read finds the pipe empty and starts a wait (a kernel `WaitCondition`).
//! - `futex`: a second thread parks in a loop while the main thread unparks it again and
//!   again, then exits. The parker's next park is a futex wait.
//!
//! Each child must exit within [`EXIT_LIMIT`]; one that does not is killed and the test
//! fails with `FERRIC_TEST_FAIL exit-busy-thread`. The race is timing-dependent, so a
//! kernel with the bug fails only some of the runs; [`ROUNDS`] makes that likely.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fmt;
use std::io::{self, Read, Write};
use std::process::{Child, Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

/// Children of each kind.
const ROUNDS: usize = 100;
/// Bytes a `pipe` child writes, and unparks a `futex` child sends, before it exits.
const WORK: usize = 4_000;
/// A child exits in milliseconds; allow a busy runner several seconds.
const EXIT_LIMIT: Duration = Duration::from_secs(10);
/// The argument that runs this program as a child, followed by its kind.
const CHILD_ARG: &str = "--child";

/// What the second thread of a child does when the main thread exits.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// Reads a pipe the main thread fills.
    Pipe,
    /// Parks while the main thread unparks it.
    Futex,
}

impl Kind {
    const ALL: [Kind; 2] = [Kind::Pipe, Kind::Futex];

    fn arg(self) -> &'static str {
        match self {
            Kind::Pipe => "pipe",
            Kind::Futex => "futex",
        }
    }

    fn from_arg(arg: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.arg() == arg)
    }
}

#[derive(Debug)]
enum Failure {
    /// This program's own path could not be found.
    Exe(io::Error),
    /// A child could not be started.
    Spawn { kind: Kind, source: io::Error },
    /// Waiting for a child failed.
    Wait { kind: Kind, source: io::Error },
    /// A child had not exited after [`EXIT_LIMIT`].
    Stuck { kind: Kind, round: usize },
    /// A child exited with a failure.
    ChildFailed {
        kind: Kind,
        round: usize,
        code: Option<i32>,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exe(_) => f.write_str("could not find this program's path"),
            Self::Spawn { kind, .. } => write!(f, "could not start a {} child", kind.arg()),
            Self::Wait { kind, .. } => write!(f, "could not wait for a {} child", kind.arg()),
            Self::Stuck { kind, round } => write!(
                f,
                "{} child {round} had not exited after {EXIT_LIMIT:?}: a thread killed by \
                 the exit blocked for good",
                kind.arg()
            ),
            Self::ChildFailed { kind, round, code } => {
                write!(f, "{} child {round} failed ({code:?})", kind.arg())
            }
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Exe(source) | Self::Spawn { source, .. } | Self::Wait { source, .. } => {
                Some(source)
            }
            Self::Stuck { .. } | Self::ChildFailed { .. } => None,
        }
    }
}

/// Reads or wake-ups the second thread has finished: each is a pass through its wait.
type Progress = Arc<AtomicUsize>;

/// Spawns `work` on a second thread, giving it a [`Progress`] to count each pass.
fn start_worker(work: impl FnOnce(&AtomicUsize) + Send + 'static) -> (Thread, Progress) {
    let progress = Progress::default();
    let counted = Arc::clone(&progress);
    let worker = thread::spawn(move || work(&counted));
    (worker.thread().clone(), progress)
}

/// Calls `poke` until the second thread has made one pass through its wait, so the main thread's
/// work (and its exit) overlaps with a worker that is going in and out of waits rather than
/// one that has not started yet. A worker that never gets there leaves the child hanging,
/// which the parent reports.
fn await_first_wait(progress: &AtomicUsize, mut poke: impl FnMut()) {
    while progress.load(Ordering::Acquire) == 0 {
        poke();
        thread::yield_now();
    }
}

/// A `pipe` child: fills a pipe its second thread drains, then exits with that thread busy.
fn pipe_child() -> io::Result<()> {
    let (mut reader, mut writer) = io::pipe()?;
    let (_, progress) = start_worker(move |progress| {
        let mut byte = [0; 1];
        // Reads until the exit kills this thread. End of file or an error cannot happen while
        // the main thread holds the write end, but if one did, park rather than spin.
        while matches!(reader.read(&mut byte), Ok(1)) {
            progress.fetch_add(1, Ordering::Release);
        }
        loop {
            thread::park();
        }
    });
    // One byte, then wait for the reader to take it: it is now reading for real.
    writer.write_all(&[1])?;
    await_first_wait(&progress, || {});
    for _ in 0..WORK {
        writer.write_all(&[1])?;
    }
    Ok(())
}

/// A `futex` child: unparks its second thread again and again, then exits with it busy.
fn futex_child() {
    let (parker, progress) = start_worker(|progress| {
        loop {
            thread::park();
            progress.fetch_add(1, Ordering::Release);
        }
    });
    await_first_wait(&progress, || parker.unpark());
    for _ in 0..WORK {
        parker.unpark();
    }
}

/// Runs this program as a child of `kind`. Its exit is the test.
fn child(kind: Kind) -> ExitCode {
    match kind {
        Kind::Pipe => {
            if let Err(err) = pipe_child() {
                eprintln!("ferric-exit-test pipe child: {err}");
                return ExitCode::FAILURE;
            }
        }
        Kind::Futex => futex_child(),
    }
    // The second thread is still busy here: this exit is what the parent waits on.
    std::process::exit(0)
}

/// Waits up to [`EXIT_LIMIT`] for `child`; kills it if it is still there.
fn await_exit(mut child: Child, kind: Kind, round: usize) -> Result<(), Failure> {
    let deadline = Instant::now() + EXIT_LIMIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(Failure::ChildFailed {
                    kind,
                    round,
                    code: status.code(),
                });
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                // The stuck thread is blocked, so a kill wakes it; the result does not
                // change the outcome.
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Stuck { kind, round });
            }
            Err(source) => return Err(Failure::Wait { kind, source }),
        }
    }
}

fn run() -> Result<(), Failure> {
    let exe = std::env::current_exe().map_err(Failure::Exe)?;
    for round in 1..=ROUNDS {
        for kind in Kind::ALL {
            let child = Command::new(&exe)
                .args([CHILD_ARG, kind.arg()])
                .spawn()
                .map_err(|source| Failure::Spawn { kind, source })?;
            await_exit(child, kind, round)?;
        }
    }
    Ok(())
}

/// Prints `err` and its sources.
fn report(err: &Failure) {
    let mut line = format!("FERRIC_TEST_FAIL exit-busy-thread: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        line.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    println!("{line}");
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(CHILD_ARG) {
        return match args.next().as_deref().and_then(Kind::from_arg) {
            Some(kind) => child(kind),
            None => ExitCode::FAILURE,
        };
    }
    match run() {
        Ok(()) => {
            println!(
                "FERRIC_TEST_PASS exit-busy-thread ({ROUNDS} pipe and {ROUNDS} futex children)"
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            report(&err);
            ExitCode::FAILURE
        }
    }
}
