//! Kernel tracing test for Ferric Oxide (`kernel.trace`).
//!
//! The image is built with `kernel.trace = true` and `kernel.trace_boot = "sched"`, so the
//! kernel has the `syscall.*`, `sched.switch`, `scheme.*` and `mm.*` tracepoints and turned
//! `sched.switch` on at boot (`ferric-iommu-test` checks the `irq.*` ones, `trace-irq`). Through the `kernel.trace` scheme the program checks, in order:
//!
//! - `trace-initfs`: the events the initfs `lib/trace.toml` leaves on (CI's image turns on
//!   `scheme.cancel` there, through ferric-config's `kernel.trace_initfs_enable`) are on and
//!   those it turns off are off, as bootstrap applied the file before init. The state the
//!   steps below expect is then restored: the events they test are turned off, and
//!   `sched.switch` on again. Without the file this step prints `FERRIC_TEST_SKIP trace-initfs`.
//! - `trace-boot`: the event listing has the kernel events, with `sched.switch` on and the
//!   `syscall` and `scheme` events off.
//! - `trace-enable`: `enable syscall scheme` turns the `syscall` and `scheme` events on (the
//!   kernel patches their tracepoints live, on every CPU).
//! - Then, after draining every ring, it makes system calls it can recognize, straight through
//!   `redox_syscall` so nothing else runs in between: a `write` to a file descriptor that is
//!   not open, a `read` of the `stats` file into a buffer of a marker length, a `write` of
//!   another marker length to the null scheme, and a `yield`. It turns the events off again and
//!   drains every CPU's ring.
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
//! - `trace-scheme`: a `write` to the null scheme (served by `zerod`) has a `scheme.request`
//!   and, with the same scheme id and SQE tag, a `scheme.response` from another process with the
//!   byte count as its result.
//! - `trace-disable`: once `disable syscall scheme` returns, the same calls leave no record.
//! - `trace-mmap`: every CPU's ring maps read-only (`PROT_READ`, `MAP_SHARED`), and the kernel
//!   refuses a writable, unreadable, partial or private mapping and an `mprotect` to writable. With the
//!   `syscall` events on again, the marker `write` is found by reading the mappings through a
//!   `ferric_trace::ring::View`; `commit <end>` past `head` or inside a record is refused
//!   and leaves `tail` alone, and `commit` of the end read moves `tail` there, after which a
//!   read of the ring no longer gives the call.
//! - `trace-fault`: with the `mm` events on, it writes to each page of a fresh anonymous
//!   mapping (the kernel maps a page on first touch), unmaps it and `read`s `stats` into it,
//!   which fails with `EFAULT`. Each page must have an `mm.page_fault` from user mode, a
//!   write, ending `mapped`, and the read one from the kernel at the buffer ending `copy
//!   failed`.
//! - `trace-mapping`: map/protect/unmap results retain the target page-table root and
//!   order across a peer context, including overlap and permission refusals. Early
//!   unaligned validation failures produce no API result; live disable stops recording.
//! - `trace-journal`: the mapping journal. A `MAP_FIXED` `mmap` over a live mapping has an
//!   `mm.replace` before its `mm.map`; `mremap` to an address the kernel picks, and with
//!   `FIXED_REPLACE` onto another live mapping (an `mm.replace` first), each has an exact
//!   `mm.move` within this address space; forking a child has one successful `mm.clone` of
//!   this address space with grants and pages copied, and the copy has an `mm.destroy` once
//!   the child execs and exits. Disabling the events stops all of them.
//! - `trace-futex`: 32/64-bit waits produce a queued wait and matching resume with the
//!   timeout or wake result; mismatched waits produce neither. Empty, zero-count and
//!   successful peer wakes record their returned counts, and disabling the events stops
//!   recording them. The check uses the shared physical key and context id across CPUs.
//!   A successful wake must precede the peer's resume; polling collects records as it
//!   goes and must retain more empty probes than one CPU ring can hold.
//! - `trace-lifecycle`: a child process (exiting 23) and a thread have ordered kernel
//!   context creation, start and exit records, joined by debug id, with normal cleanup
//!   rather than an exception. Disabling these events suppresses a second child's records.
//!   The kernel records do not carry the child's process exit status; `wait` checks it.
//!   When kernel rings are available, asynchronous chain and close probes also require
//!   kernel worker create/start/exit records with PID 0 after serving user requests.
//! - `trace-filter`: an argument filter on `syscall.enter` (`nr == write && a == <closed fd>`)
//!   is listed in `filters` and keeps only the matching `syscall.enter` records, while
//!   `syscall.exit` records every call; `filter syscall.enter none` removes it, and filters on
//!   unknown events or arguments are refused.
//! - `trace-clock`: the `clock` file names the clock timestamps come from
//!   (`ferric_trace::clock`), and two marker writes 200 ms apart have timestamps that agree
//!   with the wall time between them, within a factor of two, when the clock's tick length is
//!   known (the KVM clock, the HPET); for the TSC only their order is checked, and the
//!   frequency they imply is printed.
//! - `trace-cap`: a process that is not root cannot open the scheme by path, but can through
//!   a capability (`cap/read`, `cap/control`) root handed it, with that capability's access
//!   and no more; see `redox::cap`.
//!
//! Each prints `FERRIC_TEST_PASS <name>` or `FERRIC_TEST_FAIL <name>: <why>`; the run stops at
//! the first failure and exits non-zero. `trace-fault` prints `FERRIC_TEST_SKIP trace-fault` on a
//! kernel without `mm.page_fault`. Without a `kernel.trace` scheme (a kernel built without
//! the `trace` feature) it prints `FERRIC_TEST_SKIP trace` and exits zero.
//!
//! `ferric-trace-test crash` runs the crash-dump test instead (`kernel.trace_crash`): on a
//! crash-test kernel's first boot it panics the kernel on purpose (`trace-crash-armed`), and on
//! the boot after it checks the dump that panic left (`trace-crash-read`); see `redox::crash`.

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
