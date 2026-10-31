//! Running a command whose results the host reads back as data.
//!
//! `redoxer` asks for this by writing [`CAPTURE`] next to the command (see
//! [`ferric_abi::redoxerd`]). The command runs without a terminal, so its output arrives
//! unchanged and its two streams stay apart, and everything the host reads afterwards is
//! flushed to the disk before the guest stops: the page cache can hold written data in
//! memory, and the host reads the disk image, not the guest's memory.

use std::fs::{self, File};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::Context;
use ferric_abi::redoxerd::{CaptureRequest, Outcome, CAPTURE, STATUS, STDERR, STDOUT};

/// The capture request, when `redoxer` wrote one.
///
/// # Errors
/// Returns an error when [`CAPTURE`] exists but cannot be read.
pub fn request() -> anyhow::Result<Option<String>> {
    match fs::read_to_string(CAPTURE) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {CAPTURE}")),
    }
}

/// Runs `command` as `request` asks and records how it ended.
///
/// A command that runs and fails is a result, not an error: its status goes to
/// [`STATUS`] for the host to report.
///
/// # Errors
/// Returns an error when the request names no working directory, the output files cannot
/// be created, the command cannot be started, or the results cannot be written or flushed.
pub fn run(request: &str, mut command: Command) -> anyhow::Result<()> {
    let request = CaptureRequest::parse(request)
        .with_context(|| format!("{CAPTURE} names no working directory"))?;
    let results = Path::new(STATUS)
        .parent()
        .context("the status file has a parent directory")?;
    fs::create_dir_all(results)
        .with_context(|| format!("failed to create {}", results.display()))?;
    let stdout = File::create(STDOUT).with_context(|| format!("failed to create {STDOUT}"))?;
    let stderr = File::create(STDERR).with_context(|| format!("failed to create {STDERR}"))?;

    let status = command
        .current_dir(request.working_dir())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .with_context(|| format!("failed to run {command:?} in {}", request.working_dir()))?;

    fs::write(STATUS, outcome(status).to_string())
        .with_context(|| format!("failed to write {STATUS}"))?;
    for path in [STDOUT, STDERR, STATUS] {
        sync(Path::new(path))?;
    }
    for root in request.sync_roots() {
        sync(Path::new(root))?;
    }
    Ok(())
}

/// The [`Outcome`] `status` describes. A status with neither a code nor a signal is not
/// one a finished process has, and is reported as a failure with code -1.
fn outcome(status: ExitStatus) -> Outcome {
    match (status.code(), status.signal()) {
        (Some(code), _) => Outcome::Exited(code),
        (None, Some(signal)) => Outcome::Signaled(signal),
        (None, None) => Outcome::Exited(-1),
    }
}

/// Flushes the file at `path`, or every file under it, to the disk. A missing path is not
/// an error: the host may name an output the command did not produce.
///
/// # Errors
/// Returns an error when a directory cannot be listed or a file cannot be opened or synced.
fn sync(path: &Path) -> anyhow::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()))
        }
    };
    if metadata.is_dir() {
        let entries =
            fs::read_dir(path).with_context(|| format!("failed to list {}", path.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("failed to list {}", path.display()))?;
            sync(&entry.path())?;
        }
    } else if metadata.is_file() {
        File::open(path)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("failed to flush {}", path.display()))?;
    }
    Ok(())
}
