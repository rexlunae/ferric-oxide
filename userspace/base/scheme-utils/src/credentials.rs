//! The credentials of the process behind a scheme request, from procmgr.
//!
//! A request made through `SYS_CALL` (a `StdFsCall` such as `fchmod`, for one) carries
//! only the caller's pid: its `CallerCtx` uid and gid read as zero, which looks like root.
//! A scheme that checks permissions on such a request asks procmgr for the caller's
//! credentials through a `proc-creds-capability` handle instead. The lookup itself is the
//! shared `ferric-proc-creds` crate, which ext4d uses too.

use libredox::protocol::ProcMeta;
use syscall::Result;

/// The credentials procmgr reports for `pid`, asked through the `proc-creds-capability`
/// handle `cap_fd`. Permission checks use the effective IDs (`euid`, `egid`).
///
/// # Errors
/// Returns procmgr's error (`ESRCH` for a process that is gone), or `EINVAL` when the
/// reply is not a whole [`ProcMeta`]. Off Redox, where there is no procmgr, `ENOSYS`.
pub fn proc_credentials(cap_fd: usize, pid: usize) -> Result<ProcMeta> {
    Ok(ferric_proc_creds::proc_credentials(cap_fd, pid)?)
}
