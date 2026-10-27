//! [`enum@Error`], and [`Error::errno`], the one place an error becomes the errno a scheme call
//! returns.

use std::io;

use syscall::error::{
    EBADF, EINVAL, EIO, EISDIR, ENAMETOOLONG, ENOENT, ENOTDIR, EOPNOTSUPP, EROFS,
};
use thiserror::Error;

/// Everything that can go wrong in `fatd`'s file system layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The call would change the file system, which `fatd` serves read-only.
    #[error("the FAT file system is read-only")]
    ReadOnly,
    /// No open file has this handle.
    #[error("no open file has handle {handle}")]
    BadHandle {
        /// The handle asked for.
        handle: usize,
    },
    /// No directory entry has this name.
    #[error("no such file or directory")]
    NotFound,
    /// A directory call on a file that is not one, or a path that goes through a file.
    #[error("not a directory")]
    NotADirectory,
    /// A file call (a read) on a directory.
    #[error("is a directory")]
    IsADirectory,
    /// `O_SYMLINK` asked for a symbolic link; FAT has none.
    #[error("not a symbolic link")]
    NotASymlink,
    /// The path is longer than [`crate::fs::PATH_MAX`] bytes.
    #[error("the path is too long")]
    PathTooLong,
    /// `fatfs` refused the volume: no FAT boot sector, or one with impossible values.
    #[error("not a FAT volume, or a damaged one")]
    Mount(#[source] io::Error),
    /// Reading the volume failed: the device, or a damaged allocation table or directory.
    #[error("cannot read the FAT volume")]
    Read(#[source] io::Error),
}

impl Error {
    /// The errno a scheme call returns for this error.
    pub fn errno(&self) -> i32 {
        match self {
            Self::ReadOnly => EROFS,
            Self::BadHandle { .. } => EBADF,
            Self::NotFound => ENOENT,
            Self::NotADirectory => ENOTDIR,
            Self::IsADirectory => EISDIR,
            Self::NotASymlink => EINVAL,
            Self::PathTooLong => ENAMETOOLONG,
            Self::Mount(_) => EINVAL,
            Self::Read(err) => io_errno(err),
        }
    }
}

/// What a failed read from `fatfs` means to a caller. `fatfs` reports a damaged volume with
/// `InvalidData` or untyped errors, and the device's own failures arrive wrapped in `Other`.
fn io_errno(err: &io::Error) -> i32 {
    match err.kind() {
        io::ErrorKind::NotFound => ENOENT,
        io::ErrorKind::InvalidInput => EINVAL,
        io::ErrorKind::Unsupported => EOPNOTSUPP,
        io::ErrorKind::ReadOnlyFilesystem => EROFS,
        _ => EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_erofs() {
        assert_eq!(Error::ReadOnly.errno(), EROFS);
        let refused = io::Error::from(io::ErrorKind::ReadOnlyFilesystem);
        assert_eq!(Error::Read(refused).errno(), EROFS);
    }

    #[test]
    fn lookups_map_to_their_errno() {
        assert_eq!(Error::NotFound.errno(), ENOENT);
        assert_eq!(Error::NotADirectory.errno(), ENOTDIR);
        assert_eq!(Error::IsADirectory.errno(), EISDIR);
        assert_eq!(Error::PathTooLong.errno(), ENAMETOOLONG);
        assert_eq!(Error::BadHandle { handle: 3 }.errno(), EBADF);
    }

    #[test]
    fn damaged_volumes_are_eio_and_keep_the_source() {
        use std::error::Error as _;
        let err = Error::Read(io::Error::new(io::ErrorKind::InvalidData, "bad cluster"));
        assert_eq!(err.errno(), EIO);
        assert!(err.source().is_some());
    }
}
