//! Checks that ramfs changes a file's owner, group and mode (#246), and its times and size
//! (#375), only under the POSIX rules, using `/scheme/logging`, the ramfs instance every
//! image runs.
//!
//! As root it creates three files there: one root owns and anyone may open for writing, one
//! root owns that only root may write (holding a few bytes), and one user 1000 owns (group
//! 50) with mode `6755`. It then runs itself as user 1000 (group 1000), which checks,
//! printing `FERRIC_TEST_PASS` for each step:
//!
//! - `owner-refused`: on root's file, `fchown` to its own user, `fchown` to its own group
//!   and `fchmod` are all refused (`EPERM`), and root then finds the file unchanged.
//! - `owner-allowed`: on its own file it may set the group to its own group, which clears
//!   the set-user-ID and set-group-ID bits, and change the mode, but not give the file away;
//!   root then finds the owner, group and mode the child set.
//! - `owner-times-refused`: on root's files, `futimens` with explicit times is refused
//!   (`EPERM`) and, on the one it may not write, so is setting both to the current time
//!   (`EACCES`); root then finds neither file carrying the explicit times.
//! - `owner-times-allowed`: on root's writable file it may set both times to the current
//!   time, and on its own file it may set the access time while `UTIME_OMIT` keeps the
//!   modification time; root then finds exactly those times, the current ones within the
//!   clock readings it took before and after the child.
//! - `owner-truncate`: `ftruncate` through a descriptor opened read-only is refused (`EBADF`
//!   or `EINVAL`), even on its own file, and allowed through one opened for writing; root
//!   then finds the sizes it expects.
//!
//! `fchown` reaches ramfs as the kernel's fchown call, and `fchmod`, `futimens` and
//! `ftruncate` as a `StdFsCall`, so the steps cover both ways ramfs learns who the caller
//! is. Without `/scheme/logging` it prints `FERRIC_TEST_SKIP owner: no /scheme/logging`.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::Write;
use std::io::{self, ErrorKind};
use std::ops::RangeInclusive;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown, fchown};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use std::ptr;
use std::time::SystemTime;

use ferric_abi::time::UTIME_OMIT;

/// The ramfs directory the files go in.
const DIR: &str = "/scheme/logging";
/// The file root owns.
const THEIRS: &str = "/scheme/logging/ferric-owner-test.theirs";
/// The file root owns and only root may write.
const LOCKED: &str = "/scheme/logging/ferric-owner-test.locked";
/// The file user 1000 owns.
const MINE: &str = "/scheme/logging/ferric-owner-test.mine";
/// What [`LOCKED`] holds, which a refused `ftruncate` must leave.
const LOCKED_CONTENTS: &[u8] = b"locked";
/// The length the child truncates root's writable file to.
const THEIRS_LENGTH: u64 = 3;

/// The explicit time (seconds, nanoseconds) the child asks for.
const EXPLICIT: (i64, i64) = (1_000_000, 500);
/// The times root gives its files first, so the child's changes show.
const OLD: (i64, i64) = (86_400, 0);

/// The argument the unprivileged child runs with.
const AS_USER: &str = "--as-user";
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

/// Succeeds when `result` is the error `ftruncate` returns for a descriptor not open for
/// writing: POSIX allows `EBADF` or `EINVAL`.
fn not_writable(what: &str, result: io::Result<()>) -> Result<(), Failure> {
    match result {
        Err(err) if matches!(err.raw_os_error(), Some(libc::EBADF | libc::EINVAL)) => Ok(()),
        Err(err) => Err(Failure::Check(format!(
            "{what}: {err}, expected EBADF or EINVAL"
        ))),
        Ok(()) => Err(Failure::Check(format!("{what} was allowed"))),
    }
}

/// A `timespec` of `seconds` and `nanoseconds`, which may be a `UTIME_*` sentinel.
fn timespec((seconds, nanoseconds): (i64, i64)) -> libc::timespec {
    libc::timespec {
        tv_sec: seconds,
        tv_nsec: nanoseconds,
    }
}

/// `futimens` on `file` with the access and modification `times`, or with a null pointer
/// (both set to the current time) for `None`.
///
/// The `libc` crate's `UTIME_NOW` and `UTIME_OMIT` for Redox are not relibc's, so the
/// sentinels come from `ferric_abi::time`, and `std`'s `File::set_times` is unsupported on
/// Redox, so this calls relibc directly.
fn futimens(file: &File, times: Option<[(i64, i64); 2]>) -> io::Result<()> {
    let times = times.map(|times| times.map(timespec));
    let times_ptr = times.as_ref().map_or(ptr::null(), |times| times.as_ptr());
    // SAFETY: the descriptor is open for the call, and `times_ptr` is null or points at two
    // `timespec`s in `times`, which outlives the call.
    let status = unsafe { libc::futimens(file.as_raw_fd(), times_ptr) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The current realtime clock, as (seconds, nanoseconds), the clock ramfs reads for
/// `UTIME_NOW`.
fn now() -> Result<(i64, i64), Failure> {
    let since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|err| Failure::Check(format!("the clock is before the epoch: {err}")))?;
    let seconds = i64::try_from(since_epoch.as_secs())
        .map_err(|err| Failure::Check(format!("the clock is out of range: {err}")))?;
    Ok((seconds, i64::from(since_epoch.subsec_nanos())))
}

/// A file's access and modification times, as (seconds, nanoseconds).
fn times(path: &str) -> Result<[(i64, i64); 2], Failure> {
    let metadata = fs::metadata(path).map_err(io("stat"))?;
    Ok([
        (metadata.atime(), metadata.atime_nsec()),
        (metadata.mtime(), metadata.mtime_nsec()),
    ])
}

/// Root's part before the child runs: the three files, with their owners and modes.
fn create_files() -> Result<(), Failure> {
    File::create(THEIRS).map_err(io("create root's file"))?;
    fs::set_permissions(THEIRS, Permissions::from_mode(0o666)).map_err(io("chmod root's file"))?;
    File::create(LOCKED)
        .and_then(|mut file| file.write_all(LOCKED_CONTENTS))
        .map_err(io("create root's locked file"))?;
    fs::set_permissions(LOCKED, Permissions::from_mode(0o644))
        .map_err(io("chmod root's locked file"))?;
    for (path, what) in [
        (THEIRS, "date root's file"),
        (LOCKED, "date root's locked file"),
    ] {
        let file = File::open(path).map_err(io("open root's file"))?;
        futimens(&file, Some([OLD, OLD])).map_err(io(what))?;
    }
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

/// The child's `futimens` and `ftruncate` checks.
fn child_times() -> Result<(), Failure> {
    let explicit = Some([EXPLICIT, EXPLICIT]);
    let theirs = OpenOptions::new()
        .read(true)
        .write(true)
        .open(THEIRS)
        .map_err(io("open root's file"))?;
    refused("futimens root's file", futimens(&theirs, explicit))?;
    futimens(&theirs, None).map_err(io("touch root's writable file"))?;
    theirs
        .set_len(THEIRS_LENGTH)
        .map_err(io("ftruncate root's file opened for writing"))?;

    let locked = File::open(LOCKED).map_err(io("open root's locked file"))?;
    refused("futimens root's locked file", futimens(&locked, explicit))?;
    match futimens(&locked, None) {
        Err(err) if err.raw_os_error() == Some(libc::EACCES) => {}
        Err(err) => {
            return Err(Failure::Check(format!(
                "touch root's locked file: {err}, expected EACCES"
            )));
        }
        Ok(()) => {
            return Err(Failure::Check(
                "touch root's locked file was allowed".into(),
            ));
        }
    }
    not_writable("ftruncate root's locked file", locked.set_len(0))?;

    let mine = File::open(MINE).map_err(io("open own file"))?;
    futimens(&mine, Some([EXPLICIT, (0, UTIME_OMIT)])).map_err(io("set own access time"))?;
    not_writable("ftruncate own file opened read-only", mine.set_len(1))
}

/// The unprivileged child: all sets of checks, reported through the exit status.
fn as_user() -> ExitCode {
    match child_theirs()
        .and_then(|()| child_mine())
        .and_then(|()| child_times())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ferric-owner-test: as user {USER}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs this program as [`USER`] and checks the files afterwards.
fn run() -> Result<(), Failure> {
    create_files()?;
    let [_, mine_modified] = times(MINE)?;
    let exe = std::env::current_exe().map_err(io("current_exe"))?;
    let before = now()?;
    let status = Command::new(exe)
        .arg(AS_USER)
        .uid(USER)
        .gid(USER)
        .status()
        .map_err(io("spawn as user 1000"))?;
    let after = now()?;
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

    check_times(mine_modified, before..=after)?;

    check_sizes()?;
    println!("FERRIC_TEST_PASS owner-truncate");
    Ok(())
}

/// Root's view of the times after the child: its locked file untouched, its writable file
/// touched (`UTIME_NOW` applied as a time within `child_ran`, the clock readings around
/// the child, not stored), and the user's file carrying the explicit access time with
/// [`UTIME_OMIT`] honoured.
fn check_times(
    mine_modified: (i64, i64),
    child_ran: RangeInclusive<(i64, i64)>,
) -> Result<(), Failure> {
    let locked = times(LOCKED)?;
    if locked != [OLD, OLD] {
        return Err(Failure::Check(format!(
            "root's locked file has times {locked:?}, expected [{OLD:?}, {OLD:?}]"
        )));
    }
    for time in times(THEIRS)? {
        if !child_ran.contains(&time) {
            return Err(Failure::Check(format!(
                "root's touched file has time {time:?}, expected one in {child_ran:?}"
            )));
        }
    }
    println!("FERRIC_TEST_PASS owner-times-refused");

    let mine = times(MINE)?;
    if mine != [EXPLICIT, mine_modified] {
        return Err(Failure::Check(format!(
            "the user's file has times {mine:?}, expected [{EXPLICIT:?}, {mine_modified:?}]"
        )));
    }
    println!("FERRIC_TEST_PASS owner-times-allowed");
    Ok(())
}

/// Root's view of the sizes after the child: only the descriptor opened for writing
/// truncated.
fn check_sizes() -> Result<(), Failure> {
    for (path, expected) in [
        (THEIRS, THEIRS_LENGTH),
        (LOCKED, LOCKED_CONTENTS.len() as u64),
        (MINE, 0),
    ] {
        let size = fs::metadata(path).map_err(io("stat"))?.len();
        if size != expected {
            return Err(Failure::Check(format!(
                "{path} is {size} bytes, expected {expected}"
            )));
        }
    }
    Ok(())
}

/// Removes the files, whatever happened.
fn clean_up() {
    for path in [THEIRS, LOCKED, MINE] {
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
