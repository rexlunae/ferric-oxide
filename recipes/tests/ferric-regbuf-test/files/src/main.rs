//! Registered buffer and zero-copy grant test for Ferric Oxide (`kernel.regbuf`).
//!
//! The program forks twice. The first child serves a small scheme, `regbuf-test`, that
//! receives grant descriptors by fd passing and, on request, maps them, reads or writes the
//! pages, probes whether a mapping is still there, and tries operations that must fail; the
//! second serves a peer instance, `regbuf-test-peer`, that the first forwards reads to. The
//! parent is the
//! application: it registers a buffer in a region of `kernel.regbuf`, opens grants of it,
//! passes them to the scheme and checks what the scheme sees:
//!
//! - `regbuf-register`: registering pins the pages (the user's pinned count goes up); a second
//!   registration fails with `EEXIST`, an unaligned one with `EINVAL`.
//! - `regbuf-file-backed`: a private writable mapping of a file cannot be registered (`EPERM`),
//!   even once written: regions take anonymous memory only, and nothing is pinned.
//! - `regbuf-grant-rw`: the scheme maps a read-write grant, reads the application's data and
//!   writes new data the application then sees in its own buffer: the same pages, no copy.
//! - `regbuf-grant-ro`: a read-only grant cannot be mapped writable (`EACCES`), but can be
//!   mapped readable.
//! - `regbuf-pinned`: the scheme cannot `munmap` a grant mapping itself (`EBUSY`).
//! - `regbuf-reshare-mmap`: the scheme answers the application's `mmap` of it, shared and
//!   private, with its grant mapping's address; the kernel refuses (`EPERM`), so the pages
//!   cannot escape revocation. Had it not, the step checks whether the re-shared mapping
//!   survives a revoke; it fails either way. The revoke still takes the scheme's mapping.
//! - `regbuf-reshare-register`: the scheme cannot register its grant mapping in a region of
//!   its own (`EPERM`).
//! - `regbuf-reshare-borrow`: nor the buffer of a `read` it serves, borrowed from the
//!   application for the request (`EPERM`).
//! - `regbuf-forward-borrow`: a scheme can still pass such a buffer on in a request of its own
//!   (to a second instance of the test scheme), and the data arrives.
//! - `regbuf-revoke`: after the application revokes the read-write grant, the scheme's mapping
//!   is gone (a system call given its address gets `EFAULT`), mapping the grant again fails
//!   with `ECANCELED`, and the read-only grant still reaches the application's pages.
//! - `regbuf-unmap`: the scheme drops its read-only mapping with the `Unmap` call.
//! - `regbuf-close`: closing the region revokes the remaining grant (its fresh mapping is gone,
//!   new ones fail) and unpins the pages (the pinned count returns to where it was).
//! - `regbuf-shared`: a `MAP_SHARED` anonymous buffer registers too; the scheme and the
//!   application see each other's writes, and revoke takes the scheme's mapping away.
//! - `regbuf-bytes`: a byte-granular read-write grant maps its window; the scheme sees only the
//!   granted bytes (zeros elsewhere), writes the wholly granted page in place, and after the
//!   revoke exactly the granted bytes hold its writes. A read-only grant of 0x20 bytes inside
//!   one page shows just those.
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
