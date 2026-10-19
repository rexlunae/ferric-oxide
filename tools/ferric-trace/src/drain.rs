//! Saving a capture on the target: draining every CPU's ring of the `kernel.trace` scheme into
//! a directory that [`Capture::load`](crate::capture::Capture::load) reads back.
//!
//! [`drain_scheme`] reads the scheme's `stats` file for the list of CPUs and saves it as
//! `stats.start`, drains each `cpu/<n>` file into `cpu<n>`, then reads `stats` again and saves
//! it as `stats.end`, so records dropped while the capture ran are counted too. It also copies
//! the scheme's `clock` file, which says what the timestamps count, when the kernel has one
//! (kernels before T3d do not).
//!
//! Each CPU is read in one of two ways ([`Via`]). Through the mapping (`Via::Map`), the ring
//! is mapped read-only (`ferric_trace_map::Mapping`, Redox only), records are copied out of
//! it with [`View`], and each copy is consumed with `commit <end>` only after it was written
//! to the output, so a capture that dies keeps every record it took from the ring. Without it
//! (`Via::Read`), `cpu/<n>` is read, which consumes as it copies. `Via::Auto`, the default,
//! maps where it can and reads otherwise (a kernel before T3a, or a host stand-in).
//!
//! The capture is a snapshot: a CPU is drained until it has given the bytes `stats.start`
//! listed as pending for it, or until a read returns 0 bytes (the ring is empty), whichever
//! comes first. Draining until empty alone never ends while tracing is on, because saving a
//! capture makes events of its own (every write to the output is a scheme request), so the
//! ring refills as fast as it is read and the output grows until the disk is full. A read or a
//! commit fails with `EAGAIN` while another reader is draining the same CPU; it is retried
//! after a short wait, up to [`AGAIN_RETRIES`] times in a row. Any other error stops the capture, naming the file.
//!
//! Ring reads are destructive, so nothing drained is ever deleted. The output directory is
//! created with exclusive creation (it must not exist), an `INCOMPLETE` marker
//! ([`crate::capture::INCOMPLETE`]) is written into it first, and the marker is removed only
//! after `stats.end` is saved. If the capture fails or is killed, everything drained so far
//! stays in the directory with the marker, and [`Capture::load`](crate::capture::Capture::load)
//! refuses it unless it is asked to accept a partial capture. The output must not overlap the
//! scheme path.
//!
//! The scheme's interface is [`ferric_trace::abi`].

use std::fmt;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::Duration;

use ferric_trace::abi::{self, CpuCommand, LineError, StatsLine};
use ferric_trace::record::WORD_BYTES;
use ferric_trace::ring::{ReadError, View, ViewError, Words};

use crate::capture::INCOMPLETE;

/// How many `EAGAIN` reads in a row a CPU file may give before the capture fails.
pub const AGAIN_RETRIES: u32 = 200;

/// The name of the `stats` snapshot taken before draining.
pub const STATS_START: &str = "stats.start";

/// The name of the `stats` snapshot taken after draining.
pub const STATS_END: &str = "stats.end";

/// How long to wait after an `EAGAIN` read.
const AGAIN_WAIT: Duration = Duration::from_millis(5);

/// The read size: many records at a time, and never less than [`abi::READ_MIN`].
const READ_BYTES: usize = 64 * 1024;

const _: () = assert!(READ_BYTES >= abi::READ_MIN);

/// How [`drain_scheme`] takes records out of the rings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Via {
    /// Through the mapping where the ring can be mapped, by reading otherwise.
    #[default]
    Auto,
    /// Through the mapping only: a ring that cannot be mapped fails the capture.
    Map,
    /// By reading `cpu/<n>` only.
    Read,
}

/// A [`Via`] name that is not `auto`, `map` or `read`.
#[derive(Debug, thiserror::Error)]
#[error("not a way to read the rings (auto, map or read)")]
pub struct ViaError;

impl FromStr for Via {
    type Err = ViaError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "auto" => Ok(Self::Auto),
            "map" => Ok(Self::Map),
            "read" => Ok(Self::Read),
            _ => Err(ViaError),
        }
    }
}

impl fmt::Display for Via {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Map => "map",
            Self::Read => "read",
        })
    }
}

/// A mapped ring that stopped at a record that does not decode, which only a kernel bug
/// leaves.
#[derive(Debug, thiserror::Error)]
#[error("a record at position {position} does not decode")]
pub struct CorruptRecord {
    /// The ring position of the record.
    pub position: u64,
}

/// Why a capture could not be saved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DrainError {
    /// A file could not be opened, read, created, written or renamed.
    #[error("{}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A line of `stats` does not parse.
    #[error("{}: bad line {line:?}", path.display())]
    Stats {
        /// The `stats` path.
        path: PathBuf,
        /// The line.
        line: String,
        /// Why it does not parse.
        #[source]
        source: LineError,
    },
    /// A CPU file stayed busy (`EAGAIN`) for [`AGAIN_RETRIES`] reads in a row.
    #[error("{}: still busy after {AGAIN_RETRIES} tries; is another reader draining it?", path.display())]
    Busy {
        /// The CPU file.
        path: PathBuf,
    },
    /// The output directory already exists; it may hold an earlier capture.
    #[error("{}: already exists; give a new directory", path.display())]
    OutputExists {
        /// The output directory.
        path: PathBuf,
    },
    /// The capture failed after the output directory was created. What was drained before
    /// the failure is kept there, marked `INCOMPLETE`, since the rings no longer hold it.
    #[error("the capture in {} is partial (marked INCOMPLETE)", out.display())]
    Partial {
        /// The output directory.
        out: PathBuf,
        /// What failed.
        #[source]
        source: Box<DrainError>,
    },
    /// A CPU's ring could not be mapped, with [`Via::Map`].
    #[error("{}: cannot map the ring", path.display())]
    Map {
        /// The CPU file.
        path: PathBuf,
        /// The kernel's error, or [`ErrorKind::Unsupported`] off Redox.
        #[source]
        source: io::Error,
    },
    /// A mapped ring's header is not one this reader understands.
    #[error("{}: not a ring this reader can map", path.display())]
    View {
        /// The CPU file.
        path: PathBuf,
        /// What is wrong with the header.
        #[source]
        source: ViewError,
    },
    /// The output directory is inside the scheme path, or holds it.
    #[error("{} overlaps the scheme path {}", out.display(), scheme.display())]
    Overlap {
        /// The output directory.
        out: PathBuf,
        /// The scheme path.
        scheme: PathBuf,
    },
}

/// What was saved for one CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuDrained {
    /// The CPU.
    pub cpu: u32,
    /// The bytes saved.
    pub bytes: u64,
    /// Records the CPU had dropped since boot when the capture started.
    pub lost_before: u64,
    /// Records the CPU dropped while the capture ran.
    pub lost_during: u64,
    /// Whether the ring was read through its mapping.
    pub mapped: bool,
}

/// Drains every CPU of the trace scheme at `scheme` (normally `/scheme/kernel.trace`) into
/// directory `out`, which this creates (with any missing parents) and which must not exist
/// yet, taking the records out `via` the mapping or reads.
///
/// # Errors
///
/// [`DrainError::OutputExists`] or [`DrainError::Overlap`] for an unusable `out`, and
/// [`DrainError::Io`] if it cannot be created; nothing has been read then. Any later failure is
/// [`DrainError::Partial`], whose source is a [`DrainError::Io`] for a file that cannot be read
/// or written, a [`DrainError::Stats`] for a `stats` line that does not parse, or a
/// [`DrainError::Busy`] for a CPU file that another reader holds for too long. With
/// [`Via::Map`], it can also be a [`DrainError::Map`] or [`DrainError::View`] for a ring that
/// cannot be mapped or read in place, whichever [`Via`] is asked for. A partial
/// capture keeps everything drained before the failure and its `INCOMPLETE` marker.
pub fn drain_scheme(scheme: &Path, out: &Path, via: Via) -> Result<Vec<CpuDrained>, DrainError> {
    check_overlap(scheme, out)?;
    // Only `out` itself must be new; its parents may already exist or not.
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    fs::create_dir(out).map_err(|source| {
        if source.kind() == ErrorKind::AlreadyExists {
            DrainError::OutputExists {
                path: out.to_owned(),
            }
        } else {
            io_error(out)(source)
        }
    })?;
    drain_into(scheme, out, via).map_err(|source| DrainError::Partial {
        out: out.to_owned(),
        source: Box::new(source),
    })
}

/// Refuses an `out` inside `scheme` or holding it.
fn check_overlap(scheme: &Path, out: &Path) -> Result<(), DrainError> {
    let (out_abs, scheme_abs) = (absolute(out), absolute(scheme));
    if out_abs.starts_with(&scheme_abs) || scheme_abs.starts_with(&out_abs) {
        return Err(DrainError::Overlap {
            out: out.to_owned(),
            scheme: scheme.to_owned(),
        });
    }
    Ok(())
}

/// `path` made absolute, with symbolic links in its existing part resolved.
fn absolute(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    // The path does not exist yet: resolve its parent and add the name.
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => absolute(parent).join(name),
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_owned()),
    }
}

/// Marks `dir` incomplete, saves the start snapshot, every CPU and the end snapshot into it,
/// and removes the mark.
fn drain_into(scheme: &Path, dir: &Path, via: Via) -> Result<Vec<CpuDrained>, DrainError> {
    let marker = dir.join(INCOMPLETE);
    fs::write(&marker, "ferric-trace capture did not finish\n").map_err(io_error(&marker))?;
    copy_clock(scheme, &dir.join(crate::capture::CLOCK))?;
    let start = snapshot(scheme, &dir.join(STATS_START))?;
    let mut drained = Vec::with_capacity(start.len());
    for line in &start {
        let from = scheme.join(abi::CPU_DIR).join(line.cpu.to_string());
        let to = dir.join(format!("cpu{}", line.cpu));
        let budget = line.pending.saturating_mul(WORD_BYTES as u64);
        let (bytes, mapped) = drain_cpu(&from, &to, budget, via)?;
        drained.push(CpuDrained {
            cpu: line.cpu,
            bytes,
            lost_before: line.lost,
            lost_during: 0,
            mapped,
        });
    }
    let end = snapshot(scheme, &dir.join(STATS_END))?;
    count_losses(&mut drained, &end);
    fs::remove_file(&marker).map_err(io_error(&marker))?;
    Ok(drained)
}

/// Drains CPU file `from` into a new file `to` until `budget` bytes, through the mapping or
/// by reading as `via` says, and returns the bytes saved and whether the mapping was used.
fn drain_cpu(from: &Path, to: &Path, budget: u64, via: Via) -> Result<(u64, bool), DrainError> {
    if via != Via::Read {
        match map::open(from) {
            Ok(cpu) => return map::drain(&cpu, from, to, budget).map(|bytes| (bytes, true)),
            Err(err) if via == Via::Map => return Err(err),
            // A ring that cannot be mapped is read instead.
            Err(_) => {}
        }
    }
    let mut source = Reading(File::open(from).map_err(io_error(from))?);
    let mut sink = File::create(to).map_err(io_error(to))?;
    let bytes = drain(&mut source, from, &mut sink, to, budget, AGAIN_WAIT)?;
    Ok((bytes, false))
}

/// Mapping a CPU's ring, on Redox.
#[cfg(target_os = "redox")]
mod map {
    use std::fs::{File, OpenOptions};
    use std::io::{self, ErrorKind};
    use std::os::fd::AsRawFd;
    use std::path::Path;

    use ferric_trace::ring::View;
    use ferric_trace_map::Mapping;

    use super::{AGAIN_WAIT, DrainError, Mapped, io_error};

    /// A CPU file, open for reading and for `commit`, and its ring mapped.
    pub(super) struct Cpu {
        file: File,
        map: Mapping,
    }

    /// Opens CPU file `path` for reading and `commit`, and maps its ring.
    pub(super) fn open(path: &Path) -> Result<Cpu, DrainError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_error(path))?;
        let map_error = |source| DrainError::Map {
            path: path.to_owned(),
            source,
        };
        let fd = usize::try_from(file.as_raw_fd())
            .map_err(|_| map_error(io::Error::from(ErrorKind::InvalidInput)))?;
        let map =
            Mapping::ring(fd).map_err(|err| map_error(io::Error::from_raw_os_error(err.errno)))?;
        Ok(Cpu { file, map })
    }

    /// Drains `cpu`'s ring in place into a new file `to`.
    pub(super) fn drain(cpu: &Cpu, from: &Path, to: &Path, budget: u64) -> Result<u64, DrainError> {
        let view = View::new(cpu.map.words()).map_err(|source| DrainError::View {
            path: from.to_owned(),
            source,
        })?;
        let mut source = Mapped::new(view, &cpu.file);
        let mut sink = File::create(to).map_err(io_error(to))?;
        super::drain(&mut source, from, &mut sink, to, budget, AGAIN_WAIT)
    }
}

/// Off Redox no ring maps: the host's stand-in files are read.
#[cfg(not(target_os = "redox"))]
mod map {
    use std::io::{self, ErrorKind};
    use std::path::Path;

    use super::DrainError;

    /// A mapped CPU, which cannot exist here.
    pub(super) enum Cpu {}

    /// Fails: nothing maps off Redox.
    pub(super) fn open(path: &Path) -> Result<Cpu, DrainError> {
        Err(DrainError::Map {
            path: path.to_owned(),
            source: io::Error::from(ErrorKind::Unsupported),
        })
    }

    pub(super) fn drain(cpu: &Cpu, _: &Path, _: &Path, _: u64) -> Result<u64, DrainError> {
        match *cpu {}
    }
}

/// Where [`drain`] takes one CPU's records from.
trait Source {
    /// Copies whole records into `buf` and returns the bytes copied, 0 when there are none.
    fn copy(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Consumes what the last [`Source::copy`] gave, now that it is saved.
    fn consume(&mut self) -> io::Result<()>;
}

/// A CPU file read with `read`, which consumes as it copies.
struct Reading<R>(R);

impl<R: Read> Source for Reading<R> {
    fn copy(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }

    fn consume(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Hands a position back to the ring's owner: `commit <end>` on the CPU file.
#[cfg_attr(not(target_os = "redox"), allow(dead_code))]
trait Commit {
    /// Lets the owner reuse the ring up to `end`.
    fn commit(&mut self, end: u64) -> io::Result<()>;
}

impl Commit for &File {
    fn commit(&mut self, end: u64) -> io::Result<()> {
        // One write is one command.
        self.write(CpuCommand::Commit(end).to_string().as_bytes())
            .map(drop)
    }
}

/// A mapped ring read in place through a [`View`], each copy committed once saved.
// Off Redox only the tests make one.
#[cfg_attr(not(target_os = "redox"), allow(dead_code))]
struct Mapped<W, C> {
    view: View<W>,
    /// Where the next copy starts: the end of the last one.
    pos: u64,
    commit: C,
}

impl<W: Words, C: Commit> Mapped<W, C> {
    #[cfg_attr(not(target_os = "redox"), allow(dead_code))]
    fn new(view: View<W>, commit: C) -> Self {
        let pos = view.tail();
        Self { view, pos, commit }
    }
}

impl<W: Words, C: Commit> Source for Mapped<W, C> {
    fn copy(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // `drain` never asks for less than READ_MIN, which holds the largest record.
        let peek = self
            .view
            .peek(self.pos, buf)
            .map_err(|err: ReadError| io::Error::new(ErrorKind::InvalidInput, err))?;
        if peek.is_corrupt() && peek.is_empty() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                CorruptRecord {
                    position: self.pos.max(self.view.tail()),
                },
            ));
        }
        self.pos = peek.end();
        Ok(peek.len())
    }

    fn consume(&mut self) -> io::Result<()> {
        match self.commit.commit(self.pos) {
            // Another reader consumed past this copy already: nothing is left to hand back.
            Err(err) if err.kind() == ErrorKind::InvalidInput && self.view.tail() >= self.pos => {
                Ok(())
            }
            other => other,
        }
    }
}

/// Sets each CPU's `lost_during` from the `stats` snapshot taken after draining.
fn count_losses(drained: &mut [CpuDrained], end: &[StatsLine]) {
    for cpu in drained {
        if let Some(line) = end.iter().find(|l| l.cpu == cpu.cpu) {
            cpu.lost_during = line.lost.saturating_sub(cpu.lost_before);
        }
    }
}

/// Copies the scheme's `clock` file to `to`, if the kernel has one.
fn copy_clock(scheme: &Path, to: &Path) -> Result<(), DrainError> {
    let path = scheme.join(abi::CLOCK_PATH);
    match fs::read(&path) {
        Ok(bytes) => fs::write(to, bytes).map_err(io_error(to)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DrainError::Io { path, source }),
    }
}

/// Reads the scheme's `stats`, saves it at `to`, and parses it.
fn snapshot(scheme: &Path, to: &Path) -> Result<Vec<StatsLine>, DrainError> {
    let path = scheme.join(abi::STATS_PATH);
    let text = fs::read_to_string(&path).map_err(io_error(&path))?;
    let lines = text
        .lines()
        .map(|line| {
            StatsLine::parse(line).map_err(|source| DrainError::Stats {
                path: path.clone(),
                line: line.to_owned(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    fs::write(to, &text).map_err(io_error(to))?;
    Ok(lines)
}

/// Copies `source` (at `from`) to `sink` (at `to`) until `budget` bytes have been copied or a
/// copy returns 0 bytes, consuming each copy once it is written, retrying `EAGAIN` and `EINTR`
/// copies and commits, and returns the bytes copied.
///
/// Reads ask for no more than the bytes left in the budget, but never for less than
/// [`abi::READ_MIN`] (a ring read returns whole records and refuses a buffer too small for
/// the largest one), so the copy can end up to one record past the budget.
fn drain(
    source: &mut impl Source,
    from: &Path,
    sink: &mut impl Write,
    to: &Path,
    budget: u64,
    wait: Duration,
) -> Result<u64, DrainError> {
    let mut buf = vec![0u8; READ_BYTES];
    let mut total = 0u64;
    let mut busy = 0u32;
    while total < budget {
        let left = usize::try_from(budget - total).unwrap_or(usize::MAX);
        let want = left.clamp(abi::READ_MIN, READ_BYTES);
        let Some(n) = retry(|| source.copy(&mut buf[..want]), &mut busy, from, wait)? else {
            continue;
        };
        if n == 0 {
            break;
        }
        sink.write_all(&buf[..n]).map_err(io_error(to))?;
        total += n as u64;
        // Saved before it is consumed: a commit that never comes loses nothing.
        sink.flush().map_err(io_error(to))?;
        while retry(|| source.consume(), &mut busy, from, wait)?.is_none() {}
    }
    sink.flush().map_err(io_error(to))?;
    Ok(total)
}

/// Runs `op` once: `Some` with its result, `None` after an `EINTR` or an `EAGAIN` (waiting
/// `wait` after the latter), counting `EAGAIN`s in a row in `busy`.
///
/// # Errors
///
/// [`DrainError::Busy`] at the [`AGAIN_RETRIES`]th `EAGAIN` in a row, and
/// [`DrainError::Io`] for `from` on any other error.
fn retry<T>(
    op: impl FnOnce() -> io::Result<T>,
    busy: &mut u32,
    from: &Path,
    wait: Duration,
) -> Result<Option<T>, DrainError> {
    match op() {
        Ok(value) => {
            *busy = 0;
            Ok(Some(value))
        }
        Err(err) if err.kind() == ErrorKind::Interrupted => Ok(None),
        Err(err) if err.kind() == ErrorKind::WouldBlock => {
            *busy += 1;
            if *busy >= AGAIN_RETRIES {
                return Err(DrainError::Busy {
                    path: from.to_owned(),
                });
            }
            thread::sleep(wait);
            Ok(None)
        }
        Err(err) => Err(io_error(from)(err)),
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> DrainError + '_ {
    move |source| DrainError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Capture;
    use crate::capture::tests::bytes;
    use ferric_trace::Event;
    use ferric_trace::kernel::{SYSCALL_ENTER, SYSCALL_EXIT};
    use ferric_trace::ring::{Ring, SHARED_HEADER_WORDS, Shared};
    use std::sync::atomic::AtomicU64;

    /// A reader that gives `results` in turn, then 0 bytes.
    struct Script(Vec<io::Result<Vec<u8>>>);

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let bytes = self.0.remove(0)?;
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    fn again() -> io::Result<Vec<u8>> {
        Err(io::Error::from(ErrorKind::WouldBlock))
    }

    fn run(script: Vec<io::Result<Vec<u8>>>) -> Result<(u64, Vec<u8>), DrainError> {
        run_with(&mut Reading(Script(script)), u64::MAX)
    }

    fn run_with(source: &mut impl Source, budget: u64) -> Result<(u64, Vec<u8>), DrainError> {
        let mut sink = Vec::new();
        let n = drain(
            source,
            Path::new("cpu/0"),
            &mut sink,
            Path::new("cpu0"),
            budget,
            Duration::ZERO,
        )?;
        Ok((n, sink))
    }

    /// A ring that refills as fast as it is read, as one does while the capture's own writes
    /// are traced: every read gives one 8-byte word, and it never runs empty.
    struct Refilling {
        reads: u32,
        largest: usize,
        smallest: usize,
    }

    impl Read for Refilling {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            self.largest = self.largest.max(buf.len());
            self.smallest = self.smallest.min(buf.len());
            buf[..WORD_BYTES].fill(0xab);
            Ok(WORD_BYTES)
        }
    }

    #[test]
    fn stops_at_the_pending_bytes_of_a_ring_that_never_empties() {
        let mut ring = Refilling {
            reads: 0,
            largest: 0,
            smallest: usize::MAX,
        };
        let (n, sink) = run_with(&mut Reading(&mut ring), 10 * WORD_BYTES as u64).expect("drains");
        assert_eq!(n, 80);
        assert_eq!(sink.len(), 80);
        assert_eq!(ring.reads, 10);
        // Reads shrink with the budget left, but never below what a ring read needs.
        assert_eq!(ring.largest, abi::READ_MIN);
        assert_eq!(ring.smallest, abi::READ_MIN);
    }

    #[test]
    fn a_ring_with_nothing_pending_is_not_read() {
        let mut ring = Refilling {
            reads: 0,
            largest: 0,
            smallest: usize::MAX,
        };
        assert_eq!(
            run_with(&mut Reading(&mut ring), 0).expect("drains"),
            (0, Vec::new())
        );
        assert_eq!(ring.reads, 0);
    }

    #[test]
    fn reads_a_large_budget_in_full_size_reads() {
        let mut ring = Refilling {
            reads: 0,
            largest: 0,
            smallest: usize::MAX,
        };
        run_with(&mut Reading(&mut ring), 1 << 20).expect("drains");
        assert_eq!(ring.largest, READ_BYTES);
    }

    #[test]
    fn retries_busy_and_interrupted_reads() {
        let script = vec![
            again(),
            Ok(vec![1, 2, 3]),
            Err(io::Error::from(ErrorKind::Interrupted)),
            again(),
            again(),
            Ok(vec![4]),
        ];
        assert_eq!(run(script).expect("drains"), (4, vec![1, 2, 3, 4]));
    }

    #[test]
    fn gives_up_on_a_cpu_that_stays_busy() {
        let script = (0..AGAIN_RETRIES).map(|_| again()).collect();
        assert!(matches!(run(script), Err(DrainError::Busy { .. })));
        // One fewer busy read in a row is fine.
        let mut script: Vec<_> = (0..AGAIN_RETRIES - 1).map(|_| again()).collect();
        script.push(Ok(vec![9]));
        assert_eq!(run(script).expect("drains").0, 1);
    }

    #[test]
    fn other_errors_stop_the_capture() {
        let script = vec![
            Ok(vec![1]),
            Err(io::Error::from(ErrorKind::PermissionDenied)),
        ];
        let err = run(script).expect_err("fails");
        assert!(
            matches!(&err, DrainError::Io { path, source }
                if path == Path::new("cpu/0") && source.kind() == ErrorKind::PermissionDenied),
            "{err:?}"
        );
    }

    /// A shared ring in memory, and the commits made to it.
    struct InMemory {
        ring: Ring<Shared<Vec<AtomicU64>>>,
    }

    impl InMemory {
        fn new(records: &[(&Event, u64, &[u64])]) -> Self {
            let words = (0..SHARED_HEADER_WORDS + 64)
                .map(|_| AtomicU64::new(0))
                .collect();
            let ring = Ring::shared(words).expect("a ring");
            for (event, ts, args) in records {
                ring.push(event.id(), *ts, args).expect("fits");
            }
            Self { ring }
        }

        fn view(&self) -> View<&[AtomicU64]> {
            View::new(&self.ring.words()[..]).expect("a valid header")
        }

        fn tail(&self) -> u64 {
            self.view().tail()
        }
    }

    /// Commits to an in-memory ring, as the kernel does for `commit <end>`; `busy` commits fail
    /// with `EAGAIN` first.
    struct Committer<'a> {
        ring: &'a InMemory,
        busy: u32,
        commits: Vec<u64>,
    }

    impl Commit for Committer<'_> {
        fn commit(&mut self, end: u64) -> io::Result<()> {
            if self.busy > 0 {
                self.busy -= 1;
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            self.commits.push(end);
            self.ring
                .ring
                .commit_to(end)
                .map_err(|_| io::Error::from(ErrorKind::InvalidInput))
        }
    }

    const TWO_CALLS: &[(&Event, u64, &[u64])] = &[
        (&SYSCALL_ENTER, 10, &[1, 0, 0, 0, 0]),
        (&SYSCALL_EXIT, 20, &[1, 0]),
    ];

    #[test]
    fn a_mapped_ring_is_committed_after_each_saved_copy() {
        let ring = InMemory::new(TWO_CALLS);
        let committer = Committer {
            ring: &ring,
            busy: 2,
            commits: Vec::new(),
        };
        let mut source = Mapped::new(ring.view(), committer);
        let (n, sink) = run_with(&mut source, u64::MAX).expect("drains");
        // A 7-word and a 4-word record, consumed with one commit after two busy tries.
        assert_eq!(n, 88);
        assert_eq!(sink, bytes(TWO_CALLS));
        assert_eq!(source.commit.commits, [11]);
        assert_eq!(ring.tail(), 11);
    }

    #[test]
    fn a_mapped_ring_stops_at_the_budget() {
        let ring = InMemory::new(TWO_CALLS);
        let committer = Committer {
            ring: &ring,
            busy: 0,
            commits: Vec::new(),
        };
        let mut source = Mapped::new(ring.view(), committer);
        // A copy holds whole records, as many as fit in READ_MIN bytes: the first one here,
        // which uses up the budget, and only it is consumed.
        let (n, _) = run_with(&mut source, 8).expect("drains");
        assert_eq!(n, 56);
        assert_eq!(source.commit.commits, [7]);
        assert_eq!(ring.tail(), 7);
        let ring = InMemory::new(&[(&SYSCALL_ENTER, 10, &[1, 0, 0, 0, 0])]);
        let mut source = Mapped::new(
            ring.view(),
            Committer {
                ring: &ring,
                busy: 0,
                commits: Vec::new(),
            },
        );
        assert_eq!(run_with(&mut source, 0).expect("drains").0, 0);
        assert!(
            source.commit.commits.is_empty(),
            "nothing copied, nothing committed"
        );
        assert_eq!(ring.tail(), 0);
    }

    #[test]
    fn a_mapped_ring_read_by_another_reader_meanwhile_is_not_an_error() {
        let ring = InMemory::new(TWO_CALLS);
        let mut source = Mapped::new(ring.view(), Overtaken(&ring));
        let (n, _) = run_with(&mut source, u64::MAX).expect("drains");
        assert_eq!(n, 88);
    }

    /// A commit refused because another reader consumed further first.
    struct Overtaken<'a>(&'a InMemory);

    impl Commit for Overtaken<'_> {
        fn commit(&mut self, _: u64) -> io::Result<()> {
            let head = self.0.view().head();
            self.0
                .ring
                .commit_to(head)
                .expect("the other reader's commit");
            Err(io::Error::from(ErrorKind::InvalidInput))
        }
    }

    #[test]
    fn a_refused_commit_stops_the_capture() {
        let ring = InMemory::new(TWO_CALLS);
        struct Refused;
        impl Commit for Refused {
            fn commit(&mut self, _: u64) -> io::Result<()> {
                Err(io::Error::from(ErrorKind::InvalidInput))
            }
        }
        let mut source = Mapped::new(ring.view(), Refused);
        assert!(matches!(
            run_with(&mut source, u64::MAX),
            Err(DrainError::Io { source, .. }) if source.kind() == ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn via_names_round_trip() {
        for via in [Via::Auto, Via::Map, Via::Read] {
            assert_eq!(via.to_string().parse::<Via>().ok(), Some(via));
        }
        assert!("mmap".parse::<Via>().is_err());
    }

    #[test]
    fn drains_every_cpu_stats_lists() {
        let scheme = stand_in_scheme();
        let out = tempfile::tempdir().expect("temp dir");
        let saved = out.path().join("capture");
        let drained = drain_scheme(scheme.path(), &saved, Via::Auto).expect("drains");
        assert_eq!(
            drained,
            [
                CpuDrained {
                    cpu: 0,
                    bytes: 56,
                    lost_before: 0,
                    lost_during: 0,
                    mapped: false,
                },
                CpuDrained {
                    cpu: 2,
                    bytes: 32,
                    lost_before: 5,
                    lost_during: 0,
                    mapped: false,
                }
            ]
        );
        let capture = Capture::load(std::slice::from_ref(&saved)).expect("loads back");
        assert_eq!(capture.cpus(), [0, 2]);
        assert_eq!(capture.entries().len(), 2);
        // The stand-in has no clock file, as a kernel before T3d.
        assert_eq!(capture.clock(), None);
        assert!(saved.join(STATS_START).is_file() && saved.join(STATS_END).is_file());
        assert!(!saved.join(INCOMPLETE).exists());
    }

    #[test]
    fn copies_the_clock() {
        use ferric_trace::clock::{Clock, ClockLine};
        let scheme = stand_in_scheme();
        let line = ClockLine {
            clock: Clock::KvmClock,
            period_fs: Some(1_000_000),
        };
        fs::write(scheme.path().join("clock"), line.to_string()).expect("write");
        let out = tempfile::tempdir().expect("temp dir");
        let saved = out.path().join("capture");
        drain_scheme(scheme.path(), &saved, Via::Auto).expect("drains");
        let capture = Capture::load(std::slice::from_ref(&saved)).expect("loads back");
        assert_eq!(capture.clock(), Some(line));
    }

    #[test]
    fn creates_missing_parents() {
        let scheme = stand_in_scheme();
        let out = tempfile::tempdir().expect("temp dir");
        let nested = out.path().join("a/b/capture");
        drain_scheme(scheme.path(), &nested, Via::Auto).expect("drains");
        assert!(nested.join("cpu0").is_file());
        assert!(!nested.join(INCOMPLETE).exists());
        // The parents existing now does not make the output reusable.
        assert!(matches!(
            drain_scheme(scheme.path(), &nested, Via::Auto),
            Err(DrainError::OutputExists { .. })
        ));
    }

    #[test]
    fn never_writes_into_an_existing_directory() {
        let scheme = stand_in_scheme();
        let out = tempfile::tempdir().expect("temp dir");
        // An earlier capture, and an empty directory: both are left alone.
        let earlier = out.path().join("earlier");
        fs::create_dir(&earlier).expect("mkdir");
        fs::write(earlier.join("cpu0"), b"old").expect("write");
        let empty = out.path().join("empty");
        fs::create_dir(&empty).expect("mkdir");
        for dir in [&earlier, &empty] {
            assert!(matches!(
                drain_scheme(scheme.path(), dir, Via::Auto),
                Err(DrainError::OutputExists { .. })
            ));
        }
        assert_eq!(fs::read(earlier.join("cpu0")).expect("kept"), b"old");
        assert_eq!(fs::read_dir(&empty).expect("dir").count(), 0);
        // Nothing was drained: the stand-in's files are untouched.
        assert!(scheme.path().join("cpu/0").is_file());
    }

    #[test]
    fn a_failed_capture_keeps_what_it_drained() {
        let scheme = stand_in_scheme();
        let out = tempfile::tempdir().expect("temp dir");
        let saved = out.path().join("capture");
        fs::remove_file(scheme.path().join("cpu/2")).expect("remove");
        let err = drain_scheme(scheme.path(), &saved, Via::Auto).expect_err("CPU 2 is missing");
        let DrainError::Partial { out: at, source } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(at, &saved);
        assert!(matches!(&**source, DrainError::Io { path, .. } if path.ends_with("cpu/2")));
        // CPU 0 was drained and is kept, with the marker, so the loader refuses it.
        assert_eq!(fs::read(saved.join("cpu0")).expect("kept").len(), 56);
        assert!(saved.join(INCOMPLETE).is_file());
        assert!(saved.join(STATS_START).is_file());
        assert!(!saved.join(STATS_END).exists());
        assert!(matches!(
            Capture::load(std::slice::from_ref(&saved)),
            Err(crate::capture::CaptureError::Incomplete { .. })
        ));
        let partial = Capture::load_partial(std::slice::from_ref(&saved)).expect("loads");
        assert_eq!(partial.cpus(), [0]);

        let bad = out.path().join("bad-stats");
        fs::write(scheme.path().join("stats"), "cpu zero\n").expect("write");
        assert!(matches!(
            drain_scheme(scheme.path(), &bad, Via::Auto),
            Err(DrainError::Partial { source, .. }) if matches!(*source, DrainError::Stats { .. })
        ));
        assert!(bad.join(INCOMPLETE).is_file());
    }

    #[test]
    fn the_output_must_not_overlap_the_scheme() {
        let scheme = stand_in_scheme();
        for out in [
            scheme.path().to_owned(),
            scheme.path().join("cpu"),
            scheme.path().join("new"),
            scheme.path().join("cpu/new"),
            scheme.path().parent().expect("a parent").to_owned(),
        ] {
            assert!(
                matches!(
                    drain_scheme(scheme.path(), &out, Via::Auto),
                    Err(DrainError::Overlap { .. })
                ),
                "{}",
                out.display()
            );
        }
        assert!(!scheme.path().join("new").exists());
        let elsewhere = tempfile::tempdir().expect("temp dir");
        check_overlap(scheme.path(), &elsewhere.path().join("capture")).expect("usable");
    }

    #[test]
    fn counts_losses_during_the_capture() {
        let cpu = |cpu, lost_before| CpuDrained {
            cpu,
            bytes: 8,
            lost_before,
            lost_during: 0,
            mapped: false,
        };
        let mut drained = [cpu(0, 0), cpu(1, 4), cpu(2, 9)];
        let end = ["cpu 0 pending 0 lost 3", "cpu 1 pending 9 lost 4"]
            .map(|line| StatsLine::parse(line).expect("valid"));
        count_losses(&mut drained, &end);
        // CPU 2 is missing from the end snapshot, so nothing is known to have been lost.
        let during: Vec<_> = drained.iter().map(|c| c.lost_during).collect();
        assert_eq!(during, [3, 0, 0]);
    }

    /// A stand-in for the scheme: stats and one file per CPU, CPU 2 not numbered next.
    fn stand_in_scheme() -> tempfile::TempDir {
        let scheme = tempfile::tempdir().expect("temp dir");
        fs::write(
            scheme.path().join("stats"),
            "cpu 0 pending 7 lost 0\ncpu 2 pending 4 lost 5\n",
        )
        .expect("write");
        fs::create_dir(scheme.path().join("cpu")).expect("mkdir");
        fs::write(
            scheme.path().join("cpu/0"),
            bytes(&[(&SYSCALL_ENTER, 10, &[1, 0, 0, 0, 0])]),
        )
        .expect("write");
        fs::write(
            scheme.path().join("cpu/2"),
            bytes(&[(&SYSCALL_EXIT, 20, &[1, 0])]),
        )
        .expect("write");
        scheme
    }
}
