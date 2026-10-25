//! The served volume: open handles, reads, and what every other call gets.
//!
//! The scheme has one file, the volume itself, at its root. Only root (UID 0) may open it,
//! since it is decrypted data, and only for reading: opens for writing, creating or
//! truncating, and every call that would change it, fail with `EROFS`. Errors become errnos
//! only in [`Error::errno`].
//!
//! The scheme root handle ([`ROOT`]) is only a directory to open the volume from: it serves
//! no data and no metadata (`EBADF`), so every read goes through a handle [`Volume::open`]
//! made after checking the caller.

use std::collections::BTreeSet;

use ferric_blocklayer::BlockDevice;
use syscall::data::Stat;
use syscall::error::{EACCES, EBADF, EIO, ENOENT, ENOTDIR, EROFS};
use syscall::flag::{MODE_FILE, O_ACCMODE, O_CREAT, O_DIRECTORY, O_RDONLY, O_TRUNC};
use thiserror::Error;

/// The handle number of the scheme root, which the volume is opened from.
pub const ROOT: usize = 0;

/// The block size `fstat` reports.
const BLOCK_SIZE: u32 = 4096;

/// A volume being served, and its open handles.
#[derive(Debug)]
pub struct Volume<D> {
    device: D,
    handles: BTreeSet<usize>,
    next: usize,
}

/// A scheme call on the volume that fails.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error<E: std::error::Error + 'static> {
    /// The call would change the volume.
    #[error("the volume is read-only")]
    ReadOnly,
    /// The caller is not root.
    #[error("only root may open the volume")]
    PermissionDenied,
    /// The path names something other than the volume.
    #[error("the scheme holds only the volume")]
    NotFound,
    /// The open asked for a directory.
    #[error("the volume is not a directory")]
    NotADirectory,
    /// No handle has this number.
    #[error("no open handle {handle}")]
    BadHandle {
        /// The number.
        handle: usize,
    },
    /// Reading the volume failed.
    #[error("reading the volume failed")]
    Device(#[source] E),
}

impl<E: std::error::Error + 'static> Error<E> {
    /// The errno a scheme call returns for this error.
    pub fn errno(&self) -> i32 {
        match self {
            Self::ReadOnly => EROFS,
            Self::PermissionDenied => EACCES,
            Self::NotFound => ENOENT,
            Self::NotADirectory => ENOTDIR,
            Self::BadHandle { .. } => EBADF,
            Self::Device(_) => EIO,
        }
    }
}

impl<D: BlockDevice> Volume<D> {
    /// Serves `device`.
    pub fn new(device: D) -> Self {
        Self {
            device,
            handles: BTreeSet::new(),
            next: ROOT + 1,
        }
    }

    /// The volume's size in bytes.
    pub fn size(&self) -> u64 {
        self.device.size()
    }

    /// Opens `path` (which must name the root, the volume) with Redox `openat` `flags`, for
    /// the caller `uid`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for any other path, [`Error::NotADirectory`] for `O_DIRECTORY`,
    /// [`Error::ReadOnly`] for opens that could write, create or truncate, and
    /// [`Error::PermissionDenied`] when `uid` is not 0.
    pub fn open(&mut self, path: &str, flags: usize, uid: u32) -> Result<usize, Error<D::Error>> {
        if !path.trim_matches('/').is_empty() {
            return Err(Error::NotFound);
        }
        if flags & O_DIRECTORY != 0 {
            return Err(Error::NotADirectory);
        }
        let access = flags & O_ACCMODE;
        if (access != O_RDONLY && access != 0) || flags & (O_CREAT | O_TRUNC) != 0 {
            return Err(Error::ReadOnly);
        }
        if uid != 0 {
            return Err(Error::PermissionDenied);
        }
        let handle = self.next;
        self.next += 1;
        self.handles.insert(handle);
        Ok(handle)
    }

    /// Checks that `dirfd` is the scheme root, the only handle the volume is opened from.
    ///
    /// # Errors
    ///
    /// [`Error::NotADirectory`] for a volume handle and [`Error::BadHandle`] for any other.
    pub fn check_root(&self, dirfd: usize) -> Result<(), Error<D::Error>> {
        if dirfd == ROOT {
            Ok(())
        } else if self.handles.contains(&dirfd) {
            Err(Error::NotADirectory)
        } else {
            Err(Error::BadHandle { handle: dirfd })
        }
    }

    /// Checks that `handle` is an open volume handle: one [`Volume::open`] made. The scheme
    /// root is not one.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`] when it is not.
    pub fn check(&self, handle: usize) -> Result<(), Error<D::Error>> {
        if self.handles.contains(&handle) {
            Ok(())
        } else {
            Err(Error::BadHandle { handle })
        }
    }

    /// Reads from byte `offset` into `buf`, stopping at the end of the volume; returns how
    /// many bytes were read (0 at or past the end).
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], and [`Error::Device`] when the layers below fail.
    pub fn read(
        &self,
        handle: usize,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error<D::Error>> {
        self.check(handle)?;
        let left = self.size().saturating_sub(offset);
        let n = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        if n > 0 {
            self.device
                .read_at(offset, &mut buf[..n])
                .map_err(Error::Device)?;
        }
        Ok(n)
    }

    /// Fails any change through `handle` with [`Error::ReadOnly`].
    ///
    /// # Errors
    ///
    /// Always: [`Error::BadHandle`] or [`Error::ReadOnly`].
    pub fn refuse_change(&self, handle: usize) -> Result<(), Error<D::Error>> {
        self.check(handle)?;
        Err(Error::ReadOnly)
    }

    /// The volume's `stat`: a root-only, read-only file of the volume's size.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`].
    pub fn stat(&self, handle: usize) -> Result<Stat, Error<D::Error>> {
        self.check(handle)?;
        let size = self.size();
        Ok(Stat {
            st_mode: MODE_FILE | 0o400,
            st_nlink: 1,
            st_size: size,
            st_blksize: BLOCK_SIZE,
            st_blocks: size.div_ceil(512),
            ..Stat::default()
        })
    }

    /// Closes `handle`.
    pub fn close(&mut self, handle: usize) {
        self.handles.remove(&handle);
    }
}

#[cfg(test)]
mod tests {
    use ferric_blocklayer::{Memory, OutOfRange};
    use syscall::flag::{O_RDWR, O_STAT, O_WRONLY};

    use super::*;

    fn volume() -> Volume<Memory<Vec<u8>>> {
        Volume::new(Memory::new((0..100).collect()))
    }

    #[test]
    fn root_opens_and_reads_to_the_end() {
        let mut v = volume();
        let h = v.open("", O_RDONLY, 0).unwrap();
        assert_ne!(h, ROOT);
        assert!(v.open("/", O_RDONLY | O_STAT, 0).unwrap() > ROOT);
        let mut buf = [0; 8];
        assert_eq!(v.read(h, 95, &mut buf).unwrap(), 5);
        assert_eq!(buf[..5], [95, 96, 97, 98, 99]);
        assert_eq!(v.read(h, 100, &mut buf).unwrap(), 0);
        assert_eq!(v.read(h, u64::MAX, &mut buf).unwrap(), 0);
        let stat = v.stat(h).unwrap();
        assert_eq!((stat.st_size, stat.st_mode), (100, MODE_FILE | 0o400));
        v.close(h);
        assert_eq!(v.read(h, 0, &mut buf).unwrap_err().errno(), EBADF);
        assert_eq!(v.check_root(ROOT).map_err(|e| e.errno()), Ok(()));
        let h = v.open("", O_RDONLY, 0).unwrap();
        assert_eq!(v.check_root(h).unwrap_err().errno(), ENOTDIR);
        assert_eq!(v.check_root(99).unwrap_err().errno(), EBADF);
    }

    /// The scheme root serves nothing, so a process that is not root cannot read the volume
    /// through it, whatever handle it holds; it must open the volume, which refuses it.
    #[test]
    fn the_root_handle_serves_no_data() {
        let mut v = volume();
        let mut buf = [0; 8];
        assert_eq!(v.read(ROOT, 0, &mut buf).unwrap_err().errno(), EBADF);
        assert_eq!(v.stat(ROOT).unwrap_err().errno(), EBADF);
        assert_eq!(v.refuse_change(ROOT).unwrap_err().errno(), EBADF);
        assert_eq!(v.open("", O_RDONLY, 1000).unwrap_err().errno(), EACCES);
        assert_eq!(buf, [0; 8]);
        let h = v.open("", O_RDONLY, 0).unwrap();
        v.close(h);
        assert_eq!(v.read(h, 0, &mut buf).unwrap_err().errno(), EBADF);
    }

    #[test]
    fn changes_and_other_users_are_refused() {
        let mut v = volume();
        let errno = |r: Result<usize, Error<OutOfRange>>| r.unwrap_err().errno();
        assert_eq!(errno(v.open("", O_WRONLY, 0)), EROFS);
        assert_eq!(errno(v.open("", O_RDWR, 0)), EROFS);
        assert_eq!(errno(v.open("", O_RDONLY | O_CREAT, 0)), EROFS);
        assert_eq!(errno(v.open("", O_RDONLY | O_TRUNC, 0)), EROFS);
        assert_eq!(errno(v.open("", O_RDONLY | O_DIRECTORY, 0)), ENOTDIR);
        assert_eq!(errno(v.open("other", O_RDONLY, 0)), ENOENT);
        assert_eq!(errno(v.open("", O_RDONLY, 1000)), EACCES);
        let h = v.open("", O_RDONLY, 0).unwrap();
        assert_eq!(v.refuse_change(h).unwrap_err().errno(), EROFS);
        assert_eq!(v.refuse_change(9).unwrap_err().errno(), EBADF);
    }

    #[test]
    fn device_errors_are_eio_and_keep_their_source() {
        let err: Error<OutOfRange> = Error::Device(OutOfRange {
            offset: 0,
            len: 1,
            size: 0,
        });
        assert_eq!(err.errno(), EIO);
        assert!(std::error::Error::source(&err).is_some());
    }
}
