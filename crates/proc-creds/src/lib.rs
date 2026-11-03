//! `ferric-proc-creds`: the credentials of the process behind a scheme request, from
//! procmgr.
//!
//! A request made through `SYS_CALL`, such as a `StdFsCall` for `fchmod` or `futimens`,
//! carries only the caller's pid: the kernel sends no IDs with it, so its `CallerCtx` uid
//! and gid read as zero, which looks like root. A scheme that checks permissions on such a
//! request asks procmgr for the caller's credentials instead, through a handle on
//! [`CAPABILITY_PATH`]. Permission checks use the effective IDs ([`EffectiveIds`]).
//!
//! - [`decode`] reads procmgr's reply, a [`ProcMeta`]. It runs on the host too.
//! - [`proc_credentials`] asks for a process's credentials through a raw descriptor. It
//!   builds on every target, so host builds of the schemes that call it (`ramfs` and
//!   `ipcd`, through `scheme-utils`) compile and test; off Redox, where there is no
//!   procmgr, it fails with [`Error::Unsupported`].
//! - On Redox, `Capability` holds the handle and asks through it.
//!
//! Requests the kernel itself forwards with the caller's IDs (`openat`, the `SYS_FCHOWN`
//! path) need none of this: their `CallerCtx` is already right.
//!
//! # Examples
//!
//! ```
//! use ferric_proc_creds::{EffectiveIds, ProcMeta, decode};
//!
//! // What procmgr writes: a ProcMeta, eleven native-endian u32 fields.
//! let fields = [7, 7, 1, 0, 1000, 0, 0, 100, 0, 1, 1];
//! let reply: Vec<u8> = fields.iter().flat_map(|f: &u32| f.to_ne_bytes()).collect();
//! let meta: ProcMeta = decode(&reply)?;
//! assert_eq!(EffectiveIds::from(&meta), EffectiveIds { uid: 1000, gid: 100 });
//! # Ok::<(), ferric_proc_creds::Error>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub use libredox::protocol::ProcMeta;
use syscall::{EINVAL, ENOSYS};
use thiserror::Error;

/// The procmgr handle a scheme opens, while it still has a namespace, to ask for
/// credentials.
pub const CAPABILITY_PATH: &str = "/scheme/proc/proc-creds-capability";

/// Why a caller's credentials could not be had.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Opening [`CAPABILITY_PATH`] failed.
    #[error("cannot open {CAPABILITY_PATH}")]
    Open(#[source] libredox::error::Error),
    /// procmgr refused the request (`ESRCH` for a process that is gone).
    #[error("procmgr did not report the credentials of process {pid}")]
    Lookup {
        /// The process asked about.
        pid: usize,
        /// procmgr's error.
        #[source]
        source: libredox::error::Error,
    },
    /// procmgr's reply is shorter than a [`ProcMeta`].
    #[error("procmgr's reply is {len} bytes, shorter than a ProcMeta")]
    ShortReply {
        /// The reply's length.
        len: usize,
    },
    /// Not on Redox: there is no procmgr to ask.
    #[error("cannot ask for the credentials of process {pid}: procmgr exists only on Redox")]
    Unsupported {
        /// The process asked about.
        pid: usize,
    },
}

impl Error {
    /// The errno a scheme answers with: the underlying call's for [`Error::Open`] and
    /// [`Error::Lookup`], `EINVAL` for [`Error::ShortReply`] and `ENOSYS` for
    /// [`Error::Unsupported`].
    pub fn errno(&self) -> i32 {
        match self {
            Self::Open(source) | Self::Lookup { source, .. } => source.errno(),
            Self::ShortReply { .. } => EINVAL,
            Self::Unsupported { .. } => ENOSYS,
        }
    }
}

impl From<Error> for syscall::Error {
    fn from(err: Error) -> Self {
        Self::new(err.errno())
    }
}

/// The IDs permission checks use: a process's effective user and group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveIds {
    /// The effective user ID.
    pub uid: u32,
    /// The effective group ID.
    pub gid: u32,
}

impl From<&ProcMeta> for EffectiveIds {
    fn from(meta: &ProcMeta) -> Self {
        Self {
            uid: meta.euid,
            gid: meta.egid,
        }
    }
}

/// The [`ProcMeta`] at the start of `bytes`, procmgr's reply.
///
/// # Errors
///
/// [`Error::ShortReply`] when `bytes` is shorter than a [`ProcMeta`].
pub fn decode(bytes: &[u8]) -> Result<ProcMeta, Error> {
    // Copied rather than cast in place: a byte buffer need not be aligned for ProcMeta.
    let mut meta = ProcMeta::default();
    plain::copy_from_bytes(&mut meta, bytes).map_err(|_| Error::ShortReply { len: bytes.len() })?;
    Ok(meta)
}

/// The credentials procmgr reports for `pid`, asked through the [`CAPABILITY_PATH`]
/// handle `cap_fd`.
///
/// # Errors
///
/// [`Error::Lookup`] with procmgr's error, or [`Error::ShortReply`] when the reply is not a
/// whole [`ProcMeta`]. Off Redox, always [`Error::Unsupported`].
#[cfg(target_os = "redox")]
pub fn proc_credentials(cap_fd: usize, pid: usize) -> Result<ProcMeta, Error> {
    let mut buffer = [0u8; size_of::<ProcMeta>()];
    let len = libredox::call::get_proc_credentials(cap_fd, pid, &mut buffer)
        .map_err(|source| Error::Lookup { pid, source })?;
    decode(buffer.get(..len).ok_or(Error::ShortReply { len })?)
}

/// The credentials procmgr reports for `pid`: off Redox there is no procmgr, so this
/// fails.
///
/// # Errors
///
/// Always [`Error::Unsupported`].
#[cfg(not(target_os = "redox"))]
pub fn proc_credentials(_cap_fd: usize, pid: usize) -> Result<ProcMeta, Error> {
    Err(Error::Unsupported { pid })
}

/// An open [`CAPABILITY_PATH`] handle.
#[cfg(target_os = "redox")]
pub struct Capability(libredox::Fd);

#[cfg(target_os = "redox")]
impl Capability {
    /// Opens [`CAPABILITY_PATH`]. A scheme does this before it leaves its namespace.
    ///
    /// # Errors
    ///
    /// [`Error::Open`] when the open fails.
    pub fn open() -> Result<Self, Error> {
        use libredox::flag::{O_CLOEXEC, O_RDONLY};
        libredox::Fd::open(CAPABILITY_PATH, O_RDONLY | O_CLOEXEC, 0)
            .map(Self)
            .map_err(Error::Open)
    }

    /// The credentials procmgr reports for `pid`.
    ///
    /// # Errors
    ///
    /// As [`proc_credentials`].
    pub fn credentials(&self, pid: usize) -> Result<ProcMeta, Error> {
        proc_credentials(self.0.raw(), pid)
    }

    /// The effective IDs of `pid`, which permission checks use.
    ///
    /// # Errors
    ///
    /// As [`proc_credentials`].
    pub fn effective_ids(&self, pid: usize) -> Result<EffectiveIds, Error> {
        self.credentials(pid).map(|meta| EffectiveIds::from(&meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syscall::{ENOENT, ESRCH};

    const FIELDS: [u32; 11] = [7, 7, 1, 0, 1000, 0, 0, 100, 0, 1, 1];

    fn reply(fields: &[u32]) -> Vec<u8> {
        fields.iter().flat_map(|f| f.to_ne_bytes()).collect()
    }

    /// The reply covers every ProcMeta field, so a layout change shows up here.
    #[test]
    fn a_reply_is_one_procmeta() {
        assert_eq!(size_of::<ProcMeta>(), reply(&FIELDS).len());
    }

    /// The effective IDs come from their own fields, not the real or saved ones beside
    /// them.
    #[test]
    fn effective_ids_are_read_from_their_fields() {
        let meta = decode(&reply(&FIELDS)).unwrap();
        assert_eq!((meta.pid, meta.ruid, meta.rgid), (7, 0, 0));
        assert_eq!(
            EffectiveIds::from(&meta),
            EffectiveIds {
                uid: 1000,
                gid: 100
            }
        );
    }

    #[test]
    fn a_short_reply_is_invalid() {
        let err = decode(&[0; 4]).unwrap_err();
        assert!(matches!(err, Error::ShortReply { len: 4 }));
        assert_eq!(syscall::Error::from(err).errno, EINVAL);
    }

    #[test]
    fn an_unaligned_reply_is_read() {
        let mut bytes = vec![0];
        bytes.extend(reply(&FIELDS));
        assert_eq!(decode(&bytes[1..]).unwrap().egid, 100);
    }

    #[test]
    fn a_longer_reply_is_read_from_its_start() {
        let mut bytes = reply(&FIELDS);
        bytes.extend_from_slice(&[0xff; 8]);
        assert_eq!(decode(&bytes).unwrap().euid, 1000);
    }

    /// The errno of a failed call reaches the scheme's caller unchanged.
    #[test]
    fn call_errors_keep_their_errno() {
        let lookup = Error::Lookup {
            pid: 9,
            source: libredox::error::Error::new(ESRCH),
        };
        assert_eq!(lookup.errno(), ESRCH);
        let open = Error::Open(libredox::error::Error::new(ENOENT));
        assert_eq!(syscall::Error::from(open).errno, ENOENT);
    }

    #[cfg(not(target_os = "redox"))]
    #[test]
    fn off_redox_there_is_no_procmgr_to_ask() {
        let err = proc_credentials(3, 9).unwrap_err();
        assert!(matches!(err, Error::Unsupported { pid: 9 }));
        assert_eq!(syscall::Error::from(err).errno, ENOSYS);
    }
}
