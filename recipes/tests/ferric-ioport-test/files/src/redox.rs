//! On-target privilege checks. Only the thread-handle and effective-UID FFI
//! calls need unsafe code; the test never accesses a hardware port.

use std::error::Error;
use std::fmt;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, ExitStatus};

use libredox::Fd;
use syscall::{CallFlags, ProcSchemeVerb};

const USER: u32 = 1000;

#[derive(Debug)]
enum Failure {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    Call {
        operation: &'static str,
        source: libredox::error::Error,
    },
    Identity {
        real: usize,
        effective: usize,
    },
    UnexpectedAccess {
        effective: usize,
        result: Result<usize, libredox::error::Error>,
    },
    Child {
        mode: &'static str,
        status: ExitStatus,
    },
    Spawn {
        executable: PathBuf,
        mode: &'static str,
        source: io::Error,
    },
    Argument,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, .. } | Self::Call { operation, .. } => f.write_str(operation),
            Self::Identity { real, effective } => {
                write!(f, "unexpected IDs: real {real}, effective {effective}")
            }
            Self::UnexpectedAccess { effective, result } => write!(
                f,
                "port-I/O request as effective UID {effective}: {result:?}"
            ),
            Self::Child { mode, status } => write!(f, "{mode} child exited with {status}"),
            Self::Spawn {
                executable, mode, ..
            } => write!(f, "spawn {} {mode}", executable.display()),
            Self::Argument => f.write_str("unknown child mode"),
        }
    }
}

impl Error for Failure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Spawn { source, .. } => Some(source),
            Self::Call { source, .. } => Some(source),
            Self::UnexpectedAccess {
                result: Err(source),
                ..
            } => Some(source),
            _ => None,
        }
    }
}

fn call_error(operation: &'static str) -> impl FnOnce(libredox::error::Error) -> Failure {
    move |source| Failure::Call { operation, source }
}

fn io_error(operation: &'static str) -> impl FnOnce(io::Error) -> Failure {
    move |source| Failure::Io { operation, source }
}

fn io_handle() -> Result<Fd, Failure> {
    unsafe extern "C" {
        fn redox_cur_thrfd_v0() -> usize;
    }
    // SAFETY: relibc supplies this calling thread's live handle. It stays borrowed;
    // only the duplicate below is owned and closed by Fd.
    let thread = unsafe { redox_cur_thrfd_v0() };
    let duplicate = libredox::call::dup(thread, b"open_via_dup")
        .map_err(call_error("duplicate the current thread handle"))?;
    Ok(Fd::new(duplicate))
}

fn check_identity(expected_real: usize, expected_effective: usize) -> Result<(), Failure> {
    let real = libredox::call::getruid().map_err(call_error("read real UID"))?;
    let effective = libredox::call::geteuid().map_err(call_error("read effective UID"))?;
    if (real, effective) != (expected_real, expected_effective) {
        return Err(Failure::Identity { real, effective });
    }
    Ok(())
}

fn child(mode: &str) -> Result<(), Failure> {
    let effective = match mode {
        "--root" => {
            check_identity(0, 0)?;
            0
        }
        "--user" => {
            check_identity(USER as usize, USER as usize)?;
            USER as usize
        }
        "--drop" => {
            check_identity(0, 0)?;
            USER as usize
        }
        _ => return Err(Failure::Argument),
    };
    let handle = io_handle()?;
    if mode == "--drop" {
        // SAFETY: this child owns its credentials and is single-threaded. It
        // drops authority and exits after the request, without restoring root.
        if unsafe { libc::seteuid(USER) } != 0 {
            return Err(io_error("drop effective UID")(io::Error::last_os_error()));
        }
        check_identity(0, USER as usize)?;
    }
    let result = handle.call_wo(&[], CallFlags::empty(), &[ProcSchemeVerb::Iopl as u64]);
    match (effective, &result) {
        (0, Ok(0)) => Ok(()),
        (_, Err(error)) if effective != 0 && error.errno() == syscall::EPERM => Ok(()),
        _ => Err(Failure::UnexpectedAccess { effective, result }),
    }
}

fn run() -> Result<(), Failure> {
    check_identity(0, 0)?;
    let executable = std::env::current_exe().map_err(io_error("find this executable"))?;
    for (mode, uid, marker) in [
        ("--root", 0, "ioport-root"),
        ("--user", USER, "ioport-user"),
        ("--drop", 0, "ioport-drop"),
    ] {
        let status = Command::new(&executable)
            .arg(mode)
            .uid(uid)
            .gid(uid)
            .status()
            .map_err(|source| Failure::Spawn {
                executable: executable.clone(),
                mode,
                source,
            })?;
        if !status.success() {
            return Err(Failure::Child { mode, status });
        }
        println!("FERRIC_TEST_PASS {marker}");
    }
    Ok(())
}

pub(super) fn main() -> ExitCode {
    let result = match std::env::args().nth(1) {
        Some(mode) => child(&mode),
        None => run(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            println!("FERRIC_TEST_FAIL ioport: {error}");
            let mut source = error.source();
            while let Some(error) = source {
                println!("  caused by: {error}");
                source = error.source();
            }
            ExitCode::FAILURE
        }
    }
}
