//! Kernel tracing overhead benchmark for Ferric Oxide (roadmap Phase T, overhead budget).
//!
//! Two workloads are timed in rounds:
//!
//! - `io`: a 4 KiB `pwrite` and `pread` of a file on the root file system, one operation per
//!   pair. Each call is a request to RedoxFS through its scheme, so this is the storage path the
//!   budget for every event on is judged on. The file is opened `O_FSYNC`, which the page cache
//!   leaves to RedoxFS, so cached reads and writes do not take the requests away.
//! - `syscall`: a one-byte `write` to a file descriptor that is not open, the cheapest system
//!   call that reaches the kernel's dispatch. It shows the per-call cost of a tracepoint, and the
//!   budget for tracing compiled in but off is judged on it.
//!
//! On a kernel without `kernel.trace` (no `kernel.trace` in the `/scheme` listing) every round
//! runs as state `out`. With it, the program first turns every event off and runs the same
//! rounds with all tracepoints off (`off`), exactly as the `out` rounds run: before any toggling,
//! with the two workloads taking turns round by round, so each one's rounds spread over the
//! whole phase. Then, workload by workload, it alternates: a round with all off (`pairoff`),
//! `enable all`, the same round with every compiled-in event on (`on`), `disable all`, and a
//! drain of every CPU's ring so the next `on` round writes to empty rings instead of counting
//! drops. Toggling and draining disturb the caches, so `off` is compared with `out` and `on`
//! with `pairoff`, never across.
//!
//! For each workload and state it prints
//! `FERRIC_BENCH <workload> <state> min_ns=<n> median_ns=<n> rounds=<n>`, the nanoseconds per
//! operation of the fastest and the median round. `scripts/ferric-ci/check-trace-overhead.sh`
//! compares these across boots of kernels built with and without tracing. It then prints
//! `FERRIC_TEST_PASS trace-bench`, or `FERRIC_TEST_FAIL trace-bench: <why>` and exits non-zero.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP trace-bench: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
