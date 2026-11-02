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

/// A benchmark's scratch directory: a new directory of its own, removed with everything in it
/// by [`Scratch::remove`], or when dropped on the way out of a benchmark that failed. It never
/// removes or reuses a directory that was there before, so `--dir` may name any directory.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    /// Whether [`Scratch::remove`] already ran.
    removed: bool,
}

impl Scratch {
    /// Makes `<dir>/ferric-perf-<pid>-<name>`, a directory this process owns (and `dir` if
    /// it does not exist).
    ///
    /// # Errors
    ///
    /// When `dir` cannot be made, or the directory cannot be made or already exists.
    pub fn new(dir: &Path, name: &str) -> Result<Self, BenchError> {
        fs::create_dir_all(dir).map_err(file_error(Op::MakeDir, dir))?;
        let path = dir.join(format!("ferric-perf-{}-{name}", std::process::id()));
        fs::create_dir(&path).map_err(file_error(Op::MakeDir, &path))?;
        Ok(Self {
            path,
            removed: false,
        })
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the directory and everything in it.
    ///
    /// # Errors
    ///
    /// When it cannot be removed.
    pub fn remove(mut self) -> Result<(), BenchError> {
        self.removed = true;
        remove_all(&self.path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only a failed benchmark gets here, and its own error is what the run reports.
        if !self.removed
            && let Err(error) = remove_all(&self.path)
        {
            eprintln!("ferric-perf: {error}");
        }
    }
}

/// Removes `path` and everything in it; a path that does not exist is already removed.
fn remove_all(path: &Path) -> Result<(), BenchError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(file_error(Op::Remove, path)(error)),
    }
}

/// The `file` group.
pub fn run(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let scratch = Scratch::new(&config.dir, "files")?;
    let dir = scratch.path();
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
    scratch.remove()?;
    Ok(vec![
        Measurement {
            name: "file-create",
            latency: create,
            bytes: None,
            extra: Vec::new(),
        },
        Measurement {
            name: "file-stat",
            latency: stat,
            bytes: None,
            extra: Vec::new(),
        },
        Measurement {
            name: "file-unlink",
            latency: unlink,
            bytes: None,
            extra: Vec::new(),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ferric-perf-{name}-{}", std::process::id()))
    }

    /// What was in `--dir` before stays, including directories named like the benchmarks'.
    #[test]
    fn a_scratch_directory_is_new_and_leaves_the_rest_of_dir_alone() {
        let dir = root("scratch");
        fs::create_dir_all(dir.join("files")).expect("mkdir");
        fs::write(dir.join("files").join("keep"), b"x").expect("write");
        fs::write(dir.join("keep"), b"x").expect("write");
        let scratch = Scratch::new(&dir, "files").expect("made");
        let path = scratch.path().to_owned();
        assert!(path.starts_with(&dir) && path != dir.join("files"));
        assert_eq!(fs::read_dir(&path).expect("exists").count(), 0);
        fs::write(path.join("f"), b"x").expect("write");
        scratch.remove().expect("removed");
        assert!(!path.exists());
        assert!(dir.join("files").join("keep").exists() && dir.join("keep").exists());
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// An existing directory of the same name is refused, not reused or emptied.
    #[test]
    fn an_existing_scratch_directory_is_refused() {
        let dir = root("taken");
        let taken = dir.join(format!("ferric-perf-{}-files", std::process::id()));
        fs::create_dir_all(&taken).expect("mkdir");
        fs::write(taken.join("keep"), b"x").expect("write");
        assert!(Scratch::new(&dir, "files").is_err());
        assert!(taken.join("keep").exists());
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_failed_benchmark_leaves_no_scratch_directory() {
        let dir = root("dropped");
        let scratch = Scratch::new(&dir, "files").expect("made");
        let path = scratch.path().to_owned();
        fs::write(path.join("f"), b"x").expect("write");
        drop(scratch);
        assert!(!path.exists());
        fs::remove_dir(&dir).expect("cleanup");
    }
}
