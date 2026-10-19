//! Scheme-call buffer borrow test for Ferric Oxide.
//!
//! When a process writes to a file served by a user scheme (RedoxFS here), the kernel lends
//! the scheme the whole pages of the buffer (`Grant::borrow`). Pages of the buffer that are
//! themselves a shared mapping of a scheme's file (an `fmap`-borrowed grant) are pinned while
//! the scheme holds them, so they cannot be unmapped under it, and unpinned when it answers.
//!
//! A borrow can fail after looking at the grants: here the buffer runs from a shared file
//! mapping into an unmapped page, so the kernel refuses it. The pins of such a failed borrow
//! must not stay behind: a leaked pin made `munmap` of the mapping fail with `EBUSY` forever,
//! and the process's exit then panicked the kernel (`assertion failed:
//! !self.info.is_pinned()` in `Grant::unmap`). The checks:
//!
//! - `borrow-ok`: a one-page write from the shared mapping is written whole (the borrow
//!   works, and is unpinned when RedoxFS answers);
//! - `borrow-fail`: a two-page write from the mapping and the unmapped page after it fails or
//!   writes less than both pages, and the mapping can then be unmapped. The process then
//!   exits, which panicked the kernel before the fix.
//!
//! Running these right before `ferric-regbuf-test` showed a RedoxFS bug: RedoxFS keeps the
//! contents of a file's unmapped ranges cached, keyed by node id, and handed them out again on
//! the next `mmap` even when the file had been written since or deleted and its node id
//! reused by a new file. Two more checks cover it:
//!
//! - `remap-written`: a page mapped, unmapped, then written with `pwrite` maps with the new
//!   contents;
//! - `remap-reused`: once the mapped file is deleted, a new file maps and reads with its own
//!   contents, not the deleted file's.
//!
//! Prints one `FERRIC_TEST_PASS borrow ...` or `FERRIC_TEST_FAIL borrow ...` line and exits
//! non-zero on failure. It also runs on Linux, which helps check the test itself.

use std::ffi::{c_int, c_long, c_void};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;

const PAGE_SIZE: usize = 4096;
/// The file mapped shared.
const MAP_PATH: &str = "/tmp/ferric-borrow-test.map";
/// The file written from the mapping.
const OUT_PATH: &str = "/tmp/ferric-borrow-test.out";
/// What the mapped file holds.
const FILL: u8 = 0x5A;
/// The file `remap-written` maps, writes and maps again, then deletes.
const REMAP_PATH: &str = "/tmp/ferric-borrow-test.remap";
/// The file `remap-reused` creates after that one is deleted.
const NEW_PATH: &str = "/tmp/ferric-borrow-test.new";

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
    pub const MAP_SHARED: c_int = 0x0001;
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
        pub fn write(fd: c_int, buf: *const c_void, len: usize) -> isize;
    }
}

/// Why the test failed.
#[derive(Debug)]
enum Error {
    /// Setting up a file failed.
    File {
        path: &'static str,
        source: io::Error,
    },
    /// A mapping could not be made where the test needs it.
    Mmap(&'static str),
    /// The one-page write from the mapping did not write the page.
    ShortWrite { written: isize, errno: io::Error },
    /// The write past the end of the mapping wrote both pages.
    WholeWrite,
    /// The mapping could not be unmapped after the failed write: its pin was leaked.
    Pinned(io::Error),
    /// A mapping or a read showed contents the file no longer has.
    Stale {
        check: &'static str,
        expected: u8,
        found: u8,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { path, source } => write!(f, "{path}: {source}"),
            Self::Mmap(what) => write!(f, "mmap of {what} failed"),
            Self::ShortWrite { written, errno } => write!(
                f,
                "borrow-ok: a page written from the mapping gave {written} ({errno})"
            ),
            Self::WholeWrite => write!(
                f,
                "borrow-fail: a write running past the mapping wrote both pages"
            ),
            Self::Pinned(e) => write!(
                f,
                "borrow-fail: munmap after the failed write failed ({e}): a pin leaked"
            ),
            Self::Stale {
                check,
                expected,
                found,
            } => write!(f, "{check}: found {found:#x}, not {expected:#x}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::File { source, .. } => Some(source),
            Self::ShortWrite { errno, .. } | Self::Pinned(errno) => Some(errno),
            _ => None,
        }
    }
}

fn file_error(path: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::File { path, source }
}

/// Calls `mmap`, checking the result is `addr` when that is given.
fn map(
    addr: usize,
    len: usize,
    flags: c_int,
    fd: c_int,
    what: &'static str,
) -> Result<usize, Error> {
    // SAFETY: anonymous memory, or a shared mapping of a file this test owns; with MAP_FIXED
    // it only replaces a page this test mapped itself.
    let got = unsafe {
        sys::mmap(
            addr as *mut c_void,
            len,
            sys::PROT_READ | sys::PROT_WRITE,
            flags,
            fd,
            0 as c_long,
        )
    } as usize;
    if got == sys::MAP_FAILED || (addr != 0 && got != addr) {
        return Err(Error::Mmap(what));
    }
    Ok(got)
}

/// `write(2)` from raw memory, which may run into an unmapped page.
fn write_raw(out: &File, addr: usize, len: usize) -> isize {
    // SAFETY: the kernel checks the buffer; the test passes memory it may not read on
    // purpose, and never reads it itself.
    unsafe { sys::write(out.as_raw_fd(), addr as *const c_void, len) }
}

fn run() -> Result<(), Error> {
    fs::write(MAP_PATH, [FILL; PAGE_SIZE]).map_err(file_error(MAP_PATH))?;
    let mapped = OpenOptions::new()
        .read(true)
        .write(true)
        .open(MAP_PATH)
        .map_err(file_error(MAP_PATH))?;
    let out = File::create(OUT_PATH).map_err(file_error(OUT_PATH))?;

    // Two pages, so the page after the shared mapping is known to be free once unmapped.
    let base = map(
        0,
        2 * PAGE_SIZE,
        sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
        -1,
        "two anonymous pages",
    )?;
    map(
        base,
        PAGE_SIZE,
        sys::MAP_SHARED | sys::MAP_FIXED,
        mapped.as_raw_fd(),
        "the file",
    )?;
    // SAFETY: the second page is this test's own anonymous page.
    if unsafe { sys::munmap((base + PAGE_SIZE) as *mut c_void, PAGE_SIZE) } != 0 {
        return Err(Error::Mmap("the page after the file (munmap)"));
    }

    let written = write_raw(&out, base, PAGE_SIZE);
    if written != PAGE_SIZE as isize {
        return Err(Error::ShortWrite {
            written,
            errno: io::Error::last_os_error(),
        });
    }

    let written = write_raw(&out, base, 2 * PAGE_SIZE);
    if written == (2 * PAGE_SIZE) as isize {
        return Err(Error::WholeWrite);
    }
    // SAFETY: the shared mapping is this test's own.
    if unsafe { sys::munmap(base as *mut c_void, PAGE_SIZE) } != 0 {
        return Err(Error::Pinned(io::Error::last_os_error()));
    }

    drop((mapped, out));
    fs::remove_file(MAP_PATH).map_err(file_error(MAP_PATH))?;
    fs::remove_file(OUT_PATH).map_err(file_error(OUT_PATH))?;
    let _ = io::stdout().flush();
    Ok(())
}

/// Maps the first page of `file` with `flags`, returns its first byte and unmaps it.
fn first_mapped_byte(file: &File, flags: c_int, what: &'static str) -> Result<u8, Error> {
    let page = map(0, PAGE_SIZE, flags, file.as_raw_fd(), what)?;
    // SAFETY: just mapped readable, one page long.
    let byte = unsafe { (page as *const u8).read_volatile() };
    // SAFETY: the mapping is this test's own.
    if unsafe { sys::munmap(page as *mut c_void, PAGE_SIZE) } != 0 {
        return Err(Error::Mmap(what));
    }
    Ok(byte)
}

/// Fails with [`Error::Stale`] unless `found` is `expected`.
fn expect_byte(check: &'static str, expected: u8, found: u8) -> Result<(), Error> {
    if found == expected {
        Ok(())
    } else {
        Err(Error::Stale {
            check,
            expected,
            found,
        })
    }
}

/// `remap-written` and `remap-reused`.
fn check_remap() -> Result<(), Error> {
    const OLD: u8 = 0x11;
    const NEW: u8 = 0x22;
    const OTHER: u8 = 0x33;
    fs::write(REMAP_PATH, [OLD; PAGE_SIZE]).map_err(file_error(REMAP_PATH))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(REMAP_PATH)
        .map_err(file_error(REMAP_PATH))?;
    let shared = sys::MAP_SHARED;
    expect_byte(
        "remap-written (first mapping)",
        OLD,
        first_mapped_byte(&file, shared, "the remap file")?,
    )?;
    std::os::unix::fs::FileExt::write_all_at(&file, &[NEW; PAGE_SIZE], 0)
        .map_err(file_error(REMAP_PATH))?;
    expect_byte(
        "remap-written",
        NEW,
        first_mapped_byte(&file, shared, "the written remap file")?,
    )?;
    drop(file);
    fs::remove_file(REMAP_PATH).map_err(file_error(REMAP_PATH))?;

    fs::write(NEW_PATH, [OTHER; PAGE_SIZE]).map_err(file_error(NEW_PATH))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(NEW_PATH)
        .map_err(file_error(NEW_PATH))?;
    expect_byte(
        "remap-reused (mapping)",
        OTHER,
        first_mapped_byte(&file, sys::MAP_PRIVATE, "the new file")?,
    )?;
    drop(file);
    let read = fs::read(NEW_PATH).map_err(file_error(NEW_PATH))?;
    let found = read.iter().copied().find(|&b| b != OTHER).unwrap_or(OTHER);
    expect_byte("remap-reused (read)", OTHER, found)?;
    fs::remove_file(NEW_PATH).map_err(file_error(NEW_PATH))
}

fn main() -> ExitCode {
    match run().and_then(|()| check_remap()) {
        Ok(()) => {
            println!(
                "FERRIC_TEST_PASS borrow a failed scheme-call borrow of a shared file mapping left it unpinned; remapped and reused files map their own contents"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("FERRIC_TEST_FAIL borrow {e}");
            ExitCode::FAILURE
        }
    }
}
