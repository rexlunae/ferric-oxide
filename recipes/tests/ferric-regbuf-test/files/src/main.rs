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
//!   survives a revoke; it fails either way. The revoke still takes the region's pages from
//!   the scheme's mapping.
//! - `regbuf-reshare-register`: the scheme cannot register its grant mapping in a region of
//!   its own (`EPERM`).
//! - `regbuf-reshare-borrow`: nor the buffer of a `read` it serves, borrowed from the
//!   application for the request (`EPERM`).
//! - `regbuf-forward-borrow`: a scheme can still pass such a buffer on in a request of its own
//!   (to a second instance of the test scheme), and the data arrives.
//! - `regbuf-revoke`: after the application revokes the read-write grant, the scheme's mapping
//!   stays mapped but holds zeros (a system call given its address works, so the holder never
//!   faults), what the scheme writes there no longer reaches the application, mapping the
//!   grant again fails with `ECANCELED`, and the read-only grant still reaches the
//!   application's pages.
//! - `regbuf-unmap`: the scheme drops its read-only mapping with the `Unmap` call.
//! - `regbuf-close`: closing the region revokes the remaining grant (its fresh mapping holds
//!   zeros, new ones fail) and unpins the pages (the pinned count returns to where it was).
//! - `regbuf-shared`: a `MAP_SHARED` anonymous buffer registers too; the scheme and the
//!   application see each other's writes, and revoke takes the shared pages from the scheme's
//!   mapping (it holds zeros, and its writes stay its own).
//! - `regbuf-bytes`: a byte-granular read-write grant maps its window; the scheme sees only the
//!   granted bytes (zeros elsewhere), writes the wholly granted page in place, and after the
//!   revoke exactly the granted bytes hold its writes. A read-only grant of 0x20 bytes inside
//!   one page shows just those.
//! - `regbuf-bounce-newer`: two writable grants of bytes inside one page, one untouched and one
//!   the scheme fills; the application then writes the page. After revoking both, the page
//!   holds the application's writes except where the scheme wrote: a bounce page copies back
//!   only the bytes its holder changed.
//! - `regbuf-revoke-prot`: a revoke keeps each mapping's protection. A forked probe maps one
//!   grant readable, writable and executable and another `PROT_NONE`; after the revoke, a
//!   `ret` it writes in the first still runs, and reading the second still kills it with
//!   `SIGSEGV`. That half needs `kernel.prot_none`; on a kernel that maps `PROT_NONE`
//!   readable it prints `FERRIC_TEST_SKIP regbuf-revoke-prot-none` and checks only the first.
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
