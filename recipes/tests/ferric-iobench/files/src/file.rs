//! File jobs: a job with `--filename` runs on a file through the file system, buffered.
//!
//! Each round runs the job once through each [`Path`]: `fs`, a descriptor opened with
//! `O_FSYNC`, which RedoxFS serves itself (the page cache leaves such descriptors alone), so
//! every read and write is a request to RedoxFS as without the page cache; and `cache`, a
//! plain descriptor, which the page cache serves where it can: reads with
//! `kernel.page_cache`, and writes too with `kernel.page_cache_writeback`. The A/B line
//! compares `cache` against `fs` in the same boot. Without the cached writes, a write job's
//! two paths are the same path, and the A/B is a same-path control.
//!
//! The file is created and filled with `io_size` bytes before the first round, outside the
//! timed part, so writes overwrite data in place on both paths, and one untimed round runs
//! through each path before the timed ones. Every round ends with an
//! `fsync`, so the next round starts with no dirty pages; it is part of the round's time only
//! with `--end_fsync=1`, as in fio.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferric_iobench::job::Job;
use ferric_iobench::latency::Histogram;
use ferric_iobench::offsets::Order;
use syscall::flag::O_FSYNC;

use crate::redox::{Failure, io, nanos, run_rounds};
use crate::round::run_together;

/// How a round opens the file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    /// `O_FSYNC`: RedoxFS serves every request itself.
    Fs,
    /// A plain descriptor: the page cache serves what it can.
    Cache,
}

/// The paths a round runs, with their names in the output. The first is the A/B baseline.
const PATHS: [(Path, &str); 2] = [(Path::Fs, "fs"), (Path::Cache, "cache")];

/// The size of each write that fills the file before the first round.
const FILL_CHUNK: usize = 1 << 20;

/// Runs file job `job` on the file at `path`, in rounds through each [`Path`], prints its
/// `FERRIC_BENCH` lines, and removes the file.
pub fn bench_job(job: &Job, path: &str) -> Result<(), Failure> {
    fill(path, job.io_size)?;
    let names = PATHS.map(|(_, name)| name);
    // One untimed round through each path first: the first round after the fill pays for
    // RedoxFS settling the new file, which would count against whichever path ran first.
    let rounds = PATHS
        .iter()
        .try_for_each(|&(how, _)| run_round(path, job, how).map(drop))
        .and_then(|()| {
            run_rounds(job, "path", &names, true, |k| {
                run_round(path, job, PATHS[k].0)
            })
        });
    // Remove the file whether or not the rounds ran.
    let removed = io("remove the bench file", fs::remove_file(path));
    let rounds = rounds?;
    removed?;
    rounds.print(job);
    Ok(())
}

/// Creates the file at `path` with `len` bytes of data, on disk.
fn fill(path: &str, len: u64) -> Result<(), Failure> {
    let file = io(
        "create the bench file",
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path),
    )?;
    let chunk = vec![0x5a; FILL_CHUNK];
    let mut at = 0;
    while at < len {
        let n = usize::try_from(len - at).map_or(FILL_CHUNK, |left| left.min(FILL_CHUNK));
        io("fill the bench file", file.write_all_at(&chunk[..n], at))?;
        at += n as u64;
    }
    io("sync the bench file", file.sync_all())
}

/// Opens the file at `path` through `how`, for writing too when the job writes.
fn open(path: &str, how: Path, writes: bool) -> Result<File, Failure> {
    let mut options = OpenOptions::new();
    options.read(true).write(writes);
    if how == Path::Fs {
        options.custom_flags(O_FSYNC as i32);
    }
    io("open the bench file", options.open(path))
}

/// Runs every I/O of `job` once through `how`, with [`Job::submitters`] threads sharing one
/// descriptor, then syncs the file, and returns the round's time and the merged latencies.
fn run_round(path: &str, job: &Job, how: Path) -> Result<(u64, Box<Histogram>), Failure> {
    let file = open(path, how, job.rw.writes())?;
    let order = Order::of(job);
    let next = AtomicU64::new(0);
    let timed = run_together(job.submitters(), || submit(&file, job, &order, &next))
        .map_err(Failure::Spawn)?;
    let mut latency = Box::new(Histogram::new());
    for result in timed.results {
        latency.merge(&*result.ok_or(Failure::Thread)??);
    }
    let synced = Instant::now();
    io("fsync the bench file", file.sync_all())?;
    let fsync = if job.end_fsync {
        synced.elapsed()
    } else {
        Duration::ZERO
    };
    Ok((nanos(timed.elapsed + fsync), latency))
}

/// One submitter: takes the next block until the job has none left, one `pread` or `pwrite`
/// at a time.
fn submit(
    file: &File,
    job: &Job,
    order: &Order,
    next: &AtomicU64,
) -> Result<Box<Histogram>, Failure> {
    let bs = job.bs as usize;
    let mut latency = Box::new(Histogram::new());
    let mut buf = vec![0xa5; bs];
    while let Some(block) = order.block(next.fetch_add(1, Ordering::Relaxed)) {
        let at = block * bs as u64;
        let issued = Instant::now();
        let (what, got) = if job.rw.writes() {
            ("write", io("pwrite", file.write_at(&buf, at))?)
        } else {
            ("read", io("pread", file.read_at(&mut buf, at))?)
        };
        latency.record(nanos(issued.elapsed()));
        if got != bs {
            return Err(Failure::Short {
                what,
                at,
                len: bs,
                got,
            });
        }
    }
    Ok(latency)
}
