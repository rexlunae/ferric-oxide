//! The test on Redox: the request protocol between the application and the test scheme, and
//! the few libc calls both sides make.

mod app;
mod server;

use std::process::ExitCode;

/// The test scheme's name.
const SCHEME: &str = "regbuf-test";

/// The name of a second instance of the test scheme, which the first forwards reads to.
const PEER: &str = "regbuf-test-peer";

/// Paths of the test scheme's handles besides the root's (the empty path, for [`Op`] calls).
mod path {
    /// `read` on it registers the request's buffer, borrowed from the reader, in a regbuf
    /// region of the scheme's own; the read fails with that `Register`'s errno.
    pub const REGISTER_BORROW: &str = "register-borrow";
    /// `read` on it reads the peer scheme's [`FILL`] handle into the request's buffer: the
    /// scheme passes on a buffer it only borrows.
    pub const FORWARD: &str = "forward";
    /// `read` on it fills the buffer with [`super::FILL_BYTE`].
    pub const FILL: &str = "fill";
}

/// What a `read` of the [`path::FILL`] handle writes.
const FILL_BYTE: u8 = 0x42;

/// The errno values the test expects, as relibc numbers them.
mod errno {
    pub const EPERM: usize = 1;
    pub const EIO: usize = 5;
    pub const EFAULT: usize = 14;
    pub const EBUSY: usize = 16;
    pub const EEXIST: usize = 17;
    pub const EINVAL: usize = 22;
    pub const EACCES: usize = 13;
    pub const ECANCELED: usize = 125;
}

/// What the application asks the test scheme to do with the grant in a slot. Sent as a
/// `SYS_CALL` on the scheme with metadata `[op, slot, arg]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
enum Op {
    /// Map the whole grant, writable when `arg` is 1, and keep the mapping.
    Map = 1,
    /// Check every byte of the kept mapping is `arg`.
    Check = 2,
    /// Write `arg` to every byte of the kept mapping.
    Fill = 3,
    /// Hand the kept mapping's first byte to a system call (a pipe write): 0 while it is
    /// mapped, `EFAULT` once it is gone.
    Probe = 4,
    /// `munmap` the kept mapping, which must fail with `EBUSY`.
    Munmap = 5,
    /// Drop the kept mapping with the regbuf `Unmap` call, but remember its address.
    Unmap = 6,
    /// Map the grant again without keeping the mapping (unmapped at once with `Unmap`).
    Remap = 7,
    /// Leave the request loop and exit.
    Exit = 8,
    /// Check the granted bytes of the kept mapping are `arg` and the rest of its pages are
    /// zero: a byte-granular grant shows nothing of the region outside it.
    CheckGranted = 9,
    /// Try to register the kept mapping in a regbuf region of the scheme's own: 0 if that
    /// worked, else its errno (`EPERM`, borrowed pages cannot be registered).
    Register = 10,
}

impl Op {
    fn from_raw(raw: u64) -> Option<Self> {
        Some(match raw {
            1 => Self::Map,
            2 => Self::Check,
            3 => Self::Fill,
            4 => Self::Probe,
            5 => Self::Munmap,
            6 => Self::Unmap,
            7 => Self::Remap,
            8 => Self::Exit,
            9 => Self::CheckGranted,
            10 => Self::Register,
            _ => return None,
        })
    }
}

/// The errno of the last failed libc call.
fn last_errno() -> usize {
    std::io::Error::last_os_error()
        .raw_os_error()
        .and_then(|e| usize::try_from(e).ok())
        .unwrap_or(errno::EIO)
}

/// Runs the test.
pub fn main() -> ExitCode {
    app::run()
}
