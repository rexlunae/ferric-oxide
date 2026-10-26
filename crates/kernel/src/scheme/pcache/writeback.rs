//! Ferric Oxide: cached writes into the page cache and their writeback (the
//! `page_cache_writeback` feature; io/pagecache-design.md §4, step 3).
//!
//! A file system turns cached writes on for its queue with
//! [`Request::SetWriteback`](ferric_pcache::abi::Request::SetWriteback), giving the queue's
//! dirty limit, and attaches descriptions for writing with
//! [`Request::AttachAccess`](ferric_pcache::abi::Request::AttachAccess). Then:
//!
//! - A positioned `write` or `pwrite` of such a description (`UserScheme::kwriteoff` calls
//!   [`write_attached`] first) that lies inside the file's size, without `O_APPEND` or
//!   `O_FSYNC`, is served here: the bytes are copied from the writer into new frames with no
//!   lock held, the pages it covers only in part are filled first if they are not cached
//!   (the writer waits, as a reader does), and the new frames replace the pages' old ones,
//!   which become dirty (`ferric_pcache::object::PageIndex::write`). Anything else, a write
//!   to an object whose writes the file system holds, and every write while the queue holds
//!   twice its dirty limit, goes to the file system's scheme as before.
//! - When an object's first page becomes dirty, a `Writeback` record tells the file system;
//!   when a write takes the queue past its dirty limit, an urgent one does (once, until the
//!   count falls below half the limit). The file system pulls the dirty pages with
//!   [`take_dirty`], writes them to the file and answers with [`written_back`].
//! - A file system holds an object's writes ([`hold`]) while it changes the file itself, so a
//!   cached write never lands between its writeback and its own change.
//!
//! Dirty pages are never dropped but by a truncation (`SetSize`) or the object going: there
//! is no reclaim yet, and the index refuses to evict them when there is.

use alloc::{sync::Arc, vec::Vec};
use core::{
    num::NonZeroU32,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use ferric_pcache::{
    abi::{encode_taken, Outcome, Record, RecordPages},
    object::{Attempt, Read, Write, WritebackError},
};
use syscall::{
    error::{Error, Result, EBADF, EBUSY, EINTR, EINVAL, EIO, EOVERFLOW},
    flag::{O_APPEND, O_FSYNC},
};

use super::{queue_fills, with_index, Object, UserInner, ATTACHED, EIO_OUTCOME};
use crate::{
    memory::{RaiiFrame, PAGE_SIZE},
    sync::{CleanLockToken, L1},
    syscall::usercopy::{UserSliceRo, UserSliceRw},
};

/// A queue's cached-write settings.
pub(super) struct QueueWriteback {
    /// The dirty limit in pages; 0 while cached writes are off.
    max_dirty: AtomicU32,
    /// An urgent record went out since the count was last below half the limit.
    urgent_sent: AtomicBool,
}

impl QueueWriteback {
    /// Cached writes off.
    pub(super) const fn new() -> Self {
        Self {
            max_dirty: AtomicU32::new(0),
            urgent_sent: AtomicBool::new(false),
        }
    }

    /// Whether cached writes are on.
    pub(super) fn on(&self) -> bool {
        self.max_dirty.load(Ordering::Relaxed) != 0
    }

    /// [`Request::SetWriteback`](ferric_pcache::abi::Request::SetWriteback).
    pub(super) fn set_max_dirty(&self, pages: NonZeroU32) {
        self.max_dirty.store(pages.get(), Ordering::Relaxed);
    }

    /// The queue's dirty count fell to `dirty`: once below half the limit, the next time it
    /// goes past the limit sends an urgent record again.
    pub(super) fn cleaned(&self, dirty: usize) {
        let max = self.max_dirty.load(Ordering::Relaxed) as usize;
        if dirty < max / 2 {
            self.urgent_sent.store(false, Ordering::Relaxed);
        }
    }

    /// The limit in pages, `None` while cached writes are off.
    fn max(&self) -> Option<usize> {
        match self.max_dirty.load(Ordering::Relaxed) {
            0 => None,
            max => Some(max as usize),
        }
    }
}

/// Serves a write of `handle` on `inner`'s scheme from the page cache, if the description is
/// attached to an object for writing and the write can be cached (see the module docs):
/// `None` sends it to the scheme as before.
///
/// # Errors
///
/// `EIO` when a fill the write waited for failed, `EINTR` when a signal interrupted the
/// wait, `ENOMEM` when the fill records could not be queued, and `EFAULT` for a bad buffer.
pub fn write_attached(
    inner: &Arc<UserInner>,
    handle: usize,
    buf: UserSliceRo,
    offset: u64,
    flags: u32,
    token: &mut CleanLockToken,
) -> Option<Result<usize>> {
    // An unpositioned write leaves the position to the scheme; an append or a synchronous
    // write is the file system's to do.
    if offset == u64::MAX || flags & (O_APPEND | O_FSYNC) as u32 != 0 {
        return None;
    }
    let attachment = ATTACHED
        .lock()
        .get(&(inner.scheme_id.get(), handle))
        .cloned()?;
    let object = &attachment.object;
    if !attachment.access.writes() || !object.queue.serves(inner) {
        return None;
    }
    let max = object.queue.writeback.max()?;
    // At twice the limit, writers wait for the file system: their writes go there, and it
    // writes the file's dirty pages back before it serves them.
    if object.queue.dirty.load(Ordering::Relaxed) >= max.saturating_mul(2) {
        return None;
    }
    write(object, buf, offset, token)
}

/// [`write_attached`] for a write the object may take.
fn write(
    object: &Arc<Object>,
    buf: UserSliceRo,
    offset: u64,
    token: &mut CleanLockToken,
) -> Option<Result<usize>> {
    let plan = {
        let state = object.state.lock();
        if state.gone || state.held {
            return None;
        }
        state.index.pages_for_write(offset, buf.len())?
    };
    // The writer's bytes go into new frames before any lock is taken. No memory for them:
    // the file system serves the write instead.
    let mut frames = match stage(&plan, buf) {
        Ok(Some(frames)) => frames,
        Ok(None) => return None,
        Err(err) => return Some(Err(err)),
    };
    let mut attempt = Attempt::new();
    loop {
        let mut state = object.state.lock();
        // The object went, the file system holds its writes, or the file shrank under the
        // write while it waited: the file system serves it, nothing having been written.
        if state.gone || state.held {
            return None;
        }
        // The same bytes, so the same pages and staged frames: only the edges may differ,
        // with the size.
        let plan = state.index.pages_for_write(offset, buf.len())?;
        let step = state.index.start_write(&plan, &mut attempt);
        match step.read {
            Read::Ready => {
                merge_edges(&state.index, &plan, &mut frames);
                let frames = frames.into_iter().map(Arc::new).collect();
                let before = state.index.dirty_pages();
                let written = state.index.write(&plan, frames);
                let after = state.index.dirty_pages();
                drop(state);
                object.queue.account(before, after);
                // Unreachable once the pages were ready: the file system serves the write.
                let written = written.ok()?;
                notify_dirty(object, &plan, written.first, token);
                return Some(Ok(plan.len()));
            }
            Read::Wait if step.fills.is_empty() => {
                let mut wait_token = token.downgrade::<L1>();
                if !object.changed.wait(state, "pcache::write", &mut wait_token) {
                    return Some(Err(Error::new(EINTR)));
                }
            }
            Read::Wait | Read::Failed | Read::OutOfFillIds => {
                drop(state);
                let queued = queue_fills(object, &step.fills, token);
                match step.read {
                    Read::Failed => return Some(Err(Error::new(EIO))),
                    // The file system can still write the bytes itself.
                    Read::OutOfFillIds => return None,
                    _ => {
                        if let Err(err) = queued {
                            return Some(Err(err));
                        }
                    }
                }
            }
        }
    }
}

/// Copies the bytes of `buf` into a new frame per page of `plan`, each at its place in the
/// page (zero around it). `Ok(None)` when there is no memory for them.
///
/// # Errors
///
/// `EFAULT` for a bad buffer.
fn stage(plan: &Write, buf: UserSliceRo) -> Result<Option<Vec<RaiiFrame>>> {
    let pages = plan.pages();
    let mut frames = Vec::new();
    if frames.try_reserve_exact(pages.count() as usize).is_err() {
        return Ok(None);
    }
    for page in pages.first()..pages.end() {
        let Ok(mut frame) = RaiiFrame::allocate() else {
            return Ok(None);
        };
        let covered = plan.covered(page).ok_or(Error::new(EINVAL))?;
        let src = buf
            .advance(covered.in_buf)
            .and_then(|src| src.limit(covered.in_page.len()))
            .ok_or(Error::new(EINVAL))?;
        let dst = frame
            .bytes_mut()
            .get_mut(covered.in_page)
            .ok_or(Error::new(EINVAL))?;
        src.copy_to_slice(dst)?;
        frames.push(frame);
    }
    Ok(Some(frames))
}

/// Copies the old bytes of the pages `plan` covers only in part, around the write's, into
/// their new frames. The index says the edges are ready, so each has a frame.
fn merge_edges(
    index: &ferric_pcache::object::PageIndex<Arc<RaiiFrame>>,
    plan: &Write,
    frames: &mut [RaiiFrame],
) {
    let first = plan.pages().first();
    for page in plan.edges() {
        let (Some(old), Some(covered)) = (index.frame(page), plan.covered(page)) else {
            continue;
        };
        let Some(new) = page
            .checked_sub(first)
            .and_then(|at| usize::try_from(at).ok())
            .and_then(|at| frames.get_mut(at))
        else {
            continue;
        };
        let (old, new) = (old.bytes(), new.bytes_mut());
        let (start, end) = (covered.in_page.start, covered.in_page.end);
        if let (Some(dst), Some(src)) = (new.get_mut(..start), old.get(..start)) {
            dst.copy_from_slice(src);
        }
        if let (Some(dst), Some(src)) = (new.get_mut(end..), old.get(end..)) {
            dst.copy_from_slice(src);
        }
    }
}

/// Tells the file system about a write that dirtied `plan`'s pages: a record when they are
/// the object's first dirty pages, and an urgent one when the queue is past its limit.
fn notify_dirty(object: &Object, plan: &Write, first: bool, token: &mut CleanLockToken) {
    let queue = &object.queue;
    let past = queue
        .writeback
        .max()
        .is_some_and(|max| queue.dirty.load(Ordering::Relaxed) > max);
    let urgent = past && !queue.writeback.urgent_sent.swap(true, Ordering::Relaxed);
    if !first && !urgent {
        return;
    }
    // A write spans at most a record's worth of pages.
    let Some(pages) = RecordPages::new(plan.pages()) else {
        return;
    };
    let record = Record::Writeback {
        object: object.id,
        generation: object.generation,
        pages,
        urgent,
    };
    // A queue that closed has failed the object; one out of memory gets the next record,
    // or the file system's own writeback before it reads or changes the file.
    if queue.push_records(core::iter::once(record), token).is_err() && urgent {
        queue.writeback.urgent_sent.store(false, Ordering::Relaxed);
    }
}

/// [`Request::HoldWrites`](ferric_pcache::abi::Request::HoldWrites).
pub(super) fn hold(object: &Object, hold: bool) -> Result<usize> {
    let mut state = object.state.lock();
    if state.gone {
        return Err(Error::new(EBADF));
    }
    state.held = hold;
    Ok(0)
}

/// The errno of a writeback the index refused.
fn writeback_errno(err: WritebackError) -> Error {
    Error::new(match err {
        WritebackError::Busy => EBUSY,
        WritebackError::NoRoom | WritebackError::NotInFlight => EINVAL,
        _ => EIO,
    })
}

/// [`Request::TakeDirty`](ferric_pcache::abi::Request::TakeDirty): puts the next run of dirty
/// pages at or after `from` under writeback and copies them to `payload`. If the copy fails,
/// the run is failed at once, so its pages stay dirty and nothing is left in flight.
pub(super) fn take_dirty(object: &Object, from: u64, payload: UserSliceRw) -> Result<usize> {
    let max = u32::try_from(payload.len() / PAGE_SIZE).unwrap_or(u32::MAX);
    let run = {
        let mut state = object.state.lock();
        if state.gone {
            return Err(Error::new(EBADF));
        }
        state.index.start_writeback(from, max)
    }
    .map_err(writeback_errno)?;
    let Some(run) = run else {
        return Ok(0);
    };
    let copied = run
        .frames
        .iter()
        .zip(payload.in_exact_chunks(PAGE_SIZE))
        .try_for_each(|(frame, dst)| dst.copy_from_slice(frame.bytes()))
        .and_then(|()| {
            encode_taken(Some(run.pages))
                .and_then(|word| usize::try_from(word).ok())
                .ok_or(Error::new(EOVERFLOW))
        });
    if copied.is_err() {
        // The run was just taken, and is the object's writeback in flight.
        let _ = object
            .state
            .lock()
            .index
            .complete_writeback(run.pages, Outcome::Failed(EIO_OUTCOME));
    }
    copied
}

/// [`Request::WrittenBack`](ferric_pcache::abi::Request::WrittenBack): ends the writeback of
/// `pages`, and returns the object's dirty pages left. Wakes readers and writers waiting for
/// pages a truncation cut while they were under writeback, which are gone now.
pub(super) fn written_back(
    object: &Object,
    pages: RecordPages,
    outcome: Outcome,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let done = with_index(object, |state| {
        state.index.complete_writeback(pages, outcome)
    })
    .map_err(writeback_errno)?;
    if done.dropped != 0 {
        object.changed.notify(token);
    }
    if done.lost != 0 {
        crate::warn!(
            "pcache: object {}: {} page(s) truncated under a failed writeback lost their data",
            object.id.get(),
            done.lost
        );
    }
    Ok(done.dirty_left)
}
