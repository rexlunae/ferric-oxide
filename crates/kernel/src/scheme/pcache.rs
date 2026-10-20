//! Ferric Oxide: the `kernel.pcache` scheme, the shared page cache's read path
//! (io/pagecache-design.md in the project files; built with the `page_cache` feature).
//!
//! A file system keeps the contents of its files in kernel *objects*, and the kernel serves
//! `read` and `pread` of the files from them, without a round trip to the file system when the
//! pages are there:
//!
//! - The file system opens a *queue* ([`QUEUE_PATH`](ferric_pcache::abi::QUEUE_PATH), root
//!   only) and binds it to its scheme by passing its scheme socket
//!   ([`Request::Bind`](ferric_pcache::abi::Request::Bind)). The queue then serves only that
//!   scheme's descriptions.
//! - It opens an object path from the queue for each file it caches (an empty object of size
//!   0), sets its size, and attaches the descriptions its clients open
//!   ([`Request::Attach`](ferric_pcache::abi::Request::Attach), by the handle number its scheme
//!   gave them).
//! - A positioned `read` or `pread` of an attached description (`UserScheme::kreadoff`) comes
//!   here first ([`read_attached`](crate::scheme::pcache::read_attached)). Pages that hold
//!   the file's data are copied to the reader from the object's frames. Absent ones are marked filling and become `Fill` records on the
//!   queue, with readahead past them (`ferric_pcache::readahead`); the reader waits for them.
//!   The file system reads records from the queue, and answers each with
//!   [`Request::Filled`](ferric_pcache::abi::Request::Filled) on the object, whose payload the
//!   kernel copies into new frames. A failed fill fails the readers waiting for it with
//!   `EIO`; the next read fills again.
//! - Closing a description detaches it (`UserScheme::close` calls
//!   [`closed`](crate::scheme::pcache::closed)). Closing an object, or its queue, ends the
//!   cache of it: outstanding fills fail, their readers get `EIO`, and later reads of its
//!   descriptions go to the file system's scheme as before.
//!
//! Only reads are served here. Mappings, writes, writeback and reclaim are later steps of the
//! design, so an object holds every page read until it closes. The page index, fill planning
//! and readahead are the host-tested `ferric-pcache` crate (`crates/pcache`), which also has
//! the call encoding file systems use.
//!
//! Locks: the attachment table may be held while a queue's or an object's state is taken,
//! never the other way round; the handle table, a queue's state and an object's state are
//! otherwise taken alone. A reader waits on an object's `changed` condition holding only that
//! object's lock. Nothing is copied to or from user memory while any of them is held.

use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicUsize, Ordering};

use ferric_pcache::{
    abi::{
        Generation, ObjectId, Outcome, PathError, Record, RecordPages, Request, OBJECT_PREFIX,
        QUEUE_PATH,
    },
    object::{Attempt, FillError, PageIndex, Read},
    readahead::{Readahead, ReadaheadConfig},
};
use spin::Mutex;
use syscall::{
    error::{
        Error, Result, EACCES, EAGAIN, EBADF, EBUSY, EEXIST, EINTR, EINVAL, EIO, ENOENT, ENOMEM,
        EOPNOTSUPP, EOVERFLOW, EPERM, ESTALE,
    },
    flag::{CallFlags, EventFlags, EVENT_READ, O_NONBLOCK},
};

use super::{
    user::UserInner, CallerCtx, KernelScheme, OpenResult, SchemeExt, SchemeId, SchemeList,
    StrOrBytes,
};
use crate::{
    context::{
        self,
        file::{InternalFlags, KernelSchemeRef, LockedFileDescription},
    },
    event,
    memory::{RaiiFrame, PAGE_SIZE},
    sync::{CleanLockToken, WaitCondition, L1},
    syscall::{
        data::GlobalSchemes,
        fs::release_description,
        usercopy::{UserSliceRw, UserSliceWo},
    },
};

const _: () = assert!(ferric_pcache::abi::PAGE_SIZE as usize == PAGE_SIZE);

/// The number of the scheme root.
const ROOT: usize = 0;

/// The scheme.
pub struct PcacheScheme;

/// An open handle of the scheme.
#[derive(Clone)]
enum Handle {
    Root,
    Queue(Arc<Queue>),
    Object(Arc<Object>),
}

/// The file system's scheme a queue is bound to.
struct Bound {
    id: SchemeId,
    inner: Weak<UserInner>,
}

/// The process the current context belongs to: the pid the process manager gives every
/// thread of a process (`Context::pid`, set through `proc`'s attributes). It stays the same
/// across `exec`, which replaces the address space, and differs after `fork`.
/// (`owner_proc_id` names one context, not the process: each thread is a context of its
/// own.)
fn current_process(token: &mut CleanLockToken) -> usize {
    context::current().read(token.token()).pid
}

/// Refuses a call on `queue`, or one of its objects, from a process other than the one that
/// opened it: a descriptor passed on must not let another process bind the queue to its own
/// scheme, or attach or fill the file system's descriptions. Any thread of the opener may
/// call, before or after it `exec`s.
fn check_owner(queue: &Queue, token: &mut CleanLockToken) -> Result<()> {
    if current_process(token) == queue.owner {
        Ok(())
    } else {
        Err(Error::new(EPERM))
    }
}

/// A file system's fill queue.
struct Queue {
    /// Its handle number, for events.
    id: usize,
    /// The pid of the process that opened the queue, the only one that may use it and its
    /// objects ([`current_process`]).
    owner: usize,
    state: Mutex<QueueState>,
    /// Notified when records arrive or the queue closes.
    records_ready: WaitCondition,
}

struct QueueState {
    records: VecDeque<Record>,
    /// Closed: no more records are taken.
    closed: bool,
    bound: Option<Bound>,
    /// The queue's live objects by ID.
    objects: BTreeMap<ObjectId, Weak<Object>>,
    /// The generation the next object opened gets; `None` once they have run out.
    next_generation: Option<Generation>,
    readahead: ReadaheadConfig,
}

impl Queue {
    /// Whether the queue serves `inner`'s descriptions.
    fn serves(&self, inner: &Arc<UserInner>) -> bool {
        self.state
            .lock()
            .bound
            .as_ref()
            .is_some_and(|bound| Weak::as_ptr(&bound.inner) == Arc::as_ptr(inner))
    }

    /// Puts fill records for `object` (of `generation`) on the queue and wakes the file
    /// system.
    ///
    /// # Errors
    ///
    /// `EIO` when the queue has closed.
    fn push(
        &self,
        object: ObjectId,
        generation: Generation,
        fills: &[RecordPages],
        token: &mut CleanLockToken,
    ) -> Result<()> {
        {
            let mut state = self.state.lock();
            if state.closed {
                return Err(Error::new(EIO));
            }
            state
                .records
                .try_reserve(fills.len())
                .map_err(|_| Error::new(ENOMEM))?;
            state
                .records
                .extend(fills.iter().map(|&pages| Record::Fill {
                    object,
                    generation,
                    pages,
                }));
        }
        self.wake(token);
        Ok(())
    }

    /// Wakes readers of the queue and signals `EVENT_READ`: records are there.
    fn wake(&self, token: &mut CleanLockToken) {
        self.records_ready.notify(token);
        event::trigger(
            GlobalSchemes::Pcache.scheme_id(),
            self.id,
            EVENT_READ,
            token,
        );
    }
}

/// A cache object.
struct Object {
    id: ObjectId,
    /// Which object of that ID on the queue this is; records and answers carry it.
    generation: Generation,
    queue: Arc<Queue>,
    state: Mutex<ObjectState>,
    /// Notified when fills are answered or the object goes.
    changed: WaitCondition,
}

struct ObjectState {
    index: PageIndex<Arc<RaiiFrame>>,
    /// The object or its queue closed: nothing more is served from it.
    gone: bool,
}

/// An attached description: the object it reads and its readahead state.
#[derive(Clone)]
struct Attachment {
    object: Arc<Object>,
    readahead: Arc<Mutex<Readahead>>,
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(ROOT + 1);
static HANDLES: Mutex<BTreeMap<usize, Handle>> = Mutex::new(BTreeMap::new());
/// Attached descriptions, by the file system's scheme number and the description's handle
/// number there.
static ATTACHED: Mutex<BTreeMap<(usize, usize), Attachment>> = Mutex::new(BTreeMap::new());

/// The handle with number `id`.
fn handle(id: usize) -> Result<Handle> {
    HANDLES.lock().get(&id).cloned().ok_or(Error::new(EBADF))
}

/// The errno for a fill answer the object does not expect.
fn fill_errno(err: &FillError) -> Error {
    Error::new(match err {
        FillError::NotFilling { .. } | FillError::Frames { .. } => EINVAL,
        _ => EIO,
    })
}

/// The errno for an object path that is not one.
fn path_errno(_err: &PathError) -> Error {
    Error::new(ENOENT)
}

/// Serves a read of `handle` on `inner`'s scheme from the page cache, if the description is
/// attached to an object and the read is positioned: `None` sends it to the scheme as before.
///
/// # Errors
///
/// `EIO` when a fill the read waited for failed or the object went meanwhile, `EINTR` when a
/// signal interrupted the wait, `ENOMEM` when the fill records could not be queued, and
/// `EFAULT` for a bad buffer.
pub fn read_attached(
    inner: &Arc<UserInner>,
    handle: usize,
    buf: UserSliceWo,
    offset: u64,
    token: &mut CleanLockToken,
) -> Option<Result<usize>> {
    // An unpositioned read leaves the position to the scheme, which then keeps it.
    if offset == u64::MAX {
        return None;
    }
    let attachment = ATTACHED
        .lock()
        .get(&(inner.scheme_id.get(), handle))
        .cloned()?;
    if !attachment.object.queue.serves(inner) {
        return None;
    }
    read(&attachment, buf, offset, token)
}

/// Forgets the attachment of `handle` on `inner`'s scheme, which its client closed.
pub fn closed(inner: &Arc<UserInner>, handle: usize) {
    let mut attached = ATTACHED.lock();
    let key = (inner.scheme_id.get(), handle);
    if attached
        .get(&key)
        .is_some_and(|attachment| attachment.object.queue.serves(inner))
    {
        attached.remove(&key);
    }
}

/// [`read_attached`] for an attached description.
fn read(
    attachment: &Attachment,
    buf: UserSliceWo,
    offset: u64,
    token: &mut CleanLockToken,
) -> Option<Result<usize>> {
    let object = &attachment.object;
    let (pages, file_pages) = {
        let state = object.state.lock();
        if state.gone {
            return None;
        }
        match state.index.pages_for_read(offset, buf.len()) {
            Some((pages, _)) => (pages, state.index.file_pages()),
            None => return Some(Ok(0)),
        }
    };
    let ahead = attachment.readahead.lock().on_read(pages, file_pages);
    let mut attempt = Attempt::new();
    loop {
        let mut state = object.state.lock();
        if state.gone {
            return if attempt.is_first() {
                None
            } else {
                Some(Err(Error::new(EIO)))
            };
        }
        // The size may have changed while the reader waited.
        let Some((pages, len)) = state.index.pages_for_read(offset, buf.len()) else {
            return Some(Ok(0));
        };
        let first = attempt.is_first();
        // This records the fills `attempt` waits for, and makes it a retry next round.
        let read = state.index.start_read(pages, &mut attempt);
        let mut fills = match &read {
            Read::Wait { fills } => fills.clone(),
            Read::Ready | Read::Failed => Vec::new(),
        };
        if first && let Some(ahead) = ahead {
            fills.extend(state.index.prefetch(ahead));
        }
        match read {
            Read::Failed => return Some(Err(Error::new(EIO))),
            Read::Ready => {
                let frames = state.index.frames(pages);
                drop(state);
                // Only readahead is queued here: if the queue cannot take it, the read itself
                // can still go on.
                let _ = queue_fills(object, &fills, token);
                return Some(match frames {
                    Some(frames) => copy_out(&frames, buf, offset, len),
                    None => Err(Error::new(EIO)),
                });
            }
            Read::Wait { .. } if !fills.is_empty() => {
                drop(state);
                if let Err(err) = queue_fills(object, &fills, token) {
                    return Some(Err(err));
                }
            }
            Read::Wait { .. } => {
                let mut wait_token = token.downgrade::<L1>();
                if !object.changed.wait(state, "pcache::read", &mut wait_token) {
                    return Some(Err(Error::new(EINTR)));
                }
            }
        }
    }
}

/// Puts `fills` of `object` on its queue. If the queue cannot take them (it closed, or there
/// is no memory), the fills fail at once, so nobody waits for them.
fn queue_fills(object: &Object, fills: &[RecordPages], token: &mut CleanLockToken) -> Result<()> {
    if fills.is_empty() {
        return Ok(());
    }
    let Err(err) = object
        .queue
        .push(object.id, object.generation, fills, token)
    else {
        return Ok(());
    };
    {
        let mut state = object.state.lock();
        for &pages in fills {
            // The pages were marked filling above and nobody else answers them.
            let _ = state
                .index
                .complete_fill(pages, Outcome::Failed(EIO_OUTCOME), Vec::new());
        }
    }
    object.changed.notify(token);
    Err(err)
}

/// The outcome of a fill the kernel fails itself.
const EIO_OUTCOME: core::num::NonZeroU16 = match core::num::NonZeroU16::new(EIO as u16) {
    Some(errno) => errno,
    None => panic!("EIO is not zero"),
};

/// Copies `len` bytes from `offset` in the pages held by `frames` (the first holding the page
/// `offset` is in) to `buf`.
///
/// A copy of more than one page stages each page whose copy would be 4K-aliased through a
/// small bounce buffer (`ferric_pcache::copy`). On some cores, such as AMD Zen 3, copying a
/// page frame straight to a buffer a few bytes into a page runs several times slower. A
/// one-page read copies from a hot page and is left alone. The bounce buffer is on the
/// stack (3 KiB of the 64 KiB kernel stack), so a read never allocates and cannot fail for
/// want of heap memory.
fn copy_out(frames: &[Arc<RaiiFrame>], buf: UserSliceWo, offset: u64, len: usize) -> Result<usize> {
    use ferric_pcache::copy::{aliased, stage, Bounce, BOUNCE_LEN};

    let overflow = || Error::new(EINVAL);
    let first = usize::try_from(offset.checked_rem(PAGE_SIZE as u64).ok_or_else(overflow)?)
        .map_err(|_| overflow())?;
    let mut rest = buf.limit(len).ok_or_else(overflow)?;
    // Zeroed only by a read that needs it.
    let mut bounce: Option<Bounce> = None;
    for (i, frame) in frames.iter().enumerate() {
        if rest.is_empty() {
            break;
        }
        let at = if i == 0 { first } else { 0 };
        let src = frame.bytes().get(at..).ok_or_else(overflow)?;
        let n = src.len().min(rest.len());
        let (dst, tail) = rest.split_at(n).ok_or_else(overflow)?;
        let src = src.get(..n).ok_or_else(overflow)?;
        if frames.len() > 1 && aliased(src.as_ptr() as usize, dst.addr()) {
            let bounce = bounce.get_or_insert([0; BOUNCE_LEN]);
            stage(src, dst.addr(), bounce, |at, bytes| {
                dst.advance(at)
                    .and_then(|piece| piece.limit(bytes.len()))
                    .ok_or_else(overflow)?
                    .copy_from_slice(bytes)
            })?;
        } else {
            dst.copy_from_slice(src)?;
        }
        rest = tail;
    }
    len.checked_sub(rest.len()).ok_or_else(overflow)
}

/// Opens a queue for `caller` (root only), owned by the calling process.
fn open_queue(caller: &CallerCtx, token: &mut CleanLockToken) -> Result<Handle> {
    if caller.uid != 0 {
        return Err(Error::new(EACCES));
    }
    let owner = current_process(token);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    Ok(Handle::Queue(Arc::new(Queue {
        id,
        owner,
        state: Mutex::new(QueueState {
            records: VecDeque::new(),
            closed: false,
            bound: None,
            objects: BTreeMap::new(),
            next_generation: Some(Generation::default()),
            readahead: ReadaheadConfig::DEFAULT,
        }),
        records_ready: WaitCondition::new(),
    })))
}

/// Creates object `path` on `queue`.
fn open_object(queue: &Arc<Queue>, path: &[u8]) -> Result<Handle> {
    let id = ObjectId::parse_path(path).map_err(|err| path_errno(&err))?;
    let mut state = queue.state.lock();
    if state.closed {
        return Err(Error::new(EBADF));
    }
    if state
        .objects
        .get(&id)
        .is_some_and(|object| object.strong_count() > 0)
    {
        return Err(Error::new(EEXIST));
    }
    // Generations never repeat on a queue, so an old answer can never pass for a new one.
    let generation = state.next_generation.ok_or(Error::new(EOVERFLOW))?;
    state.next_generation = generation.next();
    let object = Arc::new(Object {
        id,
        generation,
        queue: Arc::clone(queue),
        state: Mutex::new(ObjectState {
            index: PageIndex::new(),
            gone: false,
        }),
        changed: WaitCondition::new(),
    });
    state.objects.insert(id, Arc::downgrade(&object));
    Ok(Handle::Object(object))
}

/// Ends the cache of `object`: outstanding fills fail, waiting readers wake, and its
/// descriptions are detached.
fn end_object(object: &Arc<Object>, token: &mut CleanLockToken) {
    let failed = {
        let mut state = object.state.lock();
        state.gone = true;
        state.index.fail_all()
    };
    if failed != 0 {
        info!(
            "pcache: object {} went with {failed} page(s) being filled",
            object.id.get()
        );
    }
    object.changed.notify(token);
    ATTACHED
        .lock()
        .retain(|_, attachment| !Arc::ptr_eq(&attachment.object, object));
}

/// Closes `object`'s handle. Its records still on the queue go with it, so a later object
/// with the same ID never sees them; one the file system already took is refused by its
/// generation.
fn close_object(object: &Arc<Object>, token: &mut CleanLockToken) {
    {
        let mut queue = object.queue.state.lock();
        if queue
            .objects
            .get(&object.id)
            .is_some_and(|live| Weak::as_ptr(live) == Arc::as_ptr(object))
        {
            queue.objects.remove(&object.id);
        }
        queue.records.retain(|record| {
            record.object() != object.id || record.generation() != object.generation
        });
    }
    end_object(object, token);
}

/// Closes `queue`'s handle: its objects end and anybody reading records wakes.
fn close_queue(queue: &Arc<Queue>, token: &mut CleanLockToken) {
    let objects: Vec<Arc<Object>> = {
        let mut state = queue.state.lock();
        state.closed = true;
        state.records.clear();
        core::mem::take(&mut state.objects)
            .into_values()
            .filter_map(|object| object.upgrade())
            .collect()
    };
    for object in &objects {
        end_object(object, token);
    }
    queue.records_ready.notify(token);
}

/// [`Request::Bind`]: binds `queue` to the scheme whose socket is `description`.
fn bind(
    queue: &Queue,
    description: &Arc<LockedFileDescription>,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let (scheme_ref, number) = {
        let description = description.read(token.token());
        (description.scheme_ref.clone(), description.number)
    };
    // A scheme socket is a description of the scheme list whose number is the scheme's.
    let KernelSchemeRef::SchemeMgr = scheme_ref else {
        return Err(Error::new(EINVAL));
    };
    let inner = SchemeList
        .get_user_inner(number, token)
        .ok_or(Error::new(EINVAL))?;
    let mut state = queue.state.lock();
    if state.bound.is_some() {
        return Err(Error::new(EBUSY));
    }
    state.bound = Some(Bound {
        id: inner.scheme_id,
        inner: Arc::downgrade(&inner),
    });
    Ok(0)
}

/// [`Request::Attach`]: serves `handle` of the queue's scheme from `object`.
fn attach(object: &Arc<Object>, handle: u64) -> Result<usize> {
    let handle = usize::try_from(handle).map_err(|_| Error::new(EINVAL))?;
    let (scheme, config) = {
        let queue = object.queue.state.lock();
        let bound = queue.bound.as_ref().ok_or(Error::new(EINVAL))?;
        (bound.id.get(), queue.readahead)
    };
    if object.state.lock().gone {
        return Err(Error::new(EBADF));
    }
    let mut attached = ATTACHED.lock();
    let key = (scheme, handle);
    if attached
        .get(&key)
        .is_some_and(|attachment| !attachment.object.state.lock().gone)
    {
        return Err(Error::new(EEXIST));
    }
    attached.insert(
        key,
        Attachment {
            object: Arc::clone(object),
            readahead: Arc::new(Mutex::new(Readahead::new(config))),
        },
    );
    Ok(0)
}

/// [`Request::Detach`]: stops serving `handle` from `object`.
fn detach(object: &Arc<Object>, handle: u64) -> Result<usize> {
    let handle = usize::try_from(handle).map_err(|_| Error::new(EINVAL))?;
    let scheme = {
        let queue = object.queue.state.lock();
        queue.bound.as_ref().ok_or(Error::new(EINVAL))?.id.get()
    };
    let mut attached = ATTACHED.lock();
    let key = (scheme, handle);
    match attached.get(&key) {
        Some(attachment) if Arc::ptr_eq(&attachment.object, object) => {
            attached.remove(&key);
            Ok(0)
        }
        _ => Err(Error::new(ENOENT)),
    }
}

/// [`Request::Filled`]: applies the answer to a fill of `pages`, copying the payload into new
/// frames when it is done. A copy that fails (no memory, a bad payload) leaves the pages being
/// filled, for the file system to answer again.
fn filled(
    object: &Object,
    pages: RecordPages,
    outcome: Outcome,
    payload: UserSliceRw,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let frames = match outcome {
        Outcome::Failed(_) => {
            if !payload.is_empty() {
                return Err(Error::new(EINVAL));
            }
            Vec::new()
        }
        Outcome::Done => {
            let count = pages.pages().count() as usize;
            if Some(payload.len()) != count.checked_mul(PAGE_SIZE) {
                return Err(Error::new(EINVAL));
            }
            // Check before copying, so a stray answer costs no frames.
            object
                .state
                .lock()
                .index
                .check_filling(pages)
                .map_err(|err| fill_errno(&err))?;
            copy_in(payload, count)?
        }
    };
    let result = object
        .state
        .lock()
        .index
        .complete_fill(pages, outcome, frames);
    object.changed.notify(token);
    result.map_err(|err| fill_errno(&err))?;
    Ok(payload.len())
}

/// Copies `count` whole pages from `payload` into new frames.
fn copy_in(payload: UserSliceRw, count: usize) -> Result<Vec<Arc<RaiiFrame>>> {
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(count)
        .map_err(|_| Error::new(ENOMEM))?;
    for chunk in payload.in_exact_chunks(PAGE_SIZE) {
        let mut frame = RaiiFrame::allocate().map_err(|_| Error::new(ENOMEM))?;
        chunk.copy_to_slice(frame.bytes_mut())?;
        frames.push(Arc::new(frame));
    }
    Ok(frames)
}

/// Copies whole records from `queue` to `buf`, waiting for one unless `nonblock`.
fn read_records(
    queue: &Queue,
    buf: UserSliceWo,
    nonblock: bool,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let max = buf.len() / Record::BYTES;
    if max == 0 {
        return Err(Error::new(EINVAL));
    }
    loop {
        let mut state = queue.state.lock();
        if !state.records.is_empty() {
            let n = max.min(state.records.len());
            let records: Vec<Record> = state.records.drain(..n).collect();
            drop(state);
            return copy_records(queue, &records, buf, token);
        }
        if state.closed {
            return Ok(0);
        }
        if nonblock {
            return Err(Error::new(EAGAIN));
        }
        let mut wait_token = token.downgrade::<L1>();
        if !queue
            .records_ready
            .wait(state, "pcache::records", &mut wait_token)
        {
            return Err(Error::new(EINTR));
        }
    }
}

/// Copies `records` to `buf`; if that fails, they go back to the front of the queue, so no
/// fill is lost, and the queue signals that it has records again (another reader may have
/// found it empty meanwhile and be waiting, or waiting for an event). Only records of
/// objects still open, in the same generation, go back: an object or queue that closed while
/// the records were out failed their readers already, and its records must not come back.
fn copy_records(
    queue: &Queue,
    records: &[Record],
    buf: UserSliceWo,
    token: &mut CleanLockToken,
) -> Result<usize> {
    // `read_records` takes no more records than whole chunks of `buf` hold.
    let mut copied = Ok(());
    for (record, dst) in records.iter().zip(buf.in_exact_chunks(Record::BYTES)) {
        copied = dst.copy_from_slice(&record.to_bytes());
        if copied.is_err() {
            break;
        }
    }
    match copied {
        Ok(()) => records
            .len()
            .checked_mul(Record::BYTES)
            .ok_or(Error::new(EINVAL)),
        Err(err) => {
            let restored = {
                let mut state = queue.state.lock();
                let mut restored = false;
                if !state.closed {
                    for &record in records.iter().rev() {
                        if live(&state, record) {
                            state.records.push_front(record);
                            restored = true;
                        }
                    }
                }
                restored
            };
            if restored {
                queue.wake(token);
            }
            Err(err)
        }
    }
}

/// Whether `record`'s object is open on the queue, in the record's generation.
fn live(state: &QueueState, record: Record) -> bool {
    state
        .objects
        .get(&record.object())
        .and_then(Weak::upgrade)
        .is_some_and(|object| object.generation == record.generation())
}

impl KernelScheme for PcacheScheme {
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
        token: &mut CleanLockToken,
    ) -> Result<OpenResult> {
        let path = path.as_bytes();
        let path = path.strip_prefix(b"/").unwrap_or(path);
        let from = if file == ROOT {
            Handle::Root
        } else {
            handle(file)?
        };
        let opened = match from {
            Handle::Root if path.is_empty() => Handle::Root,
            Handle::Root if path == QUEUE_PATH.as_bytes() => open_queue(&caller, token)?,
            Handle::Queue(queue) if path.starts_with(OBJECT_PREFIX.as_bytes()) => {
                check_owner(&queue, token)?;
                open_object(&queue, path)?
            }
            _ => return Err(Error::new(ENOENT)),
        };
        let id = match &opened {
            Handle::Queue(queue) => queue.id,
            _ => NEXT_ID.fetch_add(1, Ordering::Relaxed),
        };
        HANDLES.lock().insert(id, opened);
        Ok(OpenResult::SchemeLocal(id, InternalFlags::empty()))
    }

    fn close(&self, id: usize, token: &mut CleanLockToken) -> Result<()> {
        let removed = HANDLES.lock().remove(&id);
        match removed {
            Some(Handle::Queue(queue)) => close_queue(&queue, token),
            Some(Handle::Object(object)) => close_object(&object, token),
            Some(Handle::Root) | None => {}
        }
        Ok(())
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
        let target = handle(fd)?;
        match &target {
            Handle::Queue(queue) => check_owner(queue, token)?,
            Handle::Object(object) => check_owner(&object.queue, token)?,
            Handle::Root => {}
        }
        match (request, target) {
            (Request::SetSize { size }, Handle::Object(object)) => {
                object.state.lock().index.set_size(size);
                Ok(0)
            }
            (Request::Attach { handle }, Handle::Object(object)) => attach(&object, handle),
            (Request::Detach { handle }, Handle::Object(object)) => detach(&object, handle),
            (
                Request::Filled {
                    pages,
                    outcome,
                    generation,
                },
                Handle::Object(object),
            ) => {
                // An answer to a record of an earlier object with this ID.
                if generation != object.generation {
                    return Err(Error::new(ESTALE));
                }
                filled(&object, pages, outcome, payload, token)
            }
            // Writeback comes with writes, a later step.
            (Request::WrittenBack { .. }, Handle::Object(_)) => Err(Error::new(EOPNOTSUPP)),
            (Request::SetReadaheadMax { pages }, Handle::Queue(queue)) => {
                let max = pages.get();
                let initial = ReadaheadConfig::DEFAULT.initial().min(max);
                let config = ReadaheadConfig::new(initial, max).map_err(|_| Error::new(EINVAL))?;
                queue.state.lock().readahead = config;
                Ok(0)
            }
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
        let result = match (handle(id), Request::decode(metadata), descs.as_slice()) {
            (Ok(Handle::Queue(queue)), Ok(Request::Bind), [socket]) => {
                check_owner(&queue, token).and_then(|()| bind(&queue, socket, token))
            }
            (Err(err), _, _) => Err(err),
            _ => Err(Error::new(EINVAL)),
        };
        // The queue keeps the scheme's identity, not its socket.
        for description in descs {
            release_description(description, token);
        }
        result
    }

    fn kread(
        &self,
        id: usize,
        buf: UserSliceWo,
        flags: u32,
        _stored_flags: u32,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let Handle::Queue(queue) = handle(id)? else {
            return Err(Error::new(EBADF));
        };
        check_owner(&queue, token)?;
        read_records(&queue, buf, flags & O_NONBLOCK as u32 != 0, token)
    }

    fn fevent(
        &self,
        id: usize,
        flags: EventFlags,
        _token: &mut CleanLockToken,
    ) -> Result<EventFlags> {
        let Handle::Queue(queue) = handle(id)? else {
            return Ok(EventFlags::empty());
        };
        let state = queue.state.lock();
        if !state.records.is_empty() || state.closed {
            Ok(flags & EVENT_READ)
        } else {
            Ok(EventFlags::empty())
        }
    }
}
