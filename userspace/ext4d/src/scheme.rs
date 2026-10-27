//! The `ext4d` scheme: Redox scheme calls on top of [`Fs`].
//!
//! The scheme's handles are [`Fs`] handles. Reads, directory listings, `fstat`, `fstatvfs`
//! and `fpath` are served. On a volume mounted with `Fs::new_writable` (features.toml
//! `fs.ext4_write`), `futimens`, `fchmod` and `fchown` change the inode through the journal
//! ([`Fs::set_times`], [`Fs::set_mode`], [`Fs::set_owner`]), and a file opened for writing
//! is written ([`Fs::write_with_flags`], allocating the blocks it does not have yet).
//! `O_APPEND` opens and writes fail with `EOPNOTSUPP`: the current write response cannot
//! report an append's actual position to the kernel (issue #244). `O_CREAT` creates a
//! regular file, `O_TRUNC` and `ftruncate` resize one ([`Fs::truncate`]). Everything else
//! that would change the volume (`unlinkat`, `frename`, `flink`, creating directories and
//! links, and all of the above on a read-only mount) fails with `EROFS` or `EOPNOTSUPP`, as
//! [`Error::ReadOnly`] and [`Error::CreateKind`] say. Errors become errnos only through
//! [`Error::errno`]. Opens and
//! every change enforce the caller's UID/GID.

use ferric_abi::id::requested;
use redox_scheme::scheme::{SchemeState, SchemeSync, register_sync_scheme};
use redox_scheme::{CallerCtx, OpenResult, RequestKind, Response, SignalBehavior, Socket};
use syscall::data::{Stat, StatVfs, TimeSpec};
use syscall::dirent::{DirEntry, DirentBuf};
use syscall::error::{EINVAL, EOPNOTSUPP};
use syscall::schemev2::NewFdFlags;
use thiserror::Error;

use crate::Error;
use crate::fs::{Fs, OpenMode, TimeChange};

/// Why the scheme could not be set up or stopped serving.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServeError {
    /// Creating the scheme socket failed.
    #[error("cannot create the scheme socket")]
    Socket(#[source] syscall::Error),
    /// Registering the scheme name failed.
    #[error("cannot register the scheme {name}")]
    Register {
        /// The scheme name.
        name: String,
        /// The kernel's error.
        #[source]
        source: syscall::Error,
    },
    /// Entering the null namespace after registering failed.
    #[error("cannot enter the null namespace")]
    Namespace(#[source] libredox::error::Error),
    /// Taking a request off the socket, or answering one, failed.
    #[error("the scheme socket failed")]
    Request(#[source] syscall::Error),
}

/// The scheme handler for one ext4 volume.
pub struct Ext4Scheme {
    fs: Fs,
    /// `/scheme/NAME`, which `fpath` puts before paths.
    mounted: String,
}

/// A registered scheme and the socket its requests come in on.
pub struct Mount {
    socket: Socket,
    scheme: Ext4Scheme,
}

fn errno(err: &Error) -> syscall::Error {
    syscall::Error::new(err.errno())
}

impl Mount {
    /// Registers the scheme `name` for `fs`, then gives up the process's namespace: the
    /// device is already open, and the daemon needs nothing else.
    ///
    /// # Errors
    ///
    /// [`ServeError::Socket`], [`ServeError::Register`] (which includes failing to open the
    /// root directory as the scheme's root) and [`ServeError::Namespace`] for the step that
    /// failed.
    pub fn new(fs: Fs, name: &str) -> Result<Self, ServeError> {
        let socket = Socket::create().map_err(ServeError::Socket)?;
        let mut scheme = Ext4Scheme {
            fs,
            mounted: format!("/scheme/{name}"),
        };
        register_sync_scheme(&socket, name, &mut scheme).map_err(|source| {
            ServeError::Register {
                name: name.to_owned(),
                source,
            }
        })?;
        libredox::call::setrens(0, 0).map_err(ServeError::Namespace)?;
        Ok(Self { socket, scheme })
    }

    /// Serves requests until the scheme is closed.
    ///
    /// # Errors
    ///
    /// [`ServeError::Request`] when the socket fails.
    pub fn serve(mut self) -> Result<(), ServeError> {
        let mut state = SchemeState::new();
        loop {
            let Some(request) = self
                .socket
                .next_request(SignalBehavior::Restart)
                .map_err(ServeError::Request)?
            else {
                return Ok(());
            };
            let response = match request.kind() {
                RequestKind::Call(call) => call.handle_sync(&mut self.scheme, &mut state),
                RequestKind::SendFd(sendfd) => {
                    Response::new(Err(syscall::Error::new(EOPNOTSUPP)), sendfd)
                }
                RequestKind::OnClose { id } => {
                    self.scheme.fs.close(id);
                    state.on_close(id);
                    continue;
                }
                // Detach, cancellation and mapping messages need no answer from a read-only
                // file system that answers every call at once.
                _ => continue,
            };
            if !self
                .socket
                .write_response(response, SignalBehavior::Restart)
                .map_err(ServeError::Request)?
            {
                return Ok(());
            }
        }
    }
}

impl SchemeSync for Ext4Scheme {
    fn scheme_root(&mut self) -> syscall::Result<usize> {
        self.fs.root().map_err(|e| errno(&e))
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        flags: usize,
        _fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> syscall::Result<OpenResult> {
        let number = self
            .fs
            .open_as(
                Some(dirfd),
                path,
                OpenMode::from_flags(flags),
                ctx.uid,
                ctx.gid,
            )
            .map_err(|e| errno(&e))?;
        Ok(OpenResult::ThisScheme {
            number,
            flags: NewFdFlags::POSITIONED,
        })
    }

    fn unlinkat(
        &mut self,
        _fd: usize,
        _path: &str,
        _flags: usize,
        _ctx: &CallerCtx,
    ) -> syscall::Result<()> {
        Err(errno(&Error::ReadOnly))
    }

    fn inode(&self, id: usize) -> syscall::Result<usize> {
        let ino = self.fs.attributes(id).map_err(|e| errno(&e))?.ino;
        usize::try_from(ino).map_err(|_| syscall::Error::new(EINVAL))
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        self.fs.read(id, offset, buf).map_err(|e| errno(&e))
    }

    fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        // The caller's permission was checked when the file was opened for writing.
        self.fs
            .write_with_flags(id, offset, buf, fcntl_flags)
            .map_err(|e| errno(&e))
    }

    fn fsize(&mut self, id: usize, _ctx: &CallerCtx) -> syscall::Result<u64> {
        Ok(self.fs.attributes(id).map_err(|e| errno(&e))?.size)
    }

    fn fchmod(&mut self, id: usize, mode: u16, ctx: &CallerCtx) -> syscall::Result<()> {
        self.fs
            .set_mode(id, mode, ctx.uid, ctx.gid)
            .map_err(|e| errno(&e))
    }

    fn fchown(&mut self, id: usize, uid: u32, gid: u32, ctx: &CallerCtx) -> syscall::Result<()> {
        let (uid, gid) = (requested(uid), requested(gid));
        self.fs
            .set_owner(id, uid, gid, ctx.uid, ctx.gid)
            .map_err(|e| errno(&e))
    }

    fn fcntl(
        &mut self,
        id: usize,
        _cmd: usize,
        _arg: usize,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        self.fs.attributes(id).map(|_| 0).map_err(|e| errno(&e))
    }

    fn flink(&mut self, id: usize, _path: &str, _ctx: &CallerCtx) -> syscall::Result<usize> {
        self.fs.refuse_change(id).map(|()| 0).map_err(|e| errno(&e))
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> syscall::Result<usize> {
        let path = self.fs.path(id).map_err(|e| errno(&e))?;
        let tail = if path == "/" { "" } else { path };
        let full = [self.mounted.as_bytes(), tail.as_bytes()].concat();
        let n = full.len().min(buf.len());
        buf[..n].copy_from_slice(&full[..n]);
        Ok(n)
    }

    fn frename(&mut self, id: usize, _path: &str, _ctx: &CallerCtx) -> syscall::Result<usize> {
        self.fs.refuse_change(id).map(|()| 0).map_err(|e| errno(&e))
    }

    fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> syscall::Result<()> {
        *stat = self.fs.attributes(id).map_err(|e| errno(&e))?.to_stat();
        Ok(())
    }

    fn fstatvfs(&mut self, id: usize, stat: &mut StatVfs, _ctx: &CallerCtx) -> syscall::Result<()> {
        self.fs.attributes(id).map_err(|e| errno(&e))?;
        *stat = self.fs.geometry().to_statvfs();
        Ok(())
    }

    fn fsync(&mut self, id: usize, _ctx: &CallerCtx) -> syscall::Result<()> {
        // Nothing is ever dirty.
        self.fs.attributes(id).map(|_| ()).map_err(|e| errno(&e))
    }

    fn ftruncate(&mut self, id: usize, len: u64, _ctx: &CallerCtx) -> syscall::Result<()> {
        // The caller's permission was checked when the file was opened for writing.
        self.fs.truncate(id, len).map_err(|e| errno(&e))
    }

    fn futimens(&mut self, id: usize, times: &[TimeSpec], ctx: &CallerCtx) -> syscall::Result<()> {
        // relibc sends both times, with UTIME_NOW for a null pointer.
        let &[accessed, modified] = times else {
            return Err(errno(&Error::InvalidTime));
        };
        let change = |time: TimeSpec| TimeChange::from_timespec(time.tv_sec, time.tv_nsec.into());
        let (accessed, modified) = change(accessed)
            .and_then(|a| Ok((a, change(modified)?)))
            .map_err(|e| errno(&e))?;
        self.fs
            .set_times(id, accessed, modified, ctx.uid, ctx.gid)
            .map_err(|e| errno(&e))
    }

    fn getdents<'buf>(
        &mut self,
        id: usize,
        mut buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
    ) -> syscall::Result<DirentBuf<&'buf mut [u8]>> {
        let entries = self.fs.entries(id).map_err(|e| errno(&e))?;
        let skip = usize::try_from(opaque_offset).unwrap_or(usize::MAX);
        for (index, entry) in entries.iter().enumerate().skip(skip) {
            let result = buf.entry(DirEntry {
                inode: entry.ino,
                next_opaque_id: u64::try_from(index + 1).unwrap_or(u64::MAX),
                name: &entry.name,
                kind: entry.kind.dirent_kind(),
            });
            match result {
                Ok(()) => {}
                // The buffer is full: POSIX allows a partial listing once one entry is in.
                Err(err) if err.errno == EINVAL && index > skip => break,
                Err(err) => return Err(err),
            }
        }
        Ok(buf)
    }
}
