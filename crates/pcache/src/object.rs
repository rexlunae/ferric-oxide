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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// Every page holds the file's data.
    Ready,
    /// Some pages are being filled. `fills` are the records this call started, to put on the
    /// queue; empty when only fills already outstanding are waited for.
    Wait {
        /// The fill records to queue.
        fills: Vec<Fill>,
    },
    /// A fill the reader waited for failed.
    Failed,
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
}

/// The pages of one cache object.
#[derive(Clone, Debug)]
pub struct PageIndex<F> {
    size: u64,
    /// Pages that are not absent: absent pages are not stored.
    pages: BTreeMap<u64, Entry<F>>,
    /// The ID the next fill gets.
    next_fill: FillId,
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
            next_fill: FillId::new(1),
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
    /// the end become stale. Returns how many cached pages were dropped.
    pub fn set_size(&mut self, size: u64) -> usize {
        let shrunk = size < self.size;
        self.size = size;
        if shrunk {
            self.invalidate_from(size / PAGE_SIZE, u64::MAX).dropped
        } else {
            0
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

    /// Looks at `pages` for a read: see [`Read`]. Absent pages become
    /// [`PageState::Filling`], in records of at most [`crate::abi::MAX_RECORD_PAGES`] pages,
    /// and `attempt` keeps the outcome of every fill it now waits for.
    pub fn start_read(&mut self, pages: PageRange, attempt: &mut Attempt) -> Read {
        if attempt.take_failure() {
            return Read::Failed;
        }
        let fills = self.start_fills(pages);
        let mut busy = false;
        for entry in self.pages.range(pages.first()..pages.end()).map(|(_, e)| e) {
            if entry.state.busy() {
                busy = true;
                if let Some(fill) = &entry.fill {
                    attempt.wait_for(fill);
                }
            }
        }
        // Even a ready read is no longer a first look.
        attempt.waiting.get_or_insert_with(Vec::new);
        if fills.is_empty() && !busy {
            Read::Ready
        } else {
            Read::Wait { fills }
        }
    }

    /// Starts fills of the absent pages of `pages` that no reader waits for yet (readahead),
    /// and returns their records.
    pub fn prefetch(&mut self, pages: PageRange) -> Vec<Fill> {
        self.start_fills(pages)
    }

    /// Marks the absent pages of `pages` filling, and returns them as records, each with a
    /// new [`FillId`] and an outcome its pages share.
    fn start_fills(&mut self, pages: PageRange) -> Vec<Fill> {
        let mut fills = Vec::new();
        let mut run: Option<(u64, u64)> = None;
        for page in pages.first()..pages.end() {
            let entry = self.pages.entry(page).or_insert_with(Entry::absent);
            // Pages being filled or holding data are left as they are.
            if let Ok(next) = entry.state.apply(Event::FillStart) {
                entry.state = next;
                run = match run {
                    Some((first, end)) if end == page => Some((first, page + 1)),
                    other => {
                        push_run(&mut fills, other);
                        Some((page, page + 1))
                    }
                };
            }
        }
        push_run(&mut fills, run);
        fills
            .into_iter()
            .map(|pages| {
                let id = self.take_fill_id();
                let outcome = Arc::new(FillOutcome::new(id, pages.pages().count()));
                let range = pages.pages();
                for page in range.first()..range.end() {
                    if let Some(entry) = self.pages.get_mut(&page) {
                        entry.fill = Some(Arc::clone(&outcome));
                    }
                }
                Fill { id, pages }
            })
            .collect()
    }

    /// A new fill ID. They run out only after 2^64 fills of one object; the last is then
    /// reused rather than wrap round to the first.
    fn take_fill_id(&mut self) -> FillId {
        let id = self.next_fill;
        self.next_fill = id.next().unwrap_or(id);
        id
    }

    /// The frames of `pages`, in order, when every one is readable.
    pub fn frames(&self, pages: PageRange) -> Option<Vec<F>>
    where
        F: Clone,
    {
        (pages.first()..pages.end())
            .map(|page| {
                let entry = self.pages.get(&page)?;
                entry
                    .state
                    .readable()
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
    /// those being filled stale (see the module docs). Pages under writeback are left as they
    /// are: they hold data newer than the file system's, which only the write path (PR 5)
    /// creates.
    pub fn invalidate(&mut self, pages: PageRange) -> Invalidated {
        self.invalidate_from(pages.first(), pages.end())
    }

    /// [`PageIndex::invalidate`] of the pages `first..end`.
    fn invalidate_from(&mut self, first: u64, end: u64) -> Invalidated {
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
                PageState::Writeback { .. } | PageState::Absent => {}
                _ => {
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
        self.pages
            .remove(&page)
            .is_some_and(|entry| entry.frame.is_some())
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
    fn wait_pages(read: Read) -> Option<Vec<RecordPages>> {
        match read {
            Read::Wait { fills } => Some(fills.iter().map(|fill| fill.pages).collect()),
            Read::Ready | Read::Failed => None,
        }
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
            wait_pages(index.start_read(range(2, 3), &mut a)),
            Some(vec![record(2, 3)])
        );
        assert_eq!(index.state(3), PageState::Filling);
        assert_eq!(index.frames(range(2, 3)), None);
        // A second reader of overlapping pages waits for the same fill and starts one only
        // for the page nobody fills.
        assert_eq!(
            wait_pages(index.start_read(range(4, 2), &mut b)),
            Some(vec![record(5, 1)])
        );
        answer(&mut index, record(2, 3), Outcome::Done, frames(2, 3))
            .expect("pages 2..5 are filling");
        assert_eq!(index.start_read(range(2, 3), &mut a), Read::Ready);
        assert_eq!(index.frames(range(2, 3)), Some(vec![2, 3, 4]));
        assert_eq!(
            wait_pages(index.start_read(range(4, 2), &mut b)),
            Some(vec![])
        );
        assert_eq!(index.resident(), 3);
    }

    #[test]
    fn fills_are_cut_into_records_around_present_pages() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(3000 * PAGE_SIZE);
        let fills = wait_pages(index.start_read(range(5, 1), &mut Attempt::new()));
        assert_eq!(fills, Some(vec![record(5, 1)]));
        let fills = wait_pages(index.start_read(range(0, 2100), &mut Attempt::new()));
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
        let _ = index.start_read(range(0, 2), &mut waiter);
        answer(&mut index, record(0, 2), EIO, Vec::new()).expect("pages 0..2 are filling");
        assert_eq!(index.state(0), PageState::Absent);
        assert_eq!(index.start_read(range(0, 2), &mut waiter), Read::Failed);
        // A read that had not waited fills again; its retry then waits, not fails.
        let mut late = Attempt::new();
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut late)),
            Some(vec![record(1, 1)])
        );
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut late)),
            Some(vec![])
        );
    }

    #[test]
    fn a_new_fill_does_not_hide_a_failure_from_its_waiters() {
        let mut index = index();
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(1, 1), &mut waiter);
        answer(&mut index, record(1, 1), EIO, Vec::new()).expect("page 1 is filling");
        // Before the waiter runs again, a new reader fills page 1 again.
        let mut late = Attempt::new();
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut late)),
            Some(vec![record(1, 1)])
        );
        // The waiter still gets the failure of the fill it waited for.
        assert_eq!(index.start_read(range(1, 1), &mut waiter), Read::Failed);
        answer(&mut index, record(1, 1), Outcome::Done, frames(1, 1))
            .expect("page 1 is filling again");
        assert_eq!(index.start_read(range(1, 1), &mut late), Read::Ready);
        // Invalidation does not hide the failure either.
        let _ = index.invalidate(range(1, 1));
        assert_eq!(index.start_read(range(1, 1), &mut waiter), Read::Failed);
    }

    #[test]
    fn answers_act_only_on_pages_their_fill_owns() {
        let mut index = index();
        // Nothing is being filled: any answer is stale and changes nothing.
        assert_eq!(
            index.complete_fill(FillId::new(1), record(0, 1), Outcome::Done, frames(0, 1)),
            Ok(0)
        );
        let _ = index.start_read(range(0, 2), &mut Attempt::new());
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
            let _ = index.start_read(range(3, 1), &mut Attempt::new());
            ids.push(owner(&index, 3));
            answer(&mut index, record(3, 1), EIO, Vec::new()).expect("filling");
        }
        assert_eq!(ids, [FillId::new(1), FillId::new(2), FillId::new(3)]);
    }

    /// A fill fails, a reader fills the pages again, and then a late, malformed answer to the
    /// first fill arrives: the retry is untouched and completes.
    #[test]
    fn a_late_malformed_answer_to_an_old_fill_leaves_the_retry_alone() {
        let mut index = index();
        let mut first = Attempt::new();
        let _ = index.start_read(range(0, 2), &mut first);
        let old = owner(&index, 0);
        index
            .complete_fill(old, record(0, 2), EIO, Vec::new())
            .expect("filling");
        let mut retry = Attempt::new();
        assert_eq!(
            wait_pages(index.start_read(range(0, 2), &mut retry)),
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
            wait_pages(index.start_read(range(0, 2), &mut retry)),
            Some(vec![])
        );
        assert_eq!(
            index.complete_fill(new, record(0, 2), Outcome::Done, frames(0, 2)),
            Ok(2)
        );
        assert_eq!(index.start_read(range(0, 2), &mut retry), Read::Ready);
        assert_eq!(index.frames(range(0, 2)), Some(vec![0, 1]));
    }

    /// A fill owns only part of its range once some of its pages went (invalidated and
    /// filled again by another fill): its answer completes the part it still owns.
    #[test]
    fn a_fill_that_owns_part_of_its_range_completes_that_part() {
        let mut index = index();
        let mut reader = Attempt::new();
        let _ = index.start_read(range(0, 4), &mut reader);
        let old = owner(&index, 0);
        // Page 1 is invalidated, then the stale page is answered and filled anew.
        let _ = index.invalidate(range(1, 1));
        index
            .complete_fill(old, record(1, 1), Outcome::Done, vec![50])
            .expect("page 1 is the old fill's");
        assert_eq!(index.state(1), PageState::Absent);
        assert_eq!(
            wait_pages(index.start_read(range(1, 1), &mut Attempt::new())),
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
        let _ = index.start_read(range(0, 2), &mut Attempt::new());
        answer(&mut index, record(0, 2), Outcome::Done, frames(0, 2)).expect("filling");
        let _ = index.start_read(range(4, 1), &mut Attempt::new());
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
        let _ = index.start_read(range(0, 3), &mut waiter);
        let _ = index.prefetch(range(5, 2));
        assert_eq!(index.fail_all(), 5);
        assert_eq!(index.start_read(range(0, 1), &mut waiter), Read::Failed);
        assert_eq!(index.fail_all(), 0);
    }

    #[test]
    fn shrinking_drops_pages_past_the_end_and_stales_busy_ones() {
        let mut index = index();
        let _ = index.start_read(range(0, 10), &mut Attempt::new());
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
            wait_pages(index.start_read(range(4, 6), &mut Attempt::new())),
            Some(vec![record(4, 6)])
        );
    }

    #[test]
    fn shrinking_into_a_cached_page_drops_it() {
        // A 4-page file, all cached.
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(4 * PAGE_SIZE);
        let _ = index.start_read(range(0, 4), &mut Attempt::new());
        answer(&mut index, record(0, 4), Outcome::Done, frames(0, 4)).expect("filling");
        // Cut inside page 2, then grown back: page 2's old bytes past the cut must not be
        // served, so it is filled again; pages 0 and 1 still hit.
        assert_eq!(index.set_size(2 * PAGE_SIZE + 3), 2);
        assert_eq!(index.state(2), PageState::Absent);
        index.set_size(4 * PAGE_SIZE);
        assert_eq!(
            index.start_read(range(0, 2), &mut Attempt::new()),
            Read::Ready
        );
        assert_eq!(
            wait_pages(index.start_read(range(2, 2), &mut Attempt::new())),
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
            wait_pages(index.start_read(range(0, 3), &mut waiter)),
            Some(vec![record(0, 3)])
        );
        // Page 0 goes stale, then the whole record fails: pages 1 and 2 failed.
        let _ = index.invalidate(range(0, 1));
        answer(&mut index, record(0, 3), EIO, Vec::new()).expect("pages 0..3 are filling");
        assert_eq!(index.start_read(range(0, 3), &mut waiter), Read::Failed);
        // A record whose only failing page is stale has not failed: the reader fills again.
        let mut other = Attempt::new();
        let _ = index.start_read(range(5, 2), &mut other);
        let _ = index.invalidate(range(5, 2));
        answer(&mut index, record(5, 2), EIO, Vec::new()).expect("pages 5..7 are filling");
        assert_eq!(
            wait_pages(index.start_read(range(5, 2), &mut other)),
            Some(vec![record(5, 2)])
        );
    }

    #[test]
    fn answers_for_part_of_a_record_decide_it_together() {
        let mut index = index();
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(0, 4), &mut waiter);
        // The first half is answered; the record is still going for the rest.
        answer(&mut index, record(0, 2), Outcome::Done, frames(0, 2))
            .expect("pages 0..2 are filling");
        assert_eq!(
            wait_pages(index.start_read(range(0, 4), &mut waiter)),
            Some(vec![])
        );
        // The second half fails: the reader that waited for the record sees it.
        answer(&mut index, record(2, 2), EIO, Vec::new()).expect("pages 2..4 are filling");
        assert_eq!(index.start_read(range(0, 4), &mut waiter), Read::Failed);
        // And when the queue goes, every outstanding record fails.
        let mut late = Attempt::new();
        let _ = index.start_read(range(6, 3), &mut late);
        assert_eq!(index.fail_all(), 3);
        assert_eq!(index.start_read(range(6, 3), &mut late), Read::Failed);
    }

    #[test]
    fn a_failure_survives_truncation_and_regrowth() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(64 * PAGE_SIZE);
        let mut waiter = Attempt::new();
        let _ = index.start_read(range(40, 1), &mut waiter);
        answer(&mut index, record(40, 1), EIO, Vec::new()).expect("page 40 is filling");
        // The failed page is forgotten at once: the waiter holds the failure, not the index.
        assert!(index.pages.is_empty());
        // Cut below the page and grown back before the waiter runs again.
        index.set_size(3 * PAGE_SIZE);
        index.set_size(64 * PAGE_SIZE);
        assert_eq!(index.start_read(range(40, 1), &mut waiter), Read::Failed);
        // A new reader fills the page again.
        assert_eq!(
            wait_pages(index.start_read(range(40, 1), &mut Attempt::new())),
            Some(vec![record(40, 1)])
        );
    }

    #[test]
    fn failed_and_stale_fills_leave_nothing_behind() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(64 * PAGE_SIZE);
        // A stale fill past a new end, answered with a failure, and many failed fills.
        let mut stale = Attempt::new();
        let _ = index.start_read(range(50, 1), &mut stale);
        index.set_size(3 * PAGE_SIZE);
        answer(&mut index, record(50, 1), EIO, Vec::new()).expect("page 50 is filling");
        assert!(index.pages.is_empty());
        // A stale fill is not a failure for its reader, which goes on (the kernel would cut
        // its read at the new end; here it reads page 0 instead).
        assert_eq!(
            wait_pages(index.start_read(range(0, 1), &mut stale)),
            Some(vec![record(0, 1)])
        );
        answer(&mut index, record(0, 1), Outcome::Done, frames(0, 1)).expect("page 0 is filling");
        for round in 0..100 {
            index.set_size(64 * PAGE_SIZE);
            let page = 10 + round % 40;
            let _ = index.start_read(range(page, 1), &mut Attempt::new());
            answer(&mut index, record(page, 1), EIO, Vec::new()).expect("filling");
            index.set_size(3 * PAGE_SIZE);
        }
        assert_eq!(index.pages.keys().copied().collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn invalidating_drops_cached_pages() {
        let mut index = index();
        let _ = index.start_read(range(0, 4), &mut Attempt::new());
        answer(&mut index, record(0, 4), Outcome::Done, frames(0, 4))
            .expect("pages 0..4 are filling");
        assert_eq!(
            index.invalidate(range(1, 2)),
            Invalidated {
                dropped: 2,
                stale: 0
            }
        );
        assert_eq!(index.resident(), 2);
        assert_eq!(index.state(1), PageState::Absent);
        // The next read of them fills them again; the pages left alone still hit.
        assert_eq!(
            index.start_read(range(0, 1), &mut Attempt::new()),
            Read::Ready
        );
        assert_eq!(
            wait_pages(index.start_read(range(1, 2), &mut Attempt::new())),
            Some(vec![record(1, 2)])
        );
        // Absent pages and pages past the range are not counted.
        assert_eq!(index.invalidate(range(5, 5)), Invalidated::default());
    }

    #[test]
    fn a_fill_in_flight_over_invalidated_pages_is_not_taken() {
        let mut index = index();
        let mut reader = Attempt::new();
        let _ = index.start_read(range(0, 3), &mut reader);
        assert_eq!(
            index.invalidate(range(1, 1)),
            Invalidated {
                dropped: 0,
                stale: 1
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
            wait_pages(index.start_read(range(0, 3), &mut reader)),
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
        let _ = index.start_read(range(4, 1), &mut reader);
        let _ = index.invalidate(range(4, 1));
        answer(&mut index, record(4, 1), EIO, Vec::new()).expect("page 4 is filling");
        assert_eq!(index.state(4), PageState::Absent);
        assert_eq!(
            wait_pages(index.start_read(range(4, 1), &mut reader)),
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
