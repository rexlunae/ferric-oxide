//! Stale-TLB test for Ferric Oxide.
//!
//! With KPTI and PCIDs (`kernel.cpu.pcid`), the page-table switches on system
//! call and interrupt entry and exit keep the TLB, so the kernel must
//! invalidate user translations in the user PCID itself on every unmap,
//! mprotect and TLB shootdown. A missed invalidation lets a thread keep using
//! a page after another thread unmapped or write-protected it. Each check
//! below has a second thread keep one page in its TLB, entering and leaving
//! the kernel (`sched_yield`) between accesses, while the main thread changes
//! the mapping. Once the change has returned, the second thread's next access
//! must see it:
//!
//! - `remap`: the page is replaced with `MAP_FIXED`; the thread must read the
//!   new, zeroed page, not the old contents. Runs in this process.
//! - `mprotect`: the page is made read-only; the thread's next write must die
//!   with SIGSEGV. Runs in a child process (`--child mprotect`).
//! - `munmap`: the page is unmapped; the thread's next read must die with
//!   SIGSEGV. Runs in a child process (`--child munmap`).
//!
//! The thread may run on the same CPU as the main thread (no shootdown, the
//! local invalidation must do) or another (the shootdown IPI must), so each
//! check repeats.
//!
//! It also prints `FERRIC_BENCH tlb ...` with the cost of a `sched_yield`
//! round trip, alone and followed by touching 64 pages, which is what the
//! no-flush switches speed up.
//!
//! Prints one `FERRIC_TEST_PASS tlb-...` or `FERRIC_TEST_FAIL tlb-...` line per
//! check and exits non-zero on failure. It also runs on Linux, which helps
//! check the test itself.

use std::fmt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

const PAGE_SIZE: usize = 4096;
const SIGSEGV: i32 = 11;
/// Rounds of the in-process check, and child processes per signal check.
const REMAP_ROUNDS: usize = 200;
const CHILD_ROUNDS: usize = 10;
/// Accesses the thread makes before the main thread changes the mapping.
const WARM_ACCESSES: usize = 50;
/// The byte written to a page before it is replaced or protected.
const OLD_BYTE: u8 = 0xAA;

/// The few libc calls the test needs, with the constants of the platform.
mod sys {
    use std::ffi::{c_int, c_long, c_void};

    #[cfg(target_os = "redox")]
    mod consts {
        use std::ffi::c_int;
        pub const PROT_READ: c_int = 0x0004;
        pub const PROT_WRITE: c_int = 0x0002;
        pub const MAP_FIXED: c_int = 0x0004;
    }
    #[cfg(not(target_os = "redox"))]
    mod consts {
        use std::ffi::c_int;
        pub const PROT_READ: c_int = 0x0001;
        pub const PROT_WRITE: c_int = 0x0002;
        pub const MAP_FIXED: c_int = 0x0010;
    }
    pub use consts::*;
    pub const MAP_PRIVATE: c_int = 0x0002;
    pub const MAP_ANONYMOUS: c_int = 0x0020;
    pub const MAP_FAILED: usize = usize::MAX;

    unsafe extern "C" {
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            off: c_long,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
        pub fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
        pub fn sched_yield() -> c_int;
    }
}

/// Why a check could not run or failed.
#[derive(Debug)]
enum Error {
    /// `mmap` failed or put a `MAP_FIXED` page somewhere else.
    Mmap,
    /// `munmap` or `mprotect` failed.
    Change(&'static str),
    /// The toucher thread panicked.
    Thread,
    /// A child process could not be started.
    Spawn(std::io::Error),
    /// The thread read the old contents of a replaced page.
    Stale { round: usize, byte: u8 },
    /// A child's thread accessed the page after the change without faulting.
    NoFault {
        round: usize,
        status: std::process::ExitStatus,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mmap => write!(f, "mmap failed"),
            Self::Change(call) => write!(f, "{call} failed"),
            Self::Thread => write!(f, "toucher thread panicked"),
            Self::Spawn(e) => write!(f, "could not start a child: {e}"),
            Self::Stale { round, byte } => {
                write!(f, "round {round}: read {byte:#x} from a replaced page")
            }
            Self::NoFault { round, status } => {
                write!(
                    f,
                    "round {round}: access after the change did not fault ({status})"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(e) => Some(e),
            _ => None,
        }
    }
}

/// Maps one private anonymous read-write page, at `fixed` if given.
fn map_page(fixed: Option<usize>) -> Result<usize, Error> {
    let (addr, extra) = match fixed {
        Some(addr) => (addr, sys::MAP_FIXED),
        None => (0, 0),
    };
    // SAFETY: an anonymous mapping; with MAP_FIXED it only replaces a page this
    // test mapped itself.
    let page = unsafe {
        sys::mmap(
            addr as *mut _,
            PAGE_SIZE,
            sys::PROT_READ | sys::PROT_WRITE,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS | extra,
            -1,
            0,
        )
    } as usize;
    if page == sys::MAP_FAILED || fixed.is_some_and(|a| a != page) {
        return Err(Error::Mmap);
    }
    Ok(page)
}

/// A thread that keeps a page in its TLB until told to make one last access.
struct Toucher {
    go: Arc<AtomicBool>,
    warm: Arc<AtomicUsize>,
    handle: JoinHandle<u8>,
}

impl Toucher {
    /// Starts touching `page`: reading, or writing [`OLD_BYTE`] back when
    /// `write`, with a system call between accesses.
    fn start(page: usize, write: bool) -> Self {
        let go = Arc::new(AtomicBool::new(false));
        let warm = Arc::new(AtomicUsize::new(0));
        let (g, w) = (go.clone(), warm.clone());
        let handle = std::thread::spawn(move || {
            let ptr = page as *mut u8;
            loop {
                let last = g.load(Ordering::Acquire);
                // SAFETY: the page is mapped until the main thread changes it;
                // an access after that faulting is what the test looks for.
                let byte = unsafe {
                    if write {
                        ptr.write_volatile(OLD_BYTE);
                    }
                    ptr.read_volatile()
                };
                if last {
                    return byte;
                }
                w.fetch_add(1, Ordering::Release);
                // SAFETY: no arguments; enters and leaves the kernel.
                unsafe { sys::sched_yield() };
            }
        });
        Self { go, warm, handle }
    }

    /// Waits until the page is in the thread's TLB. Yields rather than spins,
    /// so the thread gets to run when both share a CPU.
    fn wait_warm(&self) {
        while self.warm.load(Ordering::Acquire) < WARM_ACCESSES {
            // SAFETY: no arguments.
            unsafe { sys::sched_yield() };
        }
    }

    /// Lets the thread make its last access and returns the byte it read.
    fn finish(self) -> Result<u8, Error> {
        self.go.store(true, Ordering::Release);
        self.handle.join().map_err(|_| Error::Thread)
    }
}

/// `remap`: a page replaced with `MAP_FIXED` must read as the new page.
fn check_remap() -> Result<(), Error> {
    for round in 0..REMAP_ROUNDS {
        let page = map_page(None)?;
        // SAFETY: just mapped read-write.
        unsafe { (page as *mut u8).write_volatile(OLD_BYTE) };
        let toucher = Toucher::start(page, false);
        toucher.wait_warm();
        let replaced = map_page(Some(page));
        let byte = toucher.finish();
        // SAFETY: the thread is gone; the page is this test's.
        unsafe { sys::munmap(page as *mut _, PAGE_SIZE) };
        replaced?;
        let byte = byte?;
        if byte != 0 {
            return Err(Error::Stale { round, byte });
        }
    }
    Ok(())
}

/// Child side of `mprotect` and `munmap`: returns only if the last access did
/// not fault.
fn child(which: &str) -> Result<(), Error> {
    let page = map_page(None)?;
    let write = which == "mprotect";
    let toucher = Toucher::start(page, write);
    toucher.wait_warm();
    // SAFETY: the page is this process's; the thread faulting on it is the point.
    let rc = unsafe {
        if write {
            sys::mprotect(page as *mut _, PAGE_SIZE, sys::PROT_READ)
        } else {
            sys::munmap(page as *mut _, PAGE_SIZE)
        }
    };
    if rc != 0 {
        return Err(Error::Change(if write { "mprotect" } else { "munmap" }));
    }
    toucher.finish().map(drop)
}

/// Runs `--child which` [`CHILD_ROUNDS`] times; each must die with SIGSEGV.
fn check_child(exe: &str, which: &str) -> Result<(), Error> {
    for round in 0..CHILD_ROUNDS {
        let status = Command::new(exe)
            .args(["--child", which])
            .status()
            .map_err(Error::Spawn)?;
        if status.signal() != Some(SIGSEGV) {
            return Err(Error::NoFault { round, status });
        }
    }
    Ok(())
}

/// Prints the cost of a `sched_yield` round trip, alone and followed by
/// touching 64 pages.
fn bench() -> Result<(), Error> {
    const CALLS: u32 = 100_000;
    const PAGES: usize = 64;
    let start = Instant::now();
    for _ in 0..CALLS {
        // SAFETY: no arguments.
        unsafe { sys::sched_yield() };
    }
    let alone = start.elapsed().as_nanos() / u128::from(CALLS);

    let pages: Vec<usize> = (0..PAGES)
        .map(|_| map_page(None))
        .collect::<Result<_, _>>()?;
    for &p in &pages {
        // SAFETY: just mapped read-write.
        unsafe { (p as *mut u8).write_volatile(1) };
    }
    let start = Instant::now();
    let mut sum = 0u8;
    for _ in 0..CALLS / 4 {
        // SAFETY: no arguments.
        unsafe { sys::sched_yield() };
        for &p in &pages {
            // SAFETY: mapped above.
            sum = sum.wrapping_add(unsafe { (p as *const u8).read_volatile() });
        }
    }
    let touched = start.elapsed().as_nanos() / u128::from(CALLS / 4);
    std::hint::black_box(sum);
    for &p in &pages {
        // SAFETY: mapped above and no longer used.
        unsafe { sys::munmap(p as *mut _, PAGE_SIZE) };
    }
    println!("FERRIC_BENCH tlb sched_yield_ns={alone} sched_yield_touch{PAGES}_ns={touched}");
    Ok(())
}

fn report(name: &str, result: Result<(), Error>, what: &str) -> bool {
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS tlb-{name} {what}");
            true
        }
        Err(e) => {
            println!("FERRIC_TEST_FAIL tlb-{name} {e}");
            false
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--child" {
        return match child(&args[2]) {
            // The access went through: the parent reports the failure.
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ferric-tlb-test child: {e}");
                ExitCode::from(2)
            }
        };
    }

    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| args[0].clone());
    let mut ok = report(
        "remap",
        check_remap(),
        "MAP_FIXED replacement seen by another thread",
    );
    ok &= report(
        "mprotect",
        check_child(&exe, "mprotect"),
        "write after mprotect(PROT_READ) faults in another thread",
    );
    ok &= report(
        "munmap",
        check_child(&exe, "munmap"),
        "read after munmap faults in another thread",
    );
    if let Err(e) = bench() {
        println!("FERRIC_TEST_FAIL tlb-bench {e}");
        ok = false;
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
