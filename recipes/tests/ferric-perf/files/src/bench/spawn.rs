//! Process start: from the call that creates a process until `waitpid` returns its exit.
//!
//! Each sample starts a program and waits for it to exit, so it holds the whole life of a
//! process that does nothing: creating it, loading its image (and, for a dynamically linked
//! program, `ld.so` loading `libc.so`), running its start-up code, exiting, and the parent
//! being woken.
//!
//! - `fork_exec`: `fork`, then `execve` in the child. On Redox `fork` is relibc's: it asks
//!   `procmgr` for a child and copies the address space, and `execve` reads the program's
//!   segments into the child.
//! - `posix_spawn`: relibc builds the child directly, without copying the parent first.
//! - `ion`: `ion -c true` through `posix_spawn`, a shell started to run one builtin, the cost
//!   every shell script line that runs a program pays at least once.

use std::fs;
use std::path::Path;
use std::time::Duration;

use super::{BenchError, Config, Measurement, Op, file_error};
use crate::measure::{Monotonic, Plan, measure};
use crate::sys::Program;

/// The plan for the small programs.
const PLAN: Plan = Plan {
    warmup: 5,
    iters: 200,
    min_iters: 10,
    budget: Duration::from_secs(2),
};

/// The plan for the shell, a larger program.
const ION_PLAN: Plan = Plan {
    warmup: 3,
    iters: 100,
    min_iters: 10,
    budget: Duration::from_secs(2),
};

/// How a process is started.
#[derive(Clone, Copy)]
enum Start {
    ForkExec,
    PosixSpawn,
}

/// `spawn-fork-exec-static`.
pub fn fork_exec_static(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    spawn(
        config,
        "spawn-fork-exec-static",
        Start::ForkExec,
        &config.static_true,
    )
}

/// `spawn-fork-exec-dynamic`.
pub fn fork_exec_dynamic(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    spawn(
        config,
        "spawn-fork-exec-dynamic",
        Start::ForkExec,
        &config.dynamic_true,
    )
}

/// `spawn-posix-spawn-static`.
pub fn posix_spawn_static(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    spawn(
        config,
        "spawn-posix-spawn-static",
        Start::PosixSpawn,
        &config.static_true,
    )
}

/// `spawn-posix-spawn-dynamic`.
pub fn posix_spawn_dynamic(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    spawn(
        config,
        "spawn-posix-spawn-dynamic",
        Start::PosixSpawn,
        &config.dynamic_true,
    )
}

/// `spawn-ion`: `ion -c true`.
pub fn ion(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let ion = &config.ion;
    run(
        config,
        "spawn-ion",
        Start::PosixSpawn,
        ion,
        &["-c", "true"],
        ION_PLAN,
    )
}

/// Starts the empty program at `path` the way `start` says, as [`PLAN`] says.
fn spawn(
    config: &Config,
    name: &'static str,
    start: Start,
    path: &Path,
) -> Result<Vec<Measurement>, BenchError> {
    run(config, name, start, path, &[], PLAN)
}

/// Starts `path` with `args` the way `start` says, as `plan` says.
fn run(
    config: &Config,
    name: &'static str,
    start: Start,
    path: &Path,
    args: &[&str],
    plan: Plan,
) -> Result<Vec<Measurement>, BenchError> {
    // A missing program would otherwise show up as exit status 127 from `fork_exec`.
    fs::metadata(path).map_err(file_error(Op::Stat, path))?;
    let program = Program::new(path, args).map_err(|source| BenchError::Nul {
        path: path.to_owned(),
        source,
    })?;
    let [latency] = measure(&config.plan(plan), &mut Monotonic::new(), |laps| {
        let status = match start {
            Start::ForkExec => program.fork_exec(),
            Start::PosixSpawn => program.spawn(),
        }
        .map_err(file_error(Op::Start, path))?;
        laps.lap();
        if status.success() {
            Ok(())
        } else {
            Err(BenchError::Exit {
                path: path.to_owned(),
                status,
            })
        }
    })?
    .phases;
    Ok(vec![Measurement {
        name,
        latency,
        bytes: None,
        extra: Vec::new(),
    }])
}
