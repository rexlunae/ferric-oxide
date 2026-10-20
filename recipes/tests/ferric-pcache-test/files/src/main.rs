//! Shared page cache read path test for Ferric Oxide (`kernel.page_cache`).
//!
//! The program is its own file system: one thread serves a scheme, `pcache-test`, holding a
//! single read-only file of 64 pages and 100 bytes whose every byte is a known function of its
//! offset. The main thread opens a `kernel.pcache` queue, binds it to that scheme, creates the
//! file's object, and answers the queue's fill records with the file's bytes; the scheme
//! attaches every description it opens to the object. Readers in other threads then `pread`
//! the file:
//!
//! - `pcache-miss`: a read of pages that are not cached becomes fill records (the reader waits
//!   for the answer) and returns the file's bytes; the scheme itself is never asked to read.
//! - `pcache-hit`: reading the same bytes again needs no record and no scheme read. Prints
//!   `FERRIC_BENCH pcache-pread4k` with the mean time of a cached 4 KiB `pread`.
//! - `pcache-readahead`: a lone read of a page fills only that page; the next page, read in
//!   sequence, fills pages past it too, so the read after that is a hit.
//! - `pcache-eio`: a fill the file system fails makes its reader's `pread` fail with `EIO`,
//!   and the next read fills the page again and gets the bytes.
//! - `pcache-bad-answer`: a done answer the kernel cannot take (its payload faults) fails the
//!   fill, so the reader gets `EIO` rather than waiting, and the next read fills the page.
//! - `pcache-close`: a reader waiting for a fill nobody answers gets `EIO` when the queue
//!   closes, and reads after that go to the scheme, as without a cache. Prints the mean time
//!   of the same `pread` through the scheme on the `FERRIC_BENCH` line.
//!
//! Each prints `FERRIC_TEST_PASS <name>` or `FERRIC_TEST_FAIL <name>: <why>`; the run stops at
//! the first failure and exits non-zero. Without a `kernel.pcache` scheme (a kernel built
//! without the `page_cache` feature) it prints `FERRIC_TEST_SKIP pcache` and exits zero.

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP pcache: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
