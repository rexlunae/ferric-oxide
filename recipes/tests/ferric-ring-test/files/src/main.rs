//! Submission/completion ring test for Ferric Oxide (`kernel.rings`).
//!
//! The image is built with `kernel.rings = true`, so the kernel has the `kernel.ring` scheme.
//! The program checks, in order:
//!
//! - `ring-setup`: a ring sets up once (`EEXIST` the second time), maps, and its header
//!   matches the layout both sides compute.
//! - `ring-refuse-setup`: `Enter` and `mmap` before `Setup`, a mapping of the wrong length, entry
//!   counts that are not powers of two, and a notify mode the kernel lacks (`SqPoll`) are
//!   refused.
//! - `ring-nop`: a batch of `Nop`s completes in order, one CQE each, in one `Enter`.
//! - `ring-file`: `Write` and `Read` at an offset move a page to and from a RedoxFS file, and
//!   at the current position they move the file position, as `write` does.
//! - `ring-chain`: one `Enter` runs a chain across three schemes: `Read` a RedoxFS file,
//!   `Write` the bytes to a kernel pipe, `Write` them to the null scheme (zerod). The pipe
//!   then holds the file's bytes.
//! - `ring-cancel`: a failed SQE (a closed descriptor) and a short `Read` each cancel the rest
//!   of their chain with `ECANCELED`, and the next chain runs.
//! - `ring-refuse-sqe`: reserved fields, an unknown opcode, fixed files, a kernel address and
//!   an overlong chain complete with their errnos.
//! - `ring-incomplete`: a chain whose end is not written yet stays in the SQ until it is.
//! - `ring-backpressure`: a chain is taken only when the CQ has room for its completions
//!   (`EBUSY` when nothing could be), so none is dropped.
//! - `ring-corrupt`: an SQ tail out of range makes `Enter` fail with `EINVAL` and changes
//!   nothing, and the ring works again once the tail is put back.
//! - `ring-event`: an event queue watching the ring sees `EVENT_READ` once `Enter` posts a
//!   completion, and not before.
//! - `ring-hold`: a chain blocked in its first SQE still writes, in its second, to the pipe
//!   its submitter named after another thread moves a different pipe onto that descriptor
//!   number with `dup2`: `Enter` holds a chain's files from when it takes the chain.
//! - `ring-release`: a pipe's write end, closed while a ring chain and a blocked `write` both
//!   hold it, is closed for good by whichever lets go last (here the `write`), so the reader
//!   sees the end of the file.
//! - `ring-bench`: times 64-byte writes to the null scheme and 4 KiB reads of a file, as
//!   system calls and as ring batches of 32, and prints `FERRIC_BENCH` lines.
//!
//! Each prints `FERRIC_TEST_PASS <name>` or `FERRIC_TEST_FAIL <name>: <why>`; the run stops at
//! the first failure and exits non-zero. Without a `kernel.ring` scheme (a kernel built without
//! the `rings` feature) it prints `FERRIC_TEST_SKIP ring` and exits zero.

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP ring: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
