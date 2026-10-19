//! Ferric Oxide: `/scheme/sys/proc`, per-process introspection (`sys_proc`).
//!
//! One block per context, in the line format of `ferric_procinfo`: the context, its memory,
//! its open file descriptors, its grants and the images its loaders recorded. Only root may open it (it lists addresses).
//!
//! The data are gathered in two passes, so no lock is held out of order: the first walks
//! the contexts, copying what each one shows and the scheme id of every descriptor; the
//! second names those schemes from the scheme table after the context locks are released.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt::Write;

use ferric_procinfo::{
    ContextLine, FdLine, GrantLine, Image, MemoryLine, Name, Prot, Provider, SchemeRef, Status,
};

use crate::{
    context::{self, contexts, memory::AddrSpaceWrapper},
    scheme::{self, handles, KernelSchemes, SchemeId},
    sync::CleanLockToken,
    syscall::error::Result,
};

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
        out.push(Snapshot {
            id: context.debug_id,
            pid: context.pid as u64,
            euid: context.euid,
            egid: context.egid,
            status: status(&context.status, context.wake.is_some()),
            cpu: context.cpu_id.map(|cpu| cpu.get()),
            running: context.running,
            // A u64 of nanoseconds lasts 584 years.
            active_ns: u64::try_from(context.active_cpu_time).unwrap_or(u64::MAX),
            sched_ns: u64::try_from(context.sched_cpu_time).unwrap_or(u64::MAX),
            name: String::from(context.name.as_str()),
            kernel_bytes: context.kfx.len().saturating_add(context.kstack.len()) as u64,
            addr_space: context.addr_space().ok().cloned(),
            fds,
        });
    }
    out
}

/// The memory line, the grants and the images of `addr_space`.
fn memory(
    addr_space: &AddrSpaceWrapper,
    kernel_bytes: u64,
    token: &mut CleanLockToken,
) -> (MemoryLine, Vec<GrantLine>, Vec<Image>) {
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
        let flags = info.flags();
        grants.push(GrantLine {
            base: base.start_address().data() as u64,
            pages: info.page_count() as u64,
            provider: provider(&info.provider),
            prot: Prot {
                read: flags.has_present(),
                write: flags.has_write(),
                execute: flags.has_execute(),
            },
        });
    }
    let line = MemoryLine {
        private,
        shared,
        kernel: kernel_bytes,
    };
    (line, grants, guard.images.clone())
}

/// The name of scheme `id` as the scheme table gives it.
fn scheme_ref<'a>(handle: Option<&'a scheme::Handle>, id: SchemeId) -> SchemeRef<'a> {
    match handle {
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
        let (memory, grants, images) = match &snapshot.addr_space {
            Some(addr_space) => memory(addr_space, snapshot.kernel_bytes, token),
            None => (
                MemoryLine {
                    private: 0,
                    shared: 0,
                    kernel: snapshot.kernel_bytes,
                },
                Vec::new(),
                Vec::new(),
            ),
        };
        let _ = write!(report, "{memory}");
        let schemes = handles().read(token.token());
        for &(fd, id, number) in &snapshot.fds {
            let scheme = match id {
                Some(id) => scheme_ref(schemes.get(&id), id),
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
        for image in &images {
            let _ = write!(report, "{}", image.line());
        }
    }
    Ok(report.into_bytes())
}
