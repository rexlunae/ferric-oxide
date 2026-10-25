//! The `ferric-blockd` scheme: Redox scheme calls on top of [`Volume`].
//!
//! The volume is opened from the scheme root, which itself serves nothing (see
//! [`crate::volume`]), so every read passes the caller check in `openat`. Reads, `fsize`,
//! `fstat`, `fpath`, `fsync` and `fcntl` are served on volume handles; writes, `ftruncate`, `fchmod`, `fchown` and `futimens` fail with `EROFS`. Errors
//! become errnos only through [`Error::errno`](crate::volume::Error::errno).

use redox_scheme::scheme::{SchemeState, SchemeSync, register_sync_scheme};
use redox_scheme::{CallerCtx, OpenResult, RequestKind, Response, SignalBehavior, Socket};
use syscall::data::{Stat, TimeSpec};
use syscall::error::EOPNOTSUPP;
use syscall::schemev2::NewFdFlags;
use thiserror::Error;

use crate::volume::{self, ROOT, Volume};
use ferric_blocklayer::BlockDevice;

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

/// The scheme handler for one volume.
pub struct BlockScheme<D> {
    volume: Volume<D>,
    /// `/scheme/NAME`, what `fpath` answers.
    mounted: String,
}

/// A registered scheme and the socket its requests come in on.
pub struct Mount<D> {
    socket: Socket,
    scheme: BlockScheme<D>,
}

fn errno<E: std::error::Error + 'static>(err: &volume::Error<E>) -> syscall::Error {
    syscall::Error::new(err.errno())
}

impl<D: BlockDevice> Mount<D> {
    /// Registers the scheme `name` for `volume`, then gives up the process's namespace: the
    /// devices are open and the key files read, and the daemon needs nothing else.
    ///
    /// # Errors
    ///
    /// [`ServeError::Socket`], [`ServeError::Register`] and [`ServeError::Namespace`] for the
    /// step that failed.
    pub fn new(volume: Volume<D>, name: &str) -> Result<Self, ServeError> {
        let socket = Socket::create().map_err(ServeError::Socket)?;
        let mut scheme = BlockScheme {
            volume,
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
                    self.scheme.volume.close(id);
                    state.on_close(id);
                    continue;
                }
                // Detach, cancellation and mapping messages need no answer from a read-only
                // volume that answers every call at once.
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

impl<D: BlockDevice> SchemeSync for BlockScheme<D> {
    fn scheme_root(&mut self) -> syscall::Result<usize> {
        Ok(ROOT)
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        flags: usize,
        _fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> syscall::Result<OpenResult> {
        self.volume.check_root(dirfd).map_err(|e| errno(&e))?;
        let number = self
            .volume
            .open(path, flags, ctx.uid)
            .map_err(|e| errno(&e))?;
        Ok(OpenResult::ThisScheme {
            number,
            flags: NewFdFlags::POSITIONED,
        })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        self.volume.read(id, offset, buf).map_err(|e| errno(&e))
    }

    fn write(
        &mut self,
        id: usize,
        _buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        self.volume
            .refuse_change(id)
            .map(|()| 0)
            .map_err(|e| errno(&e))
    }

    fn fsize(&mut self, id: usize, _ctx: &CallerCtx) -> syscall::Result<u64> {
        self.volume.check(id).map_err(|e| errno(&e))?;
        Ok(self.volume.size())
    }

    fn fchmod(&mut self, id: usize, _mode: u16, _ctx: &CallerCtx) -> syscall::Result<()> {
        self.volume.refuse_change(id).map_err(|e| errno(&e))
    }

    fn fchown(&mut self, id: usize, _uid: u32, _gid: u32, _ctx: &CallerCtx) -> syscall::Result<()> {
        self.volume.refuse_change(id).map_err(|e| errno(&e))
    }

    fn fcntl(
        &mut self,
        id: usize,
        _cmd: usize,
        _arg: usize,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        self.volume.check(id).map(|()| 0).map_err(|e| errno(&e))
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> syscall::Result<usize> {
        self.volume.check(id).map_err(|e| errno(&e))?;
        let path = self.mounted.as_bytes();
        let n = path.len().min(buf.len());
        buf[..n].copy_from_slice(&path[..n]);
        Ok(n)
    }

    fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> syscall::Result<()> {
        *stat = self.volume.stat(id).map_err(|e| errno(&e))?;
        Ok(())
    }

    fn fsync(&mut self, id: usize, _ctx: &CallerCtx) -> syscall::Result<()> {
        // Nothing is ever dirty.
        self.volume.check(id).map_err(|e| errno(&e))
    }

    fn ftruncate(&mut self, id: usize, _len: u64, _ctx: &CallerCtx) -> syscall::Result<()> {
        self.volume.refuse_change(id).map_err(|e| errno(&e))
    }

    fn futimens(
        &mut self,
        id: usize,
        _times: &[TimeSpec],
        _ctx: &CallerCtx,
    ) -> syscall::Result<()> {
        self.volume.refuse_change(id).map_err(|e| errno(&e))
    }
}
