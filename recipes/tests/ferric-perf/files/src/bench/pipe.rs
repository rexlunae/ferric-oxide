//! Pipes: context-switch ping-pong between two threads and between two processes, a 1-byte
//! round trip through one pipe without a switch, and throughput.
//!
//! - `pingpong-thread` and `pingpong-process`: one byte goes to the peer through one pipe and
//!   comes back through another. A sample is one round trip, so it holds two pipe writes, two
//!   reads, two wake-ups and at least two context switches: two when both sides share a CPU
//!   (each switches straight to the other), four when they run on two CPUs that go idle while
//!   they wait (to and from the idle context on each). The scheduler places them; the
//!   benchmark does not pin them. The process peer is this program run with `--pong`, so a
//!   switch to it also changes the address space.
//! - `pipe-rtt-1b`: the same thread writes one byte into a pipe and reads it back: the pipe's
//!   system call cost without a wake-up or switch, to subtract from the ping-pongs.
//! - `pipe-throughput`: a thread writes 64 KiB chunks (the kernel's pipe capacity) as fast as
//!   it can; a sample is the reader taking 4 MiB.

use std::env;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use super::{BenchError, Config, Measurement, Op, file_error, pipe_error};
use crate::measure::{Monotonic, Plan, measure};

/// The plan for round trips.
const ROUND_TRIP_PLAN: Plan = Plan {
    warmup: 200,
    iters: 10_000,
    min_iters: 100,
    budget: Duration::from_secs(2),
};

/// The plan for throughput rounds.
const THROUGHPUT_PLAN: Plan = Plan {
    warmup: 2,
    iters: 50,
    min_iters: 5,
    budget: Duration::from_secs(2),
};

/// Bytes per write in `pipe-throughput`.
const CHUNK: usize = 64 * 1024;

/// Bytes per `pipe-throughput` sample.
const ROUND: usize = 4 * 1024 * 1024;

/// Copies `input` to `output` one byte at a time until `input` ends: the peer of a ping-pong.
///
/// # Errors
///
/// A failed read or write.
pub fn echo(mut input: impl Read, mut output: impl Write) -> io::Result<()> {
    let mut byte = [0u8];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => output.write_all(&byte)?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// `ferric-perf --pong`: [`echo`] from standard input to standard output, both unbuffered.
///
/// # Errors
///
/// A failed read or write, or a standard stream that cannot be duplicated.
pub fn pong() -> io::Result<()> {
    let input = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    echo(input, output)
}

/// Sends the byte after `last` through `to` and checks that it comes back through `from`.
fn round_trip(to: &mut impl Write, from: &mut impl Read, last: &mut u8) -> Result<(), BenchError> {
    let sent = last.wrapping_add(1);
    *last = sent;
    to.write_all(&[sent]).map_err(pipe_error(Op::Write))?;
    let mut got = [0u8];
    from.read_exact(&mut got).map_err(pipe_error(Op::Read))?;
    if got[0] == sent {
        Ok(())
    } else {
        Err(BenchError::Echo { sent, got: got[0] })
    }
}

/// Times round trips through `to` and `from` as the round-trip plan says.
fn round_trips(
    config: &Config,
    name: &'static str,
    to: &mut impl Write,
    from: &mut impl Read,
) -> Result<Measurement, BenchError> {
    let mut last = 0;
    let [latency] = measure(
        &config.plan(ROUND_TRIP_PLAN),
        &mut Monotonic::new(),
        |laps| {
            round_trip(to, from, &mut last)?;
            laps.lap();
            Ok(())
        },
    )?
    .phases;
    Ok(Measurement {
        name,
        latency,
        bytes: None,
        extra: Vec::new(),
    })
}

/// The result of a joined helper thread.
fn joined(
    name: &'static str,
    result: thread::Result<io::Result<()>>,
    op: Op,
) -> Result<(), BenchError> {
    result
        .map_err(|_| BenchError::Panicked { name })?
        .map_err(pipe_error(op))
}

/// `pingpong-thread`.
pub fn pingpong_thread(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let (peer_in, mut to_peer) = io::pipe().map_err(pipe_error(Op::Create))?;
    let (mut from_peer, peer_out) = io::pipe().map_err(pipe_error(Op::Create))?;
    let peer = thread::Builder::new()
        .name("ferric-perf-echo".into())
        .spawn(move || echo(peer_in, peer_out))
        .map_err(|source| BenchError::Thread {
            name: "echo",
            source,
        })?;
    let measured = round_trips(config, "pingpong-thread", &mut to_peer, &mut from_peer);
    // The end of its input stops the peer, whether or not the run worked.
    drop(to_peer);
    joined("echo", peer.join(), Op::Echo)?;
    Ok(vec![measured?])
}

/// `pingpong-process`.
pub fn pingpong_process(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let exe = env::current_exe().map_err(BenchError::CurrentExe)?;
    let mut child = Command::new(&exe)
        .arg("--pong")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(file_error(Op::Start, &exe))?;
    let mut to_peer = child.stdin.take().expect("the child's input is piped");
    let mut from_peer = child.stdout.take().expect("the child's output is piped");
    let measured = round_trips(config, "pingpong-process", &mut to_peer, &mut from_peer);
    drop(to_peer);
    let status = child.wait().map_err(file_error(Op::Wait, &exe))?;
    let measured = measured?;
    if !status.success() {
        return Err(BenchError::Exit { path: exe, status });
    }
    Ok(vec![measured])
}

/// `pipe-rtt-1b`.
pub fn round_trip_1b(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let (mut reader, mut writer) = io::pipe().map_err(pipe_error(Op::Create))?;
    Ok(vec![round_trips(
        config,
        "pipe-rtt-1b",
        &mut writer,
        &mut reader,
    )?])
}

/// Reads exactly `len` bytes from `reader` through `buf`, discarding them.
fn take(reader: &mut impl Read, buf: &mut [u8], len: usize) -> io::Result<()> {
    let mut left = len;
    while left > 0 {
        let want = left.min(buf.len());
        match reader.read(&mut buf[..want]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => left -= n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reads `reader` to its end through `buf`.
fn drain(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<()> {
    loop {
        match reader.read(buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// `pipe-throughput`.
pub fn throughput(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let (mut reader, mut writer) = io::pipe().map_err(pipe_error(Op::Create))?;
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("ferric-perf-writer".into())
            .spawn(move || -> io::Result<()> {
                let chunk = vec![0xa5u8; CHUNK];
                while !stop.load(Ordering::Relaxed) {
                    writer.write_all(&chunk)?;
                }
                Ok(())
            })
            .map_err(|source| BenchError::Thread {
                name: "writer",
                source,
            })?
    };
    let mut buf = vec![0u8; CHUNK];
    let measured = measure(
        &config.plan(THROUGHPUT_PLAN),
        &mut Monotonic::new(),
        |laps| {
            take(&mut reader, &mut buf, ROUND).map_err(pipe_error(Op::Read))?;
            laps.lap();
            Ok::<_, BenchError>(())
        },
    );
    // The writer checks the flag between chunks and may be blocked on a full pipe, so keep
    // reading until it has stopped and closed its end.
    stop.store(true, Ordering::Relaxed);
    let drained = drain(&mut reader, &mut buf).map_err(pipe_error(Op::Read));
    joined("writer", writer.join(), Op::Write)?;
    let [latency] = measured?.phases;
    drained?;
    Ok(vec![Measurement {
        name: "pipe-throughput",
        latency,
        bytes: Some(ROUND as u64),
        extra: Vec::new(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_copies_until_the_end() {
        let mut out = Vec::new();
        echo(&b"abc"[..], &mut out).expect("echoes");
        assert_eq!(out, b"abc");
    }

    #[test]
    fn round_trips_check_the_byte() {
        let (mut reader, mut writer) = io::pipe().expect("pipe");
        let mut last = 0xff;
        round_trip(&mut writer, &mut reader, &mut last).expect("comes back");
        assert_eq!(last, 0);
        // A peer that answers with something else.
        let mut answer = &[7u8][..];
        assert!(matches!(
            round_trip(&mut io::sink(), &mut answer, &mut last),
            Err(BenchError::Echo { sent: 1, got: 7 })
        ));
        // One that hangs up.
        assert!(matches!(
            round_trip(&mut io::sink(), &mut io::empty(), &mut last),
            Err(BenchError::Pipe { op: Op::Read, .. })
        ));
    }

    #[test]
    fn take_reads_exactly_and_refuses_a_short_stream() {
        let mut buf = [0u8; 3];
        let mut input = &[1u8; 10][..];
        take(&mut input, &mut buf, 7).expect("enough");
        assert_eq!(input.len(), 3);
        assert_eq!(
            take(&mut input, &mut buf, 4).map_err(|e| e.kind()),
            Err(io::ErrorKind::UnexpectedEof)
        );
    }
}
