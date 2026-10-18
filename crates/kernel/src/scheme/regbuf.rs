//! Ferric Oxide: the `kernel.regbuf` scheme, registered buffers and revocable zero-copy grants
//! (io/registered-buffers-design.md in the project files; built with the `regbuf` feature).
//!
//! - Opening [`REGION_PATH`] from the scheme root gives an empty *region*.
//!   [`Request::Register`] on it pins a page-aligned range of the caller's anonymous read-write
//!   memory, private or `MAP_SHARED` (file-backed private mappings and pages the caller only
//!   borrows, see `context/reshare.rs`, are refused): each frame gets a shared reference, the
//!   one user/kernel shared pages take (`AddrSpaceWrapper::borrow_frame_enforce_rw_anonymous`).
//!   If the owner forks, its child gets copies of private frames, so the region keeps pointing
//!   at the owner's memory; `MAP_SHARED` frames stay shared with the child, as such memory
//!   always is. The pages are charged to the region opener's user ID ([`Budget`], `EDQUOT`
//!   over the limit) until the region closes.
//! - Opening a grant path (`GrantSpec::encode_path`) from a region gives a *grant* descriptor:
//!   read-only or read-write access to a byte range of the region. Its holder, typically a
//!   scheme the owner passed it to, maps the grant's window with `mmap` (`MAP_SHARED`; write
//!   access needs a read-write grant). The mapping is one kernel grant of pinned shared borrows
//!   of the region's frames, like the per-request borrows of `user.rs`: the holder cannot
//!   `munmap`, `mprotect` or `mremap` it (`EBUSY`), cannot share it onward (`EPERM`, see
//!   `context/reshare.rs`), a fork of the holder does not inherit it, and the grant records
//!   every mapping. A window page the grant covers only in part is a bounce page holding just
//!   the granted bytes ([`Bounce`]). When a writable mapping is taken down, the bytes its holder
//!   changed are copied back into the region, and only those (`ferric_regbuf::bounce`), so
//!   newer writes of the owner or of other grants to the other bytes survive. The holder drops
//!   a mapping with [`Request::Unmap`]; closing the grant drops all of them.
//! - Opening a sub-grant path (`GrantSpec::encode_sub_path`) from a grant gives a *sub-grant*:
//!   a grant of part of the parent's range, with the parent's access or less, that the holder
//!   can pass on (a file system to its disk driver). It is a grant like any other, and revoking
//!   its parent, or any grant above it, revokes it as well (`ferric_regbuf::table`).
//! - With an IOMMU, a driver maps a page-aligned grant into the DMA domain it has bound with
//!   `MapGrant` on the domain's descriptor (`scheme/iommu.rs`, [`DmaGrant`]): the region's
//!   frames at consecutive IOVAs. The grant records that mapping like a CPU mapping (a
//!   [`Mapping`] with a [`DmaRecord`]), and taking it down unmaps it from the domain with the
//!   IOTLB invalidated and waited on, so the device's DMA to the old IOVA faults.
//! - [`Request::Revoke`] on the region, or closing the region, marks grants revoked (mapping
//!   them again fails with `ECANCELED`) and unmaps every recorded mapping, with a TLB shootdown,
//!   before it returns. Closing the region also unpins its frames and refunds the charge.
//!
//! Locks: a region's lock is taken before an address space's lock (mapping) and before the
//! `iommu` scheme's handle table (`MapGrant`), never after.
//! Every teardown (revocation, closing a grant or the region, [`Request::Unmap`]) takes the
//! mappings out of the region under its lock ([`Taken`], counted in flight by the table),
//! unmaps them after releasing it, and takes it again to report each outcome
//! ([`take_down`], which also copies bounce pages back). So:
//!
//! - `Revoke` returns only once nothing of the grant (or of the grants derived from it) is
//!   mapped, in an address space or an IOMMU domain, even when another thread (the
//!   grant's close, an `Unmap`) is unmapping some of it: it waits on [`TEARDOWN`] until the
//!   table says the grant is settled.
//! - An unmap that fails leaves the mapping recorded on its revoked or closed grant, so it stays
//!   blocked from new mappings and the next `Revoke` retries it; the call reports `EIO`.
//! - A closed region keeps its frames, and their charge, until nothing maps them: its last
//!   teardown has finished and no failed one left a mapping recorded. A closed region with such
//!   a mapping goes on [`STUCK`] and is retried on the scheme's next close or `Revoke`. A
//!   registration that loses the race with the close fails and refunds its own charge.
//!
//! The grant table and budget logic is the host-tested `ferric-regbuf` crate (`crates/regbuf`),
//! which also has the call encoding clients use.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use ferric_regbuf::{
    abi::{GrantId, GrantInfo, GrantSpec, PathError, Request, Usage, REGION_PATH},
    budget::{Budget, Limits},
    table::{GrantTable, MapPlan, PartialPage, TableError, Taken},
};
use spin::Mutex;
use syscall::{
    error::{
        Error, Result, EACCES, EBADF, ECANCELED, EDQUOT, EEXIST, EINTR, EINVAL, EIO, ENOENT,
        ENOMEM, ENOSPC, EOPNOTSUPP,
    },
    flag::{CallFlags, MapFlags},
};

#[cfg(iommu_enabled)]
use super::FileHandle;
use super::{CallerCtx, KernelScheme, OpenResult, StrOrBytes};
#[cfg(iommu_enabled)]
use crate::{
    context::{self, file::KernelSchemeRef},
    syscall::data::GlobalSchemes,
};
use crate::{
    context::{
        file::InternalFlags,
        memory::{handle_notify_files, AddrSpace, AddrSpaceWrapper, Grant, PageSpan},
    },
    memory::{
        free_frames, get_page_info, used_frames, Frame, Page, RaiiFrame, RefCount, VirtualAddress,
    },
    sync::{CleanLockToken, WaitCondition, L1},
    syscall::{data::Map, usercopy::UserSliceRw},
};

/// The number of the scheme root.
const ROOT: usize = 0;

/// The scheme.
pub struct RegBufScheme;

/// A mapping a grant has made: the span of an address space its pages occupy, or, with
/// `dma`, a mapping into an IOMMU domain (then `space` is empty and `span` too).
struct Mapping {
    space: Weak<AddrSpaceWrapper>,
    span: PageSpan,
    /// The bounce pages among them.
    bounces: Vec<Bounce>,
    /// Whether the holder could write them, so bounce pages are copied back.
    writable: bool,
    /// Where the grant is mapped in an IOMMU domain (`MapGrant`).
    #[cfg(iommu_enabled)]
    dma: Option<DmaRecord>,
}

/// Where a grant is mapped in an IOMMU domain: the `iommu` scheme's handle number of the domain
/// (never reused), the IOVA, and the ticket that names this mapping there.
#[cfg(iommu_enabled)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaRecord {
    /// The domain's handle number in the `iommu` scheme.
    pub domain: usize,
    /// The IOVA of the grant's first byte.
    pub iova: u64,
    /// The mapping's ticket, unique among all grant mappings.
    pub ticket: u64,
}

impl Mapping {
    /// The record of an IOMMU mapping.
    #[cfg(iommu_enabled)]
    fn dma(record: DmaRecord) -> Self {
        Self {
            space: Weak::new(),
            span: PageSpan::empty(),
            bounces: Vec::new(),
            writable: false,
            dma: Some(record),
        }
    }

    /// A mapping of `span` in `space` without bounce pages.
    fn plain(space: &Arc<AddrSpaceWrapper>, span: PageSpan) -> Self {
        Self {
            space: Arc::downgrade(space),
            span,
            bounces: Vec::new(),
            writable: false,
            #[cfg(iommu_enabled)]
            dma: None,
        }
    }

    /// Copies a writable mapping's bounce pages back into the region's `frames`, once the
    /// mapping is gone.
    fn write_back(&self, frames: &mut [RaiiFrame]) {
        if self.writable {
            for bounce in &self.bounces {
                bounce.write_back(frames);
            }
        }
    }
}

/// A window page the grant covers only in part: a fresh frame mapped in its place, holding the
/// granted bytes and zeros elsewhere, so the holder never sees the rest of the region page.
/// It keeps the granted bytes as they were when it was filled, so that taking it down copies
/// back only what the holder changed ([`Bounce::write_back`]).
struct Bounce {
    frame: RaiiFrame,
    snapshot: Vec<u8>,
    page: PartialPage,
}

impl Bounce {
    /// A bounce page for `page`, filled from the region's frame.
    fn new(page: PartialPage, region_frame: &RaiiFrame) -> Result<Self> {
        let mut frame = RaiiFrame::allocate().map_err(|_| Error::new(ENOMEM))?;
        let granted = page.start..page.end;
        let src = region_frame
            .bytes()
            .get(granted.clone())
            .ok_or(Error::new(EINVAL))?;
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(src.len())
            .map_err(|_| Error::new(ENOMEM))?;
        snapshot.resize(src.len(), 0);
        // One read of each region byte fills both, so they agree even while the owner writes
        // the page; otherwise an untouched bounce page would look changed and its teardown
        // would copy stale bytes back.
        let bounce = frame
            .bytes_mut()
            .get_mut(granted)
            .ok_or(Error::new(EINVAL))?;
        ferric_regbuf::bounce::fill(bounce, &mut snapshot, src.iter().copied());
        Ok(Self {
            frame,
            snapshot,
            page,
        })
    }

    /// Copies the granted bytes the holder changed back into the region's frames, leaving the
    /// others as the region has them now. The mapping must be gone.
    fn write_back(&self, frames: &mut [RaiiFrame]) {
        let granted = self.page.start..self.page.end;
        let src = self.frame.bytes().get(granted.clone());
        let dst = frames
            .get_mut(self.page.region_page)
            .and_then(|frame| frame.bytes_mut().get_mut(granted));
        if let (Some(src), Some(dst)) = (src, dst) {
            ferric_regbuf::bounce::copy_changed(dst, src, &self.snapshot);
        }
    }
}

/// A region: its owner, its pinned frames (dropping them unpins) and its grants.
struct Region {
    uid: u32,
    frames: Vec<RaiiFrame>,
    grants: GrantTable<Mapping>,
}

impl Region {
    /// The frames to unpin and the user to refund, once the region has closed and nothing maps
    /// them (no teardown in flight, no failed one left a mapping); nothing before, and nothing
    /// the second time.
    fn release(&mut self) -> Option<(Vec<RaiiFrame>, u32)> {
        (self.grants.is_closed() && self.grants.drained() && !self.frames.is_empty())
            .then(|| (core::mem::take(&mut self.frames), self.uid))
    }
}

/// Unpins `frames` and refunds their charge to `uid`.
fn unpin((frames, uid): (Vec<RaiiFrame>, u32)) {
    let pages = frames.len() as u64;
    // Every mapping has its own frame references, so unpinning frees only unmapped frames.
    drop(frames);
    BUDGET.lock().refund(uid, pages);
}

/// Updates retry ownership and releases a closed region once its last teardown finishes.
/// Called with the region locked; returned pins must be released after dropping that lock.
fn finish_region_teardown(
    region: &Arc<Mutex<Region>>,
    guard: &mut Region,
) -> Option<(Vec<RaiiFrame>, u32)> {
    if guard.grants.is_closed() {
        let mut stuck = STUCK.lock();
        stuck.retain(|r| !Arc::ptr_eq(r, region));
        if guard.grants.idle() && !guard.grants.drained() {
            stuck.push(Arc::clone(region));
        }
    }
    guard.release()
}

/// An open handle.
#[derive(Clone)]
enum Handle {
    /// The scheme root opened by a user, for [`Request::Usage`].
    Root { uid: u32 },
    /// A region, the owner's capability.
    Region(Arc<Mutex<Region>>),
    /// A grant of a region.
    Grant {
        region: Arc<Mutex<Region>>,
        id: GrantId,
    },
}

/// Every open handle but the root, by number.
static HANDLES: Mutex<BTreeMap<usize, Handle>> = Mutex::new(BTreeMap::new());

/// The next handle number.
static NEXT_ID: AtomicUsize = AtomicUsize::new(ROOT + 1);

/// Pinned pages by user ID.
static BUDGET: Mutex<Budget> = Mutex::new(Budget::new());

/// Closed regions a failed unmap left with a mapping, so with their frames still pinned and
/// charged. Nobody holds a descriptor to them any more, so [`retry_stuck`] tries again on the
/// scheme's next close or `Revoke`. Changed only under the region's lock, which is taken
/// before this one, never after.
static STUCK: Mutex<Vec<Arc<Mutex<Region>>>> = Mutex::new(Vec::new());

/// Notified whenever teardowns finish, for `Revoke`s waiting on another thread's teardown of
/// their grant. One for every region: waiters check their own grant again when woken.
static TEARDOWN: WaitCondition = WaitCondition::new();

/// Stores `handle` and returns its number.
fn insert(handle: Handle) -> usize {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    HANDLES.lock().insert(id, handle);
    id
}

/// The handle with number `id`.
fn handle(id: usize) -> Result<Handle> {
    HANDLES.lock().get(&id).cloned().ok_or(Error::new(EBADF))
}

/// The limits on this machine.
fn limits() -> Limits {
    Limits::for_memory(used_frames().saturating_add(free_frames()) as u64)
}

/// The errno a failed grant table operation returns to user space.
fn table_errno(err: &TableError) -> Error {
    Error::new(match err {
        TableError::AlreadyRegistered => EEXIST,
        TableError::Unknown(_) | TableError::NoMapping(_) => ENOENT,
        TableError::Revoked(_) => ECANCELED,
        TableError::Access(_) => EACCES,
        TableError::IdsExhausted => ENOSPC,
        TableError::Closed => EBADF,
        _ => EINVAL,
    })
}

/// The errno a path that is not a grant path returns to user space.
fn path_errno(err: &PathError) -> Error {
    Error::new(match err {
        PathError::Form => ENOENT,
        _ => EINVAL,
    })
}

/// Unmaps `mapping`. A mapping whose address space has gone needs nothing. An IOMMU mapping
/// is unmapped with the IOTLB invalidated and waited on before this returns.
///
/// # Errors
///
/// What `munmap` returns, or `EIO` when the IOMMU unmap or its invalidation failed; the
/// mapping is then still there.
fn unmap_one(mapping: &Mapping, token: &mut CleanLockToken) -> Result<()> {
    #[cfg(iommu_enabled)]
    if let Some(record) = mapping.dma {
        return super::iommu::revoke_grant_mapping(record);
    }
    let Some(space) = mapping.space.upgrade() else {
        return Ok(());
    };
    // Unpinning is what lets the pinned borrows go; the holder could not have removed or
    // replaced them, so the span still holds exactly this mapping.
    let notify = space.munmap(mapping.span, true, token).inspect_err(|err| {
        warn!(
            "regbuf: cannot unmap {:#x} ({} pages): {err:?}",
            mapping.span.base.start_address().data(),
            mapping.span.count
        );
    })?;
    handle_notify_files(notify, token);
    Ok(())
}

/// Takes down the mappings `taken` out of `region`, reports each outcome to its table, and
/// unpins the region's frames if it has closed and this was its last teardown.
///
/// # Errors
///
/// The first unmap failure (`EIO` from `munmap`, or what it returned). Every mapping that failed
/// is recorded on its grant again, which is revoked or closed and so stays blocked, for the
/// next `Revoke` to retry.
fn take_down(
    region: &Arc<Mutex<Region>>,
    taken: Vec<Taken<Mapping>>,
    token: &mut CleanLockToken,
) -> Result<()> {
    let outcomes: Vec<_> = taken
        .into_iter()
        .map(|taken| {
            let outcome = unmap_one(&taken.mapping, token);
            (taken, outcome)
        })
        .collect();
    let mut failed = None;
    let released = {
        let mut guard = region.lock();
        for (taken, outcome) in outcomes {
            match outcome {
                // A closed region keeps its frames until this last teardown, and the owner may
                // still have the memory mapped, so the holder's last writes land there.
                Ok(()) => {
                    let region = &mut *guard;
                    region.grants.finish(taken).write_back(&mut region.frames);
                }
                Err(err) => {
                    guard.grants.restore(taken);
                    failed.get_or_insert(err);
                }
            }
        }
        finish_region_teardown(region, &mut guard)
    };
    TEARDOWN.notify(token);
    if let Some(released) = released {
        unpin(released);
    }
    failed.map_or(Ok(()), Err)
}

impl KernelScheme for RegBufScheme {
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
        let path = path.as_bytes();
        let path = path.strip_prefix(b"/").unwrap_or(path);
        let from = if file == ROOT {
            Handle::Root { uid: caller.uid }
        } else {
            handle(file)?
        };
        let handle = match from {
            Handle::Root { .. } if path.is_empty() => Handle::Root { uid: caller.uid },
            Handle::Root { .. } if path == REGION_PATH.as_bytes() => {
                Handle::Region(Arc::new(Mutex::new(Region {
                    uid: caller.uid,
                    frames: Vec::new(),
                    grants: GrantTable::new(),
                })))
            }
            Handle::Root { .. } => return Err(Error::new(ENOENT)),
            Handle::Region(region) => {
                let spec = GrantSpec::parse_path(path).map_err(|err| path_errno(&err))?;
                let id = region
                    .lock()
                    .grants
                    .create(spec)
                    .map_err(|err| table_errno(&err))?;
                Handle::Grant { region, id }
            }
            Handle::Grant { region, id } => {
                let spec = GrantSpec::parse_sub_path(path).map_err(|err| path_errno(&err))?;
                let sub = region
                    .lock()
                    .grants
                    .create_sub(id, spec)
                    .map_err(|err| table_errno(&err))?;
                Handle::Grant { region, id: sub }
            }
        };
        Ok(OpenResult::SchemeLocal(
            insert(handle),
            InternalFlags::empty(),
        ))
    }

    fn close(&self, id: usize, token: &mut CleanLockToken) -> Result<()> {
        retry_stuck(token);
        let handle = HANDLES.lock().remove(&id);
        match handle {
            Some(Handle::Region(region)) => close_region(&region, token),
            Some(Handle::Grant { region, id }) => {
                let gone = region.lock().grants.remove(id);
                take_down(&region, gone, token)
            }
            Some(Handle::Root { .. }) | None => Ok(()),
        }
    }

    fn kcall(
        &self,
        fds: &[usize],
        payload: UserSliceRw,
        _flags: CallFlags,
        metadata: &[u64],
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let request = Request::decode(metadata).map_err(|_| Error::new(EINVAL))?;
        let &[fd] = fds else {
            return Err(Error::new(EINVAL));
        };
        match (request, handle(fd)?) {
            (Request::Register { addr, len }, Handle::Region(region)) => {
                register(&region, addr, len, token)
            }
            (Request::Revoke(id), Handle::Region(region)) => {
                retry_stuck(token);
                revoke(&region, id, token)
            }
            (Request::Info, Handle::Grant { region, id }) => {
                let info = region
                    .lock()
                    .grants
                    .info(id)
                    .map_err(|err| table_errno(&err))?;
                copy_out(payload, &info.to_bytes(), GrantInfo::BYTES)
            }
            (Request::Unmap { addr }, Handle::Grant { region, id }) => {
                unmap(&region, id, addr, token)
            }
            (Request::Usage, Handle::Root { uid }) => usage(uid, payload),
            (Request::Usage, Handle::Region(region)) => {
                let uid = region.lock().uid;
                usage(uid, payload)
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn kfmap(
        &self,
        number: usize,
        addr_space: &Arc<AddrSpaceWrapper>,
        map: &Map,
        _consume: bool,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let Handle::Grant { region, id } = handle(number)? else {
            return Err(Error::new(EOPNOTSUPP));
        };
        if map
            .flags
            .intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE)
        {
            return Err(Error::new(EOPNOTSUPP));
        }
        if !map.flags.contains(MapFlags::MAP_SHARED) {
            return Err(Error::new(EINVAL));
        }
        let write = map.flags.contains(MapFlags::PROT_WRITE);
        let mut region = region.lock();
        let plan = region
            .grants
            .check_map(id, map.offset as u64, map.size as u64, write)
            .map_err(|err| table_errno(&err))?;
        let (frames, bounces) = plan_frames(&region.frames, &plan)?;
        let prot = map.flags & (MapFlags::PROT_READ | MapFlags::PROT_WRITE);
        let (span, result) = map_frames(addr_space, &frames, prot, token);
        let mapping = Mapping {
            space: Arc::downgrade(addr_space),
            span,
            bounces,
            writable: write,
            #[cfg(iommu_enabled)]
            dma: None,
        };
        if let Err(err) = result {
            // The pages mapped before the failure could not be unmapped again either: record
            // them on the grant, so revoking or closing it still takes them down.
            if span.count != 0 && region.grants.add_mapping(id, mapping).is_err() {
                warn!("regbuf: {id} lost a partial mapping it could not undo");
            }
            return Err(err);
        }
        if let Err((err, mapping)) = region.grants.add_mapping(id, mapping) {
            // Not reached: the region stayed locked since check_map.
            drop(region);
            unmap_one(&mapping, token)?;
            return Err(table_errno(&err));
        }
        Ok(span.base.start_address().data())
    }
}

/// Tries the teardowns of every region on [`STUCK`] again; those that fail again go back on it.
fn retry_stuck(token: &mut CleanLockToken) {
    let stuck = core::mem::take(&mut *STUCK.lock());
    for region in stuck {
        let gone = region.lock().grants.close();
        if take_down(&region, gone, token).is_err() {
            warn!("regbuf: a closed region still cannot unmap a grant mapping");
        }
    }
}

/// Closes `region`: revokes every grant, unmaps their mappings, and unpins and refunds its
/// frames, now or, when another thread is still tearing down a mapping of it, when that
/// finishes.
///
/// # Errors
///
/// `EIO` when a mapping could not be unmapped; it stays recorded on its revoked grant.
fn close_region(region: &Arc<Mutex<Region>>, token: &mut CleanLockToken) -> Result<()> {
    let gone = region.lock().grants.close();
    take_down(region, gone, token)
}

/// [`Request::Revoke`]: revokes grant `id` of `region`, with every grant derived from it, and
/// returns once nothing of them is mapped.
///
/// # Errors
///
/// `ENOENT` for a grant never made from the region, `EIO` when a mapping could not be unmapped
/// (it stays recorded, and blocked, for another `Revoke` to retry), and `EINTR` when a signal
/// interrupts the wait for another thread's teardown of the grant (`Revoke` again to wait on).
fn revoke(region: &Arc<Mutex<Region>>, id: GrantId, token: &mut CleanLockToken) -> Result<usize> {
    loop {
        let (gone, grants) = {
            let mut region = region.lock();
            let grants = region.grants.subtree_len(id);
            let gone = region.grants.revoke(id).map_err(|err| table_errno(&err))?;
            (gone, grants)
        };
        if !gone.is_empty() {
            info!(
                "regbuf: {id} revoked with {} grant(s) under it, unmapping {} mapping(s)",
                grants.saturating_sub(1),
                gone.len()
            );
        }
        take_down(region, gone, token)?;
        let guard = region.lock();
        if guard.grants.settled(id) {
            return Ok(0);
        }
        if guard.grants.recorded_under(id) != 0 {
            // Another thread's unmap failed and recorded its mapping again: try it here.
            continue;
        }
        // The grant's close or an Unmap is still unmapping some of it. Its take_down notifies
        // after reporting under the region lock, which this holds until it is waiting.
        let mut wait_token = token.downgrade::<L1>();
        if !TEARDOWN.wait(guard, "regbuf::revoke", &mut wait_token) {
            return Err(Error::new(EINTR));
        }
    }
}

/// Writes `bytes` into a payload that must be exactly `len` bytes long.
fn copy_out(payload: UserSliceRw, bytes: &[u8], len: usize) -> Result<usize> {
    if payload.len() != len {
        return Err(Error::new(EINVAL));
    }
    payload.copy_exactly(bytes)?;
    Ok(0)
}

/// [`Request::Usage`] for `uid`.
fn usage(uid: u32, payload: UserSliceRw) -> Result<usize> {
    let usage = BUDGET.lock().usage(uid, limits());
    copy_out(payload, &usage.to_bytes(), Usage::BYTES)
}

/// [`Request::Register`]: pins `len` bytes of the caller's memory from `addr` into `region`.
///
/// # Errors
///
/// `EINVAL` for an empty or unaligned range, or memory that is not mapped; `EEXIST` when the
/// region is registered already; `EDQUOT` over the pin limit; `EPERM` for memory that is not
/// private anonymous read-write memory (file-backed private mappings included); `ENOMEM` when
/// memory runs out; `EBADF` when the region closed meanwhile. Nothing stays pinned or charged
/// on error.
fn register(
    region: &Mutex<Region>,
    addr: u64,
    len: u64,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let addr = usize::try_from(addr).map_err(|_| Error::new(EINVAL))?;
    let len_bytes = usize::try_from(len).map_err(|_| Error::new(EINVAL))?;
    let span = PageSpan::validate_nonempty(VirtualAddress::new(addr), len_bytes)
        .ok_or(Error::new(EINVAL))?;
    let uid = {
        let region = region.lock();
        if region.grants.region_len() != 0 {
            return Err(Error::new(EEXIST));
        }
        region.uid
    };
    let pages = span.count as u64;
    BUDGET.lock().charge(uid, pages, limits()).map_err(|err| {
        warn!("regbuf: cannot pin {pages} pages: {err}");
        Error::new(EDQUOT)
    })?;
    // The commit is serialized with close_region by the region lock: a registration that
    // commits first has its frames released by the close, one that comes second fails with
    // EBADF (the table is closed) and refunds its charge below.
    let pinned = pin(span, token).and_then(|frames| {
        let mut region = region.lock();
        match region.grants.register(len) {
            Ok(()) => {
                region.frames = frames;
                Ok(())
            }
            Err(err) => Err(table_errno(&err)),
        }
    });
    if let Err(err) = pinned {
        BUDGET.lock().refund(uid, pages);
        return Err(err);
    }
    Ok(0)
}

/// Pins every page of `span` in the caller's address space.
fn pin(span: PageSpan, token: &mut CleanLockToken) -> Result<Vec<RaiiFrame>> {
    let space = AddrSpace::current()?;
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(span.count)
        .map_err(|_| Error::new(ENOMEM))?;
    for page in span.pages() {
        frames.push(space.borrow_frame_enforce_rw_anonymous(page, token)?);
    }
    Ok(frames)
}

/// The frames a mapping of `plan` maps, in order: the region's own, or a bounce page for each
/// page the grant covers only in part; and those bounce pages, which the mapping keeps.
fn plan_frames(region: &[RaiiFrame], plan: &MapPlan) -> Result<(Vec<Frame>, Vec<Bounce>)> {
    let pages = plan
        .pages
        .first
        .checked_add(plan.pages.count)
        .and_then(|end| region.get(plan.pages.first..end))
        .ok_or(Error::new(EINVAL))?;
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(pages.len())
        .map_err(|_| Error::new(ENOMEM))?;
    let mut bounces = Vec::new();
    for (index, frame) in pages.iter().enumerate() {
        match plan.partial_at(index) {
            Some(partial) => {
                let bounce = Bounce::new(partial, frame)?;
                frames.push(bounce.frame.get());
                bounces.push(bounce);
            }
            None => frames.push(frame.get()),
        }
    }
    Ok((frames, bounces))
}

/// Maps `frames` at a free span of `space` with protection `prot`, as one grant of pinned
/// shared borrows, and returns the span with the outcome.
///
/// On success the span covers every frame. If the page tables run out part way, the pages
/// mapped are unmapped again and the span is empty; if that unmap fails too, the span is what
/// is still mapped, for the caller to record.
fn map_frames(
    space: &Arc<AddrSpaceWrapper>,
    frames: &[Frame],
    prot: MapFlags,
    token: &mut CleanLockToken,
) -> (PageSpan, Result<()>) {
    let Some(count) = NonZeroUsize::new(frames.len()) else {
        return (PageSpan::empty(), Err(Error::new(EINVAL)));
    };
    // A shared reference cannot be added to a copy-on-write frame. The region's references are
    // shared and only its owner maps the frames privately, so this does not happen, but check
    // rather than rely on it.
    let shareable = frames.iter().all(|&frame| {
        get_page_info(frame)
            .and_then(|info| info.refcount())
            .is_some_and(|count| !matches!(count, RefCount::Cow(_)))
    });
    if !shareable {
        return (PageSpan::empty(), Err(Error::new(EIO)));
    }
    let flags = prot | MapFlags::MAP_SHARED;
    let mapped = {
        let mut lock_token = token.token();
        let mut guard = space.acquire_write(lock_token.downgrade());
        guard
            .mmap(
                space,
                None,
                count,
                flags,
                None,
                |base, f, mapper, flusher| {
                    let is_pinned = true;
                    Grant::allocated_shared(frames, base, f, mapper, flusher, is_pinned)
                },
            )
            .map(|base| {
                let pages = guard
                    .grants
                    .contains(base)
                    .map_or(0, |(_, info)| info.page_count());
                PageSpan::new(base, pages)
            })
    };
    let span = match mapped {
        Ok(span) => span,
        // Nothing was mapped: the grant takes no reference it does not keep.
        Err(err) => return (PageSpan::empty(), Err(err)),
    };
    if span.count == frames.len() {
        return (span, Ok(()));
    }
    // The page tables ran out part way.
    match unmap_one(&Mapping::plain(space, span), token) {
        Ok(()) => (PageSpan::empty(), Err(Error::new(ENOMEM))),
        Err(_) => (span, Err(Error::new(ENOMEM))),
    }
}

/// [`Request::Unmap`]: drops the caller's mapping of grant `id` that starts at `addr`.
fn unmap(
    region: &Arc<Mutex<Region>>,
    id: GrantId,
    addr: u64,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let addr = usize::try_from(addr).map_err(|_| Error::new(EINVAL))?;
    let base = Page::containing_address(VirtualAddress::new(addr));
    let space = AddrSpace::current()?;
    let mapping = region
        .lock()
        .grants
        .take_mapping(id, |m| {
            m.span.base == base && core::ptr::eq(m.space.as_ptr(), Arc::as_ptr(&space))
        })
        .map_err(|err| table_errno(&err))?;
    take_down(region, alloc::vec![mapping], token)?;
    Ok(0)
}

/// A grant a driver named by descriptor in `MapGrant`, resolved to its region.
#[cfg(iommu_enabled)]
pub struct DmaGrant {
    region: Arc<Mutex<Region>>,
    id: GrantId,
}

#[cfg(iommu_enabled)]
impl DmaGrant {
    /// The grant behind the calling process's descriptor `fd`.
    ///
    /// # Errors
    ///
    /// `EBADF` when `fd` is not open and `EINVAL` when it is not a grant of this scheme.
    pub fn from_fd(fd: usize, token: &mut CleanLockToken) -> Result<Self> {
        let (scheme, number) = {
            let current = context::current();
            let mut current = current.read(token.token());
            let (context, mut token) = current.token_split();
            let file = context
                .get_file(FileHandle::from(fd), &mut token)
                .ok_or(Error::new(EBADF))?;
            let desc = file.description.read(token.token());
            (desc.scheme_ref.clone(), desc.number)
        };
        if !matches!(scheme, KernelSchemeRef::Global(GlobalSchemes::RegBuf)) {
            return Err(Error::new(EINVAL));
        }
        match handle(number)? {
            Handle::Grant { region, id } => Ok(Self { region, id }),
            Handle::Root { .. } | Handle::Region(_) => Err(Error::new(EINVAL)),
        }
    }

    /// The grant's ID.
    pub fn id(&self) -> GrantId {
        self.id
    }

    /// The link from a mapping of this grant back to it.
    pub fn link(&self) -> DmaLink {
        DmaLink {
            region: Arc::downgrade(&self.region),
            id: self.id,
        }
    }

    /// Maps the grant into a domain: `map` gets the grant's frames in order and whether the
    /// device may write them, maps them and returns where; the grant then records the mapping,
    /// so revoking or closing it takes the mapping down. The region stays locked throughout,
    /// so a revocation either comes first (and the grant is refused) or finds the mapping.
    ///
    /// # Errors
    ///
    /// `ECANCELED` once the grant is revoked, `EINVAL` when it is not page-aligned, `ENOENT`
    /// when it is closed, and `map`'s error.
    pub fn map(&self, map: impl FnOnce(&[Frame], bool) -> Result<DmaRecord>) -> Result<DmaRecord> {
        let mut region = self.region.lock();
        let (pages, access) = region
            .grants
            .check_dma(self.id)
            .map_err(|err| table_errno(&err))?;
        let frames = pages
            .first
            .checked_add(pages.count)
            .and_then(|end| region.frames.get(pages.first..end))
            .ok_or(Error::new(EINVAL))?
            .iter()
            .map(RaiiFrame::get)
            .collect::<Vec<_>>();
        let record = map(&frames, access.allows_write())?;
        if let Err((err, _)) = region.grants.add_mapping(self.id, Mapping::dma(record)) {
            // Not reached: the region stayed locked since check_dma.
            drop(region);
            if super::iommu::revoke_grant_mapping(record).is_err() {
                warn!(
                    "regbuf: {} lost an IOMMU mapping it could not undo",
                    self.id
                );
            }
            return Err(table_errno(&err));
        }
        Ok(record)
    }
}

/// The way back from an IOMMU mapping to the grant that records it, so a driver's own
/// `UnmapGrant` can drop the record too.
#[cfg(iommu_enabled)]
pub struct DmaLink {
    region: Weak<Mutex<Region>>,
    id: GrantId,
}

#[cfg(iommu_enabled)]
impl DmaLink {
    /// Forgets the grant's record of `record`, a mapping the driver took down itself, so the
    /// grant does not keep it until it is revoked or closed. Nothing when the grant is gone,
    /// or a teardown of the grant has the record in hand already (it then finds the mapping
    /// gone, which counts as done). A closed region releases and refunds its frames once
    /// this removes its last mapping and no other teardown is in flight.
    pub fn forget(&self, record: DmaRecord) {
        let Some(region) = self.region.upgrade() else {
            return;
        };
        let released = {
            let mut guard = region.lock();
            if let Ok(taken) = guard
                .grants
                .take_teardown_mapping(self.id, |m| m.dma == Some(record))
            {
                drop(guard.grants.finish(taken));
            }
            finish_region_teardown(&region, &mut guard)
        };
        // Taking and finishing the record share the region lock, so no waiter can
        // observe this teardown in flight. Other teardowns notify when they finish.
        if let Some(released) = released {
            unpin(released);
        }
    }
}
