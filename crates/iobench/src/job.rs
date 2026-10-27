//! A benchmark job, described with fio's command-line options.
//!
//! The options, their names and their value syntax are fio's (fio's HOWTO, "Job file
//! parameters"), so a job reads the same here and on Linux:
//!
//! | Option | Values | Default |
//! | --- | --- | --- |
//! | `--rw` (`--readwrite`) | `read` (sequential), `randread`; with `--filename`, also `write`, `randwrite` | `read` |
//! | `--bs` (`--blocksize`) | a size, a multiple of 512 and of the disk's logical block size ([`Job::check_block_size`]) | `4k` |
//! | `--iodepth` | 1 to [`MAX_SUBMITTERS`] | 1 |
//! | `--numjobs` | 1 to [`MAX_SUBMITTERS`] | 1 |
//! | `--io_size` (`--size`) | a size, a whole number of blocks | `4m` |
//! | `--randseed` | a number | [`DEFAULT_SEED`] |
//! | `--ioengine` | `psync` | `psync` |
//! | `--direct` | `1` on the disk, `0` on a file | `1` on the disk, `0` on a file |
//! | `--filename` | a file to run the job on instead of the disk ([`Spec`]) | the disk |
//! | `--end_fsync` | `0`, `1` (`1` on a file only: `fsync` once the job's I/O is done) | `0` |
//! | `--name` | anything (ignored; the runner names jobs itself) | |
//!
//! Sizes take fio's suffixes with its default `kb_base=1024`: `4k` is 4096 bytes, `1m` is
//! 1 MiB, and `k`, `m`, `g`, `t` may be followed by `b` or `ib`.
//!
//! There is no asynchronous engine: the runner keeps `iodepth × numjobs` reads in flight by
//! running that many threads, each with one synchronous positioned read at a time
//! ([`Job::submitters`]). On Linux that is `--ioengine=psync --numjobs=N`; the queue depth
//! the disk sees is the same as `--ioengine=io_uring --iodepth=N` ([`FioEngine`]). On the
//! disk, writes are refused (the runner reads the disk the system runs from), and so is
//! buffered I/O (`--direct=0`), since the runner does positioned reads straight from the disk
//! scheme, which has no cache. Other engines are refused everywhere.
//!
//! A job with `--filename` is a file job ([`Job::file`]): it reads or writes a file through
//! the file system, buffered (`--direct=0`, the only mode there: Redox has no `O_DIRECT`), so
//! it measures the page cache and the file system rather than the disk. Writes are allowed
//! there, and `--end_fsync=1` adds one `fsync` at the end of the job, inside the timed part,
//! as fio does.

use core::fmt;

/// The most reads a job keeps in flight, and the largest `--iodepth` or `--numjobs`.
pub const MAX_SUBMITTERS: u32 = 256;
/// The seed of the random block order when `--randseed` is not given.
pub const DEFAULT_SEED: u64 = 0x8000_0000_f10e_5eed;
/// The sector size every block size and offset is a multiple of.
pub const SECTOR: u32 = 512;

const DEFAULT_IO_SIZE: u64 = 4 << 20;

/// The access pattern (`--rw`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rw {
    /// Sequential reads (`read`).
    Read,
    /// Random reads, every block once (`randread`).
    RandRead,
    /// Sequential writes (`write`), file jobs only.
    Write,
    /// Random writes, every block once (`randwrite`), file jobs only.
    RandWrite,
}

impl Rw {
    /// fio's name for it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::RandRead => "randread",
            Self::Write => "write",
            Self::RandWrite => "randwrite",
        }
    }

    /// Whether the job writes its blocks rather than reading them.
    pub const fn writes(self) -> bool {
        matches!(self, Self::Write | Self::RandWrite)
    }

    /// Whether the job takes its blocks in a seeded random order rather than in sequence.
    pub const fn random(self) -> bool {
        matches!(self, Self::RandRead | Self::RandWrite)
    }
}

/// A job: what to read, in which order, and how many reads to keep in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    /// The access pattern.
    pub rw: Rw,
    /// Bytes per read, a multiple of [`SECTOR`].
    pub bs: u32,
    /// Reads in flight per job.
    pub iodepth: u32,
    /// Jobs running side by side.
    pub numjobs: u32,
    /// Bytes read in all, a whole number of blocks.
    pub io_size: u64,
    /// The seed of the random block order.
    pub seed: u64,
    /// Whether the job runs on a file (`--filename`, buffered) rather than the disk.
    pub file: bool,
    /// Whether the job ends with one `fsync`, inside its timed part (`--end_fsync=1`).
    pub end_fsync: bool,
}

impl Default for Job {
    fn default() -> Self {
        Self {
            rw: Rw::Read,
            bs: 4096,
            iodepth: 1,
            numjobs: 1,
            io_size: DEFAULT_IO_SIZE,
            seed: DEFAULT_SEED,
            file: false,
            end_fsync: false,
        }
    }
}

/// A job and where it runs: [`Spec::parse`] takes `--filename` too, which a [`Job`] does not
/// keep since it has no lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec<'a> {
    /// The job; [`Job::file`] is set when `filename` is.
    pub job: Job,
    /// The file the job runs on (`--filename`), or `None` for the disk.
    pub filename: Option<&'a str>,
}

/// An option as given, kept to name it in an error found once every option is in.
type Given<'a> = Option<(&'a str, &'a str)>;

impl<'a> Spec<'a> {
    /// Parses a job and its target from fio-style options, as [`Job::parse`] does, but also
    /// takes `--filename`, which makes it a file job: buffered, and allowed to write.
    ///
    /// # Errors
    ///
    /// As [`Job::parse`], and [`JobError::Unsupported`] for a write, `--direct=0` or
    /// `--end_fsync=1` without `--filename`, or `--direct=1` with it.
    pub fn parse(args: impl IntoIterator<Item = &'a str>) -> Result<Self, JobError<'a>> {
        let mut job = Job::default();
        let mut filename = None;
        let (mut rw, mut direct, mut end_fsync): (Given<'a>, Given<'a>, Given<'a>) =
            (None, None, None);
        for arg in args {
            let arg = arg.trim_start_matches('-');
            let (option, value) = arg.split_once('=').ok_or(JobError::NoValue(arg))?;
            let bad = JobError::Value { option, value };
            match option {
                "filename" if value.is_empty() => return Err(bad),
                "filename" => filename = Some(value),
                "rw" | "readwrite" => {
                    job.rw = parse_rw(option, value)?;
                    rw = Some((option, value));
                }
                "direct" => {
                    flag(value).ok_or(bad)?;
                    direct = Some((option, value));
                }
                "end_fsync" => {
                    job.end_fsync = flag(value).ok_or(bad)?;
                    end_fsync = Some((option, value));
                }
                _ => job.set(option, value)?,
            }
        }
        job.file = filename.is_some();
        let unsupported = |(option, value)| JobError::Unsupported { option, value };
        // What the target does not do: a disk job only reads, directly; a file job is
        // buffered.
        let refused = if job.file {
            direct.filter(|&(_, value)| value == "1")
        } else {
            rw.filter(|_| job.rw.writes())
                .or(direct.filter(|&(_, value)| value == "0"))
                .or(end_fsync.filter(|_| job.end_fsync))
        };
        if let Some(given) = refused {
            return Err(unsupported(given));
        }
        job.check()?;
        Ok(Self { job, filename })
    }
}

/// Why a job's options were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobError<'a> {
    /// An option fio does not have, or this runner does not take.
    Unknown(&'a str),
    /// An option without `=value`.
    NoValue(&'a str),
    /// A value the option does not take.
    Value {
        /// The option.
        option: &'a str,
        /// The value given.
        value: &'a str,
    },
    /// A value fio takes but this runner does not run (a write, an asynchronous engine).
    Unsupported {
        /// The option.
        option: &'a str,
        /// The value given.
        value: &'a str,
    },
    /// The options do not fit together: `io_size` is not a whole number of blocks, more
    /// than [`MAX_SUBMITTERS`] reads would be in flight, or there are fewer blocks than reads
    /// in flight (some submitters would have nothing to read, and the fio command fewer jobs).
    Shape,
}

impl fmt::Display for JobError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(option) => write!(f, "unknown option {option}"),
            Self::NoValue(option) => write!(f, "option {option} needs a value"),
            Self::Value { option, value } => write!(f, "{option}: invalid value {value:?}"),
            Self::Unsupported { option, value } => write!(f, "{option}={value} is not supported"),
            Self::Shape => f.write_str(
                "io_size must be a whole number of blocks, at least one per read in flight, \
                 with at most 256 reads in flight",
            ),
        }
    }
}

impl core::error::Error for JobError<'_> {}

/// [`Job::check_block_size`]: the job's block size does not suit the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSizeMismatch {
    /// The job's block size.
    pub bs: u32,
    /// The disk's logical block size.
    pub logical: u32,
}

impl fmt::Display for BlockSizeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bs={} is not a multiple of the disk's {}-byte logical blocks",
            self.bs, self.logical
        )
    }
}

impl core::error::Error for BlockSizeMismatch {}

impl Job {
    /// Parses a disk job from fio-style options, `--option=value` (the leading dashes may be
    /// left out). Options not given keep their defaults (the table in the module docs). A
    /// job on a file needs [`Spec::parse`], which keeps the file's name.
    ///
    /// # Errors
    ///
    /// [`JobError::Unknown`], [`JobError::NoValue`], [`JobError::Value`] and
    /// [`JobError::Unsupported`] name the option at fault (`--filename` is unsupported
    /// here); [`JobError::Shape`] is for options that do not fit together.
    pub fn parse<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<Self, JobError<'a>> {
        let spec = Spec::parse(args)?;
        match spec.filename {
            None => Ok(spec.job),
            Some(value) => Err(JobError::Unsupported {
                option: "filename",
                value,
            }),
        }
    }

    fn set<'a>(&mut self, option: &'a str, value: &'a str) -> Result<(), JobError<'a>> {
        let bad = JobError::Value { option, value };
        match option {
            "bs" | "blocksize" => self.bs = block_size(value).ok_or(bad)?,
            "iodepth" => self.iodepth = count(value).ok_or(bad)?,
            "numjobs" => self.numjobs = count(value).ok_or(bad)?,
            "io_size" | "size" => self.io_size = parse_size(value).filter(|&n| n > 0).ok_or(bad)?,
            "randseed" => self.seed = value.parse().map_err(|_| bad)?,
            "ioengine" => check_engine(option, value)?,
            "name" => {}
            _ => return Err(JobError::Unknown(option)),
        }
        Ok(())
    }

    fn check<'a>(&self) -> Result<(), JobError<'a>> {
        let whole = self.io_size.is_multiple_of(u64::from(self.bs));
        let fits = self.submitters() <= MAX_SUBMITTERS;
        if whole && fits && self.ios() >= u64::from(self.submitters()) {
            Ok(())
        } else {
            Err(JobError::Shape)
        }
    }

    /// Reads in flight: `iodepth × numjobs`, each a thread with one synchronous read at a
    /// time.
    pub const fn submitters(&self) -> u32 {
        self.iodepth.saturating_mul(self.numjobs)
    }

    /// Reads in the job: `io_size / bs`.
    pub const fn ios(&self) -> u64 {
        self.io_size / self.bs as u64
    }

    /// Checks that the job's reads suit a disk whose logical block size is `logical` bytes:
    /// the disk scheme refuses a read that is not a whole number of its blocks.
    ///
    /// # Errors
    ///
    /// [`BlockSizeMismatch`] when `bs` is not a multiple of `logical` (or `logical` is 0).
    pub const fn check_block_size(&self, logical: u32) -> Result<(), BlockSizeMismatch> {
        if logical != 0 && self.bs.is_multiple_of(logical) {
            Ok(())
        } else {
            Err(BlockSizeMismatch {
                bs: self.bs,
                logical,
            })
        }
    }

    /// The job's blocks cut into `parts` byte ranges `(offset, len)` of whole blocks, in
    /// order, together covering the job's `io_size` once: the first `ios() % parts` ranges
    /// have one block more than the rest. Ranges that would be empty (fewer blocks than
    /// parts) are left out.
    pub fn ranges(&self, parts: u32) -> impl Iterator<Item = (u64, u64)> {
        let bs = u64::from(self.bs);
        let parts = u64::from(parts.max(1));
        let (each, extra) = (self.ios() / parts, self.ios() % parts);
        (0..parts).filter_map(move |i| {
            let blocks = each + u64::from(i < extra);
            let first = i * each + i.min(extra);
            (blocks > 0).then_some((first * bs, blocks * bs))
        })
    }

    /// A short name for the job, as in `randread-4k-qd32`: the pattern, the block size and
    /// the I/Os in flight, after `file-` for a file job (`file-randwrite-4k-qd1`).
    pub const fn label(&self) -> Label {
        Label(*self)
    }

    /// The fio command line that runs this job on Linux against `filename` with `engine`:
    /// the same blocks (the first `io_size` bytes, each read or written once), block size and
    /// I/Os in flight. A disk job has `--direct=1`, so Linux's page cache is bypassed as the
    /// disk scheme has none; a file job has `--direct=0`, through the page cache as on Redox,
    /// and `--end_fsync=1` when the job has it.
    pub const fn fio_command<'a>(&'a self, filename: &'a str, engine: FioEngine) -> FioCommand<'a> {
        FioCommand {
            job: self,
            filename,
            engine,
        }
    }
}

fn parse_rw<'a>(option: &'a str, value: &'a str) -> Result<Rw, JobError<'a>> {
    match value {
        "read" => Ok(Rw::Read),
        "randread" => Ok(Rw::RandRead),
        "write" => Ok(Rw::Write),
        "randwrite" => Ok(Rw::RandWrite),
        "rw" | "readwrite" | "randrw" | "trim" | "randtrim" | "trimwrite" => {
            Err(JobError::Unsupported { option, value })
        }
        _ => Err(JobError::Value { option, value }),
    }
}

/// Only `psync` is what the runner does: one positioned read per thread at a time.
fn check_engine<'a>(option: &'a str, value: &'a str) -> Result<(), JobError<'a>> {
    match value {
        "psync" => Ok(()),
        _ => Err(JobError::Unsupported { option, value }),
    }
}

/// A fio boolean option's value: `0` or `1`.
fn flag(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn block_size(value: &str) -> Option<u32> {
    let bytes = u32::try_from(parse_size(value)?).ok()?;
    (bytes > 0 && bytes.is_multiple_of(SECTOR)).then_some(bytes)
}

fn count(value: &str) -> Option<u32> {
    value
        .parse()
        .ok()
        .filter(|n| (1..=MAX_SUBMITTERS).contains(n))
}

/// Parses a size with fio's suffixes and its default `kb_base=1024`: `512`, `4k`, `1M`,
/// `2gib`. `None` for anything else, or a size past `u64::MAX`.
pub fn parse_size(value: &str) -> Option<u64> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let (number, suffix) = value.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let Some((&unit, rest)) = suffix.as_bytes().split_first() else {
        return Some(number);
    };
    let shift = match unit.to_ascii_lowercase() {
        b'k' => 10,
        b'm' => 20,
        b'g' => 30,
        b't' => 40,
        _ => return None,
    };
    let tail_ok =
        rest.is_empty() || rest.eq_ignore_ascii_case(b"b") || rest.eq_ignore_ascii_case(b"ib");
    if tail_ok {
        number.checked_mul(1 << shift)
    } else {
        None
    }
}

/// [`Job::label`]: `randread-4k-qd32`.
#[derive(Clone, Copy, Debug)]
pub struct Label(Job);

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let job = &self.0;
        if job.file {
            f.write_str("file-")?;
        }
        write!(
            f,
            "{}-{}-qd{}",
            job.rw.as_str(),
            Size(u64::from(job.bs)),
            job.submitters()
        )
    }
}

/// A size in fio's shortest exact form: `4k`, `128k`, `1m`, or bytes.
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        for (shift, unit) in [(30, "g"), (20, "m"), (10, "k")] {
            if n >= 1 << shift && n.is_multiple_of(1 << shift) {
                return write!(f, "{}{unit}", n >> shift);
            }
        }
        write!(f, "{n}")
    }
}

/// How the Linux fio run of a job keeps its reads in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FioEngine {
    /// `--ioengine=psync --numjobs=N`: N threads with one synchronous read each, as the
    /// runner does. The like-for-like comparison.
    Psync,
    /// `--ioengine=io_uring --iodepth=N`: one thread with N reads in flight, Linux's fastest
    /// path and the roadmap's exit baseline.
    IoUring,
}

/// [`Job::fio_command`]: the fio command line for a job.
#[derive(Clone, Copy, Debug)]
pub struct FioCommand<'a> {
    job: &'a Job,
    filename: &'a str,
    engine: FioEngine,
}

impl fmt::Display for FioCommand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let job = self.job;
        let n = job.submitters();
        let direct = u8::from(!job.file);
        let end_fsync = if job.end_fsync { " --end_fsync=1" } else { "" };
        match self.engine {
            // Each fio job has its own region and random map, so the region is cut into one
            // range of whole blocks per job ([`Job::ranges`]): together they read every block
            // once, as the runner's shared order does, and every offset and size stays a
            // multiple of `bs`, which direct I/O needs. Options before the first `--name` are
            // global.
            FioEngine::Psync => {
                write!(
                    f,
                    "fio --filename={} --rw={} --bs={} --direct={direct}{end_fsync} \
                     --randseed={} --ioengine=psync --group_reporting",
                    self.filename,
                    job.rw.as_str(),
                    job.bs,
                    job.seed,
                )?;
                for (i, (offset, size)) in job.ranges(n).enumerate() {
                    write!(
                        f,
                        " --name={}-{i} --offset={offset} --size={size}",
                        job.label()
                    )?;
                }
                Ok(())
            }
            FioEngine::IoUring => write!(
                f,
                "fio --name={} --filename={} --rw={} --bs={} --direct={direct}{end_fsync} \
                 --randseed={} --ioengine=io_uring --iodepth={n} --numjobs=1 --size={}",
                job.label(),
                self.filename,
                job.rw.as_str(),
                job.bs,
                job.seed,
                job.io_size
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::*;

    #[test]
    fn defaults_are_a_sequential_4k_read_at_depth_one() {
        let job = Job::parse([]).unwrap();
        assert_eq!(job, Job::default());
        assert_eq!(job.label().to_string(), "read-4k-qd1");
        assert_eq!(job.ios(), 1024);
    }

    #[test]
    fn fio_options_parse_with_or_without_dashes() {
        let job = Job::parse([
            "--rw=randread",
            "bs=128k",
            "--iodepth=8",
            "--numjobs=4",
            "--io_size=16m",
            "--randseed=7",
            "--ioengine=psync",
            "--direct=1",
            "--name=whatever",
        ])
        .unwrap();
        assert_eq!(job.rw, Rw::RandRead);
        assert_eq!(job.bs, 128 << 10);
        assert_eq!(job.submitters(), 32);
        assert_eq!(job.io_size, 16 << 20);
        assert_eq!(job.seed, 7);
        assert_eq!(job.label().to_string(), "randread-128k-qd32");
    }

    #[test]
    fn sizes_take_fio_suffixes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("4k"), Some(4096));
        assert_eq!(parse_size("4K"), Some(4096));
        assert_eq!(parse_size("4kb"), Some(4096));
        assert_eq!(parse_size("1MiB"), Some(1 << 20));
        assert_eq!(parse_size("2g"), Some(2 << 30));
        assert_eq!(parse_size("1t"), Some(1 << 40));
        assert_eq!(parse_size("4x"), None);
        assert_eq!(parse_size("k"), None);
        assert_eq!(parse_size("4kibb"), None);
        assert_eq!(parse_size("99999999999t"), None);
    }

    #[test]
    fn writes_and_async_engines_are_refused() {
        assert_eq!(
            Job::parse(["--rw=randwrite"]),
            Err(JobError::Unsupported {
                option: "rw",
                value: "randwrite"
            })
        );
        assert_eq!(
            Job::parse(["--ioengine=io_uring"]),
            Err(JobError::Unsupported {
                option: "ioengine",
                value: "io_uring"
            })
        );
    }

    #[test]
    fn options_the_runner_would_ignore_are_refused() {
        // The runner does positioned, direct reads only.
        for (arg, option, value) in [
            ("--direct=0", "direct", "0"),
            ("--ioengine=sync", "ioengine", "sync"),
            ("--ioengine=pvsync", "ioengine", "pvsync"),
        ] {
            assert_eq!(
                Job::parse([arg]),
                Err(JobError::Unsupported { option, value })
            );
        }
        assert!(Job::parse(["--direct=1", "--ioengine=psync"]).is_ok());
    }

    #[test]
    fn file_jobs_write_buffered_and_may_end_with_fsync() {
        let spec = Spec::parse([
            "--filename=/tmp/bench",
            "--rw=randwrite",
            "--bs=4k",
            "--io_size=8m",
            "--end_fsync=1",
        ])
        .unwrap();
        assert_eq!(spec.filename, Some("/tmp/bench"));
        let job = spec.job;
        assert!(job.file && job.end_fsync && job.rw.writes() && job.rw.random());
        assert_eq!(job.label().to_string(), "file-randwrite-4k-qd1");
        // The options may come in any order: --filename after the write.
        let later = Spec::parse(["--rw=write", "--direct=0", "--filename=f"]).unwrap();
        assert_eq!(later.job.rw, Rw::Write);
        assert!(!later.job.rw.random() && !later.job.end_fsync);
        // Reads of a file are file jobs too.
        assert!(Spec::parse(["--filename=f"]).unwrap().job.file);
        assert!(!Spec::parse([]).unwrap().job.file);
        assert_eq!(
            Spec::parse(["--filename=f", "--direct=1"]),
            Err(JobError::Unsupported {
                option: "direct",
                value: "1"
            })
        );
        assert_eq!(
            Spec::parse(["--filename="]),
            Err(JobError::Value {
                option: "filename",
                value: ""
            })
        );
        assert_eq!(
            Spec::parse(["--filename=f", "--end_fsync=yes"]),
            Err(JobError::Value {
                option: "end_fsync",
                value: "yes"
            })
        );
    }

    #[test]
    fn disk_jobs_only_read_directly() {
        for (arg, option, value) in [
            ("--rw=write", "rw", "write"),
            ("--readwrite=randwrite", "readwrite", "randwrite"),
            ("--end_fsync=1", "end_fsync", "1"),
            ("--direct=0", "direct", "0"),
        ] {
            assert_eq!(
                Spec::parse([arg]),
                Err(JobError::Unsupported { option, value })
            );
        }
        assert!(Spec::parse(["--end_fsync=0", "--direct=1"]).is_ok());
        // Job::parse has nowhere to keep the file's name.
        assert_eq!(
            Job::parse(["--filename=f"]),
            Err(JobError::Unsupported {
                option: "filename",
                value: "f"
            })
        );
    }

    #[test]
    fn the_fio_command_of_a_file_job_is_buffered() {
        let job = Spec::parse([
            "--filename=f",
            "--rw=write",
            "--bs=128k",
            "--io_size=256k",
            "--end_fsync=1",
        ])
        .unwrap()
        .job;
        assert_eq!(
            job.fio_command("/mnt/bench", FioEngine::Psync).to_string(),
            "fio --filename=/mnt/bench --rw=write --bs=131072 --direct=0 --end_fsync=1 \
             --randseed=9223372040899026669 --ioengine=psync --group_reporting \
             --name=file-write-128k-qd1-0 --offset=0 --size=262144"
        );
        assert_eq!(
            job.fio_command("/mnt/bench", FioEngine::IoUring)
                .to_string(),
            "fio --name=file-write-128k-qd1 --filename=/mnt/bench --rw=write --bs=131072 \
             --direct=0 --end_fsync=1 --randseed=9223372040899026669 --ioengine=io_uring \
             --iodepth=1 --numjobs=1 --size=262144"
        );
    }

    #[test]
    fn block_sizes_must_suit_the_disk() {
        let small = Job::parse(["--bs=512", "--io_size=4m"]).unwrap();
        assert_eq!(small.check_block_size(512), Ok(()));
        assert_eq!(
            small.check_block_size(4096),
            Err(BlockSizeMismatch {
                bs: 512,
                logical: 4096
            })
        );
        let big = Job::parse(["--bs=128k"]).unwrap();
        assert_eq!(big.check_block_size(512), Ok(()));
        assert_eq!(big.check_block_size(4096), Ok(()));
        assert!(big.check_block_size(0).is_err());
        assert!(
            !small
                .check_block_size(4096)
                .unwrap_err()
                .to_string()
                .is_empty()
        );
    }

    #[test]
    fn ranges_are_whole_blocks_covering_the_job_once() {
        let job = Job::parse(["--bs=4k", "--io_size=12k", "--numjobs=2"]).unwrap();
        let ranges: std::vec::Vec<_> = job.ranges(job.submitters()).collect();
        assert_eq!(ranges, [(0, 8192), (8192, 4096)]);
        // More parts than blocks: one block each, no empty ranges.
        let ranges: std::vec::Vec<_> = job.ranges(5).collect();
        assert_eq!(ranges, [(0, 4096), (4096, 4096), (8192, 4096)]);
        for parts in 1..=40 {
            let job = Job::parse(["--bs=4k", "--io_size=100k"]).unwrap();
            let mut at = 0;
            for (offset, size) in job.ranges(parts) {
                assert_eq!(offset, at);
                assert!(offset.is_multiple_of(4096) && size.is_multiple_of(4096) && size > 0);
                at += size;
            }
            assert_eq!(at, job.io_size);
        }
    }

    #[test]
    fn bad_options_name_themselves() {
        assert_eq!(Job::parse(["--bogus=1"]), Err(JobError::Unknown("bogus")));
        assert_eq!(Job::parse(["--bs"]), Err(JobError::NoValue("bs")));
        assert_eq!(
            Job::parse(["--bs=100"]),
            Err(JobError::Value {
                option: "bs",
                value: "100"
            })
        );
        assert_eq!(
            Job::parse(["--iodepth=0"]),
            Err(JobError::Value {
                option: "iodepth",
                value: "0"
            })
        );
        assert_eq!(
            Job::parse(["--direct=2"]),
            Err(JobError::Value {
                option: "direct",
                value: "2"
            })
        );
    }

    #[test]
    fn shapes_that_do_not_fit_are_refused() {
        assert_eq!(
            Job::parse(["--bs=4k", "--io_size=6k"]),
            Err(JobError::Shape)
        );
        assert_eq!(
            Job::parse(["--iodepth=32", "--numjobs=16"]),
            Err(JobError::Shape)
        );
        // Fewer blocks than submitters: three 4k blocks for four threads.
        assert_eq!(
            Job::parse(["--bs=4k", "--io_size=12k", "--numjobs=4"]),
            Err(JobError::Shape)
        );
        assert!(Job::parse(["--bs=4k", "--io_size=12k", "--numjobs=3"]).is_ok());
    }

    #[test]
    fn the_fio_command_runs_the_same_job_on_linux() {
        let job = Job::parse(["--rw=randread", "--bs=4k", "--iodepth=32", "--io_size=8m"]).unwrap();
        let psync = job
            .fio_command("/dev/nvme0n1", FioEngine::Psync)
            .to_string();
        assert!(psync.starts_with(
            "fio --filename=/dev/nvme0n1 --rw=randread --bs=4096 --direct=1 \
             --randseed=9223372040899026669 --ioengine=psync --group_reporting \
             --name=randread-4k-qd32-0 --offset=0 --size=262144 \
             --name=randread-4k-qd32-1 --offset=262144 --size=262144 "
        ));
        assert_eq!(psync.matches("--name=").count(), 32);
        assert!(psync.ends_with("--name=randread-4k-qd32-31 --offset=8126464 --size=262144"));
        // A shape that does not divide evenly still gets whole, aligned blocks.
        let odd = Job::parse(["--bs=4k", "--io_size=12k", "--numjobs=2"]).unwrap();
        assert_eq!(
            odd.fio_command("f", FioEngine::Psync).to_string(),
            "fio --filename=f --rw=read --bs=4096 --direct=1 --randseed=9223372040899026669 \
             --ioengine=psync --group_reporting --name=read-4k-qd2-0 --offset=0 --size=8192 \
             --name=read-4k-qd2-1 --offset=8192 --size=4096"
        );
        assert_eq!(
            job.fio_command("/dev/nvme0n1", FioEngine::IoUring)
                .to_string(),
            "fio --name=randread-4k-qd32 --filename=/dev/nvme0n1 --rw=randread --bs=4096 \
             --direct=1 --randseed=9223372040899026669 --ioengine=io_uring --iodepth=32 \
             --numjobs=1 --size=8388608"
        );
    }
}
