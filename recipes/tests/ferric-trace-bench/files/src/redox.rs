//! The benchmark on Redox (see the crate documentation).

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::process::ExitCode;
use std::time::Instant;

use ferric_trace::abi::{self, StatsLine};

/// The file the `io` workload writes and reads.
const IO_PATH: &str = "/tmp/ferric-trace-bench";

/// The size of each `io` transfer.
const IO_LEN: usize = 4096;

/// `io` operations (a write and a read each) per round. A round with every event on writes about a
/// hundred words per operation (system calls, scheme requests and context switches of both
/// the program and RedoxFS), so this keeps a round inside a CPU's 8192-word ring.
const IO_OPS: u32 = 50;

/// `syscall` calls per round, sized like [`IO_OPS`] for the ring.
const CALLS: u32 = 400;

/// Rounds per workload and state.
const ROUNDS: usize = 200;

/// A file descriptor no process has open.
const BAD_FD: usize = 0x7e57;

/// What failed.
#[derive(Debug)]
enum Failure {
    /// A file could not be used.
    Io { path: String, source: io::Error },
    /// The `stats` file did not parse.
    Stats(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Io { path, source } => write!(f, "{path}: {source}"),
            Failure::Stats(line) => write!(f, "bad stats line {line:?}"),
        }
    }
}

fn io_failure(path: &str) -> impl FnOnce(io::Error) -> Failure + '_ {
    move |source| Failure::Io {
        path: path.to_owned(),
        source,
    }
}

/// A workload: its name and one timed round, returning the nanoseconds per operation.
struct Workload<'a> {
    name: &'static str,
    round: Box<dyn FnMut() -> Result<f64, Failure> + 'a>,
}

/// The `io` workload over `file`.
fn io_round(file: &File) -> impl FnMut() -> Result<f64, Failure> + '_ {
    let mut buf = vec![0x5au8; IO_LEN];
    move || {
        let start = Instant::now();
        for _ in 0..IO_OPS {
            file.write_all_at(&buf, 0).map_err(io_failure(IO_PATH))?;
            file.read_exact_at(&mut buf, 0)
                .map_err(io_failure(IO_PATH))?;
        }
        Ok(start.elapsed().as_nanos() as f64 / f64::from(IO_OPS))
    }
}

/// The `syscall` workload.
fn syscall_round() -> Result<f64, Failure> {
    let byte = [0u8];
    let start = Instant::now();
    for _ in 0..CALLS {
        // Fails with EBADF, which is the point: the kernel dispatches it and returns.
        let _ = syscall::write(BAD_FD, &byte);
    }
    Ok(start.elapsed().as_nanos() as f64 / f64::from(CALLS))
}

/// The kernel's trace scheme, when there is one.
struct Trace {
    cpus: Vec<u32>,
}

fn scheme_path(file: &str) -> String {
    format!("/scheme/{}/{file}", abi::SCHEME_NAME)
}

/// Whether the kernel has the trace scheme, from the `/scheme` listing. Opening a missing
/// scheme's files fails with an errno that depends on the namespace manager, so the listing is
/// the reliable test.
fn has_scheme() -> Result<bool, Failure> {
    for entry in std::fs::read_dir("/scheme").map_err(io_failure("/scheme"))? {
        if entry.map_err(io_failure("/scheme"))?.file_name() == abi::SCHEME_NAME {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reads the `stats` file.
fn read_stats() -> Result<Vec<StatsLine>, Failure> {
    let path = scheme_path(abi::STATS_PATH);
    let mut text = String::new();
    File::open(&path)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(io_failure(&path))?;
    text.lines()
        .map(|line| StatsLine::parse(line).map_err(|_| Failure::Stats(line.to_owned())))
        .collect()
}

impl Trace {
    /// The trace scheme, or `None` on a kernel built without `kernel.trace`.
    fn open() -> Result<Option<Self>, Failure> {
        if !has_scheme()? {
            return Ok(None);
        }
        Ok(Some(Self {
            cpus: read_stats()?.iter().map(|s| s.cpu).collect(),
        }))
    }

    /// Records dropped so far, over every CPU.
    fn lost(&self) -> Result<u64, Failure> {
        Ok(read_stats()?.iter().map(|s| s.lost).sum())
    }

    fn command(&self, text: &str) -> Result<(), Failure> {
        let path = scheme_path(abi::EVENTS_PATH);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(io_failure(&path))
    }

    /// Empties every CPU's ring.
    fn drain(&self) -> Result<(), Failure> {
        let mut buf = vec![0u8; 64 * 1024];
        for cpu in &self.cpus {
            let path = scheme_path(&format!("{}/{cpu}", abi::CPU_DIR));
            let mut file = File::open(&path).map_err(io_failure(&path))?;
            while file.read(&mut buf).map_err(io_failure(&path))? != 0 {}
        }
        Ok(())
    }
}

/// The per-operation times of each state's rounds.
#[derive(Default)]
struct Samples {
    /// Compiled out: no trace scheme.
    out: Vec<f64>,
    /// Compiled in, all off, in untoggled rounds like `out`'s.
    off: Vec<f64>,
    /// Compiled in, all off, each round right before an `on` round.
    pairoff: Vec<f64>,
    /// Every compiled-in event on.
    on: Vec<f64>,
}

/// Runs every workload for [`ROUNDS`] rounds in each state, and returns their samples in
/// `workloads` order.
///
/// The untoggled rounds come first, for every workload: `out` without a trace scheme, `off`
/// with one, run the same way on both kernels, because toggling tracepoints (live patching, its
/// IPIs) and draining the rings disturb the caches and would add their cost to any round that
/// follows. The workloads take turns, one round each, so each workload's rounds are spread over
/// the whole phase instead of a burst of a few milliseconds that one stall of the runner can
/// cover from end to end. (Issue #280: with each workload's `off` block run after the previous
/// workload's toggling rounds, the null system call's `off` rounds were up to 30% slower than
/// the `pairoff` rounds of the same boot, and `out` never followed any toggling.)
///
/// Then, with a trace scheme, each workload alternates `pairoff` and `on` rounds, so both sides
/// of that comparison see the same disturbance and the same drift.
fn measure(workloads: &mut [Workload<'_>], trace: Option<&Trace>) -> Result<Vec<Samples>, Failure> {
    let mut samples: Vec<Samples> = workloads.iter().map(|_| Samples::default()).collect();
    // A warm-up round each, so the first timed one does not pay for cold caches.
    for workload in workloads.iter_mut() {
        (workload.round)()?;
    }
    for _ in 0..ROUNDS {
        for (workload, samples) in workloads.iter_mut().zip(&mut samples) {
            let ns = (workload.round)()?;
            match trace {
                None => samples.out.push(ns),
                Some(_) => samples.off.push(ns),
            }
        }
    }
    let Some(trace) = trace else {
        return Ok(samples);
    };
    for (workload, samples) in workloads.iter_mut().zip(&mut samples) {
        for _ in 0..ROUNDS {
            samples.pairoff.push((workload.round)()?);
            trace.command("enable all")?;
            samples.on.push((workload.round)()?);
            trace.command("disable all")?;
            trace.drain()?;
        }
    }
    Ok(samples)
}

/// Prints the `FERRIC_BENCH` line of `rounds`, if there are any.
fn report(name: &str, state: &str, rounds: &mut [f64]) {
    if rounds.is_empty() {
        return;
    }
    rounds.sort_by(f64::total_cmp);
    let min = rounds[0];
    let median = rounds[rounds.len() / 2];
    println!(
        "FERRIC_BENCH {name} {state} min_ns={min:.1} median_ns={median:.1} rounds={}",
        rounds.len()
    );
}

fn run() -> Result<(), Failure> {
    let trace = Trace::open()?;
    if let Some(trace) = &trace {
        trace.command("disable all")?;
        trace.drain()?;
    }
    let lost_before = trace.as_ref().map(Trace::lost).transpose()?;
    // O_FSYNC keeps the workload's requests going to RedoxFS: with the page cache's cached
    // writes (kernel.page_cache_writeback) a plain descriptor's writes and reads would be
    // served by the kernel, and the workload would no longer be the scheme round trips the
    // budget is set for.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(syscall::flag::O_FSYNC as i32)
        .open(IO_PATH)
        .map_err(io_failure(IO_PATH))?;
    file.write_all_at(&vec![0u8; IO_LEN], 0)
        .map_err(io_failure(IO_PATH))?;
    let mut workloads = [
        Workload {
            name: "io",
            round: Box::new(io_round(&file)),
        },
        Workload {
            name: "syscall",
            round: Box::new(syscall_round),
        },
    ];
    let samples = measure(&mut workloads, trace.as_ref())?;
    for (workload, mut samples) in workloads.iter().zip(samples) {
        report(workload.name, "out", &mut samples.out);
        report(workload.name, "off", &mut samples.off);
        report(workload.name, "pairoff", &mut samples.pairoff);
        report(workload.name, "on", &mut samples.on);
    }
    if let (Some(trace), Some(before)) = (&trace, lost_before) {
        // A full ring drops records instead of writing them, which would make "on" look
        // cheaper than it is, or, with kernel.trace_crash, overwrites its oldest ones; either
        // way the check script warns when this is not zero.
        println!("FERRIC_BENCH lost records={}", trace.lost()? - before);
    }
    drop(workloads);
    drop(file);
    std::fs::remove_file(IO_PATH).map_err(io_failure(IO_PATH))?;
    Ok(())
}

/// Runs the benchmark.
pub fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("FERRIC_TEST_PASS trace-bench");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            println!("FERRIC_TEST_FAIL trace-bench: {failure}");
            ExitCode::FAILURE
        }
    }
}
