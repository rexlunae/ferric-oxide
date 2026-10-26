//! One cache object's pages: which are there, which are being filled, and what a read of a
//! byte range needs before it can be served.
//!
//! The kernel keeps a [`PageIndex`] per object, holding each page's [`PageState`] and, once it
//! is filled, its frame (`F`, the kernel's reference to a physical page; tests use plain
//! values). A read goes through it in three steps:
//!
//! 1. [`PageIndex::pages_for_read`] turns the byte range into the object's pages, cut at the
//!    file's size.
//! 2. [`PageIndex::start_read`] says whether those pages can be copied now ([`Read::Ready`]),
//!    and otherwise marks the absent ones [`PageState::Filling`] and returns the fill records
//!    to queue ([`Read::Wait`]). The reader waits and calls it again with the same
//!    [`Attempt`] until the pages are ready or a fill it waited for failed
//!    ([`Read::Failed`]).
//! 3. [`PageIndex::frames`] hands out the frames of ready pages, for the copy.
//!
//! [`PageIndex::complete_fill`] applies the file system's answer, and
//! [`PageIndex::fail_all`] fails every outstanding fill when the file system goes away.
//!
//! Every fill has a [`FillId`], from 1 up and never reused, and each page being filled
//! remembers the fill it waits for. An answer acts only on the pages its fill still owns
//! ([`PageIndex::owned`]): once a fill has failed, been invalidated or been answered, a newer
//! fill of the same pages is out of reach of a late answer to the old one, however
//! malformed, and a fill that owns only part of its range any more completes that part. An
//! answer whose fill owns none of its pages is stale and changes nothing.
//!
//! Each fill record has an outcome that its pages and the readers waiting for them share: an
//! [`Attempt`] holds the outcomes of the fills it waits for, so it learns that one failed (the
//! kernel's `EIO`) whatever happens to the pages afterwards: a newer fill of them, an
//! invalidation, a truncation and regrowth. A failed page simply goes back to
//! [`PageState::Absent`], so the index keeps nothing for it, and new reads fill it again.
//!
//! [`PageIndex::set_size`] shrinking the file drops the page the new end falls in too, so its
//! bytes past the end are not served if the file grows again, and fills in flight past the
//! end are stale (below).
//!
//! [`PageIndex::invalidate`] drops pages whose bytes changed outside the cache (the file
//! system served a write itself). A page being filled then is marked stale: the answer to that
//! fill may hold the old bytes, so it is not taken. The page goes back to
//! [`PageState::Absent`] and the fill ends without failing, so the readers that waited for it
//! fill it again on their retry.
//!
//! Writes the kernel serves and their writeback are in [`PageIndex::start_write`],
//! [`PageIndex::write`], [`PageIndex::start_writeback`] and
//! [`PageIndex::complete_writeback`]; reclaim takes a page with [`PageIndex::evict`], which
//! refuses a page whose data the file system does not have. The index counts its dirty pages
//! ([`PageIndex::dirty_pages`]): `Dirty` ones and those under writeback, whose writeback can
//! still fail. A page under writeback that a truncation cuts is dropped once its writeback
//! ends; until then readers and writers of it wait, so the bytes past the cut never come
//! back.

mod write;

pub use write::{EvictError, TakenRun, Write, WriteError, WritebackError, Written, WrittenBack};

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::abi::{FillId, Outcome, PAGE_SIZE, PageRange, RecordPages};
use crate::page::{Event, PageState, TransitionError};

/// How one fill record is going, shared by the pages it fills and the readers waiting for
/// it. The record is decided page by page, in whatever order and however many answers its
/// pages get: it has failed as soon as one of its pages failed (a stale page does not count),
/// and it has ended once none of its pages is waiting for it any more.
#[derive(Debug)]
struct FillOutcome {
    /// The fill, which the answer names.
    id: FillId,
    /// A page of the record failed.
    failed: AtomicBool,
    /// The record's pages still being filled by it.
    pages: AtomicU32,
}

impl FillOutcome {
    const PENDING: u8 = 0;
    const ENDED: u8 = 1;
    const FAILED: u8 = 2;

    /// The outcome of fill `id` of `pages` pages, all being filled.
    const fn new(id: FillId, pages: u32) -> Self {
        Self {
            id,
            failed: AtomicBool::new(false),
            pages: AtomicU32::new(pages),
        }
    }

    fn get(&self) -> u8 {
        // The index is only ever used under the object's lock, which orders these.
        if self.failed.load(Ordering::Relaxed) {
            Self::FAILED
        } else if self.pages.load(Ordering::Relaxed) > 0 {
            Self::PENDING
        } else {
            Self::ENDED
        }
    }

    /// One of the record's pages is done with it, `failed` or not. A failure is never undone.
    fn end_page(&self, failed: bool) {
        if failed {
            self.failed.store(true, Ordering::Relaxed);
        }
        let _ = self
            .pages
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
}

/// One reader's look at its pages, across the [`PageIndex::start_read`] calls it makes while
/// it waits: the fills it is waiting for, whose outcomes it keeps.
#[derive(Clone, Debug, Default)]
pub struct Attempt {
    /// `None` before the first call.
    waiting: Option<Vec<Arc<FillOutcome>>>,
}

impl Attempt {
    /// A reader that has not looked at its pages yet.
    pub const fn new() -> Self {
        Self { waiting: None }
    }

    /// Whether [`PageIndex::start_read`] has not been called with this attempt yet.
    pub const fn is_first(&self) -> bool {
        self.waiting.is_none()
    }

    /// Whether a fill this reader waited for failed. Fills that ended otherwise are let go.
    fn take_failure(&mut self) -> bool {
        let Some(waiting) = &mut self.waiting else {
            return false;
        };
        if waiting.iter().any(|fill| fill.get() == FillOutcome::FAILED) {
            return true;
        }
        waiting.retain(|fill| fill.get() == FillOutcome::PENDING);
        false
    }

    /// Waits for `fill` too.
    fn wait_for(&mut self, fill: &Arc<FillOutcome>) {
        let waiting = self.waiting.get_or_insert_with(Vec::new);
        if !waiting.iter().any(|held| Arc::ptr_eq(held, fill)) {
            waiting.push(Arc::clone(fill));
        }
    }
}

/// What a read needs before its pages can be copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// Every page holds the file's data.
    Ready,
    /// Some pages are being filled: wait for their answers (after queueing
    /// [`ReadStep::fills`]) and look again.
    Wait,
    /// A fill the reader waited for failed.
    Failed,
    /// Some pages are absent, and the object has used every [`FillId`] there is, so no fill
    /// of them can start (the kernel returns `EOVERFLOW`). Nothing changed. At a billion
    /// fills a second this takes centuries; the file system must close and reopen the
    /// object to fill it again.
    OutOfFillIds,
}

/// One look at a read's pages ([`PageIndex::start_read`]): what the reader does next, and
/// every fill the look started, for the read and for readahead together.
///
/// Each fill in `fills` has its pages marked filling, and nothing else will start them
/// again: the caller must put every one on the queue, or fail it, whatever `read` says.
/// Keeping them in one list (rather than one per purpose) is what makes that a single step
/// on every path.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "every fill started must be queued or failed"]
pub struct ReadStep {
    /// What the reader does next.
    pub read: Read,
    /// The fill records to queue: the read's own, then readahead's.
    pub fills: Vec<Fill>,
}

/// A fill the index started: its pages, and the ID its record and answer carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    /// The fill's ID.
    pub id: FillId,
    /// The pages it fills.
    pub pages: RecordPages,
}

/// One page of the index.
#[derive(Clone, Debug)]
struct Entry<F> {
    state: PageState,
    /// The frame holding the page, while the page is readable.
    frame: Option<F>,
    /// The outcome of the fill in flight, while the page is being filled.
    fill: Option<Arc<FillOutcome>>,
    /// The page was invalidated while being filled: the fill's answer is not taken.
    stale: bool,
}

impl<F> Entry<F> {
    const fn absent() -> Self {
        Self {
            state: PageState::Absent,
            frame: None,
            fill: None,
            stale: false,
        }
    }

    /// Whether a reader or writer of the page waits for it: it is being filled, or it is
    /// under writeback and was truncated away meanwhile (it goes when the writeback ends).
    const fn waits(&self) -> bool {
        match self.state {
            PageState::Filling => true,
            PageState::Writeback { .. } => self.stale,
            _ => false,
        }
    }
}

/// The pages of one cache object.
#[derive(Clone, Debug)]
pub struct PageIndex<F> {
    size: u64,
    /// Pages that are not absent: absent pages are not stored.
    pages: BTreeMap<u64, Entry<F>>,
    /// The ID the next fill gets; `None` once every ID has been used. IDs are never reused.
    next_fill: Option<FillId>,
    /// Pages holding data the file system does not have: `Dirty` or under writeback.
    dirty: usize,
    /// The run of pages under writeback, if any: one at a time.
    writeback: Option<RecordPages>,
}

impl<F> Default for PageIndex<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> PageIndex<F> {
    /// An object of size 0 with no pages.
    pub const fn new() -> Self {
        Self {
            size: 0,
            pages: BTreeMap::new(),
            next_fill: Some(FillId::new(1)),
            dirty: 0,
            writeback: None,
        }
    }

    /// The file's size, in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The number of pages the file has.
    pub const fn file_pages(&self) -> u64 {
        PageRange::pages_for(self.size)
    }

    /// The number of pages holding data (with a frame).
    pub fn resident(&self) -> usize {
        self.pages.values().filter(|e| e.frame.is_some()).count()
    }

    /// The state of `page`.
    pub fn state(&self, page: u64) -> PageState {
        self.pages
            .get(&page)
            .map_or(PageState::Absent, |entry| entry.state)
    }

    /// Sets the file's size. When the file shrinks, the pages from the one the new end falls
    /// in are invalidated ([`PageIndex::invalidate`]): the page holding the new end has bytes
    /// past it that must not come back if the file grows again, and fills outstanding past
    /// the end become stale. Dirty pages there are dropped too (the truncation discards
    /// them), and pages under writeback are dropped when it ends. Returns how many cached
    /// pages were dropped.
    pub fn set_size(&mut self, size: u64) -> usize {
        let shrunk = size < self.size;
        self.size = size;
        if shrunk {
            self.invalidate_from(size / PAGE_SIZE, u64::MAX, true)
                .dropped
        } else {
            0
        }
    }

    /// The number of pages holding data the file system does not have yet: dirty, or under
    /// a writeback that has not ended.
    pub const fn dirty_pages(&self) -> usize {
        self.dirty
    }

    /// The frame of `page`, when it holds the file's data.
    pub fn frame(&self, page: u64) -> Option<&F> {
        let entry = self.pages.get(&page)?;
        if entry.state.readable() && !entry.waits() {
            entry.frame.as_ref()
        } else {
            None
        }
    }

    /// The pages holding bytes `offset..offset + len` of the file, cut at its size, and the
    /// number of bytes that leaves; `None` when nothing is left (at or past the end, or `len`
    /// 0).
    pub fn pages_for_read(&self, offset: u64, len: usize) -> Option<(PageRange, usize)> {
        let end = offset.saturating_add(len as u64).min(self.size);
        if end <= offset {
            return None;
        }
        let pages = PageRange::from_bounds(offset / PAGE_SIZE, end.div_ceil(PAGE_SIZE))?;
        // `end - offset` is at most `len`, a `usize`.
        Some((pages, usize::try_from(end - offset).unwrap_or(len)))
    }

    /// Looks at `pages` for a read: see [`ReadStep`]. Absent pages become
    /// [`PageState::Filling`], in records of at most [`crate::abi::MAX_RECORD_PAGES`] pages,
    /// and `attempt` keeps the outcome of every fill it now waits for. On the attempt's
    /// first look, and only when the read itself can go on (it is ready or waits), the
    /// absent pages of `ahead` are filled too (readahead, which no reader waits for).
    pub fn start_read(
        &mut self,
        pages: PageRange,
        attempt: &mut Attempt,
        ahead: Option<PageRange>,
    ) -> ReadStep {
        let first = attempt.is_first();
        let read = self.look(pages, attempt);
        let mut fills = match read {
            Ok(fills) => fills,
            Err(read) => {
                return ReadStep {
                    read,
                    fills: Vec::new(),
                };
            }
        };
        let read = if fills.is_empty() && !self.waits(pages) {
            Read::Ready
        } else {
            Read::Wait
        };
        if first && let Some(ahead) = ahead {
            fills.extend(self.prefetch(ahead));
        }
        ReadStep { read, fills }
    }

    /// Whether any of `pages` must be waited for before it is read or written
    /// ([`Entry::waits`]).
    fn waits(&self, pages: PageRange) -> bool {
        self.pages
            .range(pages.first()..pages.end())
            .any(|(_, entry)| entry.waits())
    }

    /// The read's own part of [`PageIndex::start_read`]: the fills it started, or why it
    /// cannot go on ([`Read::Failed`] or [`Read::OutOfFillIds`], having started nothing).
    fn look(&mut self, pages: PageRange, attempt: &mut Attempt) -> Result<Vec<Fill>, Read> {
        if attempt.take_failure() {
            return Err(Read::Failed);
        }
        let Some(fills) = self.start_fills(pages) else {
            return Err(Read::OutOfFillIds);
        };
        for entry in self.pages.range(pages.first()..pages.end()).map(|(_, e)| e) {
            if entry.state.busy()
                && let Some(fill) = &entry.fill
            {
                attempt.wait_for(fill);
            }
        }
        // Even a ready read is no longer a first look.
        attempt.waiting.get_or_insert_with(Vec::new);
        Ok(fills)
    }

    /// Starts fills of the absent pages of `pages` that no reader waits for yet (readahead),
    /// and returns their records: none once the object is out of fill IDs.
    fn prefetch(&mut self, pages: PageRange) -> Vec<Fill> {
        self.start_fills(pages).unwrap_or_default()
    }

    /// Marks the absent pages of `pages` filling, and returns them as records, each with a
    /// new [`FillId`] and an outcome its pages share. `None`, changing nothing, when there
    /// are not enough IDs left for them all.
    fn start_fills(&mut self, pages: PageRange) -> Option<Vec<Fill>> {
        let mut runs = Vec::new();
        let mut run: Option<(u64, u64)> = None;
        for page in pages.first()..pages.end() {
            // Pages being filled or holding data are left as they are.
            if self.state(page).apply(Event::FillStart).is_ok() {
                run = match run {
                    Some((first, end)) if end == page => Some((first, page + 1)),
                    other => {
                        push_run(&mut runs, other);
                        Some((page, page + 1))
                    }
                };
            }
        }
        push_run(&mut runs, run);
        if runs.len() as u64 > self.fill_ids_left() {
            return None;
        }
        Some(
            runs.into_iter()
                .filter_map(|pages| self.start_fill(pages))
                .collect(),
        )
    }

    /// Starts one fill of the absent `pages`: marks them filling with a new ID and an outcome
    /// they share. `None` when the IDs have run out (checked by the caller first).
    fn start_fill(&mut self, pages: RecordPages) -> Option<Fill> {
        let id = self.next_fill?;
        self.next_fill = id.next();
        let range = pages.pages();
        let outcome = Arc::new(FillOutcome::new(id, range.count()));
        for page in range.first()..range.end() {
            let entry = self.pages.entry(page).or_insert_with(Entry::absent);
            if let Ok(next) = entry.state.apply(Event::FillStart) {
                entry.state = next;
                entry.fill = Some(Arc::clone(&outcome));
            }
        }
        Some(Fill { id, pages })
    }

    /// How many fill IDs are left.
    fn fill_ids_left(&self) -> u64 {
        self.next_fill
            .map_or(0, |next| (u64::MAX - next.get()).saturating_add(1))
    }

    /// The frames of `pages`, in order, when every one is readable.
    pub fn frames(&self, pages: PageRange) -> Option<Vec<F>>
    where
        F: Clone,
    {
        (pages.first()..pages.end())
            .map(|page| {
                let entry = self.pages.get(&page)?;
                (entry.state.readable() && !entry.waits())
                    .then(|| entry.frame.clone())
                    .flatten()
            })
            .collect()
    }

    /// Whether `page` is being filled by fill `id`.
    fn owns(&self, id: FillId, page: u64) -> bool {
        self.pages.get(&page).is_some_and(|entry| {
            entry.state == PageState::Filling && entry.fill.as_ref().is_some_and(|f| f.id == id)
        })
    }

    /// How many of `pages` fill `id` still owns: an answer to it acts on those alone, and is
    /// stale when there are none.
    pub fn owned(&self, id: FillId, pages: RecordPages) -> u32 {
        let range = pages.pages();
        // At most `MAX_RECORD_PAGES` pages, which fits.
        (range.first()..range.end())
            .filter(|&page| self.owns(id, page))
            .count() as u32
    }

    /// Applies the file system's answer to fill `id` of `pages`, to the pages the fill still
    /// owns ([`PageIndex::owned`]); returns how many that was. A [`Outcome::Done`] answer
    /// needs `frames`, one for each page of `pages` in order, holding its data (the frames of
    /// pages the fill no longer owns are dropped); a failed one fails the fill for the
    /// readers waiting for it and drops any `frames` given. A stale answer (the fill owns
    /// none of its pages) changes nothing and returns 0, whatever it holds.
    ///
    /// # Errors
    ///
    /// [`FillError::Frames`] when a done answer that is not stale does not have one frame
    /// per page (nothing changes; fail the fill to end it).
    pub fn complete_fill(
        &mut self,
        id: FillId,
        pages: RecordPages,
        outcome: Outcome,
        frames: Vec<F>,
    ) -> Result<u32, FillError> {
        let owned = self.owned(id, pages);
        if owned == 0 {
            return Ok(0);
        }
        let range = pages.pages();
        match outcome {
            Outcome::Done => {
                if frames.len() != range.count() as usize {
                    return Err(FillError::Frames {
                        expected: range.count(),
                        got: frames.len(),
                    });
                }
                for (page, frame) in (range.first()..range.end()).zip(frames) {
                    if !self.owns(id, page) || self.take_stale(page)? {
                        continue;
                    }
                    self.transition(page, Event::FillDone)?;
                    if let Some(entry) = self.pages.get_mut(&page) {
                        entry.frame = Some(frame);
                        end_fill(entry, false);
                    }
                }
            }
            Outcome::Failed(_) => {
                for page in range.first()..range.end() {
                    if self.owns(id, page) && !self.take_stale(page)? {
                        self.fail(page)?;
                    }
                }
            }
        }
        Ok(owned)
    }

    /// Drops the cached copies of `pages`, whose bytes changed outside the cache, and marks
    /// those being filled stale (see the module docs). Dirty pages are dropped too, and
    /// counted ([`Invalidated::dirty`]): a file system that has the kernel serve writes
    /// writes them back and holds writes before it changes the file itself, so there are
    /// none. Pages under writeback are left as they are: they hold data newer than the file
    /// system's.
    pub fn invalidate(&mut self, pages: PageRange) -> Invalidated {
        self.invalidate_from(pages.first(), pages.end(), false)
    }

    /// [`PageIndex::invalidate`] of the pages `first..end`; a `truncation` also marks the
    /// pages under writeback stale, to drop when it ends.
    fn invalidate_from(&mut self, first: u64, end: u64, truncation: bool) -> Invalidated {
        let mut done = Invalidated::default();
        let touched: Vec<u64> = self
            .pages
            .range(first..end)
            .map(|(&page, _)| page)
            .collect();
        for page in touched {
            let Some(entry) = self.pages.get_mut(&page) else {
                continue;
            };
            match entry.state {
                PageState::Filling => {
                    entry.stale = true;
                    done.stale += 1;
                }
                PageState::Writeback { .. } => entry.stale |= truncation,
                PageState::Absent => {}
                state => {
                    if state.dirty() {
                        done.dirty += 1;
                    }
                    if self.drop_page(page) {
                        done.dropped += 1;
                    }
                }
            }
        }
        done
    }

    /// If `page` is stale, ends its fill without taking the answer: the page is absent again
    /// and the fill ends without failing. Returns whether it was stale.
    fn take_stale(&mut self, page: u64) -> Result<bool, FillError> {
        if !self.pages.get(&page).is_some_and(|entry| entry.stale) {
            return Ok(false);
        }
        self.transition(page, Event::FillFailed)?;
        if let Some(entry) = self.pages.get_mut(&page) {
            end_fill(entry, false);
        }
        self.drop_page(page);
        Ok(true)
    }

    /// Forgets `page`, which is absent from now on; returns whether it had a frame.
    fn drop_page(&mut self, page: u64) -> bool {
        let Some(entry) = self.pages.remove(&page) else {
            return false;
        };
        if entry.state.dirty() {
            self.dirty = self.dirty.saturating_sub(1);
        }
        entry.frame.is_some()
    }

    /// Fails every outstanding fill (the file system's queue closed), and returns how many
    /// pages were being filled.
    pub fn fail_all(&mut self) -> usize {
        let filling: Vec<u64> = self
            .pages
            .iter()
            .filter(|(_, entry)| entry.state == PageState::Filling)
            .map(|(&page, _)| page)
            .collect();
        for &page in &filling {
            // Every page here is `Filling`, which `FillFailed` always leaves.
            let _ = self.fail(page);
        }
        filling.len()
    }

    /// Applies `FillFailed` to `page`, fails its fill for the readers waiting for it, and
    /// forgets the page.
    fn fail(&mut self, page: u64) -> Result<(), FillError> {
        self.transition(page, Event::FillFailed)?;
        if let Some(entry) = self.pages.get_mut(&page) {
            end_fill(entry, true);
        }
        self.drop_page(page);
        Ok(())
    }

    /// Applies `event` to `page`.
    fn transition(&mut self, page: u64, event: Event) -> Result<(), FillError> {
        let entry = self.pages.entry(page).or_insert_with(Entry::absent);
        entry.state = entry.state.apply(event).map_err(FillError::Transition)?;
        Ok(())
    }
}

/// Lets `entry` go of the fill it was waiting for, failed or not (see [`FillOutcome`]).
fn end_fill<F>(entry: &mut Entry<F>, failed: bool) {
    if let Some(fill) = entry.fill.take() {
        fill.end_page(failed);
    }
}

/// Adds the run of pages `first..end`, if any, to `fills`, cut into records.
fn push_run(fills: &mut Vec<RecordPages>, run: Option<(u64, u64)>) {
    if let Some(pages) = run.and_then(|(first, end)| PageRange::from_bounds(first, end)) {
        fills.extend(RecordPages::split(pages));
    }
}

/// What [`PageIndex::invalidate`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Invalidated {
    /// Cached pages dropped (their frames released).
    pub dropped: usize,
    /// Pages being filled, whose fills are now stale.
    pub stale: usize,
    /// Of the dropped pages, those that were dirty.
    pub dirty: usize,
}

/// Why a fill answer was not applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FillError {
    /// A done answer came with the wrong number of frames.
    Frames {
        /// The pages it names.
        expected: u32,
        /// The frames it has.
        got: usize,
    },
    /// A page could not make the transition (a bug: the pages were checked first).
    Transition(TransitionError),
}

impl fmt::Display for FillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frames { expected, got } => {
                write!(f, "a fill of {expected} pages came with {got} frames")
            }
            Self::Transition(_) => f.write_str("a page cannot take the fill's answer"),
        }
    }
}

impl core::error::Error for FillError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Transition(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::abi::MAX_RECORD_PAGES;
    use core::num::NonZeroU16;
    use std::string::ToString;
    use std::vec;

    fn range(first: u64, count: u32) -> PageRange {
        PageRange::new(first, count).expect("a valid range")
    }

    fn record(first: u64, count: u32) -> RecordPages {
        RecordPages::from_parts(first, count).expect("a valid record")
    }

    const EIO: Outcome = Outcome::Failed(NonZeroU16::new(5).expect("non-zero"));

    /// The pages of the fills a [`Read::Wait`] started, `None` for another answer.
    fn wait_pages(step: ReadStep) -> Option<Vec<RecordPages>> {
        (step.read == Read::Wait).then(|| step.fills.iter().map(|fill| fill.pages).collect())
    }

    /// The fill that owns `page` now, or ID 0 (never given out) when none does.
    fn owner(index: &PageIndex<u64>, page: u64) -> FillId {
        index
            .pages
            .get(&page)
            .and_then(|entry| entry.fill.as_ref())
            .map_or(FillId::new(0), |fill| fill.id)
    }

    /// Answers the fill that owns the first of `pages` now, as the file system would.
    fn answer(
        index: &mut PageIndex<u64>,
        pages: RecordPages,
        outcome: Outcome,
        frames: Vec<u64>,
    ) -> Result<u32, FillError> {
        let id = owner(index, pages.pages().first());
        index.complete_fill(id, pages, outcome, frames)
    }

    /// A 10-page file (the last page partly used).
    fn index() -> PageIndex<u64> {
        let mut index = PageIndex::new();
        index.set_size(9 * PAGE_SIZE + 100);
        index
    }

    /// Frames named after their pages.
    fn frames(first: u64, count: u64) -> Vec<u64> {
        (first..first + count).collect()
    }

    #[test]
    fn reads_are_cut_at_the_size() {
        let index = index();
        assert_eq!(index.file_pages(), 10);
        assert_eq!(index.pages_for_read(0, 1), Some((range(0, 1), 1)));
        assert_eq!(
            index.pages_for_read(PAGE_SIZE - 1, 2),
            Some((range(0, 2), 2))
        );
        // Past the end, nothing; up to it, the bytes that are there.
        assert_eq!(index.pages_for_read(9 * PAGE_SIZE + 100, 10), None);
        assert_eq!(
            index.pages_for_read(9 * PAGE_SIZE, 4096),
            Some((range(9, 1), 100))
        );
        assert_eq!(index.pages_for_read(0, 0), None);
        assert_eq!(
            index.pages_for_read(0, usize::MAX),
            Some((range(0, 10), 9 * 4096 + 100))
        );
    }

    #[test]
    fn a_miss_fills_then_hits() {
        let mut index = index();
        let (mut a, mut b) = (Attempt::new(), Attempt::new());
        assert_eq!(
            wait_pages(index.start_read(range(2, 3), &mut a, None)),
            Some(vec![record(2, 3)])
        );
        assert_eq!(index.state(3), PageState::Filling);
        assert_eq!(index.frames(range(2, 3)), None);
        // A second reader of overlapping pages waits for the same fill and starts one only
        // for the page nobody fills.
        assert_eq!(
            wait_pages(index.start_read(range(4, 2), &mut b, None)),
            Some(vec![record(5, 1)])
        );
        answer(&mut index, record(2, 3), Outcome::Done, frames(2, 3))
            .expect("pages 2..5 are filling");
        assert_eq!(
            index.start_read(range(2, 3), &mut a, None).read,
            Read::Ready
        );
        assert_eq!(index.frames(range(2, 3)), Some(vec![2, 3, 4]));
        assert_eq!(
            wait_pages(index.start_read(range(4, 2), &mut b, None)),
            Some(vec![])
        );
        assert_eq!(index.resident(), 3);
    }

    #[test]
    fn fills_are_cut_into_records_around_present_pages() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(3000 * PAGE_SIZE);
        let fills = wait_pages(index.start_read(range(5, 1), &mut Attempt::new(), None));
        assert_eq!(fills, Some(vec![record(5, 1)]));
        let fills = wait_pages(index.start_read(range(0, 2100), &mut Attempt::new(), None));
        assert_eq!(
            fills.as_deref(),
            Some(
                &[
                    record(0, 5),
                    record(6, MAX_RECORD_PAGES),
                    record(6 + 1024, MAX_RECORD_PAGES),
                    record(6 + 2048, 46)
                ][..]
            )
        );
    }

    #[test]
    fn a_failed_fill_fails_its_waiters_and_is_retried_by_new_readers() {
        let mut index = index();
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(0, 2), &mut waiter, None);
        answer(&mut index, record(0, 2), EIO, Vec::new()).expect("pages 0..2 are filling");
        assert_eq!(index.state(0), PageState::Absent);
        assert_eq!(
            index.start_read(range(0, 2), &mut waiter, None).read,
            Read::Failed
        );
        // A read that had not waited fills again; its retry then waits, not fails.
        let mut late = Attempt::new();
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut late, None)),
            Some(vec![record(1, 1)])
        );
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut late, None)),
            Some(vec![])
        );
    }

    #[test]
    fn a_new_fill_does_not_hide_a_failure_from_its_waiters() {
        let mut index = index();
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(1, 1), &mut waiter, None);
        answer(&mut index, record(1, 1), EIO, Vec::new()).expect("page 1 is filling");
        // Before the waiter runs again, a new reader fills page 1 again.
        let mut late = Attempt::new();
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut late, None)),
            Some(vec![record(1, 1)])
        );
        // The waiter still gets the failure of the fill it waited for.
        assert_eq!(
            index.start_read(range(1, 1), &mut waiter, None).read,
            Read::Failed
        );
        answer(&mut index, record(1, 1), Outcome::Done, frames(1, 1))
            .expect("page 1 is filling again");
        assert_eq!(
            index.start_read(range(1, 1), &mut late, None).read,
            Read::Ready
        );
        // Invalidation does not hide the failure either.
        let _ = index.invalidate(range(1, 1));
        assert_eq!(
            index.start_read(range(1, 1), &mut waiter, None).read,
            Read::Failed
        );
    }

    #[test]
    fn answers_act_only_on_pages_their_fill_owns() {
        let mut index = index();
        // Nothing is being filled: any answer is stale and changes nothing.
        assert_eq!(
            index.complete_fill(FillId::new(1), record(0, 1), Outcome::Done, frames(0, 1)),
            Ok(0)
        );
        let _ = index.start_read(range(0, 2), &mut Attempt::new(), None);
        let id = owner(&index, 0);
        assert_eq!(id, FillId::new(1));
        // A range past the fill's pages takes only the pages the fill owns.
        assert_eq!(
            index.complete_fill(id, record(0, 2), Outcome::Done, frames(0, 1)),
            Err(FillError::Frames {
                expected: 2,
                got: 1
            })
        );
        assert_eq!(index.state(0), PageState::Filling);
        // Another fill's ID owns nothing here.
        assert_eq!(
            index.complete_fill(FillId::new(2), record(0, 2), EIO, Vec::new()),
            Ok(0)
        );
        assert_eq!(index.state(0), PageState::Filling);
        assert_eq!(
            index.complete_fill(id, record(0, 2), Outcome::Done, frames(0, 2)),
            Ok(2)
        );
        // Answered once: a second answer is stale, malformed or not.
        assert_eq!(
            index.complete_fill(id, record(0, 2), Outcome::Done, frames(0, 2)),
            Ok(0)
        );
        assert_eq!(
            index.complete_fill(id, record(0, 2), Outcome::Done, Vec::new()),
            Ok(0)
        );
        assert_eq!(index.frames(range(0, 2)), Some(vec![0, 1]));
    }

    #[test]
    fn fill_ids_increase_and_are_not_reused() {
        let mut index = index();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let _ = index.start_read(range(3, 1), &mut Attempt::new(), None);
            ids.push(owner(&index, 3));
            answer(&mut index, record(3, 1), EIO, Vec::new()).expect("filling");
        }
        assert_eq!(ids, [FillId::new(1), FillId::new(2), FillId::new(3)]);
    }

    /// The last fill ID is given out once; after it, reads that need a fill are refused
    /// and change nothing, readahead starts nothing, and cached pages still read.
    #[test]
    fn fill_ids_run_out_instead_of_repeating() {
        let mut index = index();
        // Page 1 cached, so a read of pages 0..3 needs two fills.
        let _ = index.start_read(range(1, 1), &mut Attempt::new(), None);
        answer(&mut index, record(1, 1), Outcome::Done, frames(1, 1)).expect("filling");
        index.next_fill = Some(FillId::new(u64::MAX));
        // One ID left is not enough for two fills: nothing starts.
        assert_eq!(
            index
                .start_read(range(0, 3), &mut Attempt::new(), None)
                .read,
            Read::OutOfFillIds
        );
        assert_eq!(index.state(0), PageState::Absent);
        assert_eq!(index.state(2), PageState::Absent);
        // One fill takes the last ID.
        let _ = index.start_read(range(0, 1), &mut Attempt::new(), None);
        assert_eq!(owner(&index, 0), FillId::new(u64::MAX));
        answer(&mut index, record(0, 1), EIO, Vec::new()).expect("filling");
        assert_eq!(
            index
                .start_read(range(0, 1), &mut Attempt::new(), None)
                .read,
            Read::OutOfFillIds
        );
        assert_eq!(index.state(0), PageState::Absent);
        assert!(index.prefetch(range(2, 4)).is_empty());
        assert_eq!(index.state(2), PageState::Absent);
        assert_eq!(
            index
                .start_read(range(1, 1), &mut Attempt::new(), None)
                .read,
            Read::Ready
        );
    }

    /// The pages being filled, which must be exactly the pages of the fills handed out.
    fn filling(index: &PageIndex<u64>) -> Vec<u64> {
        let pages = index.pages.iter();
        pages
            .filter(|(_, entry)| entry.state == PageState::Filling)
            .map(|(&page, _)| page)
            .collect()
    }

    fn pages_of(fills: &[Fill]) -> Vec<u64> {
        let ranges = fills.iter().map(|fill| fill.pages.pages());
        ranges.flat_map(|r| r.first()..r.end()).collect()
    }

    /// The kernel's read path, with one fill ID left: a read missing two runs, with
    /// readahead, starts nothing at all (so no page is left filling with no record to
    /// queue); a read missing one run takes the last ID and readahead starts nothing; and
    /// a read that cannot go on never starts readahead.
    #[test]
    fn a_read_out_of_fill_ids_leaves_no_page_filling_unqueued() {
        let mut index = index();
        let _ = index.start_read(range(1, 1), &mut Attempt::new(), None);
        answer(&mut index, record(1, 1), Outcome::Done, frames(1, 1)).expect("filling");
        index.next_fill = Some(FillId::new(u64::MAX));
        let ahead = Some(range(4, 4));
        // Pages 0 and 2 are two runs; one ID is not enough, and readahead does not run.
        let step = index.start_read(range(0, 3), &mut Attempt::new(), ahead);
        assert_eq!(step.read, Read::OutOfFillIds);
        assert!(step.fills.is_empty());
        assert_eq!(filling(&index), []);
        // One run takes the last ID; readahead has none left and starts nothing.
        let step = index.start_read(range(0, 1), &mut Attempt::new(), ahead);
        assert_eq!(step.read, Read::Wait);
        assert_eq!(pages_of(&step.fills), [0]);
        assert_eq!(filling(&index), pages_of(&step.fills));
        // A reader whose fill failed gets Failed and starts no readahead either.
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(0, 1), &mut waiter, None);
        index.next_fill = Some(FillId::new(7));
        answer(&mut index, record(0, 1), EIO, Vec::new()).expect("filling");
        let step = index.start_read(range(0, 1), &mut waiter, ahead);
        assert_eq!(step.read, Read::Failed);
        assert!(step.fills.is_empty());
        assert_eq!(filling(&index), []);
    }

    /// Readahead's fills come in the same list as the read's own, on the first look only.
    #[test]
    fn readahead_fills_come_with_the_reads_own() {
        let mut index = index();
        let mut reader = Attempt::new();
        let step = index.start_read(range(0, 1), &mut reader, Some(range(1, 3)));
        assert_eq!(step.read, Read::Wait);
        assert_eq!(pages_of(&step.fills), [0, 1, 2, 3]);
        assert_eq!(filling(&index), [0, 1, 2, 3]);
        // A second look (a retry) does not read ahead again.
        let step = index.start_read(range(0, 1), &mut reader, Some(range(5, 2)));
        assert!(step.fills.is_empty());
        assert_eq!(index.state(5), PageState::Absent);
    }

    /// A fill fails, a reader fills the pages again, and then a late, malformed answer to the
    /// first fill arrives: the retry is untouched and completes.
    #[test]
    fn a_late_malformed_answer_to_an_old_fill_leaves_the_retry_alone() {
        let mut index = index();
        let mut first = Attempt::new();
        let _ = index.start_read(range(0, 2), &mut first, None);
        let old = owner(&index, 0);
        index
            .complete_fill(old, record(0, 2), EIO, Vec::new())
            .expect("filling");
        let mut retry = Attempt::new();
        assert_eq!(
            wait_pages(index.start_read(range(0, 2), &mut retry, None)),
            Some(vec![record(0, 2)])
        );
        let new = owner(&index, 0);
        assert_ne!(new, old);
        // The wrong number of frames, a failure, and a done answer, all to the old fill.
        for (outcome, frames) in [
            (Outcome::Done, vec![9]),
            (EIO, Vec::new()),
            (Outcome::Done, vec![9, 9]),
        ] {
            assert_eq!(
                index.complete_fill(old, record(0, 2), outcome, frames),
                Ok(0)
            );
        }
        assert_eq!(index.state(0), PageState::Filling);
        assert_eq!(
            wait_pages(index.start_read(range(0, 2), &mut retry, None)),
            Some(vec![])
        );
        assert_eq!(
            index.complete_fill(new, record(0, 2), Outcome::Done, frames(0, 2)),
            Ok(2)
        );
        assert_eq!(
            index.start_read(range(0, 2), &mut retry, None).read,
            Read::Ready
        );
        assert_eq!(index.frames(range(0, 2)), Some(vec![0, 1]));
    }

    /// A fill owns only part of its range once some of its pages went (invalidated and
    /// filled again by another fill): its answer completes the part it still owns.
    #[test]
    fn a_fill_that_owns_part_of_its_range_completes_that_part() {
        let mut index = index();
        let mut reader = Attempt::new();
        let _ = index.start_read(range(0, 4), &mut reader, None);
        let old = owner(&index, 0);
        // Page 1 is invalidated, then the stale page is answered and filled anew.
        let _ = index.invalidate(range(1, 1));
        index
            .complete_fill(old, record(1, 1), Outcome::Done, vec![50])
            .expect("page 1 is the old fill's");
        assert_eq!(index.state(1), PageState::Absent);
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut Attempt::new(), None)),
            Some(vec![record(1, 1)])
        );
        let new = owner(&index, 1);
        assert_eq!(index.owned(old, record(0, 4)), 3);
        // The old fill's answer for all four pages takes 0, 2 and 3; page 1 is the new fill's.
        assert_eq!(
            index.complete_fill(old, record(0, 4), Outcome::Done, frames(0, 4)),
            Ok(3)
        );
        assert_eq!(index.state(1), PageState::Filling);
        assert_eq!(index.frames(range(2, 2)), Some(vec![2, 3]));
        // A failure from the old fill now touches nothing.
        assert_eq!(
            index.complete_fill(old, record(0, 4), EIO, Vec::new()),
            Ok(0)
        );
        assert_eq!(
            index.complete_fill(new, record(1, 1), Outcome::Done, vec![101]),
            Ok(1)
        );
        assert_eq!(index.frames(range(0, 4)), Some(vec![0, 101, 2, 3]));
    }

    #[test]
    fn prefetch_fills_only_absent_pages() {
        let mut index = index();
        let _ = index.start_read(range(0, 2), &mut Attempt::new(), None);
        answer(&mut index, record(0, 2), Outcome::Done, frames(0, 2)).expect("filling");
        let _ = index.start_read(range(4, 1), &mut Attempt::new(), None);
        let pages: Vec<RecordPages> = index
            .prefetch(range(0, 8))
            .iter()
            .map(|f| f.pages)
            .collect();
        assert_eq!(pages, [record(2, 2), record(5, 3)]);
        assert_eq!(index.prefetch(range(0, 8)), []);
    }

    #[test]
    fn closing_the_queue_fails_every_outstanding_fill() {
        let mut index = index();
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(0, 3), &mut waiter, None);
        let _ = index.prefetch(range(5, 2));
        assert_eq!(index.fail_all(), 5);
        assert_eq!(
            index.start_read(range(0, 1), &mut waiter, None).read,
            Read::Failed
        );
        assert_eq!(index.fail_all(), 0);
    }

    #[test]
    fn shrinking_drops_pages_past_the_end_and_stales_busy_ones() {
        let mut index = index();
        let _ = index.start_read(range(0, 10), &mut Attempt::new(), None);
        answer(&mut index, record(0, 8), Outcome::Done, frames(0, 8)).expect("filling");
        assert_eq!(index.set_size(4 * PAGE_SIZE), 4);
        assert_eq!(index.resident(), 4);
        // Pages 8 and 9 are still being filled: the answer is accepted but not kept.
        assert_eq!(index.state(9), PageState::Filling);
        assert_eq!(
            answer(&mut index, record(8, 2), Outcome::Done, frames(8, 2)),
            Ok(2)
        );
        assert_eq!(index.state(9), PageState::Absent);
        assert_eq!(index.pages_for_read(4 * PAGE_SIZE, 10), None);
        // Growing again shows none of them: they are filled anew.
        index.set_size(10 * PAGE_SIZE);
        assert_eq!(
            wait_pages(index.start_read(range(4, 6), &mut Attempt::new(), None)),
            Some(vec![record(4, 6)])
        );
    }

    #[test]
    fn shrinking_into_a_cached_page_drops_it() {
        // A 4-page file, all cached.
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(4 * PAGE_SIZE);
        let _ = index.start_read(range(0, 4), &mut Attempt::new(), None);
        answer(&mut index, record(0, 4), Outcome::Done, frames(0, 4)).expect("filling");
        // Cut inside page 2, then grown back: page 2's old bytes past the cut must not be
        // served, so it is filled again; pages 0 and 1 still hit.
        assert_eq!(index.set_size(2 * PAGE_SIZE + 3), 2);
        assert_eq!(index.state(2), PageState::Absent);
        index.set_size(4 * PAGE_SIZE);
        assert_eq!(
            index
                .start_read(range(0, 2), &mut Attempt::new(), None)
                .read,
            Read::Ready
        );
        assert_eq!(
            wait_pages(index.start_read(range(2, 2), &mut Attempt::new(), None)),
            Some(vec![record(2, 2)])
        );
        // Growing alone drops nothing.
        assert_eq!(index.set_size(8 * PAGE_SIZE), 0);
        assert_eq!(index.resident(), 2);
    }

    #[test]
    fn a_stale_page_does_not_mask_a_failure_of_its_record() {
        let mut index = index();
        let mut waiter = Attempt::new();
        assert_eq!(
            wait_pages(index.start_read(range(0, 3), &mut waiter, None)),
            Some(vec![record(0, 3)])
        );
        // Page 0 goes stale, then the whole record fails: pages 1 and 2 failed.
        let _ = index.invalidate(range(0, 1));
        answer(&mut index, record(0, 3), EIO, Vec::new()).expect("pages 0..3 are filling");
        assert_eq!(
            index.start_read(range(0, 3), &mut waiter, None).read,
            Read::Failed
        );
        // A record whose only failing page is stale has not failed: the reader fills again.
        let mut other = Attempt::new();
        let _ = index.start_read(range(5, 2), &mut other, None);
        let _ = index.invalidate(range(5, 2));
        answer(&mut index, record(5, 2), EIO, Vec::new()).expect("pages 5..7 are filling");
        assert_eq!(
            wait_pages(index.start_read(range(5, 2), &mut other, None)),
            Some(vec![record(5, 2)])
        );
    }

    #[test]
    fn answers_for_part_of_a_record_decide_it_together() {
        let mut index = index();
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(0, 4), &mut waiter, None);
        // The first half is answered; the record is still going for the rest.
        answer(&mut index, record(0, 2), Outcome::Done, frames(0, 2))
            .expect("pages 0..2 are filling");
        assert_eq!(
            wait_pages(index.start_read(range(0, 4), &mut waiter, None)),
            Some(vec![])
        );
        // The second half fails: the reader that waited for the record sees it.
        answer(&mut index, record(2, 2), EIO, Vec::new()).expect("pages 2..4 are filling");
        assert_eq!(
            index.start_read(range(0, 4), &mut waiter, None).read,
            Read::Failed
        );
        // And when the queue goes, every outstanding record fails.
        let mut late = Attempt::new();
        let _ = index.start_read(range(6, 3), &mut late, None);
        assert_eq!(index.fail_all(), 3);
        assert_eq!(
            index.start_read(range(6, 3), &mut late, None).read,
            Read::Failed
        );
    }

    #[test]
    fn a_failure_survives_truncation_and_regrowth() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(64 * PAGE_SIZE);
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(40, 1), &mut waiter, None);
        answer(&mut index, record(40, 1), EIO, Vec::new()).expect("page 40 is filling");
        // The failed page is forgotten at once: the waiter holds the failure, not the index.
        assert!(index.pages.is_empty());
        // Cut below the page and grown back before the waiter runs again.
        index.set_size(3 * PAGE_SIZE);
        index.set_size(64 * PAGE_SIZE);
        assert_eq!(
            index.start_read(range(40, 1), &mut waiter, None).read,
            Read::Failed
        );
        // A new reader fills the page again.
        assert_eq!(
            wait_pages(index.start_read(range(40, 1), &mut Attempt::new(), None)),
            Some(vec![record(40, 1)])
        );
    }

    #[test]
    fn failed_and_stale_fills_leave_nothing_behind() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(64 * PAGE_SIZE);
        // A stale fill past a new end, answered with a failure, and many failed fills.
        let mut stale = Attempt::new();
        let _ = index.start_read(range(50, 1), &mut stale, None);
        index.set_size(3 * PAGE_SIZE);
        answer(&mut index, record(50, 1), EIO, Vec::new()).expect("page 50 is filling");
        assert!(index.pages.is_empty());
        // A stale fill is not a failure for its reader, which goes on (the kernel would cut
        // its read at the new end; here it reads page 0 instead).
        assert_eq!(
            wait_pages(index.start_read(range(0, 1), &mut stale, None)),
            Some(vec![record(0, 1)])
        );
        answer(&mut index, record(0, 1), Outcome::Done, frames(0, 1)).expect("page 0 is filling");
        for round in 0..100 {
            index.set_size(64 * PAGE_SIZE);
            let page = 10 + round % 40;
            let _ = index.start_read(range(page, 1), &mut Attempt::new(), None);
            answer(&mut index, record(page, 1), EIO, Vec::new()).expect("filling");
            index.set_size(3 * PAGE_SIZE);
        }
        assert_eq!(index.pages.keys().copied().collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn invalidating_drops_cached_pages() {
        let mut index = index();
        let _ = index.start_read(range(0, 4), &mut Attempt::new(), None);
        answer(&mut index, record(0, 4), Outcome::Done, frames(0, 4))
            .expect("pages 0..4 are filling");
        assert_eq!(
            index.invalidate(range(1, 2)),
            Invalidated {
                dropped: 2,
                ..Invalidated::default()
            }
        );
        assert_eq!(index.resident(), 2);
        assert_eq!(index.state(1), PageState::Absent);
        // The next read of them fills them again; the pages left alone still hit.
        assert_eq!(
            index
                .start_read(range(0, 1), &mut Attempt::new(), None)
                .read,
            Read::Ready
        );
        assert_eq!(
            wait_pages(index.start_read(range(1, 2), &mut Attempt::new(), None)),
            Some(vec![record(1, 2)])
        );
        // Absent pages and pages past the range are not counted.
        assert_eq!(index.invalidate(range(5, 5)), Invalidated::default());
    }

    #[test]
    fn a_fill_in_flight_over_invalidated_pages_is_not_taken() {
        let mut index = index();
        let mut reader = Attempt::new();
        let _ = index.start_read(range(0, 3), &mut reader, None);
        assert_eq!(
            index.invalidate(range(1, 1)),
            Invalidated {
                stale: 1,
                ..Invalidated::default()
            }
        );
        // The answer may hold the old bytes of page 1: pages 0 and 2 are taken, page 1 is
        // absent again, not failed, and the waiting reader's retry fills it anew.
        answer(&mut index, record(0, 3), Outcome::Done, frames(0, 3))
            .expect("pages 0..3 are filling");
        assert_eq!(index.state(0), PageState::Uptodate);
        assert_eq!(index.state(1), PageState::Absent);
        assert_eq!(index.state(2), PageState::Uptodate);
        assert_eq!(
            wait_pages(index.start_read(range(0, 3), &mut reader, None)),
            Some(vec![record(1, 1)])
        );
        answer(&mut index, record(1, 1), Outcome::Done, vec![100])
            .expect("page 1 is filling again");
        assert_eq!(index.frames(range(0, 3)), Some(vec![0, 100, 2]));
    }

    #[test]
    fn a_stale_fill_that_fails_does_not_fail_its_readers() {
        let mut index = index();
        let mut reader = Attempt::new();
        let _ = index.start_read(range(4, 1), &mut reader, None);
        let _ = index.invalidate(range(4, 1));
        answer(&mut index, record(4, 1), EIO, Vec::new()).expect("page 4 is filling");
        assert_eq!(index.state(4), PageState::Absent);
        assert_eq!(
            wait_pages(index.start_read(range(4, 1), &mut reader, None)),
            Some(vec![record(4, 1)])
        );
    }

    #[test]
    fn errors_describe_themselves() {
        let transition = PageState::Absent
            .apply(Event::FillDone)
            .expect_err("absent pages are not filled");
        let messages = [
            FillError::Frames {
                expected: 2,
                got: 1,
            }
            .to_string(),
            FillError::Transition(transition).to_string(),
        ];
        for (i, message) in messages.iter().enumerate() {
            assert!(!message.is_empty());
            assert!(!messages[..i].contains(message));
        }
        assert!(core::error::Error::source(&FillError::Transition(transition)).is_some());
    }
}
