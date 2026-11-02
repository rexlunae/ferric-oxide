//! File metadata operations in a scratch directory: create, stat and unlink.
//!
//! - `file-create`: creates a new empty file (`O_CREAT | O_EXCL`) and closes it, one per
//!   sample, each with a new name.
//! - `file-stat`: gets the metadata of those files by path, in turn.
//! - `file-unlink`: removes every one of them, the warm-up's too; this phase has no warm-up
//!   and no budget, so it always removes them all.
//!
//! On Redox each operation is one or more requests to the file system's scheme daemon
//! (RedoxFS on the root file system), so they measure path lookup and the metadata path of
//! the file system, not the disk: nothing is synced.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{BenchError, Config, Measurement, Op, file_error};
use crate::measure::{Monotonic, Plan, measure};

/// The plan for creating files.
const CREATE_PLAN: Plan = Plan {
    warmup: 10,
    iters: 500,
    min_iters: 20,
    budget: Duration::from_secs(2),
};

/// The plan for getting their metadata.
const STAT_PLAN: Plan = Plan {
    warmup: 10,
    iters: 2000,
    min_iters: 20,
    budget: Duration::from_secs(1),
};

/// Removes `dir` if it exists and creates it empty.
pub fn scratch(dir: &Path) -> Result<(), BenchError> {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(file_error(Op::Remove, dir)(error)),
    }
    fs::create_dir_all(dir).map_err(file_error(Op::MakeDir, dir))
}

/// The `file` group.
pub fn run(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let dir = config.dir.join("files");
    scratch(&dir)?;
    let mut clock = Monotonic::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    let [create] = measure(&config.plan(CREATE_PLAN), &mut clock, |laps| {
        let path = dir.join(format!("f{:06}", paths.len()));
        laps.restart();
        // The file is closed at the end of the statement, inside the sample.
        File::create_new(&path).map_err(file_error(Op::Create, &path))?;
        laps.lap();
        paths.push(path);
        Ok::<_, BenchError>(())
    })?
    .phases;
    let mut next = paths.iter().cycle();
    let [stat] = measure(&config.plan(STAT_PLAN), &mut clock, |laps| {
        if let Some(path) = next.next() {
            fs::metadata(path).map_err(file_error(Op::Stat, path))?;
        }
        laps.lap();
        Ok::<_, BenchError>(())
    })?
    .phases;
    let count = u32::try_from(paths.len()).unwrap_or(u32::MAX);
    let all = Plan {
        warmup: 0,
        iters: count,
        min_iters: count,
        budget: Duration::MAX,
    };
    let mut next = paths.iter();
    let [unlink] = measure(&all, &mut clock, |laps| {
        if let Some(path) = next.next() {
            fs::remove_file(path).map_err(file_error(Op::Remove, path))?;
        }
        laps.lap();
        Ok::<_, BenchError>(())
    })?
    .phases;
    fs::remove_dir(&dir).map_err(file_error(Op::Remove, &dir))?;
    Ok(vec![
        Measurement {
            name: "file-create",
            latency: create,
            bytes: None,
        },
        Measurement {
            name: "file-stat",
            latency: stat,
            bytes: None,
        },
        Measurement {
            name: "file-unlink",
            latency: unlink,
            bytes: None,
        },
    ])
}
