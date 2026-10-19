//! Ferric Oxide: the `kernel.ring` scheme, submission/completion rings whose requests the
//! kernel runs on any scheme (io/rings-design.md in the project files; built with the `rings`
//! feature; format and calls in `ferric_ring`).
//!
//! - Opening [`RING_PATH`] from the scheme root gives an unset ring. [`Request::Setup`]
//!   allocates its memory, zeroed pages the ring owns ([`Frames`]), and writes the header;
//!   the application maps them with `mmap` (`MAP_SHARED`, the whole [`Layout::map_len`]).
//!   The mapping holds its own references to the frames, so the application may unmap it, and
//!   the ring's frames are freed once both the ring and every mapping are gone.
//! - [`Request::Enter`] is a *brokered* submission: in the caller's own context, the kernel
//!   takes complete chains from the SQ (`ferric_ring::chain`) and runs each SQE as the system
//!   call it stands for on the caller's descriptor: `Read` and `Write` as `read`/`write`
//!   (offset [`CURRENT_POSITION`]) or `SYS_READ2`/`SYS_WRITE2`, `Fsize`, `Fstat` and `Fsync`
//!   as the system calls of those names (`syscall/fs.rs`'s description helpers). A user
//!   scheme sees an ordinary scheme request (`scheme/user.rs`), so every scheme works
//!   unchanged, and the buffer is checked like any system call's. Each SQE gets one CQE; a
//!   chain is taken only when the CQ has room for all of its CQEs, so none is ever dropped:
//!   `Enter` sets the room aside first (`ferric_ring::Reserving`). After posting completions,
//!   `Enter` triggers `EVENT_READ` on the ring.
//! - A chain whose first SQE has `SqeFlags::ASYNC` goes to the ring's worker instead
//!   ([`worker`]): a kernel context the ring starts on first use, which runs such chains in
//!   order with the submitter's address space and credentials and posts their CQEs when they
//!   end. `Enter` with `min_complete` waits for completions (`completed`) while any are still
//!   to come.
//! - `fevent` reports `EVENT_READ` while the CQ holds completions the application has not
//!   taken.
//! - The file table ([`FileTable`]) holds file descriptions sent with
//!   [`Request::RegisterFiles`] (an `FD` call, `kfdwrite`). A fixed-file SQE runs on the
//!   description in its slot, so the file stays open for the ring after the application closes
//!   its descriptor. Descriptions of rings are refused, so no ring holds a ring open and closing
//!   the last descriptor of a ring always frees it. A description leaves the table through
//!   [`Request::UnregisterFiles`] or when the ring is closed, and every reference the scheme
//!   takes is given back through [`release_description`], which closes the description when it was the
//!   last one.
//!
//! The ring memory is shared with the application, which may write any of it at any time.
//! The kernel reads it only through its own mapping of the frames, with atomic accesses
//! (`RaiiFrame::words`), keeps its SQ head and CQ tail privately (`ferric_ring::queue`),
//! copies each SQE out once and checks it, and treats an out-of-range application index as a
//! corrupt ring (`EINVAL`). Only one `Enter` takes SQEs from a ring at a time: it takes the SQ
//! reader out of its lock for the duration (`Entered`), and a second one gets `EBUSY`. The CQ
//! producer is shared by `Enter` and the worker and locked only to set room aside or post. No
//! lock is held while an SQE runs, since that may block in another scheme.
//! `Enter` looks up every file a chain names when it takes the chain, before any of it runs,
//! and holds those descriptions until the chain is done ([`resolve_files`]), so a chain acts
//! on the files its submitter named even if another thread closes or reuses one of the
//! descriptors meanwhile, as a single system call would. An asynchronous chain's files are
//! looked up the same way and travel with it to the worker.
//!
//! Locks: `sq` before `cq` (only `Setup` holds both); `work`, `files` and `cq` are never held
//! together.
//!
//! Not yet (later PRs of the design): grant buffers (`EOPNOTSUPP`), the `SqPoll` and `Umwait`
//! notify modes (`EOPNOTSUPP`), and rings served natively by user schemes.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use ferric_ring::{
    abi::{header, Layout, NotifyMode, Request, PAGE_SIZE as RING_PAGE_SIZE, RING_PATH},
    chain::{Chain, Completions, Next, SqReader},
    entry::{Buffer, FileRef, Op, SqeError, CURRENT_POSITION},
    files::{FileTable, Refused, TableError},
    notify::wait_done,
    queue::{HeaderInfo, Memory, RingError},
    Consumer, Cqe, Reserving, Sqe,
};
use spin::{Mutex, Once};
use syscall::{
    error::{
        Error, Result, EBADF, EBUSY, ECANCELED, EEXIST, EINTR, EINVAL, EIO, ENOENT, ENOMEM,
        EOPNOTSUPP, EOVERFLOW,
    },
    flag::{CallFlags, EventFlags, MapFlags, EVENT_READ},
};

use super::{CallerCtx, FileHandle, KernelScheme, OpenResult, SchemeExt, StrOrBytes};
use crate::{
    context::{
        file::{InternalFlags, KernelSchemeRef, LockedFileDescription},
        memory::{handle_notify_files, AddrSpaceWrapper, Grant, PageSpan},
    },
    event,
    memory::{Frame, RaiiFrame, PAGE_SIZE},
    sync::{CleanLockToken, WaitCondition, L1},
    syscall::{
        data::{GlobalSchemes, Map},
        fs::{
            description_op, file_description, fstat_description, read_description,
            release_description, write_description,
        },
        usercopy::{UserSlice, UserSliceRw},
    },
};

/// The number of the scheme root.
const ROOT: usize = 0;

/// 64-bit words per page.
const WORDS_PER_PAGE: usize = PAGE_SIZE / 8;

const _: () = assert!(RING_PAGE_SIZE == PAGE_SIZE);

/// The scheme.
pub struct RingScheme;

/// A ring's memory: zeroed frames the ring owns, reached through the kernel's linear map.
struct Frames(Vec<RaiiFrame>);

impl Memory for Frames {
    fn word(&self, index: usize) -> Option<&AtomicU64> {
        self.0
            .get(index / WORDS_PER_PAGE)?
            .words()
            .get(index % WORDS_PER_PAGE)
    }

    fn len(&self) -> usize {
        self.0.len().saturating_mul(WORDS_PER_PAGE)
    }
}

/// What `Setup` fixes for the ring's life.
struct Shared {
    frames: Frames,
    layout: Layout,
}

mod worker;

/// A file description the ring holds a reference to.
type Description = Arc<LockedFileDescription>;

/// A ring.
struct Ring {
    /// The ring's handle number, which its events carry.
    id: usize,
    shared: Once<Shared>,
    /// The SQ's reader, with the kernel's private SQ head: `None` before `Setup`, and while
    /// an `Enter` holds it.
    sq: Mutex<Option<SqReader>>,
    /// The CQ's producer, with the kernel's private CQ tail: `None` before `Setup`.
    cq: Mutex<Option<Reserving<Cqe>>>,
    /// `None` once the ring is closed, so nothing is registered after the table was emptied.
    files: Mutex<Option<FileTable<Description>>>,
    /// The asynchronous chains and the worker.
    work: Mutex<worker::Work>,
    /// Notified when the worker has chains to run or the ring closes.
    jobs_ready: WaitCondition,
    /// Notified after completions are posted, for `Enter`s waiting with `min_complete`.
    completed: WaitCondition,
}

impl Ring {
    /// Sets aside room in the CQ for `count` completions; returns whether there was room.
    fn reserve(&self, mem: &Frames, count: u32) -> Result<bool> {
        self.cq
            .lock()
            .as_mut()
            .ok_or(Error::new(EINVAL))?
            .try_reserve(mem, count)
            .map_err(ring_errno)
    }

    /// Gives back room set aside for completions that will not be posted.
    fn unreserve(&self, count: u32) {
        if let Some(cq) = self.cq.lock().as_mut() {
            cq.unreserve(count);
        }
    }

    /// Posts a chain's completions into room set aside for them.
    ///
    /// # Errors
    ///
    /// `EINVAL` when the application moved its CQ head out of range.
    fn post(&self, mem: &Frames, done: &Completions) -> Result<()> {
        self.cq
            .lock()
            .as_mut()
            .ok_or(Error::new(EINVAL))?
            .post(mem, done.entries())
            .map_err(|_| Error::new(EINVAL))
    }

    /// Wakes `Enter`s waiting for completions, and event queues watching the ring.
    fn notify_completed(&self, token: &mut CleanLockToken) {
        self.completed.notify(token);
        event::trigger(GlobalSchemes::Ring.scheme_id(), self.id, EVENT_READ, token);
    }
}

/// The SQ reader of a ring, taken by the running `Enter` and put back when it ends.
struct Entered<'r> {
    ring: &'r Ring,
    sq: Option<SqReader>,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        *self.ring.sq.lock() = self.sq.take();
    }
}

/// Every open ring, by handle number.
static HANDLES: Mutex<BTreeMap<usize, Arc<Ring>>> = Mutex::new(BTreeMap::new());

/// The next handle number.
static NEXT_ID: AtomicUsize = AtomicUsize::new(ROOT + 1);

/// The ring with handle number `id`.
fn ring(id: usize) -> Result<Arc<Ring>> {
    HANDLES.lock().get(&id).cloned().ok_or(Error::new(EBADF))
}

/// The errno for a ring that cannot be used: the application corrupted it.
fn ring_errno(_err: RingError) -> Error {
    Error::new(EINVAL)
}

/// The errno an SQE that is not a valid request completes with.
fn sqe_errno(err: SqeError) -> Error {
    Error::new(match err {
        SqeError::Opcode(_) => EOPNOTSUPP,
        _ => EINVAL,
    })
}

/// The errno for a file table call the table refused.
fn table_errno(err: TableError) -> Error {
    Error::new(match err {
        TableError::Occupied { .. } => EBUSY,
        TableError::NoMemory => ENOMEM,
        _ => EINVAL,
    })
}

/// [`release_description`] for each of `descriptions`.
fn release_all(descriptions: Vec<Description>, token: &mut CleanLockToken) {
    for description in descriptions {
        release_description(description, token);
    }
}

/// [`Request::RegisterFiles`]: puts `descriptions` into the ring's file table from slot
/// `first` on, or hands them all back with the errno.
fn register_files(
    ring: &Ring,
    first: u32,
    descriptions: Vec<Description>,
    token: &mut CleanLockToken,
) -> core::result::Result<usize, (Error, Vec<Description>)> {
    let mut any_ring = false;
    for description in &descriptions {
        any_ring |= is_ring(description, token);
    }
    if any_ring {
        return Err((Error::new(EINVAL), descriptions));
    }
    let count = descriptions.len();
    let mut files = ring.files.lock();
    let Some(table) = files.as_mut() else {
        return Err((Error::new(EBADF), descriptions));
    };
    table
        .register(first, descriptions)
        .map(|()| count)
        .map_err(|Refused { error, files }| (table_errno(error), files))
}

/// [`Request::UnregisterFiles`]: empties `count` slots from `first` on and returns how many
/// held a file.
fn unregister_files(
    ring: &Ring,
    first: u32,
    count: u32,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let removed = ring
        .files
        .lock()
        .as_mut()
        .ok_or(Error::new(EBADF))?
        .unregister(first, count)
        .map_err(table_errno)?;
    let removed_count = removed.len();
    // Closing may call into another scheme, so it happens with the table unlocked.
    release_all(removed, token);
    Ok(removed_count)
}

/// [`Request::Setup`]: allocates the ring's memory and writes its header.
///
/// # Errors
///
/// `EOPNOTSUPP` for a notify mode other than `Event`, `EINVAL` for entry counts that make no
/// layout, `ENOMEM` when the frames cannot be allocated, `EEXIST` when the ring is set up.
fn setup(ring: &Ring, sq_entries: u32, cq_entries: u32, notify: NotifyMode) -> Result<usize> {
    if notify != NotifyMode::Event {
        return Err(Error::new(EOPNOTSUPP));
    }
    let layout = Layout::new(sq_entries, cq_entries).map_err(|_| Error::new(EINVAL))?;
    if ring.shared.is_completed() {
        return Err(Error::new(EEXIST));
    }
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(layout.pages())
        .map_err(|_| Error::new(ENOMEM))?;
    for _ in 0..layout.pages() {
        frames.push(RaiiFrame::allocate().map_err(|_| Error::new(ENOMEM))?);
    }
    let frames = Frames(frames);
    HeaderInfo::write(&frames, &layout, notify).map_err(|_| Error::new(EIO))?;

    let mut sq = ring.sq.lock();
    // A concurrent Setup may have won; its memory stays, this one's frames are freed.
    if ring.shared.is_completed() {
        return Err(Error::new(EEXIST));
    }
    ring.shared.call_once(|| Shared { frames, layout });
    *ring.cq.lock() = Some(Reserving::new(layout.cq()));
    *sq = Some(SqReader::new(Consumer::new(layout.sq())));
    Ok(0)
}

/// [`Request::Enter`]: submits up to `to_submit` SQEs ([`submit`]), wakes whoever watches
/// for the completions it posted, then waits until the CQ holds `min_complete` completions or
/// none is still to come; returns the SQEs consumed. With `to_submit` 0 it only waits, so any
/// number of threads may wait while one submits.
///
/// # Errors
///
/// As [`submit`]; `EINVAL` for a `min_complete` above the CQ's entries, and `EINTR` when a
/// signal ends the wait before anything was consumed (otherwise the count is returned).
fn enter(
    ring: &Arc<Ring>,
    to_submit: u32,
    min_complete: u32,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let shared = ring.shared.get().ok_or(Error::new(EINVAL))?;
    if min_complete > shared.layout.cq_entries() {
        return Err(Error::new(EINVAL));
    }
    let consumed = if to_submit == 0 {
        0
    } else {
        submit(ring, shared, to_submit, token)?
    };
    if min_complete > 0
        && let Err(err) = wait_for(ring, shared, min_complete, token)
        && consumed == 0
    {
        return Err(err);
    }
    Ok(consumed)
}

/// Takes complete chains from the SQ until `to_submit` SQEs are consumed (the last chain may
/// take it past), the SQ has no complete chain, or the CQ has no room for the next one. Runs
/// each chain, or hands it to the worker when it is asynchronous; returns the SQEs consumed.
///
/// Each chain counts as in flight from before it is consumed until its completions are posted
/// (or the worker takes it), and the completions are announced before the next chain runs, so an `Enter` waiting meanwhile neither stops
/// waiting early nor sleeps past completions already posted.
///
/// # Errors
///
/// `EINVAL` for a corrupt ring, `EBUSY` while another `Enter` runs or when the CQ has no room
/// for the first chain.
fn submit(
    ring: &Arc<Ring>,
    shared: &Shared,
    to_submit: u32,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let mut entered = Entered {
        ring,
        sq: ring.sq.lock().take(),
    };
    let sq = entered.sq.as_mut().ok_or(Error::new(EBUSY))?;
    let mem = &shared.frames;
    let mut consumed: u32 = 0;
    while consumed < to_submit {
        let next = sq.peek(mem).map_err(ring_errno)?;
        let (Next::Chain(chain) | Next::Refused(chain)) = next else {
            break;
        };
        if !ring.reserve(mem, chain.len())? {
            if consumed == 0 {
                return Err(Error::new(EBUSY));
            }
            break;
        }
        // The chain counts as in flight before its consumption is published, and until its
        // CQEs are posted (or the worker counts it), so a waiting `Enter` that sees it gone
        // from the SQ also sees it in flight.
        ring.work.lock().start_sync(chain.len());
        if let Err(err) = sq.consume(mem, &next) {
            ring.work.lock().end_sync(chain.len());
            ring.unreserve(chain.len());
            // A waiting `Enter` may have seen the chain in flight; with it gone, wake it to
            // check again, or it could sleep until some later completion.
            ring.notify_completed(token);
            return Err(ring_errno(err));
        }
        consumed = consumed.saturating_add(chain.len());

        let mut done = Completions::new();
        if matches!(next, Next::Refused(_)) {
            chain.refuse(errno_result(EINVAL), |cqe| {
                done.push(cqe);
            });
        } else if chain.is_async() {
            match worker::queue(ring, chain, token) {
                Ok(()) => {
                    // The worker counts the chain now.
                    ring.work.lock().end_sync(chain.len());
                    continue;
                }
                Err(errno) => chain.refuse(errno_result(errno), |cqe| {
                    done.push(cqe);
                }),
            }
        } else {
            let files = resolve_files(ring, &chain, token);
            run_chain(&chain, files, token, |cqe| {
                done.push(cqe);
            });
        }
        // The room was set aside, so posting fails only if the application moved its CQ head
        // out of range meanwhile.
        let posted = ring.post(mem, &done);
        ring.work.lock().end_sync(chain.len());
        ring.notify_completed(token);
        posted?;
    }
    Ok(consumed as usize)
}

/// The completions in the CQ that the application has not taken.
fn cq_pending(shared: &Shared) -> u32 {
    let load = |index| {
        shared
            .frames
            .word(index)
            .map_or(0, |word| word.load(Ordering::Acquire))
    };
    let pending = load(header::CQ_TAIL).wrapping_sub(load(header::CQ_HEAD));
    // A head the application moved past the tail counts as nothing pending.
    u32::try_from(pending).unwrap_or(0)
}

/// Waits until the CQ holds `min_complete` completions, or no asynchronous chain is still to
/// complete ([`wait_done`]).
///
/// # Errors
///
/// `EINTR` when a signal wakes the caller.
fn wait_for(
    ring: &Ring,
    shared: &Shared,
    min_complete: u32,
    token: &mut CleanLockToken,
) -> Result<()> {
    loop {
        let work = ring.work.lock();
        if wait_done(cq_pending(shared), min_complete, work.in_flight()) {
            return Ok(());
        }
        // The worker posts, then lowers `in_flight` under this lock, then notifies; waiting
        // with the lock handed to the wait means neither change is missed.
        let mut wait_token = token.downgrade::<L1>();
        if !ring.completed.wait(work, "ring::enter", &mut wait_token) {
            return Err(Error::new(EINTR));
        }
    }
}

/// The CQE result of a failure with `errno`.
fn errno_result(errno: i32) -> i64 {
    i64::from(errno).saturating_neg()
}

/// Each SQE's file in a chain, looked up when `Enter` takes the chain: `None` for an SQE
/// that names none.
type ChainFiles = Vec<Option<Result<Description>>>;

/// Looks up every file `chain` names and holds its description, before any of the chain
/// runs.
fn resolve_files(ring: &Ring, chain: &Chain, token: &mut CleanLockToken) -> ChainFiles {
    chain
        .entries()
        .iter()
        .map(|sqe| {
            let file = sqe.op().ok().and_then(|op| op.file());
            file.map(|file| resolve(ring, file, token))
        })
        .collect()
}

/// Whether `description` is a ring's.
fn is_ring(description: &Description, token: &mut CleanLockToken) -> bool {
    matches!(
        description.read(token.token()).scheme_ref,
        KernelSchemeRef::Global(GlobalSchemes::Ring)
    )
}

/// Refuses, with `EINVAL`, every ring among the files of an asynchronous chain, and lets go
/// of it. A job holds its files until it is done, and a ring closes only when the last
/// reference to its description goes, so a job holding its own ring while an earlier SQE
/// blocks would keep the ring from ever closing, and its worker from ever being cancelled.
fn refuse_rings(files: ChainFiles, token: &mut CleanLockToken) -> ChainFiles {
    files
        .into_iter()
        .map(|file| match file {
            Some(Ok(description)) if is_ring(&description, token) => {
                release_description(description, token);
                Some(Err(Error::new(EINVAL)))
            }
            other => other,
        })
        .collect()
}

/// [`release_description`] for every file of a chain that is still held.
fn release_files(files: ChainFiles, token: &mut CleanLockToken) {
    for description in files.into_iter().flatten().flatten() {
        release_description(description, token);
    }
}

/// Runs `chain` by its break rules on the files [`resolve_files`] found for it, passes each
/// CQE to `post`, and lets go of the files.
fn run_chain(
    chain: &Chain,
    mut files: ChainFiles,
    token: &mut CleanLockToken,
    post: impl FnMut(Cqe),
) {
    // `Chain::run` runs the SQEs in order, so the next slot is always the running SQE's.
    let mut slots = files.iter_mut();
    chain.run(
        |sqe| {
            let file = slots.next().and_then(Option::take);
            run(sqe, file, token)
        },
        errno_result(ECANCELED),
        post,
    );
    release_files(files, token);
}

/// Runs `sqe` on `file`, the file it names as `Enter` found it, and returns its CQE result.
fn run(sqe: &Sqe, file: Option<Result<Description>>, token: &mut CleanLockToken) -> i64 {
    let mut file = file;
    let result = sqe
        .op()
        .map_err(sqe_errno)
        .and_then(|op| run_op(op, &mut file, token));
    if let Some(Ok(description)) = file {
        release_description(description, token);
    }
    match result {
        Ok(value) => Cqe::encode_result(Ok(value as u64)),
        Err(err) => errno_result(err.errno),
    }
}

/// The description `file` names: the caller's descriptor's, or the one in a slot of the
/// ring's file table.
///
/// # Errors
///
/// `EBADF` when the descriptor is not open or the slot is empty.
fn resolve(ring: &Ring, file: FileRef, token: &mut CleanLockToken) -> Result<Description> {
    match file {
        FileRef::Fd(fd) => file_description(FileHandle::from(fd as usize), token),
        FileRef::Fixed(slot) => ring
            .files
            .lock()
            .as_ref()
            .and_then(|table| table.get(slot))
            .cloned()
            .ok_or(Error::new(EBADF)),
    }
}

/// Runs `op` on the SQE's file, taken from `file`; `op` lets go of it.
fn with_file<T>(
    file: &mut Option<Result<Description>>,
    token: &mut CleanLockToken,
    op: impl FnOnce(Description, &mut CleanLockToken) -> Result<T>,
) -> Result<T> {
    // The op hands the reference on to `description_op`, which lets go of it afterwards.
    op(file.take().unwrap_or(Err(Error::new(EBADF)))?, token)
}

/// The address and length of a buffer in the caller's address space.
fn user_buffer(buf: Buffer) -> Result<(usize, usize)> {
    match buf {
        Buffer::Addr { addr, len } => Ok((
            usize::try_from(addr).map_err(|_| Error::new(EINVAL))?,
            len as usize,
        )),
        Buffer::Grant { .. } => Err(Error::new(EOPNOTSUPP)),
    }
}

/// Runs `op` as the system call it stands for, on the caller's memory and `file`, the
/// description of the descriptor it names.
fn run_op(
    op: Op,
    file: &mut Option<Result<Description>>,
    token: &mut CleanLockToken,
) -> Result<usize> {
    match op {
        Op::Nop => Ok(0),
        Op::Read { offset, buf, .. } => {
            let (addr, len) = user_buffer(buf)?;
            let buf = UserSlice::wo(addr, len)?;
            with_file(file, token, |description, token| {
                if offset == CURRENT_POSITION {
                    return read_description(description, buf, token);
                }
                description_op(description, token, |scheme, _, desc, token| {
                    scheme.kreadoff(desc.number, buf, offset, desc.flags, desc.flags, token)
                })
            })
        }
        Op::Write { offset, buf, .. } => {
            let (addr, len) = user_buffer(buf)?;
            let buf = UserSlice::ro(addr, len)?;
            with_file(file, token, |description, token| {
                if offset == CURRENT_POSITION {
                    return write_description(description, buf, token);
                }
                description_op(description, token, |scheme, _, desc, token| {
                    scheme.kwriteoff(desc.number, buf, offset, desc.flags, desc.flags, token)
                })
            })
        }
        Op::Fsize { .. } => with_file(file, token, |description, token| {
            description_op(description, token, |scheme, _, desc, token| {
                usize::try_from(scheme.fsize(desc.number, token)?)
                    .map_err(|_| Error::new(EOVERFLOW))
            })
        }),
        Op::Fstat { buf, .. } => {
            let (addr, len) = user_buffer(buf)?;
            let buf = UserSlice::wo(addr, len)?;
            with_file(file, token, |description, token| {
                fstat_description(description, buf, token).map(|()| 0)
            })
        }
        Op::Fsync { .. } => with_file(file, token, |description, token| {
            description_op(description, token, |scheme, _, desc, token| {
                scheme.fsync(desc.number, token).map(|()| 0)
            })
        }),
        _ => Err(Error::new(EOPNOTSUPP)),
    }
}

/// Maps the ring's `frames` into `space`, anywhere, as one shared grant the process may unmap.
///
/// # Errors
///
/// `ENOMEM` when the address space or its page tables have no room.
fn map_frames(
    space: &Arc<AddrSpaceWrapper>,
    frames: &[RaiiFrame],
    prot: MapFlags,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let frames: Vec<Frame> = frames.iter().map(RaiiFrame::get).collect();
    let count = NonZeroUsize::new(frames.len()).ok_or(Error::new(EINVAL))?;
    let span = {
        let mut lock_token = token.token();
        let mut guard = space.acquire_write(lock_token.downgrade());
        let base = guard.mmap(
            space,
            None,
            count,
            prot | MapFlags::MAP_SHARED,
            None,
            |base, flags, mapper, flusher| {
                let is_pinned = false;
                Grant::allocated_shared(&frames, base, flags, mapper, flusher, is_pinned)
            },
        )?;
        let pages = guard
            .grants
            .contains(base)
            .map_or(0, |(_, info)| info.page_count());
        PageSpan::new(base, pages)
    };
    if span.count == frames.len() {
        return Ok(span.base.start_address().data());
    }
    // The page tables ran out part way: undo the part that was mapped.
    let notify = space.munmap(span, false, token)?;
    handle_notify_files(notify, token);
    Err(Error::new(ENOMEM))
}

impl KernelScheme for RingScheme {
    fn scheme_root(&self, _token: &mut CleanLockToken) -> Result<usize> {
        Ok(ROOT)
    }

    fn kopenat(
        &self,
        file: usize,
        path: StrOrBytes,
        _flags: usize,
        _fcntl_flags: u32,
        _caller: CallerCtx,
        _token: &mut CleanLockToken,
    ) -> Result<OpenResult> {
        let path = path.as_bytes();
        let path = path.strip_prefix(b"/").unwrap_or(path);
        if file != ROOT || path != RING_PATH.as_bytes() {
            return Err(Error::new(ENOENT));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let ring = Arc::new(Ring {
            id,
            shared: Once::new(),
            sq: Mutex::new(None),
            cq: Mutex::new(None),
            files: Mutex::new(Some(FileTable::new())),
            work: Mutex::new(worker::Work::new()),
            jobs_ready: WaitCondition::new(),
            completed: WaitCondition::new(),
        });
        HANDLES.lock().insert(id, ring);
        Ok(OpenResult::SchemeLocal(id, InternalFlags::empty()))
    }

    fn close(&self, id: usize, token: &mut CleanLockToken) -> Result<()> {
        // A running Enter and the worker keep their own references; the frames go with the
        // last one, or with the last mapping. The files go now: an Enter or a chain still
        // running holds its own reference to the one it is using. The worker cancels the
        // chains still waiting and ends.
        let ring = HANDLES.lock().remove(&id).ok_or(Error::new(EBADF))?;
        let files = ring.files.lock().take();
        if let Some(mut table) = files {
            release_all(table.take_all(), token);
        }
        worker::close(&ring, token);
        Ok(())
    }

    fn kcall(
        &self,
        fds: &[usize],
        _payload: UserSliceRw,
        _flags: CallFlags,
        metadata: &[u64],
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let &[id] = fds else {
            return Err(Error::new(EINVAL));
        };
        let ring = ring(id)?;
        match Request::decode(metadata).map_err(|_| Error::new(EINVAL))? {
            Request::Setup {
                sq_entries,
                cq_entries,
                notify,
            } => setup(&ring, sq_entries, cq_entries, notify),
            Request::Enter {
                to_submit,
                min_complete,
            } => enter(&ring, to_submit, min_complete, token),
            Request::UnregisterFiles { first, count } => {
                unregister_files(&ring, first, count, token)
            }
            // RegisterFiles needs descriptors: it comes as an FD call, to `kfdwrite`.
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn kfdwrite(
        &self,
        id: usize,
        descs: Vec<Arc<LockedFileDescription>>,
        _flags: CallFlags,
        metadata: &[u64],
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let registered = match (ring(id), Request::decode(metadata)) {
            (Ok(ring), Ok(Request::RegisterFiles { first })) => {
                register_files(&ring, first, descs, token)
            }
            (Err(err), _) => Err((err, descs)),
            (Ok(_), _) => Err((Error::new(EINVAL), descs)),
        };
        // The descriptors were sent to the ring; any it does not keep are closed, as a
        // scheme closes the descriptors of a call it refuses.
        registered.map_err(|(err, descs)| {
            release_all(descs, token);
            err
        })
    }

    fn fevent(
        &self,
        id: usize,
        flags: EventFlags,
        _token: &mut CleanLockToken,
    ) -> Result<EventFlags> {
        let ring = ring(id)?;
        let Some(shared) = ring.shared.get() else {
            return Ok(EventFlags::empty());
        };
        if cq_pending(shared) != 0 {
            Ok(flags & EVENT_READ)
        } else {
            Ok(EventFlags::empty())
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
        let ring = ring(number)?;
        let shared = ring.shared.get().ok_or(Error::new(EINVAL))?;
        if map
            .flags
            .intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE)
        {
            return Err(Error::new(EOPNOTSUPP));
        }
        if !map.flags.contains(MapFlags::MAP_SHARED)
            || map.offset != 0
            || map.size != shared.layout.map_len()
        {
            return Err(Error::new(EINVAL));
        }
        let prot = map.flags & (MapFlags::PROT_READ | MapFlags::PROT_WRITE);
        map_frames(addr_space, &shared.frames.0, prot, token)
    }
}
