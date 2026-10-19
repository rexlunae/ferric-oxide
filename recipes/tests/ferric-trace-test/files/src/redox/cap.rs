//! `trace-cap`: capability-gated access to the `kernel.trace` scheme.
//!
//! As root the test opens `cap/read` and `cap/control`, checks that an unknown access does not
//! open, and then runs itself twice as user [`USER`], each time with one capability as its
//! standard input. Each child (`ferric-trace-test cap-read` or `cap-control`):
//!
//! - fails to open the scheme's `stats` file by path (`EACCES`): it is not root;
//! - opens `stats` through the capability (`dup(cap, "stats")`) and parses it;
//! - writes a harmless command (`disable scheme.cancel`, which the earlier steps left off) to
//!   `events` opened through the capability: refused with `EACCES` for read access, done for
//!   control access;
//! - narrows the capability to `cap/read`, and with read access fails to widen it to
//!   `cap/control` (`EACCES`).
//!
//! A child exits 0 when every check holds, printing why otherwise.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};

use ferric_trace::abi::{self, Access, StatsLine};
use syscall::EACCES;

use super::{Failure, check, io_failure, path};

/// The unprivileged user the children run as.
const USER: u32 = 1000;

/// The argument a child runs with: `cap-` and the access of the capability on its stdin.
const CHILD_PREFIX: &str = "cap-";

/// A command that changes nothing at this point of the test: `scheme.cancel` is off.
const HARMLESS: &str = "disable scheme.cancel";

/// The child's standard input: the capability.
const CAP_FD: usize = 0;

/// `trace-cap`, run as root after the other steps.
pub fn check_cap() -> Result<(), Failure> {
    let step = "trace-cap";
    let unknown = path(&format!("{}/write", abi::CAP_DIR));
    match File::open(&unknown) {
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        other => {
            return Err(check(
                step,
                format!("{unknown} opened or failed oddly: {other:?}"),
            ));
        }
    }
    for access in [Access::Read, Access::Control] {
        let cap_path = path(&access.to_string());
        let cap = File::open(&cap_path).map_err(io_failure(step, &cap_path))?;
        let exe = std::env::current_exe().map_err(io_failure(step, "current_exe"))?;
        let status = Command::new(exe)
            .arg(format!("{CHILD_PREFIX}{}", access.name()))
            .stdin(Stdio::from(cap))
            .uid(USER)
            .gid(USER)
            .status()
            .map_err(io_failure(step, "spawn as user 1000"))?;
        if !status.success() {
            return Err(check(
                step,
                format!("user {USER} with {access} failed ({status})"),
            ));
        }
    }
    println!(
        "FERRIC_TEST_PASS trace-cap (user {USER}: read and control capabilities, no path access)"
    );
    Ok(())
}

/// Whether `arg` names a child run, and with which access.
pub fn child_access(arg: &str) -> Option<Access> {
    Access::from_name(arg.strip_prefix(CHILD_PREFIX)?)
}

/// A child of `trace-cap`: runs as [`USER`] with a capability of `access` as its stdin.
pub fn child(access: Access) -> ExitCode {
    match child_checks(access) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            println!("ferric-trace-test: user {USER} with {access}: {why}");
            ExitCode::FAILURE
        }
    }
}

/// Opens `file` through the capability on [`CAP_FD`].
fn through_cap(file: &str) -> io::Result<File> {
    let fd = libredox::call::dup(CAP_FD, file)?;
    let fd = i32::try_from(fd).map_err(|_| io::Error::from(ErrorKind::InvalidData))?;
    // SAFETY: `dup` just returned `fd`, a new descriptor nothing else owns.
    Ok(unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) })
}

fn child_checks(access: Access) -> Result<(), String> {
    let stats = path(abi::STATS_PATH);
    match File::open(&stats) {
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {}
        other => {
            return Err(format!(
                "opening {stats} by path gave {other:?}, not EACCES"
            ));
        }
    }
    let mut text = String::new();
    io::Read::read_to_string(
        &mut through_cap(abi::STATS_PATH).map_err(|e| format!("dup stats: {e}"))?,
        &mut text,
    )
    .map_err(|e| format!("read stats: {e}"))?;
    let cpus = text
        .lines()
        .map(StatsLine::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("stats through the capability: {e:?}"))?;
    if cpus.is_empty() {
        return Err("stats through the capability lists no CPU".into());
    }
    let mut events = through_cap(abi::EVENTS_PATH).map_err(|e| format!("dup events: {e}"))?;
    let wrote = io::Write::write_all(&mut events, HARMLESS.as_bytes());
    match (access, wrote) {
        (Access::Control, Ok(())) => {}
        (Access::Read, Err(err)) if err.raw_os_error() == Some(EACCES) => {}
        (_, other) => return Err(format!("writing {HARMLESS:?} gave {other:?}")),
    }
    through_cap(&Access::Read.to_string()).map_err(|e| format!("narrowing to read: {e}"))?;
    let widened = through_cap(&Access::Control.to_string()).map(drop);
    match (access, widened) {
        (Access::Control, Ok(())) => {}
        (Access::Read, Err(err)) if err.raw_os_error() == Some(EACCES) => {}
        (_, other) => return Err(format!("widening to control gave {other:?}")),
    }
    Ok(())
}
