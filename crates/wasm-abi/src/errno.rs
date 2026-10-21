//! Results of `ferric:sys` imports.
//!
//! An import that can fail returns an `i64`: a value in `0..=i64::MAX` on success, or
//! `-errno` on failure. Error numbers are Redox's (`redox_syscall::error`), which are the
//! traditional Unix numbers, so wasi-libc and relibc can pass them through.

use core::fmt;

/// An error number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Errno(u16);

impl Errno {
    /// Operation not permitted.
    pub const EPERM: Self = Self(1);
    /// No such file or directory.
    pub const ENOENT: Self = Self(2);
    /// No such process.
    pub const ESRCH: Self = Self(3);
    /// Interrupted call.
    pub const EINTR: Self = Self(4);
    /// I/O error.
    pub const EIO: Self = Self(5);
    /// Argument list too long.
    pub const E2BIG: Self = Self(7);
    /// Bad file descriptor.
    pub const EBADF: Self = Self(9);
    /// No child processes.
    pub const ECHILD: Self = Self(10);
    /// Try again.
    pub const EAGAIN: Self = Self(11);
    /// Out of memory.
    pub const ENOMEM: Self = Self(12);
    /// Permission denied.
    pub const EACCES: Self = Self(13);
    /// Bad address: a guest pointer outside linear memory.
    pub const EFAULT: Self = Self(14);
    /// Device or resource busy.
    pub const EBUSY: Self = Self(16);
    /// File exists.
    pub const EEXIST: Self = Self(17);
    /// Not a directory.
    pub const ENOTDIR: Self = Self(20);
    /// Is a directory.
    pub const EISDIR: Self = Self(21);
    /// Invalid argument.
    pub const EINVAL: Self = Self(22);
    /// Too many open files.
    pub const EMFILE: Self = Self(24);
    /// Result too large.
    pub const ERANGE: Self = Self(34);
    /// Function not implemented.
    pub const ENOSYS: Self = Self(38);
    /// Bad message (a code cache entry whose MAC does not match).
    pub const EBADMSG: Self = Self(74);
    /// Value too large for its type.
    pub const EOVERFLOW: Self = Self(75);
    /// Operation not supported.
    pub const EOPNOTSUPP: Self = Self(95);
    /// Timed out.
    pub const ETIMEDOUT: Self = Self(110);

    /// The largest error number: results in `-4095..=-1` are errors, as on Redox.
    pub const MAX: u16 = 4095;

    /// An error number, if it is in `1..=MAX`.
    pub const fn new(errno: u16) -> Option<Self> {
        if errno == 0 || errno > Self::MAX {
            None
        } else {
            Some(Self(errno))
        }
    }

    /// The number.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Errno({})", self.0)
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error {}", self.0)
    }
}

impl core::error::Error for Errno {}

/// Encodes an import's result. A value too large for `i64` becomes `EOVERFLOW`.
pub fn encode_result(result: Result<u64, Errno>) -> i64 {
    match result {
        Ok(value) => i64::try_from(value).unwrap_or(-i64::from(Errno::EOVERFLOW.get())),
        Err(errno) => -i64::from(errno.get()),
    }
}

/// Decodes an import's result, as guest bindings do. A negative value outside
/// `-MAX..=-1` cannot be produced by [`encode_result`] and decodes as `EINVAL`.
pub fn decode_result(raw: i64) -> Result<u64, Errno> {
    if let Ok(value) = u64::try_from(raw) {
        return Ok(value);
    }
    let errno = raw
        .checked_neg()
        .and_then(|errno| u16::try_from(errno).ok())
        .and_then(Errno::new);
    Err(errno.unwrap_or(Errno::EINVAL))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_round_trip() {
        for value in [0, 1, 4096, i64::MAX as u64] {
            assert_eq!(decode_result(encode_result(Ok(value))), Ok(value));
        }
        for errno in [Errno::EPERM, Errno::EFAULT, Errno::new(Errno::MAX).unwrap()] {
            assert_eq!(encode_result(Err(errno)), -i64::from(errno.get()));
            assert_eq!(decode_result(encode_result(Err(errno))), Err(errno));
        }
        assert_eq!(encode_result(Ok(u64::MAX)), -75);
        assert_eq!(decode_result(-5000), Err(Errno::EINVAL));
        assert_eq!(decode_result(i64::MIN), Err(Errno::EINVAL));
        assert_eq!(Errno::new(0), None);
        assert_eq!(Errno::new(4096), None);
    }
}
