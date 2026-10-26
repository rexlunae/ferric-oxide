//! The benchmark proper; see the crate documentation for what it runs and prints.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferric_blkqueue::QueuePolicy;
use ferric_blkqueue::ab::{self, Comparison, Verdict};
use ferric_iobench::job::{BlockSizeMismatch, Job, JobError};
use ferric_iobench::latency::Histogram;
use ferric_iobench::offsets::Order;
use ferric_iobench::report::Summary;
use ferric_trace::tlb::{self, Counters, ModeLine, PerOp, Snapshot};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_DIRECTORY, O_RDONLY};
use syscall::CallFlags;
use syscall::error::{EIO, EOPNOTSUPP};

use crate::DEFAULT_JOBS;
use crate::round::{SpawnError, run_together_at};

/// The queue policies a round runs, with their names in the output. The first is the A/B
/// baseline (the one-queue path) and the second the candidate (the default).
const POLICIES: [(QueuePolicy, &str); 3] = [
    (QueuePolicy::MainQueue, "main"),
    (QueuePolicy::Adaptive, "adaptive"),
    (QueuePolicy::Workers, "workers"),
];

/// Why the benchmark could not run.
#[derive(Debug)]
enum Failure {
    /// A call failed with an errno.
    Errno { what: &'static str, errno: i32 },
    /// The disk has no partitions.
    NoPartition,
    /// The largest partition is smaller than a job's `io_size`.
    TooSmall { need: u64, have: u64 },
    /// A read returned fewer bytes than asked for.
    ShortRead { at: u64, len: usize, got: usize },
    /// A submitter thread panicked.
    Thread,
    /// The job's block size does not suit the disk.
    BlockSize(BlockSizeMismatch),
    /// A submitter thread could not be started.
    Spawn(SpawnError),
    /// `/scheme/sys/tlb` could not be read.
    TlbRead(std::io::Error),
    /// `/scheme/sys/tlb` did not parse.
    TlbParse(tlb::ParseError),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno { what, errno } => write!(f, "{what} failed with errno {errno}"),
            Self::NoPartition => f.write_str("the NVMe disk has no partitions"),
            Self::TooSmall { need, have } => write!(
                f,
                "the largest partition has {have} bytes, the job reads {need}"
            ),
            Self::ShortRead { at, len, got } => {
                write!(f, "a read of {len} bytes at {at} returned {got}")
            }
            Self::Thread => f.write_str("a submitter thread panicked"),
            Self::BlockSize(_) => f.write_str("the job does not suit the disk"),
            Self::Spawn(_) => f.write_str("a round could not start its submitters"),
            Self::TlbRead(_) => f.write_str("could not read the kernel's TLB counters"),
            Self::TlbParse(_) => f.write_str("the kernel's TLB counters did not parse"),
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BlockSize(err) => Some(err),
            Self::Spawn(err) => Some(err),
            Self::TlbRead(err) => Some(err),
            Self::TlbParse(err) => Some(err),
            _ => None,
        }
    }
}

/// Wraps a `std::io` result with what it was doing.
fn io<T>(what: &'static str, r: std::io::Result<T>) -> Result<T, Failure> {
    r.map_err(|e| Failure::Errno {
        what,
        errno: e.raw_os_error().unwrap_or(EIO),
    })
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let jobs = match parse_jobs(&args) {
        Ok(jobs) => jobs,
        Err(err) => {
            println!("FERRIC_TEST_FAIL iobench: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(scheme) = nvme_scheme() else {
        println!("FERRIC_TEST_SKIP iobench: no NVMe disk scheme");
        return ExitCode::SUCCESS;
    };
    match run(&scheme, &jobs) {
        Ok(()) => {
            println!("FERRIC_TEST_PASS iobench");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL iobench: {}", Chain(&err));
            ExitCode::FAILURE
        }
    }
}

/// An error and its sources, `: `-separated, for the one line a failure prints.
struct Chain<'a>(&'a dyn std::error::Error);

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(err) = source {
            write!(f, ": {err}")?;
            source = err.source();
        }
        Ok(())
    }
}

/// The jobs to run: the one `args` describe, or [`DEFAULT_JOBS`] without arguments.
fn parse_jobs(args: &[String]) -> Result<Vec<Job>, JobError<'_>> {
    if args.is_empty() {
        DEFAULT_JOBS
            .iter()
            .map(|job| Job::parse(job.split_whitespace()))
            .collect()
    } else {
        Ok(vec![Job::parse(args.iter().map(String::as_str))?])
    }
}

/// A partition of the NVMe disk scheme, opened for reading.
struct Disk {
    file: File,
    len: u64,
    /// The disk's logical block size, which every read must be a multiple of.
    block: u32,
}

/// The NVMe disk scheme's name, if there is one.
fn nvme_scheme() -> Option<String> {
    std::fs::read_dir("/scheme")
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|name| name.starts_with("disk.") && name.ends_with("-nvme"))
}

/// The largest partition the scheme lists (`NpM`).
fn largest_partition(scheme: &str) -> Result<Disk, Failure> {
    let dir = Fd::open(
        &format!("/scheme/{scheme}"),
        O_RDONLY | O_DIRECTORY | O_CLOEXEC,
        0,
    )
    .map_err(|e| Failure::Errno {
        what: "list the disk scheme",
        errno: e.errno(),
    })?;
    let mut list = vec![0; 4096];
    let n = dir.read(&mut list).map_err(|e| Failure::Errno {
        what: "read the disk list",
        errno: e.errno(),
    })?;
    list.truncate(n);
    let mut best: Option<Disk> = None;
    for name in String::from_utf8_lossy(&list).lines() {
        if !name.contains('p') {
            continue;
        }
        let path = format!("/scheme/{scheme}/{name}");
        let file = io("open a partition", OpenOptions::new().read(true).open(path))?;
        let meta = io("stat a partition", file.metadata())?;
        let len = meta.len();
        // The disk scheme reports its logical block size as `st_blksize`.
        let block = u32::try_from(meta.blksize()).unwrap_or(0);
        if best.as_ref().is_none_or(|disk| len > disk.len) {
            best = Some(Disk { file, len, block });
        }
    }
    best.ok_or(Failure::NoPartition)
}

/// Sets the disk's per-CPU queue policy (`ferric_blkqueue::abi`); `false` when the driver
/// has no per-CPU queue pairs (`EOPNOTSUPP`).
fn set_policy(disk: &Disk, policy: QueuePolicy) -> Result<bool, Failure> {
    let words = ferric_blkqueue::abi::encode(policy);
    let fd = disk.file.as_raw_fd() as usize;
    match libredox::call::call_wo(fd, &[], CallFlags::empty(), &words) {
        Ok(_) => Ok(true),
        Err(err) if err.errno() == EOPNOTSUPP => Ok(false),
        Err(err) => Err(Failure::Errno {
            what: "set the queue policy",
            errno: err.errno(),
        }),
    }
}

/// The kernel's TLB counters (`/scheme/sys/tlb`, `kernel.tlb_stats`), when it has them.
struct TlbStats {
    path: String,
    mode: ModeLine,
}

impl TlbStats {
    /// The counters' file, or `None` for a kernel built without them.
    ///
    /// # Errors
    ///
    /// [`Failure::TlbRead`] or [`Failure::TlbParse`] when the file exists but cannot be read.
    fn probe() -> Result<Option<Self>, Failure> {
        let path = format!("/scheme/sys/{}", tlb::SYS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mode = Snapshot::parse(&text).map_err(Failure::TlbParse)?.mode;
                Ok(Some(Self { path, mode }))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Failure::TlbRead(err)),
        }
    }

    /// Every CPU's counters now, summed.
    fn read(&self) -> Result<Counters, Failure> {
        let text = std::fs::read_to_string(&self.path).map_err(Failure::TlbRead)?;
        Ok(Snapshot::parse(&text).map_err(Failure::TlbParse)?.total)
    }
}

fn run(scheme: &str, jobs: &[Job]) -> Result<(), Failure> {
    let disk = largest_partition(scheme)?;
    let percore = set_policy(&disk, QueuePolicy::Adaptive)?;
    let tlb = TlbStats::probe()?;
    let result = jobs
        .iter()
        .try_for_each(|job| bench_job(&disk, job, percore, tlb.as_ref()));
    if percore {
        // Leave the default in place whatever happened.
        set_policy(&disk, QueuePolicy::Adaptive)?;
    }
    result
}

/// Every round of one policy, together.
struct Totals {
    /// Each round's IOPS, for the A/B.
    iops: [u32; ab::MAX_ROUNDS],
    elapsed_ns: u64,
    latency: Box<Histogram>,
    /// The kernel's TLB counters' growth over the rounds' timed parts, when it has them.
    tlb: Counters,
}

impl Totals {
    fn new() -> Self {
        Self {
            iops: [0; ab::MAX_ROUNDS],
            elapsed_ns: 0,
            latency: Box::new(Histogram::new()),
            tlb: Counters::new(),
        }
    }
}

/// Runs `job` with every policy, in rounds, and prints its `FERRIC_BENCH` lines.
fn bench_job(disk: &Disk, job: &Job, percore: bool, tlb: Option<&TlbStats>) -> Result<(), Failure> {
    job.check_block_size(disk.block)
        .map_err(Failure::BlockSize)?;
    if disk.len < job.io_size {
        return Err(Failure::TooSmall {
            need: job.io_size,
            have: disk.len,
        });
    }
    let policies = if percore {
        &POLICIES[..]
    } else {
        &POLICIES[..1]
    };
    let mut totals: Vec<Totals> = policies.iter().map(|_| Totals::new()).collect();
    let mut rounds = 0;
    let mut batch = ab::ROUNDS;
    let cmp = loop {
        for round in rounds..rounds + batch {
            run_policies(disk, job, policies, percore, round, tlb, &mut totals)?;
        }
        rounds += batch;
        if !percore {
            break None;
        }
        let cmp = Comparison::new(&totals[0].iops[..rounds], &totals[1].iops[..rounds]);
        if cmp.verdict() != Verdict::Confirm {
            break Some(cmp);
        }
        batch = ab::CONFIRM_ROUNDS;
    };
    print_job(job, policies, &totals, rounds, cmp, percore);
    if let Some(tlb) = tlb {
        print_tlb(job, policies, &totals, percore, tlb.mode);
    }
    Ok(())
}

/// Runs round `round` of `job` with every policy in `policies`, adding each one's results
/// to `totals[policy]`. Odd rounds run the policies in reverse order, so drift during the run
/// does not favour one.
fn run_policies(
    disk: &Disk,
    job: &Job,
    policies: &[(QueuePolicy, &str)],
    percore: bool,
    round: usize,
    tlb: Option<&TlbStats>,
    totals: &mut [Totals],
) -> Result<(), Failure> {
    for step in 0..policies.len() {
        let k = if round.is_multiple_of(2) {
            step
        } else {
            policies.len() - 1 - step
        };
        if percore {
            set_policy(disk, policies[k].0)?;
        }
        let (elapsed_ns, latency, tlb_delta) = run_round(disk, job, tlb)?;
        let summary = Summary {
            bs: job.bs,
            elapsed_ns,
            latency: &latency,
        };
        let total = &mut totals[k];
        total.iops[round] = u32::try_from(summary.iops()).unwrap_or(u32::MAX);
        total.elapsed_ns += elapsed_ns;
        total.latency.merge(&latency);
        total.tlb = total.tlb.sum(&tlb_delta);
    }
    Ok(())
}

fn print_job(
    job: &Job,
    policies: &[(QueuePolicy, &str)],
    totals: &[Totals],
    rounds: usize,
    cmp: Option<Comparison>,
    percore: bool,
) {
    let label = job.label();
    for (&(_, name), total) in policies.iter().zip(totals) {
        let name = if percore { name } else { "single" };
        let summary = Summary {
            bs: job.bs,
            elapsed_ns: total.elapsed_ns,
            latency: &total.latency,
        };
        println!("FERRIC_BENCH iobench-{label} policy={name} rounds={rounds} {summary}");
    }
    match cmp {
        Some(cmp) => {
            let verdict = if cmp.verdict() == Verdict::Pass {
                "pass"
            } else {
                "fail"
            };
            println!(
                "FERRIC_BENCH iobench-{label} ab_baseline_iops={} ab_candidate_iops={} \
                 ab_ratio={} ab_tolerance={} ab_rounds={rounds} ab={verdict}",
                cmp.baseline, cmp.candidate, cmp.ratio, cmp.tolerance,
            );
        }
        None => println!("FERRIC_BENCH iobench-{label} ab_rounds={rounds} ab=none"),
    }
}

/// Prints each policy's TLB counter deltas per read, as `FERRIC_BENCH iobench-<job>
/// tlb_policy=<policy> kpti=<on|off> pcid=<on|off> tlb_ops=… flushes_per_op=…` lines
/// (`ferric_trace::tlb::PerOp`). The key is `tlb_policy`, not `policy`, so tools that take a
/// `policy=` line for a result line skip these.
fn print_tlb(
    job: &Job,
    policies: &[(QueuePolicy, &str)],
    totals: &[Totals],
    percore: bool,
    mode: ModeLine,
) {
    let label = job.label();
    let on_off = |on| if on { "on" } else { "off" };
    for (&(_, name), total) in policies.iter().zip(totals) {
        let name = if percore { name } else { "single" };
        let per_op = PerOp {
            delta: &total.tlb,
            ops: total.latency.count(),
        };
        println!(
            "FERRIC_BENCH iobench-{label} tlb_policy={name} kpti={} pcid={} {per_op}",
            on_off(mode.kpti),
            on_off(mode.pcid),
        );
    }
}

/// Reads every block of `job` once, with [`Job::submitters`] threads that share a counter
/// of reads started, and returns the wall-clock time, the merged latencies and, with `tlb`,
/// how much the kernel's TLB counters grew. The clock starts once every thread is ready
/// ([`run_together_at`]), and the first TLB snapshot is taken just before it.
fn run_round(
    disk: &Disk,
    job: &Job,
    tlb: Option<&TlbStats>,
) -> Result<(u64, Box<Histogram>, Counters), Failure> {
    let order = Order::of(job);
    let next = AtomicU64::new(0);
    let bs = job.bs as usize;
    let mut before = None;
    let timed = run_together_at(
        job.submitters(),
        || submit(disk, &order, &next, bs),
        || before = tlb.map(TlbStats::read),
    )
    .map_err(Failure::Spawn)?;
    let tlb_delta = match (tlb, before) {
        (Some(tlb), Some(before)) => tlb.read()?.delta(&before?),
        _ => Counters::new(),
    };
    let mut latency = Box::new(Histogram::new());
    for result in timed.results {
        latency.merge(&*result.ok_or(Failure::Thread)??);
    }
    Ok((nanos(timed.elapsed), latency, tlb_delta))
}

/// One submitter: takes the next read until the job has none left, one `pread` at a time.
fn submit(
    disk: &Disk,
    order: &Order,
    next: &AtomicU64,
    bs: usize,
) -> Result<Box<Histogram>, Failure> {
    let mut latency = Box::new(Histogram::new());
    let mut buf = vec![0; bs];
    while let Some(block) = order.block(next.fetch_add(1, Ordering::Relaxed)) {
        let at = block * bs as u64;
        let issued = Instant::now();
        let got = io("pread", disk.file.read_at(&mut buf, at))?;
        latency.record(nanos(issued.elapsed()));
        if got != bs {
            return Err(Failure::ShortRead { at, len: bs, got });
        }
    }
    Ok(latency)
}

fn nanos(time: Duration) -> u64 {
    u64::try_from(time.as_nanos()).unwrap_or(u64::MAX)
}
