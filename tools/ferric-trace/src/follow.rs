//! Following a guest's host-visible trace rings from the host: the `ferric-trace follow`
//! command (Linux).
//!
//! A kernel built with `kernel.trace_host` and booted with `FERRIC_TRACE_HOST=<KiB per CPU>`
//! keeps its trace rings in one region of RAM, described by a directory page
//! ([`ferric_trace::host`]). With the guest's RAM in a shared file (QEMU's
//! `memory-backend-file,share=on`), the host maps the file ([`GuestRam`]) and reads the rings
//! while the guest runs. Nothing runs in the guest for it, and the host never writes guest
//! memory: each ring is followed by an [`Observer`], which keeps the host's own position in
//! it and judges from the ring's `head` what the kernel overwrote first.
//!
//! [`run`] finds the directory, at the address the kernel logged
//! (`trace: host rings at phys 0x...`) or by scanning the RAM for a page that holds the
//! directory magic *and* its own address, and then polls every ring each interval. It
//! writes what it reads into a capture directory in the format `ferric-trace capture` uses,
//! so `text`, `counts`, `perfetto` and `otlp` read it (with `--partial` while it runs):
//!
//! | File | Content |
//! | --- | --- |
//! | `INCOMPLETE` | present until the follower stops cleanly ([`crate::capture::INCOMPLETE`]) |
//! | `clock` | the trace clock, from the directory |
//! | `cpu<n>` | CPU n's records, appended as they are read |
//! | `stats.start` | per CPU, the words in the ring when following began and the records the kernel had dropped by then (`lost_before`) |
//! | `stats.end` | per CPU, the words left unread and `lost_before` plus the follower's own losses |
//! | [`HOST_CLOCK`] | samples tying host time to trace time, [`ClockSampleLine`]s |
//!
//! The rings overwrite their oldest records, so the kernel never waits: a follower that falls
//! more than a ring behind loses records. It counts the words it had to skip exactly; the
//! records they held are gone, so the `lost` it adds to `stats.end` is an estimate, the
//! skipped words divided by the mean record length it read on that CPU (at least one per
//! skip).
//!
//! It stops on [`run`]'s stop flag (SIGINT or SIGTERM in the command), after
//! [`Options::duration`], when the RAM file is removed, or when the guest boots again (the
//! directory changes or a ring starts over), and saves `stats.end` and removes `INCOMPLETE`
//! in each case.

pub mod live;

use std::fs::{self, File};
use std::io::{self, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use ferric_guest_ram::GuestRam;
use ferric_trace::abi::{READ_MIN, StatsLine};
use ferric_trace::host::{
    self, ClockSampleLine, DIRECTORY_MAGIC, DIRECTORY_WORDS, Directory, DirectoryError,
};
use ferric_trace::record::{MAX_RECORD_WORDS, Records, WORD_BYTES};
use ferric_trace::ring::{ObserveError, Observer, ViewError};

use crate::capture::{CLOCK, INCOMPLETE};
use crate::drain::{STATS_END, STATS_START};
use live::LiveCounts;

/// The file of [`ClockSampleLine`]s in a followed capture.
pub const HOST_CLOCK: &str = "host-clock";

/// The least time between two [`HOST_CLOCK`] samples of one CPU.
pub const HOST_CLOCK_EVERY: Duration = Duration::from_millis(100);

/// How often to look for the directory again while scanning finds none.
const SCAN_EVERY: Duration = Duration::from_millis(500);

/// The bytes read from a ring at a time.
const READ_BYTES: usize = 1 << 20;

const _: () = assert!(READ_BYTES >= READ_MIN);

/// The most bytes read from one ring in one poll, so a busy CPU does not starve the others.
pub const POLL_BYTES: u64 = 64 << 20;

/// How [`run`] follows the rings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The directory's physical address, as the kernel logged it; `None` scans for it.
    pub phys: Option<u64>,
    /// How long to wait between polls.
    pub interval: Duration,
    /// How long to follow for once the rings are found; `None` until stopped.
    pub duration: Option<Duration>,
    /// Whether to write a [`live`] line a second.
    pub live: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            phys: None,
            interval: Duration::from_millis(10),
            duration: None,
            live: false,
        }
    }
}

/// Why following stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    /// The stop flag was set.
    Stopped,
    /// [`Options::duration`] passed.
    Duration,
    /// The RAM file was removed.
    RamGone,
    /// The guest booted again: the directory changed or a ring started over.
    Rebooted,
}

impl StopReason {
    /// A short description, for the command's report.
    pub const fn describe(self) -> &'static str {
        match self {
            StopReason::Stopped => "stopped",
            StopReason::Duration => "the duration passed",
            StopReason::RamGone => "the guest RAM file was removed",
            StopReason::Rebooted => "the guest booted again",
        }
    }
}

/// What was read from one CPU's ring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuSummary {
    /// The CPU.
    pub cpu: u32,
    /// The bytes of records saved.
    pub bytes: u64,
    /// The records saved.
    pub records: u64,
    /// The records the kernel had dropped or overwritten before following began.
    pub lost_before: u64,
    /// The words the follower skipped because the kernel overwrote them first.
    pub lost_words: u64,
    /// The times it had to skip.
    pub skips: u64,
    /// The records those words held, estimated (see the [module docs](self)).
    pub lost_records: u64,
}

/// What [`run`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The directory's physical address.
    pub phys: u64,
    /// Why it stopped.
    pub stop: StopReason,
    /// Each CPU's ring.
    pub cpus: Vec<CpuSummary>,
}

/// Why following failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FollowError {
    /// The output directory already exists.
    #[error("{}: already exists; give a new directory", path.display())]
    OutputExists {
        /// The output directory.
        path: PathBuf,
    },
    /// A file of the capture could not be created or written.
    #[error("{}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The RAM file could not be examined.
    #[error("cannot examine the guest RAM file")]
    GuestRam(#[source] io::Error),
    /// The address given is not in the guest's RAM.
    #[error("{phys:#x} is not in the guest's RAM (check --ram-mib)")]
    NotRam {
        /// The address given.
        phys: u64,
    },
    /// The page at the address given holds a directory this follower cannot read.
    #[error("the page at {phys:#x} is not a trace ring directory this follower reads")]
    Directory {
        /// The address given.
        phys: u64,
        /// What is wrong with it.
        #[source]
        source: DirectoryError,
    },
    /// Following stopped before the directory was found.
    #[error("no trace ring directory found ({})", reason.describe())]
    NotFound {
        /// Why it stopped.
        reason: StopReason,
    },
    /// A ring the directory lists is not in RAM or not a ring.
    #[error("CPU {cpu}'s ring at {phys:#x} is not a trace ring")]
    Ring {
        /// The CPU.
        cpu: u32,
        /// The ring's address.
        phys: u64,
        /// What is wrong with it, when it is in RAM.
        #[source]
        source: Option<ViewError>,
    },
}

/// The host's `CLOCK_MONOTONIC` in nanoseconds, the clock [`HOST_CLOCK`] samples use.
pub fn monotonic_ns() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(now.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(now.tv_nsec).unwrap_or(0))
}

/// Finds the directory once: at `phys` when given, otherwise by scanning the RAM's data pages
/// for one that holds the directory magic and its own address. `Ok(None)` when there is
/// none (yet).
///
/// # Errors
///
/// [`FollowError::NotRam`] when `phys` is not in RAM, [`FollowError::Directory`] when the
/// page there has the magic but not a directory this follower reads, and
/// [`FollowError::GuestRam`] when the file system cannot list the RAM's data.
pub fn find_directory(
    ram: &GuestRam,
    phys: Option<u64>,
) -> Result<Option<(u64, Directory)>, FollowError> {
    if let Some(phys) = phys {
        let words = ram
            .phys_words(phys, DIRECTORY_WORDS)
            .ok_or(FollowError::NotRam { phys })?;
        return match Directory::read(words) {
            Ok(directory) if directory.self_phys() == phys => Ok(Some((phys, directory))),
            // Not written yet, or a copy of one that lives elsewhere.
            Ok(_) | Err(DirectoryError::Magic) => Ok(None),
            Err(source) => Err(FollowError::Directory { phys, source }),
        };
    }
    let layout = ram.layout();
    let page = ferric_guest_ram::PAGE_BYTES;
    for range in ram.data_ranges().map_err(FollowError::GuestRam)? {
        for offset in (range.start..range.end).step_by(page as usize) {
            let Some(words) = ram.words_at(offset, DIRECTORY_WORDS) else {
                continue;
            };
            if !host::has_magic(words) {
                continue;
            }
            let Some(at) = layout.phys_of(offset) else {
                continue;
            };
            if let Ok(directory) = Directory::read(words)
                && directory.self_phys() == at
            {
                return Ok(Some((at, directory)));
            }
        }
    }
    Ok(None)
}

/// Follows the rings of the guest whose RAM `ram` maps into a new capture directory `out`,
/// until `stop` is set or one of the other ends in the [module docs](self), writing a
/// [`live`] line a second to `live_out` when [`Options::live`] asks for it.
///
/// # Errors
///
/// [`FollowError::OutputExists`] or [`FollowError::Io`] for an unusable `out`;
/// [`FollowError::NotFound`] when it stops before finding the directory (the capture
/// directory then holds only `INCOMPLETE`); the errors of [`find_directory`];
/// [`FollowError::Ring`] for a ring the directory lists that is not one; and
/// [`FollowError::Io`] when a capture file cannot be written, after which the capture keeps
/// what was saved, marked `INCOMPLETE`.
pub fn run(
    ram: &GuestRam,
    out: &Path,
    options: &Options,
    stop: &AtomicBool,
    live_out: &mut dyn Write,
) -> Result<Summary, FollowError> {
    create_output(out)?;
    let (phys, directory) = locate(ram, options, stop)?;
    let start_ns = monotonic_ns();
    let mut follower = Follower::start(ram, phys, &directory, out)?;
    let mut live = options.live.then(|| LiveCounts::new(start_ns));
    let deadline = options
        .duration
        .map(|duration| start_ns.saturating_add(duration.as_nanos() as u64));
    let reason = loop {
        let now = monotonic_ns();
        if follower.poll(now, live.as_mut())? == Poll::Rebooted {
            break StopReason::Rebooted;
        }
        if let Some(line) = live
            .as_mut()
            .and_then(|live| live.line(now, follower.lost_records()))
        {
            writeln!(live_out, "{line}")
                .and_then(|()| live_out.flush())
                .map_err(io_error(Path::new("<stdout>")))?;
        }
        if stop.load(Ordering::Relaxed) {
            break StopReason::Stopped;
        }
        if deadline.is_some_and(|deadline| now >= deadline) {
            break StopReason::Duration;
        }
        if ram.is_unlinked().map_err(FollowError::GuestRam)? {
            break StopReason::RamGone;
        }
        thread::sleep(options.interval);
    };
    follower.finish(reason)
}

/// Waits for the directory, until `stop` is set or the duration passes.
fn locate(
    ram: &GuestRam,
    options: &Options,
    stop: &AtomicBool,
) -> Result<(u64, Directory), FollowError> {
    let start_ns = monotonic_ns();
    let every = if options.phys.is_some() {
        options.interval.max(Duration::from_millis(50))
    } else {
        SCAN_EVERY
    };
    loop {
        if let Some(found) = find_directory(ram, options.phys)? {
            return Ok(found);
        }
        let reason = if stop.load(Ordering::Relaxed) {
            Some(StopReason::Stopped)
        } else if options.duration.is_some_and(|duration| {
            monotonic_ns().saturating_sub(start_ns) >= duration.as_nanos() as u64
        }) {
            Some(StopReason::Duration)
        } else if ram.is_unlinked().map_err(FollowError::GuestRam)? {
            Some(StopReason::RamGone)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(FollowError::NotFound { reason });
        }
        thread::sleep(every);
    }
}

/// Creates `out`, which must not exist (its parents may), and marks it incomplete.
fn create_output(out: &Path) -> Result<(), FollowError> {
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    fs::create_dir(out).map_err(|source| {
        if source.kind() == ErrorKind::AlreadyExists {
            FollowError::OutputExists {
                path: out.to_owned(),
            }
        } else {
            io_error(out)(source)
        }
    })?;
    let marker = out.join(INCOMPLETE);
    fs::write(&marker, "ferric-trace follow has not stopped\n").map_err(io_error(&marker))
}

/// What one poll found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    /// The rings were read.
    Read,
    /// The guest booted again; nothing more will come from these rings.
    Rebooted,
}

/// One CPU's ring being followed.
struct CpuState<'a> {
    observer: Observer<&'a [AtomicU64]>,
    file: BufWriter<File>,
    path: PathBuf,
    /// Where the next copy starts.
    pos: u64,
    /// Whether a copy was made: the first one's skip is what was lost before following.
    started: bool,
    summary: CpuSummary,
    /// Words of the records read, for the loss estimate.
    words: u64,
    /// When this CPU's last [`HOST_CLOCK`] sample was taken.
    last_sample_ns: Option<u64>,
}

impl CpuState<'_> {
    /// The estimated records lost while following.
    fn lost_records(&self) -> u64 {
        let words = u128::from(self.summary.lost_words);
        let estimate = if self.summary.records == 0 || self.words == 0 {
            words.div_ceil(MAX_RECORD_WORDS as u128)
        } else {
            (words * u128::from(self.summary.records)).div_ceil(u128::from(self.words))
        };
        u64::try_from(estimate)
            .unwrap_or(u64::MAX)
            .max(self.summary.skips)
    }
}

/// The rings of one directory being followed into a capture directory.
pub struct Follower<'a> {
    ram: &'a GuestRam,
    out: PathBuf,
    phys: u64,
    generation: u64,
    cpus: Vec<CpuState<'a>>,
    host_clock: BufWriter<File>,
    buf: Vec<u8>,
}

impl<'a> Follower<'a> {
    /// Starts following the rings `directory` (found at `phys`) lists into the capture
    /// directory `out`, which exists: writes `clock` and `stats.start` and creates the
    /// `cpu<n>` and [`HOST_CLOCK`] files.
    ///
    /// # Errors
    ///
    /// [`FollowError::Ring`] for a ring that is not in RAM or not a ring, and
    /// [`FollowError::Io`] for a file that cannot be written.
    pub fn start(
        ram: &'a GuestRam,
        phys: u64,
        directory: &Directory,
        out: &Path,
    ) -> Result<Self, FollowError> {
        let clock = out.join(CLOCK);
        fs::write(&clock, directory.clock().to_string()).map_err(io_error(&clock))?;
        let mut cpus = Vec::with_capacity(directory.cpu_count());
        let mut start = String::new();
        for index in 0..directory.cpu_count() {
            let cpu = u32::try_from(index).unwrap_or(u32::MAX);
            let state = open_cpu(ram, directory, cpu, out)?;
            let observer = &state.observer;
            start.push_str(
                &StatsLine {
                    cpu,
                    pending: observer.head().saturating_sub(observer.tail()),
                    lost: state.summary.lost_before,
                }
                .to_string(),
            );
            cpus.push(state);
        }
        let stats = out.join(STATS_START);
        fs::write(&stats, start).map_err(io_error(&stats))?;
        let host_clock = out.join(HOST_CLOCK);
        let host_clock = File::create(&host_clock)
            .map(BufWriter::new)
            .map_err(io_error(&host_clock))?;
        Ok(Self {
            ram,
            out: out.to_owned(),
            phys,
            generation: directory.generation(),
            cpus,
            host_clock,
            buf: vec![0; READ_BYTES],
        })
    }

    /// Reads every ring until it is drained (or [`POLL_BYTES`] were read from it) and saves
    /// what it read; `now_ns` is the host time for the [`HOST_CLOCK`] samples. Counts each
    /// record in `live` when given.
    ///
    /// # Errors
    ///
    /// [`FollowError::Io`] when a capture file cannot be written.
    pub fn poll(
        &mut self,
        now_ns: u64,
        mut live: Option<&mut LiveCounts>,
    ) -> Result<Poll, FollowError> {
        if !self.same_boot() {
            return Ok(Poll::Rebooted);
        }
        for index in 0..self.cpus.len() {
            if self.poll_cpu(index, now_ns, live.as_deref_mut())? == Poll::Rebooted {
                return Ok(Poll::Rebooted);
            }
        }
        self.host_clock
            .flush()
            .map_err(io_error(&self.out.join(HOST_CLOCK)))?;
        Ok(Poll::Read)
    }

    /// Whether the directory is still the one followed: same magic and generation.
    fn same_boot(&self) -> bool {
        self.ram
            .phys_words(self.phys, DIRECTORY_WORDS)
            .and_then(|words| {
                let magic = words.first()?.load(Ordering::Relaxed);
                let generation = words.get(3)?.load(Ordering::Relaxed);
                Some(magic == DIRECTORY_MAGIC && generation == self.generation)
            })
            .unwrap_or(false)
    }

    /// Reads CPU `index`'s ring until it is drained.
    fn poll_cpu(
        &mut self,
        index: usize,
        now_ns: u64,
        mut live: Option<&mut LiveCounts>,
    ) -> Result<Poll, FollowError> {
        let Some(state) = self.cpus.get_mut(index) else {
            return Ok(Poll::Read);
        };
        let mut newest = None;
        let mut read = 0;
        while read < POLL_BYTES {
            let copy = match state.observer.observe(state.pos, &mut self.buf) {
                Ok(copy) => copy,
                Err(ObserveError::Ahead { .. }) => return Ok(Poll::Rebooted),
                // The buffer holds the longest record, so only a ring that breaks the format
                // gets here: stop reading it until the next poll.
                Err(_) => break,
            };
            if state.started {
                state.summary.lost_words += copy.skipped();
                state.summary.skips += u64::from(copy.skipped() > 0);
            }
            state.started = true;
            let bytes = self.buf.get(..copy.len()).unwrap_or_default();
            for record in Records::new(bytes).flatten() {
                if let Some(live) = live.as_deref_mut() {
                    live.record(record.event());
                }
                newest = Some(record.timestamp());
                state.summary.records += 1;
            }
            state.file.write_all(bytes).map_err(io_error(&state.path))?;
            state.summary.bytes += bytes.len() as u64;
            state.words += (bytes.len() / WORD_BYTES) as u64;
            read += bytes.len() as u64;
            state.pos = copy.end();
            if copy.is_corrupt() {
                // Nothing after a record that does not decode can be split into records.
                let head = state.observer.head().max(state.pos);
                state.summary.lost_words += head - state.pos;
                state.summary.skips += 1;
                state.pos = head;
            } else if copy.is_empty() {
                break;
            }
        }
        state.file.flush().map_err(io_error(&state.path))?;
        if let Some(timestamp) = newest
            && state.last_sample_ns.is_none_or(|last| {
                now_ns.saturating_sub(last) >= HOST_CLOCK_EVERY.as_nanos() as u64
            })
        {
            state.last_sample_ns = Some(now_ns);
            let line = ClockSampleLine {
                host_ns: now_ns,
                cpu: state.summary.cpu,
                head: state.pos,
                timestamp,
            };
            write!(self.host_clock, "{line}").map_err(io_error(&self.out.join(HOST_CLOCK)))?;
        }
        Ok(Poll::Read)
    }

    /// The records lost while following, estimated, over all CPUs.
    pub fn lost_records(&self) -> u64 {
        self.cpus.iter().map(CpuState::lost_records).sum()
    }

    /// Reads the rings a last time (unless the guest booted again), saves `stats.end`,
    /// removes `INCOMPLETE` and returns what was read.
    ///
    /// # Errors
    ///
    /// [`FollowError::Io`] when a capture file cannot be written.
    pub fn finish(mut self, reason: StopReason) -> Result<Summary, FollowError> {
        if reason != StopReason::Rebooted {
            self.poll(monotonic_ns(), None)?;
        }
        let mut end = String::new();
        let mut cpus = Vec::with_capacity(self.cpus.len());
        for state in &mut self.cpus {
            state.summary.lost_records = state.lost_records();
            let pending = state.observer.head().saturating_sub(state.pos);
            end.push_str(
                &StatsLine {
                    cpu: state.summary.cpu,
                    pending,
                    lost: state.summary.lost_before + state.summary.lost_records,
                }
                .to_string(),
            );
            cpus.push(state.summary);
        }
        let stats = self.out.join(STATS_END);
        fs::write(&stats, end).map_err(io_error(&stats))?;
        let marker = self.out.join(INCOMPLETE);
        fs::remove_file(&marker).map_err(io_error(&marker))?;
        Ok(Summary {
            phys: self.phys,
            stop: reason,
            cpus,
        })
    }
}

/// Opens CPU `cpu`'s ring and its capture file.
fn open_cpu<'a>(
    ram: &'a GuestRam,
    directory: &Directory,
    cpu: u32,
    out: &Path,
) -> Result<CpuState<'a>, FollowError> {
    let (phys, bytes) = directory.ring_phys(cpu as usize).unwrap_or_default();
    let ring_error = |source| FollowError::Ring { cpu, phys, source };
    let words = usize::try_from(bytes / WORD_BYTES as u64)
        .ok()
        .and_then(|count| ram.phys_words(phys, count))
        .ok_or(ring_error(None))?;
    let observer = Observer::new(words).map_err(|err| ring_error(Some(err)))?;
    let path = out.join(format!("cpu{cpu}"));
    let file = File::create(&path)
        .map(BufWriter::new)
        .map_err(io_error(&path))?;
    let lost_before = observer.lost();
    Ok(CpuState {
        observer,
        file,
        path,
        pos: 0,
        started: false,
        summary: CpuSummary {
            cpu,
            lost_before,
            ..CpuSummary::default()
        },
        words: 0,
        last_sample_ns: None,
    })
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> FollowError + '_ {
    move |source| FollowError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Capture;
    use ferric_guest_ram::RamLayout;
    use ferric_trace::Event;
    use ferric_trace::clock::{Clock, ClockLine};
    use ferric_trace::host::{Published, RegionLayout, RingSize};
    use ferric_trace::kernel::{SYSCALL_ENTER, SYSCALL_EXIT};
    use ferric_trace::ring::{Ring, Shared, WhenFull};
    use std::collections::BTreeMap;
    use std::os::unix::fs::FileExt;

    const MIB: u64 = 1 << 20;

    const CLOCK_LINE: ClockLine = ClockLine {
        clock: Clock::KvmClock,
        period_fs: Some(1_000_000),
    };

    /// A guest's RAM file holding a host ring region that the test fills with the kernel's
    /// own producer, `Ring::push`, and copies into the file as the guest's stores would land.
    struct Guest {
        ram_file: tempfile::NamedTempFile,
        layout: RamLayout,
        phys: u64,
        region: &'static [AtomicU64],
        rings: Vec<Ring<Shared<&'static [AtomicU64]>>>,
        next_timestamp: u64,
        generation: u64,
    }

    impl Guest {
        /// A guest of `layout`'s RAM with `cpus` rings of `kib` KiB in a region at `phys`.
        fn new(layout: RamLayout, phys: u64, cpus: usize, kib: u32) -> Self {
            let ram_file = tempfile::NamedTempFile::new().expect("a temporary file");
            ram_file
                .as_file()
                .set_len(layout.ram_bytes())
                .expect("sized");
            let region_layout =
                RegionLayout::new(cpus, RingSize::from_kib(kib).expect("valid")).expect("fits");
            let words = region_layout.bytes() / WORD_BYTES;
            // Leaked: the rings borrow it for the rest of the test.
            let region: &'static [AtomicU64] =
                Box::leak((0..words).map(|_| AtomicU64::new(0)).collect());
            let rings = region_layout
                .entries()
                .map(|entry| {
                    let start = entry.offset as usize / WORD_BYTES;
                    let len = entry.bytes as usize / WORD_BYTES;
                    Ring::shared(&region[start..start + len])
                        .expect("a ring")
                        .when_full(WhenFull::OverwriteOldest)
                })
                .collect();
            let mut guest = Self {
                ram_file,
                layout,
                phys,
                region,
                rings,
                next_timestamp: 1,
                generation: 77,
            };
            guest.publish();
            guest
        }

        fn publish(&mut self) {
            let published = Published {
                self_phys: self.phys,
                generation: self.generation,
                clock: CLOCK_LINE,
            };
            let cpus = self.rings.len();
            let entries = RegionLayout::new(cpus, self.ring_size())
                .expect("fits")
                .entries();
            host::publish(&self.region[..DIRECTORY_WORDS], &published, entries).expect("published");
        }

        fn ring_size(&self) -> RingSize {
            let data_bytes = self.rings[0].capacity() * WORD_BYTES;
            RingSize::from_kib((data_bytes / 1024) as u32).expect("valid")
        }

        /// Pushes `count` records of `event` on `cpu`, timestamped in order, and returns
        /// their timestamps.
        fn push(&mut self, cpu: usize, event: &Event, count: usize) -> Vec<u64> {
            let args = vec![5u64; event.args().len()];
            (0..count)
                .map(|_| {
                    let timestamp = self.next_timestamp;
                    self.next_timestamp += 1;
                    self.rings[cpu]
                        .push(event.id(), timestamp, &args)
                        .expect("an overwriting ring takes every record");
                    timestamp
                })
                .collect()
        }

        /// Copies the region into the RAM file.
        fn sync(&self) {
            let bytes: Vec<u8> = self
                .region
                .iter()
                .flat_map(|word| word.load(Ordering::Relaxed).to_ne_bytes())
                .collect();
            let offset = self
                .layout
                .offset_of(self.phys, bytes.len() as u64)
                .expect("the region is in RAM");
            self.ram_file
                .as_file()
                .write_all_at(&bytes, offset)
                .expect("written");
        }

        /// Writes a copy of the directory page at `phys`, which does not hold its address.
        fn decoy(&self, phys: u64) {
            let bytes: Vec<u8> = self.region[..DIRECTORY_WORDS]
                .iter()
                .flat_map(|word| word.load(Ordering::Relaxed).to_ne_bytes())
                .collect();
            let offset = self.layout.offset_of(phys, 4096).expect("in RAM");
            self.ram_file
                .as_file()
                .write_all_at(&bytes, offset)
                .expect("written");
        }

        fn map(&self) -> GuestRam {
            GuestRam::open(self.ram_file.path(), Some(self.layout)).expect("maps")
        }
    }

    /// The timestamps of each CPU's records in the capture at `dir`.
    fn captured(dir: &Path) -> BTreeMap<u32, Vec<u64>> {
        let capture = Capture::load(&[dir.to_owned()]).expect("a complete capture");
        assert_eq!(capture.clock(), Some(CLOCK_LINE));
        let mut by_cpu: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
        for entry in capture.entries() {
            by_cpu.entry(entry.cpu).or_default().push(entry.timestamp);
        }
        by_cpu
    }

    fn stats(path: &Path) -> Vec<StatsLine> {
        fs::read_to_string(path)
            .expect("stats")
            .lines()
            .map(|line| StatsLine::parse(line).expect("a stats line"))
            .collect()
    }

    /// Records of both lengths, in turns: 7 words then 4.
    fn pairs(guest: &mut Guest, cpu: usize, count: usize) -> Vec<u64> {
        let mut all = Vec::new();
        for _ in 0..count {
            all.extend(guest.push(cpu, &SYSCALL_ENTER, 1));
            all.extend(guest.push(cpu, &SYSCALL_EXIT, 1));
        }
        all
    }

    #[test]
    fn follows_through_wrap_around_and_overwrites_into_a_capture() {
        let mut guest = Guest::new(RamLayout::q35(8 * MIB), 3 * MIB, 2, 4);
        let out_dir = tempfile::tempdir().expect("a directory");
        let out = out_dir.path().join("trace");

        // Phase 1: less than a ring each.
        let mut expected = pairs(&mut guest, 0, 20);
        let cpu1 = guest.push(1, &SYSCALL_EXIT, 10);
        guest.sync();
        guest.decoy(MIB);
        let ram = guest.map();
        let (phys, directory) = find_directory(&ram, None)
            .expect("scans")
            .expect("finds the directory, not the decoy");
        assert_eq!(phys, 3 * MIB);
        create_output(&out).expect("created");
        let mut follower = Follower::start(&ram, phys, &directory, &out).expect("starts");
        assert_eq!(follower.poll(1_000, None).expect("polls"), Poll::Read);

        // Phase 2: past the end of CPU 0's 512 words, but never a ring ahead of the reader.
        expected.extend(pairs(&mut guest, 0, 30));
        guest.sync();
        assert_eq!(follower.poll(2_000, None).expect("polls"), Poll::Read);
        assert_eq!(follower.lost_records(), 0);

        // Phase 3: two rings' worth without a poll: the oldest are overwritten.
        let overwritten = pairs(&mut guest, 0, 100);
        guest.sync();
        assert_eq!(follower.poll(300_000_000, None).expect("polls"), Poll::Read);
        let summary = follower.finish(StopReason::Duration).expect("finishes");

        assert!(!out.join(INCOMPLETE).exists());
        let by_cpu = captured(&out);
        assert_eq!(by_cpu[&1], cpu1);
        let cpu0 = &by_cpu[&0];
        assert_eq!(
            cpu0[..expected.len()],
            expected[..],
            "nothing lost before the overwrite"
        );
        let kept = &cpu0[expected.len()..];
        assert!(!kept.is_empty() && kept.len() < overwritten.len());
        assert_eq!(
            kept,
            &overwritten[overwritten.len() - kept.len()..],
            "the newest records, without a gap"
        );

        let cpu = summary.cpus[0];
        assert_eq!((cpu.skips, cpu.lost_before), (1, 0));
        let lost = (overwritten.len() - kept.len()) as u64;
        assert!(cpu.lost_words >= lost * 4 && cpu.lost_words <= lost * 7);
        assert!(
            cpu.lost_records.abs_diff(lost) <= 1,
            "estimated {} for {lost}",
            cpu.lost_records
        );
        assert_eq!(cpu.records, cpu0.len() as u64);
        assert_eq!(summary.cpus[1].lost_records, 0);
        assert_eq!(summary.stop, StopReason::Duration);

        let start = stats(&out.join(STATS_START));
        assert_eq!(
            start,
            [
                StatsLine {
                    cpu: 0,
                    pending: 220,
                    lost: 0
                },
                StatsLine {
                    cpu: 1,
                    pending: 40,
                    lost: 0
                },
            ]
        );
        let end = stats(&out.join(STATS_END));
        assert_eq!(end[0].pending, 0);
        assert_eq!(end[0].lost, cpu.lost_records);
        assert_eq!(end[1].lost, 0);

        let samples: Vec<ClockSampleLine> = fs::read_to_string(out.join(HOST_CLOCK))
            .expect("samples")
            .lines()
            .map(|line| ClockSampleLine::parse(line).expect("a sample"))
            .collect();
        // CPU 1 had records in the first poll only, CPU 0 in three; the second poll came
        // too soon after the first for another sample.
        let cpu0_samples: Vec<_> = samples.iter().filter(|s| s.cpu == 0).collect();
        assert_eq!(cpu0_samples.len(), 2, "{samples:?}");
        assert_eq!(
            cpu0_samples[1].timestamp,
            *overwritten.last().expect("records")
        );
        assert_eq!(cpu0_samples[1].head, 220 + 330 + 1100);
        assert_eq!(cpu0_samples[1].host_ns, 300_000_000);
        assert_eq!(samples.iter().filter(|s| s.cpu == 1).count(), 1);
    }

    #[test]
    fn the_scan_finds_rings_above_4_gib_past_a_decoy() {
        let layout = RamLayout::split(8 * MIB, 4 * MIB).expect("valid");
        let phys = ferric_guest_ram::FOUR_GIB + MIB;
        let mut guest = Guest::new(layout, phys, 1, 4);
        guest.push(0, &SYSCALL_EXIT, 3);
        guest.sync();
        guest.decoy(2 * MIB);
        let ram = guest.map();
        let found = |at| find_directory(&ram, at).expect("reads").map(|(p, _)| p);
        assert_eq!(found(None), Some(phys));
        assert_eq!(found(Some(phys)), Some(phys));
        assert_eq!(
            found(Some(2 * MIB)),
            None,
            "the decoy's address does not match"
        );
        assert!(matches!(
            find_directory(&ram, Some(5 * MIB)),
            Err(FollowError::NotRam { phys: 0x50_0000 })
        ));
        // The decoy alone is not taken for a directory.
        let directory_offset = layout.offset_of(phys, 8).expect("in RAM");
        guest
            .ram_file
            .as_file()
            .write_all_at(&[0; 8], directory_offset)
            .expect("written");
        assert_eq!(found(None), None);
    }

    #[test]
    fn a_new_boot_ends_the_follow() {
        let mut guest = Guest::new(RamLayout::q35(4 * MIB), MIB, 1, 4);
        guest.push(0, &SYSCALL_EXIT, 5);
        guest.sync();
        let ram = guest.map();
        let out_dir = tempfile::tempdir().expect("a directory");
        let out = out_dir.path().join("trace");
        create_output(&out).expect("created");
        let (phys, directory) = find_directory(&ram, Some(MIB))
            .expect("reads")
            .expect("found");
        let mut follower = Follower::start(&ram, phys, &directory, &out).expect("starts");
        assert_eq!(follower.poll(1, None).expect("polls"), Poll::Read);
        guest.generation += 1;
        guest.publish();
        guest.sync();
        assert_eq!(follower.poll(2, None).expect("polls"), Poll::Rebooted);
        let summary = follower.finish(StopReason::Rebooted).expect("finishes");
        assert_eq!(summary.cpus[0].records, 5);
        assert_eq!(captured(&out)[&0].len(), 5);
    }

    #[test]
    fn run_saves_a_capture_until_the_ram_file_goes_away() {
        let mut guest = Guest::new(RamLayout::q35(4 * MIB), 2 * MIB, 2, 4);
        let timestamps = guest.push(1, &SYSCALL_ENTER, 12);
        guest.sync();
        let ram = guest.map();
        let path = guest.ram_file.path().to_owned();
        fs::remove_file(&path).expect("removed");
        let out_dir = tempfile::tempdir().expect("a directory");
        let out = out_dir.path().join("nested").join("trace");
        let stop = AtomicBool::new(false);
        let options = Options {
            live: true,
            ..Options::default()
        };
        let mut live = Vec::new();
        let summary = run(&ram, &out, &options, &stop, &mut live).expect("runs");
        assert_eq!(summary.stop, StopReason::RamGone);
        assert_eq!(summary.phys, 2 * MIB);
        assert_eq!(captured(&out)[&1], timestamps);
        assert!(live.is_empty(), "stopped within the first second");
        assert!(matches!(
            run(&ram, &out, &options, &stop, &mut live),
            Err(FollowError::OutputExists { .. })
        ));
    }

    #[test]
    fn run_gives_up_on_a_directory_that_never_comes() {
        let guest = Guest::new(RamLayout::q35(4 * MIB), 2 * MIB, 1, 4);
        guest.sync();
        guest.decoy(MIB);
        let ram = guest.map();
        let out_dir = tempfile::tempdir().expect("a directory");
        let out = out_dir.path().join("trace");
        let options = Options {
            phys: Some(MIB),
            duration: Some(Duration::ZERO),
            ..Options::default()
        };
        let stop = AtomicBool::new(false);
        assert!(matches!(
            run(&ram, &out, &options, &stop, &mut io::sink()),
            Err(FollowError::NotFound {
                reason: StopReason::Duration
            })
        ));
        assert!(out.join(INCOMPLETE).exists());
        let stopped = AtomicBool::new(true);
        let options = Options {
            phys: Some(MIB),
            ..Options::default()
        };
        assert!(matches!(
            run(
                &ram,
                &out_dir.path().join("again"),
                &options,
                &stopped,
                &mut io::sink()
            ),
            Err(FollowError::NotFound {
                reason: StopReason::Stopped
            })
        ));
    }

    #[test]
    fn a_follow_with_a_duration_stops_after_it() {
        let mut guest = Guest::new(RamLayout::q35(4 * MIB), 2 * MIB, 1, 4);
        let timestamps = guest.push(0, &SYSCALL_EXIT, 4);
        guest.sync();
        let ram = guest.map();
        let out_dir = tempfile::tempdir().expect("a directory");
        let out = out_dir.path().join("trace");
        let options = Options {
            phys: Some(2 * MIB),
            duration: Some(Duration::ZERO),
            ..Options::default()
        };
        let summary = run(
            &ram,
            &out,
            &options,
            &AtomicBool::new(false),
            &mut io::sink(),
        )
        .expect("runs");
        assert_eq!(summary.stop, StopReason::Duration);
        assert_eq!(captured(&out)[&0], timestamps);
    }
}
