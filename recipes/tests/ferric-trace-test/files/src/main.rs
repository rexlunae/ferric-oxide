//! Kernel tracing test for Ferric Oxide (`kernel.trace`).
//!
//! The image is built with `kernel.trace = true` and `kernel.trace_boot = "sched"`, so the
//! kernel has the `syscall.*` and `sched.switch` tracepoints and turned `sched.switch` on at
//! boot. Through the `kernel.trace` scheme the program checks, in order:
//!
//! - `trace-boot`: the event listing has the three kernel events, with `sched.switch` on and
//!   the `syscall` events off.
//! - `trace-enable`: `enable syscall` turns both `syscall` events on (the kernel patches their
//!   tracepoints live, on every CPU).
//! - Then, after draining every ring, it makes three system calls it can recognize, straight
//!   through `redox_syscall` so nothing else runs in between: a `write` to a file descriptor
//!   that is not open, a `read` of the `stats` file into a buffer of a marker length, and a
//!   `yield`. It turns the `syscall` events off and drains every CPU's ring.
//! - `trace-decode`: every byte read decodes as whole records of events in the shared catalog
//!   (`ferric_trace::kernel::CATALOG`), each with as many arguments as the catalog names.
//! - `trace-order`: the timestamps on each CPU never go backwards.
//! - `trace-write`, `trace-read`: a `syscall.enter` with the call's number and arguments, and
//!   right after it on the same CPU the `syscall.exit` with its number and result (`EBADF`
//!   and the byte count). Neither call blocks, and the kernel runs system calls with interrupts
//!   off, so nothing else can come between the two records.
//! - `trace-yield`: the `yield`'s enter record and an exit with result 0 after it; a yield
//!   switches contexts, so the exit may come later and on another CPU.
//! - `trace-sched`: `sched.switch` records, from the event turned on at boot.
//! - `trace-disable`: once `disable syscall` returns, the same calls leave no record.
//!
//! Each prints `FERRIC_TEST_PASS <name>` or `FERRIC_TEST_FAIL <name>: <why>`; the run stops at
//! the first failure and exits non-zero. Without a `kernel.trace` scheme (a kernel built without
//! the `trace` feature) it prints `FERRIC_TEST_SKIP trace` and exits zero.

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP trace: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
