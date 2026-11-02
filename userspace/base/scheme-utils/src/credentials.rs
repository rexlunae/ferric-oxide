//! The credentials of the process behind a scheme request, from procmgr.
//!
//! A request made through `SYS_CALL` (a `StdFsCall` such as `fchmod`, for one) carries
//! only the caller's pid: its `CallerCtx` uid and gid read as zero, which looks like root.
//! A scheme that checks permissions on such a request asks procmgr for the caller's
//! credentials through a `proc-creds-capability` handle instead.

use libredox::protocol::ProcMeta;
use syscall::{EINVAL, Error, Result};

/// The credentials procmgr reports for `pid`, asked through the `proc-creds-capability`
/// handle `cap_fd`. Permission checks use the effective IDs (`euid`, `egid`).
///
/// # Errors
/// Returns procmgr's error (`ESRCH` for a process that is gone), or `EINVAL` when the
/// reply is not a whole [`ProcMeta`].
pub fn proc_credentials(cap_fd: usize, pid: usize) -> Result<ProcMeta> {
    let mut buffer = [0u8; size_of::<ProcMeta>()];
    let len = libredox::call::get_proc_credentials(cap_fd, pid, &mut buffer)?;
    decode(buffer.get(..len).ok_or(Error::new(EINVAL))?)
}

/// The [`ProcMeta`] at the start of `bytes`.
fn decode(bytes: &[u8]) -> Result<ProcMeta> {
    plain::from_bytes::<ProcMeta>(bytes)
        .copied()
        .map_err(|_| Error::new(EINVAL))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The effective IDs come from their own fields, not the real ones beside them.
    #[test]
    fn effective_ids_are_read_from_their_fields() {
        let meta = ProcMeta {
            pid: 7,
            pgid: 7,
            ppid: 1,
            ruid: 0,
            euid: 1000,
            suid: 0,
            rgid: 0,
            egid: 100,
            sgid: 0,
            ens: 1,
            rns: 1,
        };
        // SAFETY: ProcMeta is `Plain`: every byte pattern is valid and it has no padding.
        let bytes = unsafe { plain::as_bytes(&meta) };
        let read = decode(bytes).unwrap();
        assert_eq!((read.pid, read.euid, read.egid), (7, 1000, 100));
        assert_eq!((read.ruid, read.rgid), (0, 0));
    }

    #[test]
    fn a_short_reply_is_invalid() {
        assert_eq!(decode(&[0; 4]).unwrap_err().errno, EINVAL);
    }
}
