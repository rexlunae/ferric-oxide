//! `ferric-perf`: process, context-switch, pipe, logging and file microbenchmarks for Ferric
//! Oxide (roadmap Phase 0, benchmarks).
//!
//! Each benchmark repeats one operation and prints one `FERRIC_BENCH perf-<name>` line per
//! measured phase with the iterations and the mean, median and 99th percentile in nanoseconds
//! ([`report`] describes the fields). The groups, in the order they run ([`bench::GROUPS`]):
//!
//! | Lines | One sample |
//! | --- | --- |
//! | `clock` | Reading the monotonic clock: the overhead in every other sample |
//! | `spawn-fork-exec-{static,dynamic}` | `fork`, `execve` of an empty program, `waitpid` |
//! | `spawn-posix-spawn-{static,dynamic}` | `posix_spawn` of the same program, `waitpid` |
//! | `spawn-ion` | `ion -c true` through `posix_spawn` |
//! | `pingpong-thread`, `pingpong-process` | A byte to a peer thread or process and back, over two pipes |
//! | `pipe-rtt-1b` | One thread writes a byte into a pipe and reads it back |
//! | `pipe-throughput` | Reading 4 MiB a writer thread sends in 64 KiB writes |
//! | `write-null`, `write-log` | One 70-byte line written to `/scheme/null`, or to `/scheme/log` (what services' standard error is) |
//! | `file-create`, `file-stat`, `file-unlink` | Creating, getting the metadata of, and removing an empty file |
//! | `mmap-map`, `mmap-touch`, `mmap-unmap` | Mapping a 32 MiB file read-only, reading every page, unmapping it |
//!
//! Every benchmark warms up first, then runs up to a fixed number of iterations or for a few
//! seconds, whichever ends first ([`measure`]); a whole run takes well under a minute under
//! KVM. Positional arguments pick benchmarks by name prefix (`ferric-perf spawn pipe`), and
//! `--list` lists them. A benchmark that fails prints `FERRIC_TEST_FAIL perf-<group>: <why>`
//! and the others still run; the run ends with `FERRIC_TEST_PASS perf`, or
//! `FERRIC_TEST_FAIL perf: ...` and a non-zero exit status.
//!
//! The benchmarks are plain POSIX and also run on Linux, given paths that exist there
//! (`--log /dev/stderr --null /dev/null --static-true /usr/bin/true ...`), for comparison. The
//! timing and statistics code ([`measure`], [`report`]) is tested on the host.

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(unix)]
mod bench;
mod measure;
mod report;
#[cfg(unix)]
#[expect(
    unsafe_code,
    reason = "fork, exec, posix_spawn and mmap; this module keeps the crate's unsafe code"
)]
mod sys;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Process, context-switch, pipe, logging and file microbenchmarks; prints FERRIC_BENCH lines
#[derive(Parser)]
#[command(version, args_override_self = true)]
struct Cli {
    /// Run only the benchmarks whose names start with one of these (default: all)
    filters: Vec<String>,
    /// List the benchmark groups and the lines each prints, then exit
    #[arg(long)]
    list: bool,
    /// Run each benchmark only a few times, to check that it works
    #[arg(long)]
    quick: bool,
    /// The statically linked program the spawn benchmarks start
    #[arg(long, default_value = "/usr/bin/ferric-perf-true")]
    static_true: PathBuf,
    /// The dynamically linked program the spawn benchmarks start
    #[arg(long, default_value = "/usr/bin/ferric-perf-true-dynamic")]
    dynamic_true: PathBuf,
    /// The shell spawn-ion runs as `ion -c true`
    #[arg(long, default_value = "/usr/bin/ion")]
    ion: PathBuf,
    /// The log write-log writes to
    #[arg(long, default_value = "/scheme/log")]
    log: PathBuf,
    /// The sink write-null writes to
    #[arg(long, default_value = "/scheme/null")]
    null: PathBuf,
    /// A scratch directory for the file and mapping benchmarks; it is emptied first and
    /// removed at the end
    #[arg(long, default_value = "/tmp/ferric-perf")]
    dir: PathBuf,
    /// The size of the file the mmap benchmark maps, in MiB
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u64).range(1..=4096))]
    mmap_mib: u64,
    /// Copy standard input to standard output a byte at a time (pingpong-process's peer)
    #[arg(long, hide = true)]
    pong: bool,
}

/// `error` and its sources, joined by `: `.
#[cfg(unix)]
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(unix)]
fn run(cli: Cli) -> ExitCode {
    if cli.pong {
        return match bench::pipe::pong() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("ferric-perf --pong: {}", chain(&error));
                ExitCode::FAILURE
            }
        };
    }
    let groups = match bench::select(&cli.filters) {
        Ok(groups) => groups,
        Err(error) => {
            eprintln!("ferric-perf: {error}");
            return ExitCode::from(2);
        }
    };
    if cli.list {
        for group in &groups {
            println!("{}: {}", group.name, group.lines.join(" "));
        }
        return ExitCode::SUCCESS;
    }
    let config = bench::Config {
        static_true: cli.static_true,
        dynamic_true: cli.dynamic_true,
        ion: cli.ion,
        log: cli.log,
        null: cli.null,
        dir: cli.dir,
        mmap_mib: cli.mmap_mib,
        quick: cli.quick,
    };
    let mut failed = 0;
    for group in &groups {
        match (group.run)(&config) {
            Ok(measurements) => {
                for measurement in &measurements {
                    println!("{}", measurement.line());
                }
            }
            Err(error) => {
                failed += 1;
                println!(
                    "FERRIC_TEST_FAIL {}{}: {}",
                    report::PREFIX,
                    group.name,
                    chain(&error)
                );
            }
        }
    }
    if failed == 0 {
        println!("FERRIC_TEST_PASS perf");
        ExitCode::SUCCESS
    } else {
        println!(
            "FERRIC_TEST_FAIL perf: {failed} of {} benchmark groups failed",
            groups.len()
        );
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    #[cfg(unix)]
    {
        run(cli)
    }
    #[cfg(not(unix))]
    {
        drop(cli);
        println!("FERRIC_TEST_SKIP perf: only runs on Unix-like systems");
        ExitCode::SUCCESS
    }
}
