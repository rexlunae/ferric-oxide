//! Ferric Oxide: the `kernel.regbuf` scheme, registered buffers and revocable zero-copy grants
//! (io/registered-buffers-design.md in the project files; built with the `regbuf` feature).
//!
//! - Opening [`REGION_PATH`] from the scheme root gives an empty *region*.
//!   [`Request::Register`] on it pins a page-aligned range of the caller's private anonymous
//!   read-write memory (file-backed private mappings are refused): each frame gets a shared
//!   reference, the one user/kernel shared pages take
//!   (`AddrSpaceWrapper::borrow_frame_enforce_rw_anonymous`). If the owner forks, its child
//!   gets copies of those frames, so the region keeps pointing at the owner's memory. The pages
//!   are charged to the region opener's user ID ([`Budget`], `EDQUOT` over the limit) until the
//!   region closes.
//! - Opening a grant path (`GrantSpec::encode_path`) from a region gives a *grant* descriptor:
//!   read-only or read-write access to a page range of the region. Its holder, typically a
//!   scheme the owner passed it to, maps it with `mmap` (`MAP_SHARED`; write access needs a
//!   read-write grant). Each page becomes a pinned shared borrow of the region's frame, like the
//!   per-request borrows of `user.rs`: the holder cannot `munmap`, `mprotect` or `mremap` it
//!   (`EBUSY`), a fork of the holder does not inherit it, and the grant records every mapping.
//!   The holder drops one with [`Request::Unmap`]; closing the grant drops all of them.
//! - [`Request::Revoke`] on the region, or closing the region, marks grants revoked (mapping
//!   them again fails with `ECANCELED`) and unmaps every recorded mapping, with a TLB shootdown,
//!   before it returns. Closing the region also unpins its frames and refunds the charge.
//!
//! Locks: a region's lock is taken before an address space's lock (mapping), never after.
//! Every teardown (revocation, closing a grant or the region, [`Request::Unmap`]) takes the
//! mappings out of the region under its lock ([`Taken`], counted in flight by the table),
//! unmaps them after releasing it, and takes it again to report each outcome
//! ([`take_down`]). So:
//!
//! - `Revoke` returns only once nothing of the grant is mapped, even when another thread (the
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
    table::{GrantTable, TableError, Taken},
};
use spin::Mutex;
use syscall::{
    error::{
        Error, Result, EACCES, EBADF, ECANCELED, EDQUOT, EEXIST, EINTR, EINVAL, EIO, ENOENT,
        ENOMEM, ENOSPC, EOPNOTSUPP,
    },
    flag::{CallFlags, MapFlags},
};

use super::{CallerCtx, KernelScheme, OpenResult, StrOrBytes};
use crate::{
    context::{
        file::InternalFlags,
        memory::{handle_notify_files, AddrSpace, AddrSpaceWrapper, Grant, PageSpan},
    },
    memory::{free_frames, get_page_info, used_frames, Page, RaiiFrame, RefCount, VirtualAddress},
    sync::{CleanLockToken, WaitCondition, L1},
    syscall::{data::Map, usercopy::UserSliceRw},
};

/// The number of the scheme root.
const ROOT: usize = 0;

/// The scheme.
pub struct RegBufScheme;

/// A mapping a grant has made: the span of an address space its pages occupy.
struct Mapping {
    space: Weak<AddrSpaceWrapper>,
    span: PageSpan,
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

/// Unmaps `mapping`. A mapping whose address space has gone needs nothing.
///
/// # Errors
///
/// What `munmap` returns; the mapping is then still there.
fn unmap_one(mapping: &Mapping, token: &mut CleanLockToken) -> Result<()> {
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
                Ok(()) => drop(guard.grants.finish(taken)),
                Err(err) => {
                    guard.grants.restore(taken);
                    failed.get_or_insert(err);
                }
            }
        }
        if guard.grants.is_closed() {
            // Under the region lock, so the list agrees with the table. A region is on it
            // while only restored mappings are left; a teardown still in flight decides when it
            // finishes, and the one that drains the region takes it off.
            let mut stuck = STUCK.lock();
            stuck.retain(|r| !Arc::ptr_eq(r, region));
            if guard.grants.idle() && !guard.grants.drained() {
                stuck.push(Arc::clone(region));
            }
        }
        guard.release()
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
            Handle::Grant { .. } => return Err(Error::new(EACCES)),
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
        let range = region
            .grants
            .check_map(id, map.offset as u64, map.size as u64, write)
            .map_err(|err| table_errno(&err))?;
        let frames = range
            .first
            .checked_add(range.count)
            .and_then(|end| region.frames.get(range.first..end))
            .ok_or(Error::new(EINVAL))?;
        let prot = map.flags & (MapFlags::PROT_READ | MapFlags::PROT_WRITE);
        let (span, result) = map_frames(addr_space, frames, prot, token);
        let mapping = Mapping {
            space: Arc::downgrade(addr_space),
            span,
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

/// [`Request::Revoke`]: revokes grant `id` of `region` and returns once nothing of it is mapped.
///
/// # Errors
///
/// `ENOENT` for a grant never made from the region, `EIO` when a mapping could not be unmapped
/// (it stays recorded, and blocked, for another `Revoke` to retry), and `EINTR` when a signal
/// interrupts the wait for another thread's teardown of the grant (`Revoke` again to wait on).
fn revoke(region: &Arc<Mutex<Region>>, id: GrantId, token: &mut CleanLockToken) -> Result<usize> {
    loop {
        let gone = region
            .lock()
            .grants
            .revoke(id)
            .map_err(|err| table_errno(&err))?;
        if !gone.is_empty() {
            info!("regbuf: {id} revoked, unmapping {} mapping(s)", gone.len());
        }
        take_down(region, gone, token)?;
        let guard = region.lock();
        if guard.grants.settled(id) {
            return Ok(0);
        }
        if guard.grants.mapping_count(id) != 0 {
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

/// Maps `frames` at a free span of `space` with protection `prot`, as pinned shared borrows,
/// and returns the span with the outcome.
///
/// On success the span covers every frame. On failure the pages mapped before it are unmapped
/// again and the span is empty; if that unmap fails too, the span is what is still mapped, for
/// the caller to record.
fn map_frames(
    space: &Arc<AddrSpaceWrapper>,
    frames: &[RaiiFrame],
    prot: MapFlags,
    token: &mut CleanLockToken,
) -> (PageSpan, Result<()>) {
    let flags = prot | MapFlags::MAP_SHARED | MapFlags::MAP_FIXED_NOREPLACE;
    // A shared reference cannot be added to a copy-on-write frame. The region's references are
    // shared and only its owner maps the frames privately, so this does not happen, but check
    // rather than rely on it.
    let shareable = frames.iter().all(|frame| {
        get_page_info(frame.get())
            .and_then(|info| info.refcount())
            .is_some_and(|count| !matches!(count, RefCount::Cow(_)))
    });
    if !shareable {
        return (PageSpan::empty(), Err(Error::new(EIO)));
    }
    let mapped = {
        let mut lock_token = token.token();
        let mut guard = space.acquire_write(lock_token.downgrade());
        let Some(free) = guard.grants.find_free(guard.mmap_min, frames.len()) else {
            return (PageSpan::empty(), Err(Error::new(ENOMEM)));
        };
        let base = free.base;
        let mut outcome = (frames.len(), Ok(()));
        for (done, (frame, page)) in frames
            .iter()
            .zip(PageSpan::new(base, frames.len()).pages())
            .enumerate()
        {
            let mapped = guard.mmap(
                space,
                Some(page),
                NonZeroUsize::MIN,
                flags,
                None,
                |page, f, mapper, flusher| {
                    Grant::allocated_shared_one_page(frame.get(), page, f, mapper, flusher, true)
                },
            );
            if let Err(err) = mapped {
                outcome = (done, Err(err));
                break;
            }
        }
        (base, outcome.0, outcome.1)
    };
    let (base, done, result) = mapped;
    // A failed page took no reference (`Grant::allocated_shared_one_page` drops it), so undoing
    // the `done` pages before it leaves nothing behind.
    let span = PageSpan::new(base, done);
    if let Err(err) = result {
        let undo = Mapping {
            space: Arc::downgrade(space),
            span,
        };
        return match unmap_one(&undo, token) {
            Ok(()) => (PageSpan::empty(), Err(err)),
            Err(_) => (span, Err(err)),
        };
    }
    (span, Ok(()))
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
