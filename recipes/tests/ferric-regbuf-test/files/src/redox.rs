//! The test on Redox: the request protocol between the application and the test scheme, and
//! the few libc calls both sides make.

mod app;
mod server;

use std::process::ExitCode;

/// The test scheme's name.
const SCHEME: &str = "regbuf-test";

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
