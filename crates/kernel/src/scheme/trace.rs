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
//! - `clock`: the clock record timestamps come from and its tick length
//!   (`ferric_trace::clock::ClockLine`), chosen once at boot (`trace::clock`).
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
//! - `cap/read`, `cap/control`: a capability (`ferric_trace::abi::Access`). `dup(cap, path)`
//!   opens `path` with the capability's access whatever the caller's user, so root can hand an
//!   unprivileged collector exactly the access it needs. Read access reads and drains;
//!   writing `events` or `crash` needs control access. A capability can be narrowed by
//!   `dup(cap, "cap/read")`, never widened.
//!
//! - `emit`: user space writes one `ferric_trace::abi::UserRecord` per write, for one of the
//!   events only user space reports (`ferric_trace::kernel::USER_EVENTS`: the process
//!   manager's fork, exec, exit and wait). Opening it needs control access. The kernel checks
//!   the event and its argument count, and stores the record if the event is on
//!   (`trace::emit_user`); it vouches for the record's time and CPU, not its arguments.
//!
//! `events`, `stats`, `clock`, `filters` and `crash` are read from a snapshot taken at the first read of each open, as in
//! the `sys` scheme. Opening by path is for root only (with control access): traces leak
//! timing and kernel addresses.

use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicUsize, Ordering},
};

use ferric_trace::{
    abi::{
        Access, Command, CpuCommand, EventLine, FilterBusyLine, FilterLine, Path, StatsLine,
        UserRecord, CPU_MAP_BYTES,
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
    trace::{self, ControlError, FilterControlError, Mode, UserEmitError},
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
    Clock,
    Crash,
}

/// An open handle.
#[derive(Clone)]
enum Handle {
    /// `events`, `stats`, `clock`, `filters` or `crash`, with the snapshot its reads come
    /// from once taken, and the access it was opened with.
    Text {
        text: Text,
        snapshot: Option<Arc<[u8]>>,
        access: Access,
    },
    /// `cpu/<n>`. Every access may drain, map and commit it.
    Cpu(LogicalCpuId),
    /// `cap/<access>`: opens other paths through `dup`.
    Cap(Access),
    /// `emit`: user-space records, opened with control access.
    Emit,
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
        Text::Clock => {
            let _ = write!(out, "{}", trace::clock::choice().line());
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
    let mut bytes: Vec<u8> = alloc::vec![0; buf.len()];
    let len = buf.copy_common_bytes_to_slice(&mut bytes)?;
    bytes.truncate(len);
    Ok((bytes, len))
}

/// Opens `path` with `access`: from a path open by root (`Access::Control`) or through a
/// capability's `dup`.
fn open(path: &str, access: Access) -> Result<OpenResult> {
    let (handle, flags) = match Path::parse(path).ok_or(Error::new(ENOENT))? {
        Path::Root => return Err(Error::new(ENOENT)),
        Path::Events => (Text::Events, InternalFlags::POSITIONED),
        Path::Stats => (Text::Stats, InternalFlags::POSITIONED),
        Path::Filters => (Text::Filters, InternalFlags::POSITIONED),
        Path::Clock => (Text::Clock, InternalFlags::POSITIONED),
        Path::Crash if crash_exists() => (Text::Crash, InternalFlags::POSITIONED),
        Path::Crash => return Err(Error::new(ENOENT)),
        Path::Cpu(cpu) if cpu < crate::cpu_count() => {
            return Ok(insert(
                Handle::Cpu(LogicalCpuId::new(cpu)),
                InternalFlags::empty(),
            ));
        }
        Path::Cpu(_) => return Err(Error::new(ENOENT)),
        // Narrowing only: a capability never yields more access than it has.
        Path::Cap(wanted) if access.includes(wanted) => {
            return Ok(insert(Handle::Cap(wanted), InternalFlags::empty()));
        }
        Path::Cap(_) => return Err(Error::new(EACCES)),
        Path::Emit => {
            require_control(access)?;
            return Ok(insert(Handle::Emit, InternalFlags::empty()));
        }
    };
    let handle = Handle::Text {
        text: handle,
        snapshot: None,
        access,
    };
    Ok(insert(handle, flags))
}

/// Stores `handle` under a new number.
fn insert(handle: Handle, flags: InternalFlags) -> OpenResult {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    HANDLES.lock().insert(id, handle);
    OpenResult::SchemeLocal(id, flags)
}

/// `EACCES` unless `access` allows control: writing commands to `events` or `crash`.
fn require_control(access: Access) -> Result<()> {
    if access.includes(Access::Control) {
        Ok(())
    } else {
        Err(Error::new(EACCES))
    }
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
            crate::info!("trace: {command} ({changed} event(s) changed)");
            Ok(len)
        }
        Err(err) => {
            crate::warn!("trace: {command}: {err}");
            Err(control_errno(&err))
        }
    }
}

/// Runs `filter <event> <filter>`: sets or removes the event's argument filter.
fn filter_command(event: &str, filter: &str, len: usize) -> Result<usize> {
    match trace::set_filter(event, filter) {
        Ok((event, Some(set))) => {
            crate::info!("trace: filter {} {}", event.name(), set.display(event));
            Ok(len)
        }
        Ok((event, None)) => {
            crate::info!("trace: filter {} none", event.name());
            Ok(len)
        }
        Err(err) => {
            crate::warn!("trace: filter {event} {filter}: {err}");
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

/// Stores the user-space record written to `emit` (`trace::emit_user`).
fn emit_command(buf: UserSliceRo) -> Result<usize> {
    if buf.len() > UserRecord::MAX_BYTES {
        return Err(Error::new(EINVAL));
    }
    let mut bytes = [0u8; UserRecord::MAX_BYTES];
    let len = buf.copy_common_bytes_to_slice(&mut bytes)?;
    let record =
        UserRecord::decode(bytes.get(..len).unwrap_or(&[])).map_err(|_| Error::new(EINVAL))?;
    match trace::emit_user(&record) {
        Ok(_) => Ok(len),
        Err(UserEmitError::NotCompiled(_)) => Err(Error::new(EOPNOTSUPP)),
        Err(UserEmitError::NotUserEvent | UserEmitError::ArgCount) => Err(Error::new(EINVAL)),
    }
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
        // By path for root only; anyone else opens through a capability (`kdup`).
        if caller.uid != 0 {
            return Err(Error::new(EACCES));
        }
        let path = path.as_str().map_err(|_| Error::new(ENOENT))?;
        open(path, Access::Control)
    }

    fn kdup(
        &self,
        id: usize,
        buf: UserSliceRo,
        _caller: CallerCtx,
        _token: &mut CleanLockToken,
    ) -> Result<OpenResult> {
        // Only a capability opens other paths, and with its own access: this is what lets a
        // process that is not root use the scheme.
        let Some(Handle::Cap(access)) = HANDLES.lock().get(&id).cloned() else {
            return Err(Error::new(EOPNOTSUPP));
        };
        let (bytes, _len) = command_bytes(buf)?;
        let path = core::str::from_utf8(&bytes).map_err(|_| Error::new(ENOENT))?;
        open(path, access)
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
            Handle::Cap(_) | Handle::Emit => Err(Error::new(EBADF)),
            Handle::Text { text, snapshot, .. } => {
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
                let mut records = alloc::vec![0u8; buf.len().min(MAX_READ)];
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
                text: Text::Events,
                access,
                ..
            } => require_control(access).and_then(|()| events_command(buf)),
            Handle::Cpu(cpu) => cpu_command(cpu, buf),
            Handle::Emit => emit_command(buf),
            Handle::Text {
                text: Text::Crash,
                access,
                ..
            } => require_control(access).and_then(|()| crash_command(buf)),
            Handle::Text {
                text: Text::Stats | Text::Clock | Text::Filters,
                ..
            }
            | Handle::Cap(_) => Err(Error::new(EBADF)),
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
                text: Text::Clock, ..
            } => alloc::format!("/scheme/kernel.trace/{}", ferric_trace::abi::CLOCK_PATH),
            Handle::Text {
                text: Text::Crash, ..
            } => alloc::format!("/scheme/kernel.trace/{}", ferric_trace::abi::CRASH_PATH),
            Handle::Cpu(cpu) => alloc::format!(
                "/scheme/kernel.trace/{}/{}",
                ferric_trace::abi::CPU_DIR,
                cpu.get()
            ),
            Handle::Cap(access) => alloc::format!("/scheme/kernel.trace/{access}"),
            Handle::Emit => alloc::format!("/scheme/kernel.trace/{}", ferric_trace::abi::EMIT_PATH),
        };
        buf.copy_common_bytes_from_slice(path.as_bytes())
    }
}
