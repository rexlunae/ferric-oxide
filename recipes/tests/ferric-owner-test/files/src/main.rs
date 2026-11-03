//! Checks that ramfs changes a file's owner, group and mode only under the POSIX rules
//! (#246), using `/scheme/logging`, the ramfs instance every image runs.
//!
//! As root it creates two files there: one root owns and anyone may open for writing, and
//! one user 1000 owns (group 50) with mode `6755`. It then runs itself as user 1000 (group
//! 1000), which checks, printing `FERRIC_TEST_PASS` for each step:
//!
//! - `owner-refused`: on root's file, `fchown` to its own user, `fchown` to its own group
//!   and `fchmod` are all refused (`EPERM`), and root then finds the file unchanged.
//! - `owner-allowed`: on its own file it may set the group to its own group, which clears
//!   the set-user-ID and set-group-ID bits, and change the mode, but not give the file away;
//!   root then finds the owner, group and mode the child set.
//! - `owner-write`: `write` through a descriptor it opened read-only, on its own file, is
//!   refused (`EBADF`), and through one it opened write-only, on root's writable file, it is
//!   allowed (#400); root then finds its own file empty and root's holding what was written.
//!
//! `fchown` reaches ramfs as the kernel's fchown call and `fchmod` as a `StdFsCall`, so the
//! steps cover both ways ramfs learns who the caller is. Without `/scheme/logging` it prints
//! `FERRIC_TEST_SKIP owner: no /scheme/logging`.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown, fchown};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

/// The ramfs directory the files go in.
const DIR: &str = "/scheme/logging";
/// The file root owns.
const THEIRS: &str = "/scheme/logging/ferric-owner-test.theirs";
/// The file user 1000 owns.
const MINE: &str = "/scheme/logging/ferric-owner-test.mine";

/// The argument the unprivileged child runs with.
const AS_USER: &str = "--as-user";
/// What the child writes into [`THEIRS`].
const WRITTEN: &[u8] = b"written";

/// The unprivileged user (and group) the child runs as.
const USER: u32 = 1000;
/// The group [`MINE`] starts in, which the child is not a member of.
const OTHER_GROUP: u32 = 50;

/// Why a step failed.
#[derive(Debug)]
enum Failure {
    Io {
        what: &'static str,
        source: io::Error,
    },
    Check(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { what, source } => write!(f, "{what}: {source}"),
            Self::Check(message) => f.write_str(message),
        }
    }
}

fn io(what: &'static str) -> impl FnOnce(io::Error) -> Failure {
    move |source| Failure::Io { what, source }
}

/// A file's owner, group and permission bits.
fn ownership(path: &str) -> Result<(u32, u32, u32), Failure> {
    let metadata = fs::metadata(path).map_err(io("stat"))?;
    Ok((metadata.uid(), metadata.gid(), metadata.mode() & 0o7777))
}

/// Succeeds when `result` is the `EPERM` a refused change returns. `EACCES`, which the
/// standard library also reports as `PermissionDenied`, is a failure.
fn refused(what: &str, result: io::Result<()>) -> Result<(), Failure> {
    match result {
        Err(err) if err.raw_os_error() == Some(libc::EPERM) => Ok(()),
        Err(err) => Err(Failure::Check(format!("{what}: {err}, expected EPERM"))),
        Ok(()) => Err(Failure::Check(format!("{what} was allowed"))),
    }
}

/// Succeeds when `result` is the `EBADF` a transfer through a descriptor not opened for it
/// returns.
fn bad_descriptor(what: &str, result: io::Result<usize>) -> Result<(), Failure> {
    match result {
        Err(err) if err.raw_os_error() == Some(libc::EBADF) => Ok(()),
        Err(err) => Err(Failure::Check(format!("{what}: {err}, expected EBADF"))),
        Ok(written) => Err(Failure::Check(format!(
            "{what} was allowed ({written} bytes)"
        ))),
    }
}

/// Root's part before the child runs: the two files, with their owners and modes.
fn create_files() -> Result<(), Failure> {
    File::create(THEIRS).map_err(io("create root's file"))?;
    fs::set_permissions(THEIRS, Permissions::from_mode(0o666)).map_err(io("chmod root's file"))?;
    File::create(MINE).map_err(io("create the user's file"))?;
    chown(MINE, Some(USER), Some(OTHER_GROUP)).map_err(io("give the user its file"))?;
    fs::set_permissions(MINE, Permissions::from_mode(0o6755)).map_err(io("chmod the user's file"))
}

/// The child's checks on root's file: every change refused.
fn child_theirs() -> Result<(), Failure> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(THEIRS)
        .map_err(io("open root's file"))?;
    refused("fchown to own user", fchown(&file, Some(USER), None))?;
    refused("fchown to own group", fchown(&file, None, Some(USER)))?;
    refused(
        "fchmod",
        file.set_permissions(Permissions::from_mode(0o777)),
    )
}

/// The child's checks on its own file: a regroup and a chmod allowed, giving it away not.
fn child_mine() -> Result<(), Failure> {
    let file = File::open(MINE).map_err(io("open own file"))?;
    fchown(&file, None, Some(USER)).map_err(io("regroup own file"))?;
    let after = ownership(MINE)?;
    if after != (USER, USER, 0o755) {
        return Err(Failure::Check(format!(
            "after the regroup the file is {after:?}, expected ({USER}, {USER}, 0o755)"
        )));
    }
    file.set_permissions(Permissions::from_mode(0o600))
        .map_err(io("chmod own file"))?;
    refused("fchown to root", fchown(&file, Some(0), None))
}

/// The child's `write` checks: refused through a read-only descriptor, even on its own file,
/// and allowed through a write-only one.
fn child_write() -> Result<(), Failure> {
    let mut read_only = File::open(MINE).map_err(io("open own file read-only"))?;
    bad_descriptor(
        "write through a read-only descriptor",
        read_only.write(WRITTEN),
    )?;
    OpenOptions::new()
        .write(true)
        .open(THEIRS)
        .and_then(|mut file| file.write_all(WRITTEN))
        .map_err(io("write through a write-only descriptor"))
}

/// The unprivileged child: every set of checks, reported through the exit status.
fn as_user() -> ExitCode {
    match child_theirs()
        .and_then(|()| child_mine())
        .and_then(|()| child_write())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ferric-owner-test: as user {USER}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs this program as [`USER`] and checks both files afterwards.
fn run() -> Result<(), Failure> {
    create_files()?;
    let exe = std::env::current_exe().map_err(io("current_exe"))?;
    let status = Command::new(exe)
        .arg(AS_USER)
        .uid(USER)
        .gid(USER)
        .status()
        .map_err(io("spawn as user 1000"))?;
    if !status.success() {
        return Err(Failure::Check(format!(
            "user {USER}'s checks failed ({status})"
        )));
    }

    let theirs = ownership(THEIRS)?;
    if theirs != (0, 0, 0o666) {
        return Err(Failure::Check(format!(
            "root's file is now {theirs:?}, expected (0, 0, 0o666)"
        )));
    }
    println!("FERRIC_TEST_PASS owner-refused");

    let mine = ownership(MINE)?;
    if mine != (USER, USER, 0o600) {
        return Err(Failure::Check(format!(
            "the user's file is {mine:?}, expected ({USER}, {USER}, 0o600)"
        )));
    }
    println!("FERRIC_TEST_PASS owner-allowed");

    let mine = fs::read(MINE).map_err(io("read the user's file"))?;
    if !mine.is_empty() {
        return Err(Failure::Check(format!(
            "the user's file holds {mine:?} after a refused write, expected nothing"
        )));
    }
    let theirs = fs::read(THEIRS).map_err(io("read root's file"))?;
    if theirs != WRITTEN {
        return Err(Failure::Check(format!(
            "root's file holds {theirs:?}, expected {WRITTEN:?}"
        )));
    }
    println!("FERRIC_TEST_PASS owner-write");
    Ok(())
}

/// Removes both files, whatever happened.
fn clean_up() {
    for path in [THEIRS, MINE] {
        if let Err(err) = fs::remove_file(path)
            && err.kind() != ErrorKind::NotFound
        {
            eprintln!("ferric-owner-test: remove {path}: {err}");
        }
    }
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some(AS_USER) {
        return as_user();
    }
    if fs::metadata(DIR).is_err() {
        println!("FERRIC_TEST_SKIP owner: no {DIR}");
        return ExitCode::SUCCESS;
    }
    let result = run();
    clean_up();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            println!("FERRIC_TEST_FAIL owner: {err}");
            ExitCode::FAILURE
        }
    }
}
