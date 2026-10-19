//! Ferric Oxide: the `kernel.trace` scheme, run-time control of tracepoints and the reader of the
//! per-CPU trace rings (`kernel.trace`; paths and formats in `ferric_trace::abi`).
//!
//! - `events`: reading lists the compiled-in events and whether each is on; writing
//!   `enable <selection>` or `disable <selection>` turns events on or off, patching their
//!   tracepoints live (`arch::static_key`).
//! - `stats`: one line per CPU with the words waiting in its ring and the records it lost.
//!   Writing `filter <event> <filter>` to `events` sets that event's argument filter
//!   (`ferric_trace::filter`), and `filter <event> none` removes it.
//! - `filters`: one line per filtered event, written as the command that sets its filter.
//! - `cpu/<n>`: reading drains whole records from CPU n's ring; 0 bytes when it is empty, and
//!   `EINVAL` when the buffer cannot hold the next record, and `EAGAIN` while another reader
//!   is reading that CPU. Records are consumed only once the copy to the caller succeeded.
//!   `mmap` of the descriptor (`PROT_READ`, `MAP_SHARED`, the whole `CPU_MAP_BYTES` from offset
//!   0) maps the ring itself read-only, header page and data; writing `commit <end>` then
//!   consumes it up to `end`, under the same one-reader lock as a read.
//!
//! - `crash` (`kernel.trace_crash`): the crash dump the previous boot's panic left
//!   (`trace::crash`), read from any offset; it exists only when there is one. With
//!   `kernel.trace_crash_test` it always exists (empty without a dump), and writing `panic` to it
//!   panics the kernel, which writes the dump and resets the machine.
//!
//! A mapped ring is never writable by its reader: `PROT_WRITE` is refused, and `mprotect`
//! cannot add it to a shared grant later. So the kernel stays the only writer of those words,
//! and it never trusts what they say beyond its own stores: the ring's size is kept in the
//! kernel, every position is masked to the data before use, and the one value a reader passes
//! back, `commit`'s `end`, is accepted only as a record boundary between the kernel's `tail`
//! and `head` (`Ring::commit_to`). The reader, for its part, sees `head` and `tail` move under
//! it and must copy before it decodes (`ferric_trace::ring::View`). A `read` of the same CPU
//! can consume records while a mapped reader copies them, and the producer can then reuse
//! those words, so `View::peek` checks `tail` after each copy and retries one it overtook.
//! The mapping needs `PROT_READ`: a mapping without it would still be readable.
//!
//! `events`, `stats` and `filters` are read from a snapshot taken at the first read of each open, as in
//! the `sys` scheme. Only root may open the scheme for now: traces leak timing and kernel
//! addresses, and a Phase 4 capability will replace the uid check.

use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicUsize, Ordering},
};

use ferric_trace::{
    abi::{
        Command, CpuCommand, EventLine, FilterBusyLine, FilterLine, Path, StatsLine, CPU_MAP_BYTES,
    },
    filter::Loaded,
};
use spin::{Mutex, MutexGuard};
use syscall::{
    error::{Error, Result, EACCES, EAGAIN, EBADF, EINVAL, ENOENT, EOPNOTSUPP},
    flag::MapFlags,
};

use super::{shared_frames::map_frames, CallerCtx, KernelScheme, OpenResult, StrOrBytes};
use crate::{
    context::{file::InternalFlags, memory::AddrSpaceWrapper},
    cpu_set::{LogicalCpuId, MAX_CPU_COUNT},
    sync::CleanLockToken,
    syscall::{
        data::Map,
        usercopy::{UserSliceRo, UserSliceWo},
    },
    trace::{self, ControlError, FilterControlError, Mode},
};

/// The number of the scheme root.
const ROOT: usize = 0;

/// The longest command `events` accepts.
const MAX_COMMAND: usize = ferric_trace::abi::MAX_COMMAND_BYTES;

/// The most bytes one read of `cpu/<n>` copies.
const MAX_READ: usize = 64 * 1024;

/// The scheme.
pub struct TraceScheme;

/// A text file of the scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Text {
    Events,
    Stats,
    Filters,
    Crash,
}

/// An open handle.
#[derive(Clone)]
enum Handle {
    /// `events`, `stats` or `filters`, with the snapshot its reads come from once taken.
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

/// Each ring's consumer: one reader at a time, held from the copy to the commit.
static READERS: [Mutex<()>; MAX_CPU_COUNT as usize] =
    [const { Mutex::new(()) }; MAX_CPU_COUNT as usize];

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
        Text::Crash => return crash_dump().unwrap_or_else(|| Arc::from([])),
        Text::Stats => {
            for cpu in 0..crate::cpu_count() {
                let (pending, lost) = trace::ring(LogicalCpuId::new(cpu))
                    .map_or((0, 0), |ring| (ring.pending() as u64, ring.lost()));
                let _ = write!(out, "{}", StatsLine { cpu, pending, lost });
            }
        }
        Text::Filters => {
            for (event, loaded) in trace::filters() {
                let _ = match loaded {
                    Loaded::Set(filter) => write!(
                        out,
                        "{}",
                        FilterLine {
                            event,
                            filter: &filter
                        }
                    ),
                    Loaded::Busy => write!(out, "{}", FilterBusyLine { event }),
                    Loaded::Empty => Ok(()),
                };
            }
        }
    }
    Arc::from(out.into_bytes())
}

/// The previous boot's crash dump, if this kernel keeps one and there is one.
fn crash_dump() -> Option<Arc<[u8]>> {
    #[cfg(feature = "trace_crash")]
    return trace::crash::saved();
    #[cfg(not(feature = "trace_crash"))]
    None
}

/// Whether `crash` exists: when there is a dump, and always on a crash-test kernel, which needs
/// it for the `panic` command.
fn crash_exists() -> bool {
    cfg!(feature = "trace_crash_test") || crash_dump().is_some()
}

/// Runs a command written to `crash`: `panic`, on a crash-test kernel.
fn crash_command(buf: UserSliceRo) -> Result<usize> {
    let (bytes, _len) = command_bytes(buf)?;
    if bytes.trim_ascii() != ferric_trace::abi::CRASH_TEST_COMMAND.as_bytes() {
        return Err(Error::new(EINVAL));
    }
    #[cfg(feature = "trace_crash_test")]
    trace::crash::test_panic();
    #[cfg(not(feature = "trace_crash_test"))]
    Err(Error::new(EOPNOTSUPP))
}

/// Becomes `cpu`'s ring's one consumer, or fails with `EAGAIN` while another reader is: the
/// copy to the caller can fault and block, so a second reader does not spin.
fn consumer(cpu: LogicalCpuId) -> Result<MutexGuard<'static, ()>> {
    READERS
        .get(cpu.get() as usize)
        .and_then(Mutex::try_lock)
        .ok_or(Error::new(EAGAIN))
}

/// Reads a command of at most [`MAX_COMMAND`] bytes from `buf`, and returns it with its
/// length.
fn command_bytes(buf: UserSliceRo) -> Result<(Vec<u8>, usize)> {
    if buf.len() > MAX_COMMAND {
        return Err(Error::new(EINVAL));
    }
    let mut bytes: Vec<u8> = vec![0; buf.len()];
    let len = buf.copy_common_bytes_to_slice(&mut bytes)?;
    bytes.truncate(len);
    Ok((bytes, len))
}

/// Runs a command written to `events`.
fn events_command(buf: UserSliceRo) -> Result<usize> {
    let (bytes, len) = command_bytes(buf)?;
    let command = Command::parse(&bytes).map_err(|_| Error::new(EINVAL))?;
    let (spec, on) = match command {
        Command::Enable(spec) => (spec, true),
        Command::Disable(spec) => (spec, false),
        Command::Filter { event, filter } => return filter_command(event, filter, len),
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

/// Runs `filter <event> <filter>`: sets or removes the event's argument filter.
fn filter_command(event: &str, filter: &str, len: usize) -> Result<usize> {
    match trace::set_filter(event, filter) {
        Ok((event, Some(set))) => {
            info!("trace: filter {} {}", event.name(), set.display(event));
            Ok(len)
        }
        Ok((event, None)) => {
            info!("trace: filter {} none", event.name());
            Ok(len)
        }
        Err(err) => {
            warn!("trace: filter {event} {filter}: {err}");
            Err(Error::new(match err {
                FilterControlError::UnknownEvent(_) => ENOENT,
                FilterControlError::NotCompiled(_) => EOPNOTSUPP,
                FilterControlError::Filter(_) => EINVAL,
            }))
        }
    }
}

/// Runs a command written to `cpu/<n>`: `commit <end>` consumes the mapped ring up to `end`,
/// which must be a record boundary the kernel can check (`Ring::commit_to`).
fn cpu_command(cpu: LogicalCpuId, buf: UserSliceRo) -> Result<usize> {
    let (bytes, len) = command_bytes(buf)?;
    let CpuCommand::Commit(end) = CpuCommand::parse(&bytes).map_err(|_| Error::new(EINVAL))? else {
        return Err(Error::new(EINVAL));
    };
    let ring = trace::ring(cpu).ok_or(Error::new(EINVAL))?;
    let _consumer = consumer(cpu)?;
    ring.commit_to(end).map_err(|_| Error::new(EINVAL))?;
    Ok(len)
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
            Path::Filters => (Text::Filters, InternalFlags::POSITIONED),
            Path::Crash if crash_exists() => (Text::Crash, InternalFlags::POSITIONED),
            Path::Crash => return Err(Error::new(ENOENT)),
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
                // One reader at a time, from the copy to the commit. The records are consumed
                // only once they reached the caller, so a failed copy loses none.
                let _consumer = consumer(cpu)?;
                let peek = ring.peek(&mut records).map_err(|_| Error::new(EINVAL))?;
                let len =
                    buf.copy_common_bytes_from_slice(records.get(..peek.len()).unwrap_or(&[]))?;
                ring.commit(peek);
                Ok(len)
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
        let handle = HANDLES.lock().get(&id).cloned().ok_or(Error::new(EBADF))?;
        match handle {
            Handle::Text {
                text: Text::Events, ..
            } => events_command(buf),
            Handle::Cpu(cpu) => cpu_command(cpu, buf),
            Handle::Text {
                text: Text::Crash, ..
            } => crash_command(buf),
            Handle::Text {
                text: Text::Stats | Text::Filters,
                ..
            } => Err(Error::new(EBADF)),
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

    fn kfmap(
        &self,
        id: usize,
        addr_space: &Arc<AddrSpaceWrapper>,
        map: &Map,
        _consume: bool,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let Some(Handle::Cpu(cpu)) = HANDLES.lock().get(&id).cloned() else {
            return Err(Error::new(EBADF));
        };
        if map
            .flags
            .intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE)
        {
            return Err(Error::new(EOPNOTSUPP));
        }
        // Read-only, so the kernel stays the only writer of the ring's words.
        if map
            .flags
            .intersects(MapFlags::PROT_WRITE | MapFlags::PROT_EXEC)
        {
            return Err(Error::new(EACCES));
        }
        if !map
            .flags
            .contains(MapFlags::PROT_READ | MapFlags::MAP_SHARED)
            || map.offset != 0
            || map.size != CPU_MAP_BYTES
        {
            return Err(Error::new(EINVAL));
        }
        trace::init_rings();
        let ring = trace::ring(cpu).ok_or(Error::new(EINVAL))?;
        map_frames(
            addr_space,
            ring.words().frames(),
            MapFlags::PROT_READ,
            token,
        )
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
            Handle::Text {
                text: Text::Filters,
                ..
            } => alloc::format!("/scheme/kernel.trace/{}", ferric_trace::abi::FILTERS_PATH),
            Handle::Text {
                text: Text::Crash, ..
            } => alloc::format!("/scheme/kernel.trace/{}", ferric_trace::abi::CRASH_PATH),
            Handle::Cpu(cpu) => alloc::format!(
                "/scheme/kernel.trace/{}/{}",
                ferric_trace::abi::CPU_DIR,
                cpu.get()
            ),
        };
        buf.copy_common_bytes_from_slice(path.as_bytes())
    }
}
