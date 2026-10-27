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
    /// The volume is exFAT, and this `fatd` was built without the `exfat` feature
    /// (features.toml `fs.exfat`).
    #[error("the volume is exFAT, and fatd was built without exFAT support")]
    ExFatDisabled,
    /// The exFAT reader refused the volume, or failed reading it.
    #[error("cannot read the exFAT volume")]
    #[cfg(feature = "exfat")]
    ExFat(#[source] ferric_exfat::Error<io::Error>),
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
            Self::ExFatDisabled => EOPNOTSUPP,
            #[cfg(feature = "exfat")]
            Self::ExFat(ferric_exfat::Error::Device(err)) => io_errno(err),
            #[cfg(feature = "exfat")]
            Self::ExFat(err) if err.is_mount_error() => EINVAL,
            // Damage found while reading: a bad chain, cluster or directory entry set.
            #[cfg(feature = "exfat")]
            Self::ExFat(_) => EIO,
        }
    }

    /// An error of the exFAT reader, with the device's error carried as an [`io::Error`]
    /// like `fatfs` carries it.
    #[cfg(feature = "exfat")]
    pub fn from_exfat<E>(err: ferric_exfat::Error<E>) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::ExFat(err.map_device(io::Error::other))
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
    #[cfg(feature = "exfat")]
    fn exfat_errors_map_by_kind() {
        use ferric_exfat::{BootSectorError, Error as ExFatError};
        let mount = Error::from_exfat::<io::Error>(ExFatError::BootSector(BootSectorError::Layout));
        assert_eq!(mount.errno(), EINVAL);
        assert_eq!(
            Error::from_exfat::<io::Error>(ExFatError::Chain { first: 5 }).errno(),
            EIO
        );
        // A device's own error arrives wrapped, as through `Disk`, and keeps its source.
        let device = Error::from_exfat(ExFatError::Device(ferric_blocklayer::OutOfRange {
            offset: 0,
            len: 1,
            size: 0,
        }));
        assert_eq!(device.errno(), EIO);
        assert!(std::error::Error::source(&device).is_some());
        assert_eq!(Error::ExFatDisabled.errno(), EOPNOTSUPP);
    }

    #[test]
    fn damaged_volumes_are_eio_and_keep_the_source() {
        use std::error::Error as _;
        let err = Error::Read(io::Error::new(io::ErrorKind::InvalidData, "bad cluster"));
        assert_eq!(err.errno(), EIO);
        assert!(err.source().is_some());
    }
}
