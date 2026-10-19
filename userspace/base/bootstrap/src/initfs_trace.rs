//! Ferric Oxide: applies the initfs trace configuration (`ferric_trace::config`) before init
//! starts, so the early boot of user space can be traced.
//!
//! When the initfs has `lib/trace.toml` (ferric-config's `kernel.trace_initfs_*` options put it
//! there), bootstrap parses it and writes its `enable` and `disable` commands to
//! `/scheme/kernel.trace/events`, as `echo "enable ..."` would. The kernel logs each change.
//! Nothing here is fatal: a missing file is the normal case, and a bad file, a kernel without
//! `kernel.trace` or a refused command is logged and boot goes on.

use alloc::string::ToString;
use alloc::vec::Vec;

use ferric_trace::abi::{EVENTS_PATH, SCHEME_NAME};
use ferric_trace::config::{BootConfig, INITFS_PATH};
use libredox::protocol::O_CLOEXEC;
use redox_rt::proc::{FdGuard, FdGuardUpper};
use syscall::flag::{O_DIRECTORY, O_RDONLY, O_WRONLY};
use syscall::{ENOENT, Error};

/// The largest `trace.toml` read; a configuration of selections is a few hundred bytes.
const MAX_LEN: usize = 64 * 1024;

/// Reads the whole of `file`, or fails with `EFBIG` past [`MAX_LEN`].
fn read_all(file: &FdGuardUpper) -> syscall::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let len = file.read(&mut chunk)?;
        if len == 0 {
            return Ok(bytes);
        }
        if bytes.len() + len > MAX_LEN {
            return Err(Error::new(syscall::EFBIG));
        }
        bytes.extend_from_slice(chunk.get(..len).unwrap_or_default());
    }
}

/// Applies `lib/trace.toml` from `initfs_root`, opening the trace scheme through the
/// namespace `ns`.
pub fn apply<const R: bool, const N: bool>(initfs_root: &FdGuard<R>, ns: &FdGuard<N>) {
    let file = match initfs_root.openat_into_upper(INITFS_PATH, O_RDONLY | O_CLOEXEC, 0) {
        Ok(file) => file,
        Err(Error { errno: ENOENT }) => return,
        Err(err) => {
            log::warn!("trace: cannot open /scheme/initfs/{INITFS_PATH}: {err}");
            return;
        }
    };
    let config = match read_all(&file) {
        Ok(bytes) => BootConfig::parse(&bytes),
        Err(err) => {
            log::warn!("trace: cannot read /scheme/initfs/{INITFS_PATH}: {err}");
            return;
        }
    };
    let config = match config {
        Ok(config) => config,
        Err(err) => {
            // The source names the line and column; print the chain.
            match core::error::Error::source(&err) {
                Some(source) => log::warn!("trace: {err}: {source}"),
                None => log::warn!("trace: {err}"),
            }
            return;
        }
    };
    let Some(events) = open_events(ns) else {
        return;
    };
    for command in config.commands() {
        if let Err(err) = events.write(command.to_string().as_bytes()) {
            log::warn!("trace: {INITFS_PATH}: {command}: {err}");
        }
    }
}

/// Opens `/scheme/kernel.trace/events` for writing through the namespace `ns`.
///
/// The init namespace only hands out a scheme's root capability (it needs `O_DIRECTORY` and
/// ignores the rest of the path), so this opens the root and then the events file under it, as
/// relibc does for an ordinary process.
fn open_events<const N: bool>(ns: &FdGuard<N>) -> Option<FdGuardUpper> {
    let root = alloc::format!("/scheme/{SCHEME_NAME}");
    let scheme = match ns.openat_into_upper(&root, O_DIRECTORY | O_CLOEXEC, 0) {
        Ok(scheme) => scheme,
        Err(err) => {
            log::warn!("trace: {INITFS_PATH} ignored, cannot open {root}: {err}");
            return None;
        }
    };
    match scheme.openat_into_upper(EVENTS_PATH, O_WRONLY | O_CLOEXEC, 0) {
        Ok(events) => Some(events),
        Err(err) => {
            log::warn!("trace: {INITFS_PATH} ignored, cannot open {root}/{EVENTS_PATH}: {err}");
            None
        }
    }
}
