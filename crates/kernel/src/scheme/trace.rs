//! Ferric Oxide: the `kernel.trace` scheme, run-time control of tracepoints and the reader of the
//! per-CPU trace rings (`kernel.trace`; paths and formats in `ferric_trace::abi`).
//!
//! - `events`: reading lists the compiled-in events and whether each is on; writing
//!   `enable <selection>` or `disable <selection>` turns events on or off, patching their
//!   tracepoints live (`arch::static_key`).
//! - `stats`: one line per CPU with the words waiting in its ring and the records it lost.
//! - `cpu/<n>`: reading drains whole records from CPU n's ring; 0 bytes when it is empty.
//!
//! `events` and `stats` are read from a snapshot taken at the first read of each open, as in
//! the `sys` scheme. Only root may open the scheme for now: traces leak timing and kernel
//! addresses, and a Phase 4 capability will replace the uid check.

use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicUsize, Ordering},
};

use ferric_trace::abi::{Command, EventLine, Path, StatsLine};
use spin::Mutex;
use syscall::error::{Error, Result, EACCES, EBADF, EINVAL, ENOENT, EOPNOTSUPP};

use super::{CallerCtx, KernelScheme, OpenResult, StrOrBytes};
use crate::{
    context::file::InternalFlags,
    cpu_set::LogicalCpuId,
    sync::CleanLockToken,
    syscall::usercopy::{UserSliceRo, UserSliceWo},
    trace::{self, ControlError, Mode},
};

/// The number of the scheme root.
const ROOT: usize = 0;

/// The longest command `events` accepts.
const MAX_COMMAND: usize = 4096;

/// The most bytes one read of `cpu/<n>` copies.
const MAX_READ: usize = 64 * 1024;

/// The scheme.
pub struct TraceScheme;

/// A text file of the scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Text {
    Events,
    Stats,
}

/// An open handle.
#[derive(Clone)]
enum Handle {
    /// `events` or `stats`, with the snapshot its reads come from once taken.
    Text {
        text: Text,
        snapshot: Option<Arc<[u8]>>,
    },
    /// `cpu/<n>`.
    Cpu(LogicalCpuId),
}

/// Every open handle but the root, by number.
static HANDLES: Mutex<BTreeMap<usize, Handle>> = Mutex::new(BTreeMap::new());

/// The next handle number.
static NEXT_ID: AtomicUsize = AtomicUsize::new(ROOT + 1);

/// Serializes the rings' consumers: each ring has one reader at a time.
static READER: Mutex<()> = Mutex::new(());

/// The contents of `text` now.
fn render(text: Text) -> Arc<[u8]> {
    let mut out = String::new();
    match text {
        Text::Events => {
            for event in ferric_trace::kernel::CATALOG.events() {
                if trace::compiled_in(event) {
                    let _ = write!(out, "{}", EventLine::new(event, trace::is_on(event.id())));
                }
            }
        }
        Text::Stats => {
            for cpu in 0..crate::cpu_count() {
                let (pending, lost) = trace::ring(LogicalCpuId::new(cpu))
                    .map_or((0, 0), |ring| (ring.pending() as u64, ring.lost()));
                let _ = write!(out, "{}", StatsLine { cpu, pending, lost });
            }
        }
    }
    Arc::from(out.into_bytes())
}

/// The errno for a failed `enable` or `disable`.
fn control_errno(err: &ControlError<'_>) -> Error {
    Error::new(match err {
        ControlError::Select(_) => ENOENT,
        ControlError::NotCompiled => EOPNOTSUPP,
    })
}

impl KernelScheme for TraceScheme {
    fn scheme_root(&self, _token: &mut CleanLockToken) -> Result<usize> {
        Ok(ROOT)
    }

    fn kopenat(
        &self,
        file: usize,
        path: StrOrBytes,
        _flags: usize,
        _fcntl_flags: u32,
        caller: CallerCtx,
        _token: &mut CleanLockToken,
    ) -> Result<OpenResult> {
        if file != ROOT {
            return Err(Error::new(EACCES));
        }
        // Phase 4 replaces this with a capability.
        if caller.uid != 0 {
            return Err(Error::new(EACCES));
        }
        let path = path.as_str().map_err(|_| Error::new(ENOENT))?;
        let (handle, flags) = match Path::parse(path).ok_or(Error::new(ENOENT))? {
            Path::Root => return Err(Error::new(ENOENT)),
            Path::Events => (Text::Events, InternalFlags::POSITIONED),
            Path::Stats => (Text::Stats, InternalFlags::POSITIONED),
            Path::Cpu(cpu) if cpu < crate::cpu_count() => {
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                HANDLES
                    .lock()
                    .insert(id, Handle::Cpu(LogicalCpuId::new(cpu)));
                return Ok(OpenResult::SchemeLocal(id, InternalFlags::empty()));
            }
            Path::Cpu(_) => return Err(Error::new(ENOENT)),
        };
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        HANDLES.lock().insert(
            id,
            Handle::Text {
                text: handle,
                snapshot: None,
            },
        );
        Ok(OpenResult::SchemeLocal(id, flags))
    }

    fn close(&self, id: usize, _token: &mut CleanLockToken) -> Result<()> {
        HANDLES
            .lock()
            .remove(&id)
            .map(|_| ())
            .ok_or(Error::new(EBADF))
    }

    fn kreadoff(
        &self,
        id: usize,
        buf: UserSliceWo,
        offset: u64,
        _flags: u32,
        _stored_flags: u32,
        _token: &mut CleanLockToken,
    ) -> Result<usize> {
        let handle = HANDLES.lock().get(&id).cloned().ok_or(Error::new(EBADF))?;
        match handle {
            Handle::Text { text, snapshot } => {
                let data = match snapshot {
                    Some(data) => data,
                    None => {
                        let data = render(text);
                        if let Some(Handle::Text { snapshot, .. }) = HANDLES.lock().get_mut(&id) {
                            *snapshot = Some(Arc::clone(&data));
                        }
                        data
                    }
                };
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                buf.copy_common_bytes_from_slice(data.get(start..).unwrap_or(&[]))
            }
            Handle::Cpu(cpu) => {
                let Some(ring) = trace::ring(cpu) else {
                    return Ok(0);
                };
                let mut records = vec![0u8; buf.len().min(MAX_READ)];
                let len = {
                    let _reader = READER.lock();
                    ring.read(&mut records)
                };
                buf.copy_common_bytes_from_slice(records.get(..len).unwrap_or(&[]))
            }
        }
    }

    fn kwrite(
        &self,
        id: usize,
        buf: UserSliceRo,
        _flags: u32,
        _stored_flags: u32,
        _token: &mut CleanLockToken,
    ) -> Result<usize> {
        match HANDLES.lock().get(&id) {
            Some(Handle::Text {
                text: Text::Events, ..
            }) => {}
            Some(_) => return Err(Error::new(EBADF)),
            None => return Err(Error::new(EBADF)),
        }
        if buf.len() > MAX_COMMAND {
            return Err(Error::new(EINVAL));
        }
        let mut bytes: Vec<u8> = vec![0; buf.len()];
        let len = buf.copy_common_bytes_to_slice(&mut bytes)?;
        let command =
            Command::parse(bytes.get(..len).unwrap_or(&[])).map_err(|_| Error::new(EINVAL))?;
        let (spec, on) = match command {
            Command::Enable(spec) => (spec, true),
            Command::Disable(spec) => (spec, false),
        };
        match trace::control(spec, on, Mode::Live) {
            Ok(changed) => {
                info!("trace: {command} ({changed} event(s) changed)");
                Ok(len)
            }
            Err(err) => {
                warn!("trace: {command}: {err}");
                Err(control_errno(&err))
            }
        }
    }

    fn kwriteoff(
        &self,
        id: usize,
        buf: UserSliceRo,
        _offset: u64,
        flags: u32,
        stored_flags: u32,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        // `events` is positioned for its reads; a command ignores the offset.
        self.kwrite(id, buf, flags, stored_flags, token)
    }

    fn kfpath(&self, id: usize, buf: UserSliceWo, _token: &mut CleanLockToken) -> Result<usize> {
        let handle = HANDLES.lock().get(&id).cloned().ok_or(Error::new(EBADF))?;
        let path = match handle {
            Handle::Text {
                text: Text::Events, ..
            } => alloc::format!("/scheme/kernel.trace/{}", ferric_trace::abi::EVENTS_PATH),
            Handle::Text {
                text: Text::Stats, ..
            } => alloc::format!("/scheme/kernel.trace/{}", ferric_trace::abi::STATS_PATH),
            Handle::Cpu(cpu) => alloc::format!(
                "/scheme/kernel.trace/{}/{}",
                ferric_trace::abi::CPU_DIR,
                cpu.get()
            ),
        };
        buf.copy_common_bytes_from_slice(path.as_bytes())
    }
}
