//! Ferric Oxide: `/scheme/sys/proc`, per-process introspection (`sys_proc`).
//!
//! One block per context, in the line format of `ferric_procinfo`: the context, its memory,
//! its open file descriptors and its grants. Only root may open it (it lists addresses).
//!
//! The data are gathered in two passes, so no lock is held out of order: the first walks
//! the contexts, copying what each one shows and the scheme id of every descriptor; the
//! second names those schemes from the scheme table after the context locks are released.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt::Write;

use ferric_procinfo::{
    cpu_time_ns, ContextLine, FdLine, GrantLine, MemoryLine, Name, Prot, Provider, SchemeRef,
    Status,
};
use hashbrown::HashMap;
use syscall::flag::MapFlags;

use crate::{
    context::{self, contexts, memory::AddrSpaceWrapper},
    scheme::{self, handles, KernelSchemes, SchemeId},
    sync::CleanLockToken,
    syscall::error::Result,
};

/// The name `context::init` gives each CPU's idle context. The scheduler never charges an
/// idle context's time, so its current period is not counted either.
const IDLE_NAME: &str = "[kmain]";

/// One context, copied out under its lock.
struct Snapshot {
    id: u32,
    pid: u64,
    euid: u32,
    egid: u32,
    status: Status,
    cpu: Option<u32>,
    running: bool,
    active_ns: u64,
    sched_ns: u64,
    name: String,
    kernel_bytes: u64,
    addr_space: Option<Arc<AddrSpaceWrapper>>,
    /// Each descriptor: its number, its scheme's id (`None` if the scheme is gone) and the
    /// scheme's handle number.
    fds: Vec<(usize, Option<SchemeId>, usize)>,
}

fn status(status: &context::Status, awake: bool) -> Status {
    match status {
        context::Status::Runnable => Status::Runnable,
        context::Status::Blocked | context::Status::HardBlocked { .. } if awake => Status::Sleeping,
        context::Status::Blocked | context::Status::HardBlocked { .. } => Status::Blocked,
        context::Status::Dead { .. } => Status::Dead,
    }
}

fn provider(provider: &context::memory::Provider) -> Provider {
    match provider {
        context::memory::Provider::Allocated { .. } => Provider::Allocated,
        context::memory::Provider::AllocatedShared { .. } => Provider::AllocatedShared,
        context::memory::Provider::PhysBorrowed { .. } => Provider::PhysBorrowed,
        context::memory::Provider::External { .. } => Provider::External,
        context::memory::Provider::FmapBorrowed { .. } => Provider::FmapBorrowed,
    }
}

/// Copies every context out, with its descriptors' scheme ids.
fn snapshots(token: &mut CleanLockToken) -> Vec<Snapshot> {
    let now = crate::time::monotonic(token);
    let mut out = Vec::new();
    let mut contexts = contexts(token.downgrade());
    let (contexts, mut token) = contexts.token_split();
    for context_ref in contexts.iter() {
        let mut guard = context_ref.read(token.token());
        let (context, mut token) = guard.token_split();
        let mut fds = Vec::new();
        {
            let mut files = context.files.read(token.token());
            let (files, mut token) = files.token_split();
            for (fd, file) in files.enumerate_fds() {
                let description = file.description.read(token.token());
                let scheme = description.scheme_ref.upgrade().ok().map(|s| s.scheme_id());
                fds.push((fd, scheme, description.number));
            }
        }
        // The scheduler adds a period to these counters only when it ends.
        let charged = !(context.name.as_str() == IDLE_NAME && !context.userspace);
        let switched_in = (charged && context.running).then_some(context.switch_time);
        let scheduled = (charged && context.currently_scheduled).then_some(context.schedule_time);
        out.push(Snapshot {
            id: context.debug_id,
            pid: context.pid as u64,
            euid: context.euid,
            egid: context.egid,
            status: status(&context.status, context.wake.is_some()),
            cpu: context.cpu_id.map(|cpu| cpu.get()),
            running: context.running,
            active_ns: cpu_time_ns(context.active_cpu_time, switched_in, now),
            sched_ns: cpu_time_ns(context.sched_cpu_time, scheduled, now),
            name: String::from(context.name.as_str()),
            kernel_bytes: context.kfx.len().saturating_add(context.kstack.len()) as u64,
            addr_space: context.addr_space().ok().cloned(),
            fds,
        });
    }
    out
}

/// The memory line and the grants of `addr_space`.
fn memory(
    addr_space: &AddrSpaceWrapper,
    kernel_bytes: u64,
    token: &mut CleanLockToken,
) -> (MemoryLine, Vec<GrantLine>) {
    let guard = addr_space.acquire_read(token.downgrade());
    let (mut private, mut shared) = (0u64, 0u64);
    let mut grants = Vec::new();
    for (base, info) in guard.grants.iter() {
        let bytes = info.page_count().saturating_mul(crate::memory::PAGE_SIZE) as u64;
        match &info.provider {
            context::memory::Provider::Allocated { .. } => private = private.saturating_add(bytes),
            // Device memory and the like: not RAM this context holds.
            context::memory::Provider::PhysBorrowed { .. } => {}
            _ => shared = shared.saturating_add(bytes),
        }
        // What user space may do, as mmap reports it: a guard page mapped without the user
        // bit (`prot_none`) is present but `---`.
        let flags = context::memory::map_flags(info.flags());
        grants.push(GrantLine {
            base: base.start_address().data() as u64,
            pages: info.page_count() as u64,
            provider: provider(&info.provider),
            prot: Prot {
                read: flags.contains(MapFlags::PROT_READ),
                write: flags.contains(MapFlags::PROT_WRITE),
                execute: flags.contains(MapFlags::PROT_EXEC),
            },
        });
    }
    let line = MemoryLine {
        private,
        shared,
        kernel: kernel_bytes,
    };
    (line, grants)
}

/// The name of the scheme a descriptor with handle `number` on scheme `id` is open on, as
/// the scheme table gives it. A descriptor on the scheme manager names a scheme table entry
/// by its number; the one naming the scheme creation capability is that capability.
fn scheme_ref<'a>(
    schemes: &'a HashMap<SchemeId, scheme::Handle>,
    id: SchemeId,
    number: usize,
) -> SchemeRef<'a> {
    match schemes.get(&id) {
        Some(scheme::Handle::Scheme(KernelSchemes::SchemeMgr))
            if matches!(
                schemes.get(&SchemeId::from(number)),
                Some(scheme::Handle::SchemeCreationCapability)
            ) =>
        {
            SchemeRef::Capability
        }
        Some(scheme::Handle::Scheme(KernelSchemes::Global(global))) => {
            SchemeRef::Kernel(global.as_str())
        }
        Some(scheme::Handle::Scheme(KernelSchemes::User(_))) => SchemeRef::User(id.0 as u64),
        Some(scheme::Handle::Scheme(KernelSchemes::SchemeMgr)) => SchemeRef::Manager,
        Some(scheme::Handle::SchemeCreationCapability) => SchemeRef::Capability,
        _ => SchemeRef::Gone,
    }
}

pub fn resource(token: &mut CleanLockToken) -> Result<Vec<u8>> {
    let mut all = snapshots(token);
    all.sort_by_key(|s| (s.pid, s.id));
    let mut report = String::new();
    for snapshot in &all {
        let _ = write!(
            report,
            "{}",
            ContextLine {
                id: snapshot.id,
                pid: snapshot.pid,
                euid: snapshot.euid,
                egid: snapshot.egid,
                status: snapshot.status,
                cpu: snapshot.cpu,
                running: snapshot.running,
                active_ns: snapshot.active_ns,
                sched_ns: snapshot.sched_ns,
                name: Name(&snapshot.name),
            }
        );
        let (memory, grants) = match &snapshot.addr_space {
            Some(addr_space) => memory(addr_space, snapshot.kernel_bytes, token),
            None => (
                MemoryLine {
                    private: 0,
                    shared: 0,
                    kernel: snapshot.kernel_bytes,
                },
                Vec::new(),
            ),
        };
        let _ = write!(report, "{memory}");
        let schemes = handles().read(token.token());
        for &(fd, id, number) in &snapshot.fds {
            let scheme = match id {
                Some(id) => scheme_ref(&schemes, id, number),
                None => SchemeRef::Gone,
            };
            let _ = write!(
                report,
                "{}",
                FdLine {
                    fd: fd as u64,
                    scheme,
                    number: number as u64,
                }
            );
        }
        drop(schemes);
        for grant in grants {
            let _ = write!(report, "{grant}");
        }
    }
    Ok(report.into_bytes())
}
