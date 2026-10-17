//! Registered buffer and zero-copy grant test for Ferric Oxide (`kernel.regbuf`).
//!
//! The program forks. The child serves a small scheme, `regbuf-test`, that receives grant
//! descriptors by fd passing and, on request, maps them, reads or writes the pages, probes
//! whether a mapping is still there, and tries operations that must fail. The parent is the
//! application: it registers a buffer in a region of `kernel.regbuf`, opens grants of it,
//! passes them to the scheme and checks what the scheme sees:
//!
//! - `regbuf-register`: registering pins the pages (the user's pinned count goes up); a second
//!   registration fails with `EEXIST`, an unaligned one with `EINVAL`.
//! - `regbuf-grant-rw`: the scheme maps a read-write grant, reads the application's data and
//!   writes new data the application then sees in its own buffer: the same pages, no copy.
//! - `regbuf-grant-ro`: a read-only grant cannot be mapped writable (`EACCES`), but can be
//!   mapped readable.
//! - `regbuf-pinned`: the scheme cannot `munmap` a grant mapping itself (`EBUSY`).
//! - `regbuf-revoke`: after the application revokes the read-write grant, the scheme's mapping
//!   is gone (a system call given its address gets `EFAULT`), mapping the grant again fails
//!   with `ECANCELED`, and the read-only grant still reaches the application's pages.
//! - `regbuf-unmap`: the scheme drops its read-only mapping with the `Unmap` call.
//! - `regbuf-close`: closing the region revokes the remaining grant (its fresh mapping is gone,
//!   new ones fail) and unpins the pages (the pinned count returns to where it was).
//!
//! Each prints `FERRIC_TEST_PASS <name>` or `FERRIC_TEST_FAIL <name>: <why>`; the run stops at
//! the first failure and exits non-zero. Without a `kernel.regbuf` scheme (a kernel built
//! without the `regbuf` feature) it prints `FERRIC_TEST_SKIP regbuf` and exits zero.

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP regbuf: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
