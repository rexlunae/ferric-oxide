//! Thread stack guard page test for Ferric Oxide.
//!
//! relibc maps a `PROT_NONE` guard page (the default `guardsize`) directly
//! below every thread stack it allocates, so a stack overflow faults instead
//! of writing into whatever is mapped below. This test starts a thread, asks
//! `pthread_getattr_np` where its stack is, and in a child process reads:
//!
//! - `--probe guard`: the page below the stack, which must die with SIGSEGV;
//! - `--probe stack`: the lowest page of the stack, which must exit normally.
//!   That shows the address is right and the guard is what faults.
//! - `--probe main-guard`: the page below the main thread's stack, which exec
//!   maps `PROT_NONE`, and which must also die with SIGSEGV. relibc only knows
//!   where the main stack is when exec reports it (the ASLR build), so this
//!   part is skipped when the main stack's base is unknown.
//!
//! The layout checked is relibc's; glibc reports thread stacks differently, so
//! the test only means something on Ferric Oxide.
//!
//! Prints one result line, `FERRIC_TEST_PASS guard ...` or `FERRIC_TEST_FAIL
//! guard ...`, and exits non-zero on failure.

use std::ffi::c_void;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitCode};

const PAGE_SIZE: usize = 4096;
const SIGSEGV: i32 = 11;

/// Big enough for relibc's `pthread_attr_t` (32 bytes) with room to spare.
#[repr(C, align(8))]
struct PthreadAttr([u8; 128]);

extern "C" {
    fn pthread_self() -> usize;
    fn pthread_getattr_np(thread: usize, attr: *mut PthreadAttr) -> i32;
    fn pthread_attr_getstack(
        attr: *const PthreadAttr,
        stack: *mut *mut c_void,
        size: *mut usize,
    ) -> i32;
}

/// Lowest address and size of the calling thread's stack mapping.
fn own_stack() -> Result<(usize, usize), String> {
    let mut attr = PthreadAttr([0; 128]);
    let mut base = std::ptr::null_mut();
    let mut size = 0;
    // SAFETY: `attr` is large enough and the out pointers are valid.
    unsafe {
        if pthread_getattr_np(pthread_self(), &mut attr) != 0 {
            return Err("pthread_getattr_np failed".into());
        }
        if pthread_attr_getstack(&attr, &mut base, &mut size) != 0 {
            return Err("pthread_attr_getstack failed".into());
        }
    }
    Ok((base as usize, size))
}

/// Child mode: read one byte of the guard page or of the page above it, from a
/// new thread.
fn probe(which: &str) -> ExitCode {
    if which == "main-guard" {
        let (base, _) = own_stack().expect("stack bounds");
        // SAFETY: deliberately touches the guard page; a fault is the point.
        let byte = unsafe { std::ptr::read_volatile((base - PAGE_SIZE) as *const u8) };
        std::hint::black_box(byte);
        return ExitCode::SUCCESS;
    }
    let which = which.to_owned();
    let handle = std::thread::spawn(move || {
        let (base, _) = own_stack().expect("stack bounds");
        // `pthread_getattr_np` reports the usable stack, so the guard is the
        // page just below it.
        let addr = if which == "guard" {
            base - PAGE_SIZE
        } else {
            base
        };
        // SAFETY: deliberately touches the guard page; a fault is the point.
        let byte = unsafe { std::ptr::read_volatile(addr as *const u8) };
        std::hint::black_box(byte);
    });
    match handle.join() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(2),
    }
}

fn run_probe(exe: &str, which: &str) -> Result<std::process::ExitStatus, String> {
    Command::new(exe)
        .args(["--probe", which])
        .status()
        .map_err(|e| format!("could not run {exe}: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--probe" {
        return probe(&args[2]);
    }

    let bounds = std::thread::spawn(own_stack).join();
    let (base, size) = match bounds {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            println!("FERRIC_TEST_FAIL guard {e}");
            return ExitCode::FAILURE;
        }
        Err(_) => {
            println!("FERRIC_TEST_FAIL guard thread panicked");
            return ExitCode::FAILURE;
        }
    };

    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| args[0].clone());
    let guard = run_probe(&exe, "guard");
    let stack = run_probe(&exe, "stack");
    match (guard, stack) {
        (Ok(g), Ok(s)) if g.signal() == Some(SIGSEGV) && s.success() => {
            println!("FERRIC_TEST_PASS guard stack={base:#x}+{size:#x}, guard page faults");
            main_guard(&exe)
        }
        (Ok(g), Ok(s)) => {
            println!(
                "FERRIC_TEST_FAIL guard stack={base:#x}+{size:#x} guard read: {g}, stack read: {s}"
            );
            ExitCode::FAILURE
        }
        (Err(e), _) | (_, Err(e)) => {
            println!("FERRIC_TEST_FAIL guard {e}");
            ExitCode::FAILURE
        }
    }
}

/// Checks the guard page below the main thread's stack, when its base is known.
fn main_guard(exe: &str) -> ExitCode {
    let base = match own_stack() {
        Ok((base, _)) if base > PAGE_SIZE => base,
        _ => {
            println!("FERRIC_TEST_SKIP guard-main main stack base unknown");
            return ExitCode::SUCCESS;
        }
    };
    match run_probe(exe, "main-guard") {
        Ok(g) if g.signal() == Some(SIGSEGV) => {
            println!("FERRIC_TEST_PASS guard-main stack base={base:#x}, guard page faults");
            ExitCode::SUCCESS
        }
        Ok(g) => {
            println!("FERRIC_TEST_FAIL guard-main stack base={base:#x} guard read: {g}");
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("FERRIC_TEST_FAIL guard-main {e}");
            ExitCode::FAILURE
        }
    }
}
