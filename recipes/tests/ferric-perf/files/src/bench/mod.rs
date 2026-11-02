//! The benchmarks, in groups: each [`Group`] runs as one unit and prints one or more lines.
//!
//! Groups run in [`GROUPS`] order. The spawn benchmarks come before any that start a thread,
//! so `fork` copies a process with a single thread, as most programs that fork are.

use std::ffi::NulError;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use ferric_iobench::latency::Histogram;

use crate::measure::{Monotonic, Plan, measure};
use crate::report::Line;
use crate::sys;

mod file;
mod mmap;
pub mod pipe;
mod spawn;
mod write;

/// Where the benchmarks find their programs and files, and how long they run.
#[derive(Clone, Debug)]
pub struct Config {
    /// The statically linked program the spawn benchmarks start.
    pub static_true: PathBuf,
    /// The dynamically linked program the spawn benchmarks start.
    pub dynamic_true: PathBuf,
    /// The shell `spawn-ion` runs as `ion -c true`.
    pub ion: PathBuf,
    /// The log `write-log` writes to.
    pub log: PathBuf,
    /// The sink `write-null` writes to.
    pub null: PathBuf,
    /// The directory under which the file and mapping benchmarks make their scratch
    /// directories, `files` and `mmap` ([`file::Scratch`]); it is created if needed and left
    /// in place.
    pub dir: PathBuf,
    /// The size of the file `mmap` maps, in MiB.
    pub mmap_mib: u64,
    /// Run each benchmark only briefly ([`Plan::quick`]).
    pub quick: bool,
}

impl Config {
    /// `plan`, or its quick version with `--quick`.
    pub fn plan(&self, plan: Plan) -> Plan {
        if self.quick { plan.quick() } else { plan }
    }
}

/// What a benchmark was doing when it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Creating a file or pipe.
    Create,
    /// Opening a file.
    Open,
    /// Reading.
    Read,
    /// Writing.
    Write,
    /// Getting a file's metadata.
    Stat,
    /// Removing a file or directory.
    Remove,
    /// Creating a directory.
    MakeDir,
    /// Mapping a file.
    Map,
    /// Unmapping a file.
    Unmap,
    /// Starting a process or thread.
    Start,
    /// Waiting for a process.
    Wait,
    /// Echoing bytes back (the peer of a ping-pong).
    Echo,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Op::Create => "create",
            Op::Open => "open",
            Op::Read => "read",
            Op::Write => "write",
            Op::Stat => "stat",
            Op::Remove => "remove",
            Op::MakeDir => "make directory",
            Op::Map => "map",
            Op::Unmap => "unmap",
            Op::Start => "start",
            Op::Wait => "wait for",
            Op::Echo => "echo through",
        })
    }
}

/// Why a benchmark failed.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// A file, directory or program could not be used.
    #[error("cannot {op} {}", path.display())]
    File {
        /// What failed.
        op: Op,
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A pipe could not be used.
    #[error("cannot {op} a pipe")]
    Pipe {
        /// What failed.
        op: Op,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A thread could not be started.
    #[error("cannot start the {name} thread")]
    Thread {
        /// The thread's role.
        name: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A thread panicked.
    #[error("the {name} thread panicked")]
    Panicked {
        /// The thread's role.
        name: &'static str,
    },
    /// The monotonic clock's resolution could not be read.
    #[error("cannot read the monotonic clock's resolution")]
    Resolution(#[source] io::Error),
    /// This program's own path, which `pingpong-process` runs as its peer, is unknown.
    #[error("cannot find this program's path")]
    CurrentExe(#[source] io::Error),
    /// A program path or argument holds a NUL byte.
    #[error("{} cannot be passed to exec", path.display())]
    Nul {
        /// The program.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: NulError,
    },
    /// A started program did not exit successfully.
    #[error("{} exited with {status}", path.display())]
    Exit {
        /// The program.
        path: PathBuf,
        /// How it ended.
        status: ExitStatus,
    },
    /// A ping-pong peer sent back another byte than it got.
    #[error("the peer answered {got:#04x} to {sent:#04x}")]
    Echo {
        /// The byte sent.
        sent: u8,
        /// The byte that came back.
        got: u8,
    },
    /// A mapped file did not read back what was written to it.
    #[error("{} reads back as byte sum {got} through the mapping, not {expected}", path.display())]
    MappedContent {
        /// The file.
        path: PathBuf,
        /// The sum of the bytes written at the touched offsets.
        expected: u64,
        /// The sum read.
        got: u64,
    },
}

/// The error of a failed `op` on `path`.
pub fn file_error(op: Op, path: &Path) -> impl FnOnce(io::Error) -> BenchError + '_ {
    move |source| BenchError::File {
        op,
        path: path.to_owned(),
        source,
    }
}

/// The error of a failed `op` on a pipe.
pub fn pipe_error(op: Op) -> impl FnOnce(io::Error) -> BenchError {
    move |source| BenchError::Pipe { op, source }
}

/// One measured phase: a line of output.
#[derive(Clone, Debug)]
pub struct Measurement {
    /// Its name, without the `perf-` prefix.
    pub name: &'static str,
    /// Its times.
    pub latency: Histogram,
    /// Bytes each iteration moved, for a throughput.
    pub bytes: Option<u64>,
    /// More integer fields for the line ([`Line::extra`]).
    pub extra: Vec<(&'static str, u64)>,
}

impl Measurement {
    /// The `FERRIC_BENCH` line.
    pub fn line(&self) -> Line<'_> {
        Line {
            name: self.name,
            latency: &self.latency,
            bytes: self.bytes,
            extra: &self.extra,
        }
    }
}

/// A benchmark group.
pub struct Group {
    /// Its name.
    pub name: &'static str,
    /// The names of the lines it prints, in order.
    pub lines: &'static [&'static str],
    /// Runs it.
    pub run: fn(&Config) -> Result<Vec<Measurement>, BenchError>,
}

/// Every group, in the order they run.
pub const GROUPS: &[Group] = &[
    Group {
        name: "clock",
        lines: &["clock"],
        run: clock,
    },
    Group {
        name: "spawn-fork-exec-static",
        lines: &["spawn-fork-exec-static"],
        run: spawn::fork_exec_static,
    },
    Group {
        name: "spawn-fork-exec-dynamic",
        lines: &["spawn-fork-exec-dynamic"],
        run: spawn::fork_exec_dynamic,
    },
    Group {
        name: "spawn-posix-spawn-static",
        lines: &["spawn-posix-spawn-static"],
        run: spawn::posix_spawn_static,
    },
    Group {
        name: "spawn-posix-spawn-dynamic",
        lines: &["spawn-posix-spawn-dynamic"],
        run: spawn::posix_spawn_dynamic,
    },
    Group {
        name: "spawn-ion",
        lines: &["spawn-ion"],
        run: spawn::ion,
    },
    Group {
        name: "pingpong-thread",
        lines: &["pingpong-thread"],
        run: pipe::pingpong_thread,
    },
    Group {
        name: "pingpong-process",
        lines: &["pingpong-process"],
        run: pipe::pingpong_process,
    },
    Group {
        name: "pipe-rtt-1b",
        lines: &["pipe-rtt-1b"],
        run: pipe::round_trip_1b,
    },
    Group {
        name: "pipe-throughput",
        lines: &["pipe-throughput"],
        run: pipe::throughput,
    },
    Group {
        name: "write-null",
        lines: &["write-null"],
        run: write::null,
    },
    Group {
        name: "write-log",
        lines: &["write-log"],
        run: write::log,
    },
    Group {
        name: "file",
        lines: &["file-create", "file-stat", "file-unlink"],
        run: file::run,
    },
    Group {
        name: "mmap",
        lines: &["mmap-map", "mmap-touch", "mmap-unmap"],
        run: mmap::run,
    },
];

/// A filter that names no benchmark.
#[derive(Debug, thiserror::Error)]
#[error("no benchmark name starts with {0:?} (--list shows them)")]
pub struct UnknownFilter(pub String);

/// The groups that print a line whose name starts with one of `filters`, in run order; every
/// group when there are no filters.
///
/// # Errors
///
/// [`UnknownFilter`] for a filter no line name starts with, so a typo does not silently run
/// nothing.
pub fn select(filters: &[String]) -> Result<Vec<&'static Group>, UnknownFilter> {
    if let Some(unknown) = filters.iter().find(|filter| {
        !GROUPS.iter().any(|group| {
            group
                .lines
                .iter()
                .any(|line| line.starts_with(filter.as_str()))
        })
    }) {
        return Err(UnknownFilter(unknown.clone()));
    }
    Ok(GROUPS
        .iter()
        .filter(|group| {
            filters.is_empty()
                || group.lines.iter().any(|line| {
                    filters
                        .iter()
                        .any(|filter| line.starts_with(filter.as_str()))
                })
        })
        .collect())
}

/// `clock`: what reading the clock costs, the overhead in every other measurement's samples,
/// and its resolution (`res_ns`), which bounds how finely any sample can be timed.
fn clock(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let resolution = sys::monotonic_resolution_ns().map_err(BenchError::Resolution)?;
    let plan = config.plan(Plan {
        warmup: 1000,
        iters: 10_000,
        min_iters: 100,
        budget: Duration::from_millis(500),
    });
    let [latency] = measure(&plan, &mut Monotonic::new(), |laps| {
        laps.lap();
        Ok::<_, BenchError>(())
    })?
    .phases;
    Ok(vec![Measurement {
        name: "clock",
        latency,
        bytes: None,
        extra: vec![("res_ns", resolution)],
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(groups: &[&Group]) -> Vec<&'static str> {
        groups.iter().map(|group| group.name).collect()
    }

    #[test]
    fn filters_pick_groups_by_their_lines() {
        assert_eq!(select(&[]).expect("all").len(), GROUPS.len());
        let filters =
            |list: &[&str]| -> Vec<String> { list.iter().map(|f| f.to_string()).collect() };
        assert_eq!(
            names(&select(&filters(&["spawn-posix"])).expect("known")),
            ["spawn-posix-spawn-static", "spawn-posix-spawn-dynamic"]
        );
        // A line of a group selects the whole group, and run order wins over filter order.
        assert_eq!(
            names(&select(&filters(&["mmap-touch", "file-stat", "clock"])).expect("known")),
            ["clock", "file", "mmap"]
        );
        assert!(matches!(
            select(&filters(&["spawn", "fork"])),
            Err(UnknownFilter(filter)) if filter == "fork"
        ));
    }

    #[test]
    fn names_are_unique_and_groups_print_their_own_name_first() {
        let mut lines: Vec<&str> = GROUPS
            .iter()
            .flat_map(|g| g.lines.iter().copied())
            .collect();
        let count = lines.len();
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines.len(), count);
        for group in GROUPS {
            assert!(group.lines[0].starts_with(group.name), "{}", group.name);
        }
    }
}
