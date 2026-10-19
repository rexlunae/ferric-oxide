//! Checks `/scheme/sys/proc`, the kernel's per-process introspection file (`kernel.sys_proc`).
//!
//! Steps, each printing `FERRIC_TEST_PASS proc-<step>`:
//!
//! - `proc-parse`: every line of the file parses with `ferric_procinfo`, and it lists more
//!   than one context, init (pid 1) among them.
//! - `proc-self`: this process's own block: its name, `runnable` and running, some CPU time,
//!   the `sys` file it holds open, and a grant of at least the [`ALLOC_BYTES`] it allocated,
//!   counted in its private memory.
//! - `proc-root-only`: the same program run as user 1000 cannot open the file (`EPERM`).
//!
//! Without the file (a kernel built without `sys_proc`) it prints
//! `FERRIC_TEST_SKIP proc: no /scheme/sys/proc`.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fmt;
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

use ferric_procinfo::{FileError, Process, Provider, SchemeRef, Status};

/// The file.
const PATH: &str = "/scheme/sys/proc";

/// The allocation `proc-self` looks for: large enough that the allocator maps it on its own.
const ALLOC_BYTES: usize = 8 << 20;

/// The argument the `proc-root-only` child runs with.
const AS_USER: &str = "--as-user";

/// The unprivileged user the child runs as.
const USER: u32 = 1000;

/// Why a step failed.
#[derive(Debug)]
enum Failure {
    Io {
        what: &'static str,
        source: io::Error,
    },
    Parse(FileError),
    Check(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Io { what, source } => write!(f, "{what}: {source}"),
            Failure::Parse(err) => write!(f, "{PATH}: {err}"),
            Failure::Check(why) => f.write_str(why),
        }
    }
}

fn check(why: impl Into<String>) -> Failure {
    Failure::Check(why.into())
}

/// `proc-parse` and `proc-self`.
fn check_self(text: &str, held: &[u8]) -> Result<(), Failure> {
    let all = ferric_procinfo::processes(text).map_err(Failure::Parse)?;
    if all.len() < 2 || !all.iter().any(|p| p.context.pid == 1) {
        return Err(check(format!(
            "{} contexts, init not among them",
            all.len()
        )));
    }
    println!("FERRIC_TEST_PASS proc-parse ({} contexts)", all.len());

    let pid = u64::from(std::process::id());
    let me: Vec<&Process<'_>> = all.iter().filter(|p| p.context.pid == pid).collect();
    let Some(me) = me.iter().find(|p| p.context.running) else {
        return Err(check(format!("no running context of pid {pid}")));
    };
    let context = &me.context;
    if !context.name.0.contains("ferric-proc-test") {
        return Err(check(format!("own context is named {:?}", context.name.0)));
    }
    if context.status != Status::Runnable || context.active_ns == 0 {
        return Err(check(format!(
            "own context is {:?} with {} ns of CPU time",
            context.status, context.active_ns
        )));
    }
    if !me
        .fds
        .iter()
        .any(|fd| fd.scheme == SchemeRef::Kernel("sys"))
    {
        return Err(check("the open sys file is not among the descriptors"));
    }
    let pages = (ALLOC_BYTES / 4096) as u64;
    let Some(grant) = me
        .grants
        .iter()
        .find(|g| g.provider == Provider::Allocated && g.pages >= pages && g.prot.write)
    else {
        return Err(check(format!(
            "no writable allocated grant of {pages} pages"
        )));
    };
    let private = me.memory.map_or(0, |m| m.private);
    if private < ALLOC_BYTES as u64 {
        return Err(check(format!(
            "private memory {private} below the allocation"
        )));
    }
    // Keep the allocation alive until here.
    std::hint::black_box(held);
    println!(
        "FERRIC_TEST_PASS proc-self (context {}, {} fds, {} grants, {} pages at {:#x}, {} ns)",
        context.id,
        me.fds.len(),
        me.grants.len(),
        grant.pages,
        grant.base,
        context.active_ns
    );
    Ok(())
}

/// `proc-root-only`: runs this program as [`USER`], which must be refused the file.
fn check_root_only() -> Result<(), Failure> {
    let exe = std::env::current_exe().map_err(|source| Failure::Io {
        what: "current_exe",
        source,
    })?;
    let status = Command::new(exe)
        .arg(AS_USER)
        .uid(USER)
        .gid(USER)
        .status()
        .map_err(|source| Failure::Io {
            what: "spawn as user 1000",
            source,
        })?;
    if !status.success() {
        return Err(check(format!(
            "user {USER} was not refused {PATH} ({status})"
        )));
    }
    println!("FERRIC_TEST_PASS proc-root-only");
    Ok(())
}

/// The child of `proc-root-only`: succeeds only if opening the file is refused.
fn as_user() -> ExitCode {
    match File::open(PATH) {
        Err(err) if err.kind() == ErrorKind::PermissionDenied => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ferric-proc-test: as user {USER}: {PATH}: {err}");
            ExitCode::FAILURE
        }
        Ok(_) => {
            eprintln!("ferric-proc-test: as user {USER}: {PATH} opened");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Failure> {
    // Held open and allocated before the read, so the file must list them.
    let _sys = File::open("/scheme/sys/uname").map_err(|source| Failure::Io {
        what: "/scheme/sys/uname",
        source,
    })?;
    let held = vec![0x5au8; ALLOC_BYTES];
    let text = fs::read_to_string(PATH).map_err(|source| Failure::Io { what: PATH, source })?;
    check_self(&text, &held)?;
    check_root_only()
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some(AS_USER) {
        return as_user();
    }
    match fs::metadata(PATH) {
        Err(err) if err.kind() == ErrorKind::NotFound => {
            println!("FERRIC_TEST_SKIP proc: no {PATH}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            println!("FERRIC_TEST_FAIL proc: {err}");
            ExitCode::FAILURE
        }
    }
}
