//! Checks `/scheme/sys/proc`, the kernel's per-process introspection file (`kernel.sys_proc`).
//!
//! Steps, each printing `FERRIC_TEST_PASS proc-<step>`:
//!
//! - `proc-parse`: every line of the file parses with `ferric_procinfo`, and it lists more
//!   than one context, init (pid 1) among them.
//! - `proc-self`: this process's own block: its name, `runnable` and running, some CPU time,
//!   the `sys` file it holds open, and a grant of at least the [`ALLOC_BYTES`] it allocated,
//!   counted in its private memory.
//! - `proc-slice`: two reads in a row show this running context's CPU time growing, so the
//!   current timeslice is counted and not only the ones that ended.
//! - `proc-guard`: with a thread parked, the grant right below the one holding its stack is
//!   its guard (`PROT_NONE`), shown with no permissions (`---`), as `kernel.prot_none` (on by
//!   default) maps it.
//! - `proc-cap`: the scheme creation capability, which the init namespace manager holds for
//!   its whole life, is listed as `cap`, and no scheme manager descriptor naming its entry
//!   (number 0) is shown as `mgr`.
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

use ferric_procinfo::{FileError, GrantLine, Process, Provider, SchemeRef, Status};

/// The file.
const PATH: &str = "/scheme/sys/proc";

/// The page size the grant lines count in.
const PAGE_BYTES: u64 = 4096;

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
    let pages = ALLOC_BYTES as u64 / PAGE_BYTES;
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

/// This process's running context in `all`.
fn running<'a, 'b>(all: &'b [Process<'a>]) -> Result<&'b Process<'a>, Failure> {
    let pid = u64::from(std::process::id());
    all.iter()
        .find(|p| p.context.pid == pid && p.context.running)
        .ok_or_else(|| check(format!("no running context of pid {pid}")))
}

fn read() -> Result<String, Failure> {
    fs::read_to_string(PATH).map_err(|source| Failure::Io { what: PATH, source })
}

/// `proc-slice`: two reads in a row, with no switch likely between them.
fn check_slice() -> Result<(), Failure> {
    let (first, second) = (read()?, read()?);
    let first = ferric_procinfo::processes(&first).map_err(Failure::Parse)?;
    let second = ferric_procinfo::processes(&second).map_err(Failure::Parse)?;
    let (a, b) = (
        running(&first)?.context.active_ns,
        running(&second)?.context.active_ns,
    );
    if b <= a {
        return Err(check(format!(
            "running context's CPU time went from {a} to {b} ns"
        )));
    }
    println!("FERRIC_TEST_PASS proc-slice ({a} then {b} ns)");
    Ok(())
}

/// `proc-guard` and `proc-cap`.
fn check_guard_and_cap() -> Result<(), Failure> {
    let (parked, done) = std::sync::mpsc::channel::<()>();
    let (sent, stack) = std::sync::mpsc::channel::<u64>();
    let thread = std::thread::spawn(move || {
        let local = 0u8;
        // An address on this thread's stack, which relibc mapped above a PROT_NONE guard.
        let _ = sent.send(std::ptr::from_ref(&local).addr() as u64);
        let _ = done.recv();
        std::hint::black_box(&local);
    });
    let stack = stack
        .recv()
        .map_err(|_| check("the parked thread did not report its stack"));
    let text = stack.and_then(|stack| Ok((stack, read()?)));
    drop(parked);
    let _ = thread.join();
    let (stack, text) = text?;
    let all = ferric_procinfo::processes(&text).map_err(Failure::Parse)?;
    let me = running(&all)?;
    let end = |g: &GrantLine| g.base.saturating_add(g.pages.saturating_mul(PAGE_BYTES));
    let Some(stack_grant) = me.grants.iter().find(|g| (g.base..end(g)).contains(&stack)) else {
        return Err(check(format!(
            "no grant holds the thread stack at {stack:#x}"
        )));
    };
    let Some(guard) = me.grants.iter().find(|g| end(g) == stack_grant.base) else {
        return Err(check(format!(
            "no grant right below the thread stack grant at {:#x}",
            stack_grant.base
        )));
    };
    if guard.prot.read || guard.prot.write || guard.prot.execute {
        return Err(check(format!(
            "the guard at {:#x} below the thread stack is {}",
            guard.base, guard.prot
        )));
    }
    println!(
        "FERRIC_TEST_PASS proc-guard ({} pages at {:#x}, below the stack at {:#x})",
        guard.pages, guard.base, stack_grant.base
    );
    let wrong = all
        .iter()
        .flat_map(|p| p.fds.iter().map(move |fd| (p.context.pid, fd)))
        .find(|(_, fd)| fd.scheme == SchemeRef::Manager && fd.number == 0);
    if let Some((pid, fd)) = wrong {
        return Err(check(format!(
            "pid {pid} fd {} names the scheme creation capability as mgr",
            fd.fd
        )));
    }
    let caps = all
        .iter()
        .flat_map(|p| p.fds.iter())
        .filter(|fd| fd.scheme == SchemeRef::Capability)
        .count();
    if caps == 0 {
        return Err(check(
            "no cap descriptor, though the init namespace manager holds one",
        ));
    }
    println!("FERRIC_TEST_PASS proc-cap ({caps} cap descriptors)");
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
    let text = read()?;
    check_self(&text, &held)?;
    check_slice()?;
    check_guard_and_cap()?;
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
