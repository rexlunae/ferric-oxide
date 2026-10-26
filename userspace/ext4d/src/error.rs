//! [`enum@Error`], and [`Error::errno`], the one place an error becomes the errno a scheme call
//! returns.

use ext4_view::Ext4Error;
use syscall::error::{
    EACCES, EBADF, EFBIG, EILSEQ, EINVAL, EIO, EISDIR, ELOOP, ENAMETOOLONG, ENOENT, ENOTDIR,
    EOPNOTSUPP, EPERM, EROFS,
};
use thiserror::Error;

use crate::device::WindowError;
use crate::superblock::SuperblockError;

/// Everything that can go wrong in `ext4d`'s file system layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The call would change the file system, which `ext4d` serves read-only.
    #[error("the ext4 file system is read-only")]
    ReadOnly,
    /// No open file has this handle.
    #[error("no open file has handle {handle}")]
    BadHandle {
        /// The handle asked for.
        handle: usize,
    },
    /// A directory call on a file that is not one.
    #[error("not a directory")]
    NotADirectory,
    /// A file call (a read) on a directory.
    #[error("is a directory")]
    IsADirectory,
    /// `O_NOFOLLOW` without `O_SYMLINK` or `O_STAT` named a symbolic link.
    #[error("the path names a symbolic link and O_NOFOLLOW is set")]
    SymlinkNotFollowed,
    /// The caller cannot search a directory or read the final inode.
    #[error("permission denied")]
    PermissionDenied,
    /// `O_SYMLINK` named an object that is not a symbolic link.
    #[error("not a symbolic link")]
    NotASymlink,
    /// The superblock could not be read.
    #[error("cannot read the superblock")]
    Device(#[source] WindowError),
    /// The superblock was refused.
    #[error("bad superblock")]
    Superblock(#[from] SuperblockError),
    /// The ext4 reader failed.
    #[error("ext4")]
    Ext4(#[from] Ext4Error),
    /// Setting explicit times on a file the caller does not own (POSIX `futimens`: only
    /// `UTIME_NOW` for both times is allowed with write permission alone).
    #[error("only the owner can set a file's times")]
    NotOwner,
    /// A `futimens` time whose nanoseconds are out of range and not `UTIME_NOW` or
    /// `UTIME_OMIT`, or a call without exactly two times.
    #[error("invalid time")]
    InvalidTime,
    /// The path's inode could not be found for a change (`write`).
    #[cfg(feature = "write")]
    #[error("cannot find the inode to change")]
    Lookup(#[from] crate::lookup::LookupError),
    /// Changing the volume through its journal failed (`write`).
    #[cfg(feature = "write")]
    #[error("cannot change the volume")]
    Write(#[from] crate::write::WriteError),
}

impl Error {
    /// The errno a scheme call returns for this error.
    pub fn errno(&self) -> i32 {
        match self {
            Self::ReadOnly => EROFS,
            Self::PermissionDenied => EACCES,
            Self::NotASymlink => EINVAL,
            Self::BadHandle { .. } => EBADF,
            Self::NotADirectory => ENOTDIR,
            Self::IsADirectory => EISDIR,
            Self::SymlinkNotFollowed => ELOOP,
            Self::Device(_) => EIO,
            Self::Superblock(_) => EINVAL,
            Self::Ext4(err) => ext4_errno(err),
            Self::NotOwner => EPERM,
            Self::InvalidTime => EINVAL,
            #[cfg(feature = "write")]
            Self::Lookup(err) => lookup_errno(err),
            #[cfg(feature = "write")]
            Self::Write(err) => write_errno(err),
        }
    }
}

#[cfg(feature = "write")]
fn lookup_errno(err: &crate::lookup::LookupError) -> i32 {
    use crate::lookup::LookupError;
    match err {
        LookupError::NotFound { .. } => ENOENT,
        LookupError::NotADirectory { .. } => ENOTDIR,
        LookupError::Unsupported { .. } => EOPNOTSUPP,
        // Malformed volumes and device failures, and anything added later.
        _ => EIO,
    }
}

#[cfg(feature = "write")]
fn write_errno(err: &crate::write::WriteError) -> i32 {
    use crate::write::WriteError;
    match err {
        // The volume cannot be written as it is (checked at mount, but it can change).
        WriteError::NeedsReplay | WriteError::NoJournal | WriteError::UnsupportedFeature { .. } => {
            EROFS
        }
        WriteError::TimeOutOfRange { .. } => EINVAL,
        // Damaged inodes, journals and devices, and anything added later.
        _ => EIO,
    }
}

fn ext4_errno(err: &Ext4Error) -> i32 {
    match err {
        Ext4Error::NotFound => ENOENT,
        Ext4Error::NotADirectory => ENOTDIR,
        Ext4Error::IsADirectory => EISDIR,
        Ext4Error::NotAbsolute | Ext4Error::MalformedPath | Ext4Error::NotASymlink => EINVAL,
        Ext4Error::PathTooLong => ENAMETOOLONG,
        Ext4Error::TooManySymlinks => ELOOP,
        Ext4Error::FileTooLarge => EFBIG,
        Ext4Error::NotUtf8 => EILSEQ,
        // Device files, encrypted files and volumes with features the reader lacks.
        Ext4Error::IsASpecialFile | Ext4Error::Encrypted | Ext4Error::Incompatible(_) => EOPNOTSUPP,
        // I/O failures and corrupt volumes, and anything a newer ext4-view adds.
        _ => EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_erofs() {
        assert_eq!(Error::ReadOnly.errno(), EROFS);
    }

    #[test]
    fn lookups_map_to_their_errno() {
        assert_eq!(Error::from(Ext4Error::NotFound).errno(), ENOENT);
        assert_eq!(Error::from(Ext4Error::NotADirectory).errno(), ENOTDIR);
        assert_eq!(Error::from(Ext4Error::TooManySymlinks).errno(), ELOOP);
        assert_eq!(Error::from(Ext4Error::PathTooLong).errno(), ENAMETOOLONG);
        assert_eq!(Error::BadHandle { handle: 3 }.errno(), EBADF);
        assert_eq!(Error::IsADirectory.errno(), EISDIR);
    }

    #[test]
    fn times_errors_map_to_their_errno() {
        assert_eq!(Error::NotOwner.errno(), EPERM);
        assert_eq!(Error::InvalidTime.errno(), EINVAL);
    }

    #[cfg(feature = "write")]
    #[test]
    fn write_errors_map_to_their_errno() {
        use crate::lookup::LookupError;
        use crate::write::WriteError;
        let not_found = LookupError::NotFound {
            directory: 2,
            name: "x".to_owned(),
        };
        assert_eq!(Error::from(not_found).errno(), ENOENT);
        let unsupported = LookupError::Unsupported {
            number: 2,
            what: "inline-data directories",
        };
        assert_eq!(Error::from(unsupported).errno(), EOPNOTSUPP);
        assert_eq!(Error::from(WriteError::NoJournal).errno(), EROFS);
        assert_eq!(Error::from(WriteError::NeedsReplay).errno(), EROFS);
        let late = WriteError::TimeOutOfRange {
            number: 12,
            seconds: 1 << 40,
            nanoseconds: 0,
        };
        assert_eq!(Error::from(late).errno(), EINVAL);
        assert_eq!(
            Error::from(WriteError::FreeInode { number: 12 }).errno(),
            EIO
        );
    }

    #[test]
    fn the_source_is_kept() {
        use std::error::Error as _;
        let err = Error::from(Ext4Error::NotFound);
        assert!(err.source().is_some());
    }
}
