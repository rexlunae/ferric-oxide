//! Writes the kernel serves from an object's pages, their writeback to the file system, and
//! taking clean pages for reclaim.
//!
//! A cached write goes through the index in three steps, as a read does:
//!
//! 1. [`PageIndex::pages_for_write`] turns the byte range into a [`Write`]: its pages, and
//!    which of them it covers only in part ([`Write::edges`]). Only a write that lies
//!    inside the file's size is served here; one that extends the file goes to the file
//!    system, which allocates its blocks.
//! 2. [`PageIndex::start_write`] says whether the write can be applied now
//!    ([`Read::Ready`]), and otherwise starts fills of the edges it needs the old bytes of
//!    and waits for them, and for any page of the write that is being filled
//!    ([`Read::Wait`]); the writer waits and looks again with the same [`Attempt`].
//! 3. The writer copies its bytes into new frames, one per page, with the old bytes of the
//!    edges ([`PageIndex::frame`]) around them, and [`PageIndex::write`] puts them in place:
//!    every page of the write becomes [`PageState::Dirty`] (or stays under writeback,
//!    redirtied). A frame is never changed in place, so a reader copying from the old one
//!    while the write lands sees the old bytes or the new, never a mix inside a page.
//!
//! Writeback is pulled by the file system, one run at a time: [`PageIndex::start_writeback`]
//! takes the next run of dirty pages at or after a page, in page order, marks it under
//! writeback and hands out its frames; [`PageIndex::complete_writeback`] applies the answer.
//! A file system that walks the file from page 0, each time from the end of the run before,
//! writes every page that was dirty when it started, in order, and stops: pages written to
//! behind it are left for the next pass, so a busy writer cannot keep it going for good.
//!
//! [`PageIndex::evict`] takes a clean page's frame for reclaim. It refuses a dirty page, one
//! under writeback (which can still fail) and one being filled, so reclaim never drops data
//! the file system does not have: [`PageIndex::evictable`] is the test to give
//! [`crate::clock::Clock::evict`].

use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use super::{Attempt, Entry, PageIndex, Read, ReadStep};
use crate::abi::{MAX_RECORD_PAGES, Outcome, PAGE_SIZE, PageRange, RecordPages};
use crate::page::{Event, PageState, TransitionError};

/// The page size as a `usize`, for offsets in a page.
const PAGE: usize = PAGE_SIZE as usize;

/// A write the kernel serves from an object's pages ([`PageIndex::pages_for_write`]): bytes
/// `offset..offset + len` of the file, all inside its size, at most [`MAX_RECORD_PAGES`]
/// pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    offset: u64,
    len: usize,
    pages: PageRange,
    /// The file's size when the write was planned.
    size: u64,
}

impl Write {
    /// The pages the write touches.
    pub const fn pages(&self) -> PageRange {
        self.pages
    }

    /// The number of bytes it writes.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether it writes nothing (never, for a [`Write`] the index planned).
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes of `page` the write covers, as offsets in the page, and where they start
    /// in the write's buffer; `None` for a page the write does not touch.
    pub fn covered(&self, page: u64) -> Option<Covered> {
        if !self.pages.contains(page) {
            return None;
        }
        let page_start = page.checked_mul(PAGE_SIZE)?;
        let end = self.offset.checked_add(self.len as u64)?;
        let start = self.offset.max(page_start);
        let stop = end.min(page_start.checked_add(PAGE_SIZE)?);
        // Both are inside the page, and `start - offset` is inside the buffer.
        let in_page =
            usize::try_from(start - page_start).ok()?..usize::try_from(stop - page_start).ok()?;
        let in_buf = usize::try_from(start - self.offset).ok()?;
        Some(Covered { in_page, in_buf })
    }

    /// Whether the write needs the old bytes of `page`: it covers only part of the bytes of
    /// the page that are inside the file. (The bytes past the end of the file are zero in
    /// every frame, a new one included.)
    pub fn needs_old(&self, page: u64) -> bool {
        let Some(covered) = self.covered(page) else {
            return false;
        };
        let in_file = self
            .size
            .saturating_sub(page.saturating_mul(PAGE_SIZE))
            .min(PAGE_SIZE);
        // At most a page.
        let in_file = usize::try_from(in_file).unwrap_or(PAGE);
        covered.in_page.start > 0 || covered.in_page.end < in_file
    }

    /// The pages whose old bytes the write needs ([`Write::needs_old`]): at most its first
    /// and its last, in order.
    pub fn edges(&self) -> impl Iterator<Item = u64> + '_ {
        let first = self.pages.first();
        let last = self.pages.end() - 1;
        let last = (last != first).then_some(last);
        [Some(first), last]
            .into_iter()
            .flatten()
            .filter(|&page| self.needs_old(page))
    }
}

/// Where a [`Write`]'s bytes go in one of its pages ([`Write::covered`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Covered {
    /// The bytes of the page written, as offsets in the page.
    pub in_page: Range<usize>,
    /// Where they start in the write's buffer.
    pub in_buf: usize,
}

/// What [`PageIndex::write`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    /// Pages that were clean and are dirty now.
    pub dirtied: u32,
    /// The object had no dirty page before: the file system should hear of it.
    pub first: bool,
}

/// A run of dirty pages [`PageIndex::start_writeback`] put under writeback: its pages, and
/// their frames in order, whose bytes go to the file system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakenRun<F> {
    /// The pages.
    pub pages: RecordPages,
    /// Their frames, one per page.
    pub frames: Vec<F>,
}

/// What [`PageIndex::complete_writeback`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WrittenBack {
    /// Pages that are clean now.
    pub cleaned: u32,
    /// Pages a truncation cut while they were under writeback, dropped now.
    pub dropped: u32,
    /// Of those, pages whose writeback failed: their data, which the truncation kept below
    /// the new end, is lost. A file system that truncates only after writing a file back
    /// never has any.
    pub lost: u32,
    /// The object's dirty pages left ([`PageIndex::dirty_pages`]).
    pub dirty_left: usize,
}

/// Why [`PageIndex::write`] did not apply a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteError {
    /// It came with the wrong number of frames.
    Frames {
        /// The pages it writes.
        expected: u32,
        /// The frames it has.
        got: usize,
    },
    /// The file's size changed since it was planned: plan it again.
    Resized,
    /// A page it needs is being filled or is not there: [`PageIndex::start_write`] again.
    NotReady,
    /// A page could not take the write (a bug: the pages were checked first).
    Transition(TransitionError),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frames { expected, got } => {
                write!(f, "a write of {expected} pages came with {got} frames")
            }
            Self::Resized => f.write_str("the file's size changed under a write"),
            Self::NotReady => f.write_str("a page of the write is not ready"),
            Self::Transition(_) => f.write_str("a page cannot take the write"),
        }
    }
}

impl core::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Transition(err) => Some(err),
            _ => None,
        }
    }
}

/// Why a writeback could not start or end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WritebackError {
    /// A run of the object is under writeback already (the kernel's `EBUSY`).
    Busy,
    /// There is no room for a page (the kernel's `EINVAL`).
    NoRoom,
    /// The answer is not for the run under writeback (the kernel's `EINVAL`).
    NotInFlight,
    /// A dirty page has no frame (a bug: every dirty page has one).
    Unframed {
        /// The page.
        page: u64,
    },
}

impl fmt::Display for WritebackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("a writeback of the object is in flight already"),
            Self::NoRoom => f.write_str("a writeback needs room for at least one page"),
            Self::NotInFlight => f.write_str("the answer is not for the writeback in flight"),
            Self::Unframed { page } => write!(f, "dirty page {page} has no frame"),
        }
    }
}

impl core::error::Error for WritebackError {}

/// Why [`PageIndex::evict`] did not take a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvictError {
    /// The page.
    pub page: u64,
    /// Its state: absent (nothing to take), being filled, dirty or under writeback.
    pub state: PageState,
}

impl fmt::Display for EvictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "page {} is {:?}, not evictable", self.page, self.state)
    }
}

impl core::error::Error for EvictError {}

impl<F> PageIndex<F> {
    /// The [`Write`] of bytes `offset..offset + len`, when the kernel may serve it from the
    /// cache: `len` is not 0, the bytes are inside the file's size, and they span at most
    /// [`MAX_RECORD_PAGES`] pages. `None` sends the write to the file system.
    pub fn pages_for_write(&self, offset: u64, len: usize) -> Option<Write> {
        let end = offset.checked_add(u64::try_from(len).ok()?)?;
        if len == 0 || end > self.size {
            return None;
        }
        let pages = PageRange::from_bounds(offset / PAGE_SIZE, end.div_ceil(PAGE_SIZE))?;
        if pages.count() > MAX_RECORD_PAGES {
            return None;
        }
        Some(Write {
            offset,
            len,
            pages,
            size: self.size,
        })
    }

    /// Looks at a write's pages: see the module docs. Absent edges become
    /// [`PageState::Filling`], each in a record of its own, and `attempt` keeps the outcome
    /// of every fill it now waits for: those, and the fills of other pages of the write. Each
    /// fill in the step's `fills` must be queued or failed, as for a read. The step's `read`
    /// is [`Read::Ready`] when [`PageIndex::write`] can take the write now, [`Read::Wait`]
    /// when the writer waits and looks again, [`Read::Failed`] when a fill it waited for
    /// failed (the write fails with `EIO`), and [`Read::OutOfFillIds`] when an edge needs a
    /// fill and none can start.
    pub fn start_write(&mut self, write: &Write, attempt: &mut Attempt) -> ReadStep {
        if attempt.take_failure() {
            return ReadStep {
                read: Read::Failed,
                fills: Vec::new(),
            };
        }
        let absent: Vec<u64> = write
            .edges()
            .filter(|&page| self.state(page) == PageState::Absent)
            .collect();
        if absent.len() as u64 > self.fill_ids_left() {
            return ReadStep {
                read: Read::OutOfFillIds,
                fills: Vec::new(),
            };
        }
        let fills: Vec<_> = absent
            .into_iter()
            .filter_map(|page| RecordPages::from_parts(page, 1))
            .filter_map(|pages| self.start_fill(pages))
            .collect();
        let pages = write.pages;
        for (_, entry) in self.pages.range(pages.first()..pages.end()) {
            if entry.state == PageState::Filling
                && let Some(fill) = &entry.fill
            {
                attempt.wait_for(fill);
            }
        }
        // Even a ready write is no longer a first look.
        attempt.waiting.get_or_insert_with(Vec::new);
        let read = if fills.is_empty() && self.write_ready(write) {
            Read::Ready
        } else {
            Read::Wait
        };
        ReadStep { read, fills }
    }

    /// Whether [`PageIndex::write`] can take `write` now: the size is the one it was planned
    /// with, no page of it is waited for, and every edge holds the file's data.
    fn write_ready(&self, write: &Write) -> bool {
        write.size == self.size
            && !self.waits(write.pages)
            && write.edges().all(|page| self.frame(page).is_some())
    }

    /// Applies `write`, whose bytes the caller has copied into `frames` (one per page of
    /// the write, in order, each with the old bytes of its page around the write's): every
    /// page of the write takes its new frame and becomes dirty. Nothing changes on an error.
    ///
    /// # Errors
    ///
    /// [`WriteError::Frames`] for the wrong number of frames, [`WriteError::Resized`] when the
    /// file's size changed since `write` was planned, and [`WriteError::NotReady`] when a page
    /// of it is waited for or an edge is not there ([`PageIndex::start_write`] again).
    pub fn write(&mut self, write: &Write, frames: Vec<F>) -> Result<Written, WriteError> {
        let pages = write.pages;
        if frames.len() != pages.count() as usize {
            return Err(WriteError::Frames {
                expected: pages.count(),
                got: frames.len(),
            });
        }
        if write.size != self.size {
            return Err(WriteError::Resized);
        }
        if !self.write_ready(write) {
            return Err(WriteError::NotReady);
        }
        // Every transition is checked before any page changes.
        let next: Vec<PageState> = (pages.first()..pages.end())
            .map(|page| match self.state(page) {
                // A whole page, or one whose other bytes are past the end: a new frame holds
                // all of its data.
                PageState::Absent => Ok(PageState::Dirty),
                state => state.apply(Event::Write).map_err(WriteError::Transition),
            })
            .collect::<Result<_, _>>()?;
        let first = self.dirty == 0;
        let mut dirtied = 0;
        for ((page, frame), next) in (pages.first()..pages.end()).zip(frames).zip(next) {
            let entry = self.pages.entry(page).or_insert_with(Entry::absent);
            if !entry.state.dirty() {
                dirtied += 1;
            }
            entry.state = next;
            entry.frame = Some(frame);
        }
        self.dirty += dirtied as usize;
        Ok(Written {
            dirtied,
            first: first && dirtied > 0,
        })
    }

    /// Puts the next run of dirty pages at or after page `from` under writeback: at most
    /// `max` pages (and at most [`MAX_RECORD_PAGES`]), in page order. Returns them and their
    /// frames, or `None` when no page at or after `from` is dirty. Pages under writeback
    /// still read, and a write to one keeps it dirty after the writeback.
    ///
    /// # Errors
    ///
    /// [`WritebackError::Busy`] while a run is under writeback, [`WritebackError::NoRoom`] for
    /// a `max` of 0, and [`WritebackError::Unframed`] for a dirty page without a frame (a
    /// bug); nothing changes.
    pub fn start_writeback(
        &mut self,
        from: u64,
        max: u32,
    ) -> Result<Option<TakenRun<F>>, WritebackError>
    where
        F: Clone,
    {
        if self.writeback.is_some() {
            return Err(WritebackError::Busy);
        }
        let max = max.min(MAX_RECORD_PAGES);
        if max == 0 {
            return Err(WritebackError::NoRoom);
        }
        let mut run: Option<(u64, u32)> = None;
        for (&page, entry) in self.pages.range(from..) {
            match (entry.state == PageState::Dirty, run) {
                (true, None) => run = Some((page, 1)),
                (true, Some((first, count))) if first + u64::from(count) == page && count < max => {
                    run = Some((first, count + 1));
                }
                (false, None) => {}
                _ => break,
            }
        }
        let Some(pages) = run.and_then(|(first, count)| RecordPages::from_parts(first, count))
        else {
            return Ok(None);
        };
        let range = pages.pages();
        let frames = (range.first()..range.end())
            .map(|page| {
                self.pages
                    .get(&page)
                    .and_then(|entry| entry.frame.clone())
                    .ok_or(WritebackError::Unframed { page })
            })
            .collect::<Result<Vec<F>, _>>()?;
        for page in range.first()..range.end() {
            if let Some(entry) = self.pages.get_mut(&page) {
                entry.state = PageState::Writeback { redirtied: false };
            }
        }
        self.writeback = Some(pages);
        Ok(Some(TakenRun { pages, frames }))
    }

    /// Applies the file system's answer to the writeback of `pages`: done, its pages are
    /// clean unless they were written to meanwhile; failed, they stay dirty for a later
    /// writeback. Pages a truncation cut meanwhile are dropped.
    ///
    /// # Errors
    ///
    /// [`WritebackError::NotInFlight`] when `pages` is not the run under writeback; nothing
    /// changes.
    pub fn complete_writeback(
        &mut self,
        pages: RecordPages,
        outcome: Outcome,
    ) -> Result<WrittenBack, WritebackError> {
        if self.writeback != Some(pages) {
            return Err(WritebackError::NotInFlight);
        }
        self.writeback = None;
        let event = match outcome {
            Outcome::Done => Event::WritebackDone,
            Outcome::Failed(_) => Event::WritebackFailed,
        };
        let mut done = WrittenBack::default();
        let range = pages.pages();
        for page in range.first()..range.end() {
            let Some(entry) = self.pages.get_mut(&page) else {
                continue;
            };
            let Ok(next) = entry.state.apply(event) else {
                continue;
            };
            if entry.stale {
                done.dropped += 1;
                if next.dirty() {
                    done.lost += 1;
                }
                // Still under writeback, so counted dirty until it goes.
                self.drop_page(page);
                continue;
            }
            entry.state = next;
            if !next.dirty() {
                self.dirty = self.dirty.saturating_sub(1);
                done.cleaned += 1;
            }
        }
        done.dirty_left = self.dirty;
        Ok(done)
    }

    /// Whether reclaim may take `page`'s frame now: it holds the file's data, and the file
    /// system has it too ([`PageState::Uptodate`]).
    pub fn evictable(&self, page: u64) -> bool {
        self.pages
            .get(&page)
            .is_some_and(|entry| entry.state == PageState::Uptodate && entry.frame.is_some())
    }

    /// Takes `page`'s frame for reclaim: the page is absent afterwards, and the next read of
    /// it fills it again.
    ///
    /// # Errors
    ///
    /// [`EvictError`] when the page is not [`PageIndex::evictable`]: absent, being filled,
    /// dirty or under writeback. Nothing changes.
    pub fn evict(&mut self, page: u64) -> Result<F, EvictError> {
        let state = self.state(page);
        let refused = EvictError { page, state };
        if state.apply(Event::Evict).is_err() || !self.evictable(page) {
            return Err(refused);
        }
        self.pages
            .remove(&page)
            .and_then(|entry| entry.frame)
            .ok_or(refused)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::clock::Clock;
    use core::num::NonZeroU16;
    use std::string::ToString;
    use std::vec;

    const EIO: Outcome = Outcome::Failed(NonZeroU16::new(5).expect("non-zero"));

    fn range(first: u64, count: u32) -> PageRange {
        PageRange::new(first, count).expect("a valid range")
    }

    fn record(first: u64, count: u32) -> RecordPages {
        RecordPages::from_parts(first, count).expect("a valid record")
    }

    /// A file of `pages` whole pages, all filled with frames named after their pages.
    fn filled(pages: u32) -> PageIndex<u64> {
        let mut index = PageIndex::new();
        index.set_size(u64::from(pages) * PAGE_SIZE);
        let step = index.start_read(range(0, pages), &mut Attempt::new(), None);
        for fill in step.fills {
            let r = fill.pages.pages();
            let frames = (r.first()..r.end()).collect();
            index
                .complete_fill(fill.id, fill.pages, Outcome::Done, frames)
                .expect("filling");
        }
        index
    }

    /// Writes bytes `offset..offset + len` as the kernel would, with frames `100 + page`;
    /// panics unless the write is ready.
    fn write(index: &mut PageIndex<u64>, offset: u64, len: usize) -> Written {
        let plan = index.pages_for_write(offset, len).expect("inside the file");
        let step = index.start_write(&plan, &mut Attempt::new());
        assert_eq!(step.read, Read::Ready, "{plan:?}");
        let pages = plan.pages();
        let frames = (pages.first()..pages.end()).map(|p| 100 + p).collect();
        index.write(&plan, frames).expect("ready")
    }

    fn dirty(index: &PageIndex<u64>) -> Vec<u64> {
        let pages = index.pages.iter();
        pages
            .filter(|(_, entry)| entry.state == PageState::Dirty)
            .map(|(&page, _)| page)
            .collect()
    }

    #[test]
    fn only_writes_inside_the_file_are_served() {
        let index = filled(4);
        assert!(index.pages_for_write(0, 1).is_some());
        assert!(index.pages_for_write(4 * PAGE_SIZE - 1, 1).is_some());
        // Past the end, straddling it, empty, overflowing.
        assert_eq!(index.pages_for_write(4 * PAGE_SIZE, 1), None);
        assert_eq!(index.pages_for_write(4 * PAGE_SIZE - 1, 2), None);
        assert_eq!(index.pages_for_write(0, 0), None);
        assert_eq!(index.pages_for_write(u64::MAX, 1), None);
        // More than a record's worth of pages.
        let mut big: PageIndex<u64> = PageIndex::new();
        big.set_size(2048 * PAGE_SIZE);
        assert!(big.pages_for_write(0, 1024 * PAGE).is_some());
        assert_eq!(big.pages_for_write(1, 1024 * PAGE), None);
    }

    #[test]
    fn a_write_covers_its_bytes_and_needs_the_old_ones_around_them() {
        let mut index: PageIndex<u64> = PageIndex::new();
        // Pages 0..3 and 100 bytes of page 3.
        index.set_size(3 * PAGE_SIZE + 100);
        let plan = index.pages_for_write(100, 2 * PAGE).expect("inside");
        assert_eq!(plan.pages(), range(0, 3));
        assert_eq!(
            plan.covered(0),
            Some(Covered {
                in_page: 100..PAGE,
                in_buf: 0
            })
        );
        assert_eq!(
            plan.covered(1),
            Some(Covered {
                in_page: 0..PAGE,
                in_buf: PAGE - 100
            })
        );
        assert_eq!(
            plan.covered(2),
            Some(Covered {
                in_page: 0..100,
                in_buf: 2 * PAGE - 100
            })
        );
        assert_eq!(plan.covered(3), None);
        assert_eq!(plan.edges().collect::<Vec<_>>(), [0, 2]);
        // Whole pages need nothing, and nor does a last page written up to the end.
        let plan = index
            .pages_for_write(PAGE_SIZE, 2 * PAGE + 100)
            .expect("inside");
        assert_eq!(plan.edges().count(), 0);
        // One page, partly: one edge.
        let plan = index.pages_for_write(5, 10).expect("inside");
        assert_eq!(plan.edges().collect::<Vec<_>>(), [0]);
        assert_eq!(plan.len(), 10);
        assert!(!plan.is_empty());
    }

    #[test]
    fn a_write_fills_only_the_edges_it_needs_then_dirties_every_page() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(8 * PAGE_SIZE);
        let plan = index.pages_for_write(10, 3 * PAGE).expect("inside");
        let mut writer = Attempt::new();
        let step = index.start_write(&plan, &mut writer);
        assert_eq!(step.read, Read::Wait);
        let fills: Vec<RecordPages> = step.fills.iter().map(|f| f.pages).collect();
        // Pages 1 and 2 are covered whole and are not read.
        assert_eq!(fills, [record(0, 1), record(3, 1)]);
        assert_eq!(index.state(1), PageState::Absent);
        assert_eq!(index.write(&plan, vec![0; 4]), Err(WriteError::NotReady));
        for fill in step.fills {
            let page = fill.pages.pages().first();
            index
                .complete_fill(fill.id, fill.pages, Outcome::Done, vec![page])
                .expect("filling");
        }
        assert_eq!(index.start_write(&plan, &mut writer).read, Read::Ready);
        // The kernel copies the edges' old bytes into the new frames.
        assert_eq!(index.frame(0), Some(&0));
        assert_eq!(index.frame(3), Some(&3));
        assert_eq!(
            index.write(&plan, vec![10, 11, 12]),
            Err(WriteError::Frames {
                expected: 4,
                got: 3
            })
        );
        assert_eq!(
            index.write(&plan, vec![10, 11, 12, 13]),
            Ok(Written {
                dirtied: 4,
                first: true
            })
        );
        assert_eq!(dirty(&index), [0, 1, 2, 3]);
        assert_eq!(index.dirty_pages(), 4);
        assert_eq!(index.frames(range(0, 4)), Some(vec![10, 11, 12, 13]));
        // Writing them again dirties nothing new, and is not the first.
        assert_eq!(write(&mut index, 0, PAGE), Written::default());
    }

    #[test]
    fn a_write_waits_for_a_page_being_filled_and_fails_with_its_fill() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(4 * PAGE_SIZE);
        let _ = index.start_read(range(1, 1), &mut Attempt::new(), None);
        let plan = index.pages_for_write(0, 2 * PAGE).expect("inside");
        let mut writer = Attempt::new();
        let step = index.start_write(&plan, &mut writer);
        assert_eq!((step.read, step.fills.len()), (Read::Wait, 0));
        let fill = index
            .pages
            .get(&1)
            .and_then(|e| e.fill.clone())
            .expect("filling");
        index
            .complete_fill(fill.id, record(1, 1), EIO, Vec::new())
            .expect("filling");
        assert_eq!(index.start_write(&plan, &mut writer).read, Read::Failed);
        // A new writer goes ahead: both pages are whole.
        assert_eq!(write(&mut index, 0, 2 * PAGE).dirtied, 2);
    }

    #[test]
    fn a_write_planned_before_a_truncation_is_refused() {
        let mut index = filled(4);
        let plan = index.pages_for_write(0, PAGE).expect("inside");
        index.set_size(2 * PAGE_SIZE);
        assert_eq!(index.write(&plan, vec![100]), Err(WriteError::Resized));
        assert_eq!(index.dirty_pages(), 0);
    }

    #[test]
    fn writeback_takes_runs_in_page_order_one_at_a_time() {
        let mut index = filled(16);
        for page in [9, 2, 5, 6, 7] {
            write(&mut index, page * PAGE_SIZE, PAGE);
        }
        assert_eq!(index.dirty_pages(), 5);
        let run = index.start_writeback(0, 64).expect("idle").expect("dirty");
        assert_eq!((run.pages, run.frames), (record(2, 1), vec![102]));
        // One run at a time.
        assert_eq!(index.start_writeback(3, 64), Err(WritebackError::Busy));
        let done = index
            .complete_writeback(record(2, 1), Outcome::Done)
            .expect("in flight");
        assert_eq!(
            done,
            WrittenBack {
                cleaned: 1,
                dirty_left: 4,
                ..WrittenBack::default()
            }
        );
        // From the end of that run: 5..8, cut at `max`.
        let run = index.start_writeback(3, 2).expect("idle").expect("dirty");
        assert_eq!(run.pages, record(5, 2));
        index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        let run = index.start_writeback(7, 64).expect("idle").expect("dirty");
        assert_eq!(run.pages, record(7, 1));
        index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        let run = index.start_writeback(8, 64).expect("idle").expect("dirty");
        assert_eq!(run.pages, record(9, 1));
        let done = index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        assert_eq!(done.dirty_left, 0);
        assert_eq!(index.start_writeback(10, 64), Ok(None));
        assert_eq!(index.start_writeback(0, 64), Ok(None));
        assert_eq!(index.start_writeback(0, 0), Err(WritebackError::NoRoom));
        assert!(index.evictable(9));
    }

    #[test]
    fn data_written_during_writeback_or_failed_stays_dirty() {
        let mut index = filled(4);
        write(&mut index, 0, 2 * PAGE);
        let run = index.start_writeback(0, 64).expect("idle").expect("dirty");
        assert_eq!(run.frames, [100, 101]);
        // Pages under writeback still read, and take writes.
        assert_eq!(index.frames(range(0, 2)), Some(vec![100, 101]));
        let plan = index.pages_for_write(PAGE_SIZE, PAGE).expect("inside");
        assert_eq!(
            index.start_write(&plan, &mut Attempt::new()).read,
            Read::Ready
        );
        assert_eq!(index.write(&plan, vec![201]), Ok(Written::default()));
        // An answer for other pages is refused and changes nothing.
        assert_eq!(
            index.complete_writeback(record(0, 1), Outcome::Done),
            Err(WritebackError::NotInFlight)
        );
        let done = index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        assert_eq!((done.cleaned, done.dirty_left), (1, 1));
        assert_eq!(index.state(0), PageState::Uptodate);
        assert_eq!(index.state(1), PageState::Dirty);
        assert_eq!(index.frame(1), Some(&201));
        // A failed writeback keeps the data, dirty, for the next pass.
        let run = index.start_writeback(0, 64).expect("idle").expect("dirty");
        assert_eq!(run.pages, record(1, 1));
        let done = index.complete_writeback(run.pages, EIO).expect("in flight");
        assert_eq!((done.cleaned, done.dirty_left), (0, 1));
        assert_eq!(index.state(1), PageState::Dirty);
        // A second answer to the same writeback is refused.
        assert_eq!(
            index.complete_writeback(run.pages, Outcome::Done),
            Err(WritebackError::NotInFlight)
        );
    }

    #[test]
    fn reclaim_never_takes_a_page_the_file_system_does_not_have() {
        let mut index = filled(8);
        write(&mut index, 2 * PAGE_SIZE, 2 * PAGE);
        let run = index.start_writeback(0, 1).expect("idle").expect("dirty");
        assert_eq!(run.pages, record(2, 1));
        index.invalidate(range(6, 1));
        let _ = index.start_read(range(6, 1), &mut Attempt::new(), None);
        // Page 2 under writeback, 3 dirty, 6 being filled.
        let mut clock = Clock::new();
        for page in 0..8 {
            clock.insert(page).expect("new");
        }
        let mut evicted = Vec::new();
        while let Some(page) = clock.evict(|page| index.evictable(page)) {
            evicted.push(index.evict(page).expect("evictable"));
        }
        evicted.sort_unstable();
        assert_eq!(evicted, [0, 1, 4, 5, 7]);
        for page in [2, 3, 6] {
            assert_eq!(
                index.evict(page),
                Err(EvictError {
                    page,
                    state: index.state(page)
                })
            );
        }
        assert_eq!(index.dirty_pages(), 2);
        assert_eq!(index.frames(range(2, 2)), Some(vec![102, 103]));
        // Written back, they are clean and may go.
        index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        let run = index.start_writeback(0, 64).expect("idle").expect("dirty");
        index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        assert_eq!(index.evict(2), Ok(102));
        assert_eq!(index.evict(3), Ok(103));
        assert_eq!(
            index.evict(3),
            Err(EvictError {
                page: 3,
                state: PageState::Absent
            })
        );
    }

    #[test]
    fn a_truncation_drops_dirty_pages_and_cut_pages_under_writeback_when_it_ends() {
        let mut index = filled(8);
        write(&mut index, 0, 8 * PAGE);
        let run = index.start_writeback(4, 2).expect("idle").expect("dirty");
        assert_eq!(run.pages, record(4, 2));
        // Cut inside page 3: pages 3, 6 and 7 go at once, 4 and 5 when their writeback ends.
        index.set_size(3 * PAGE_SIZE + 10);
        assert_eq!(index.dirty_pages(), 5);
        // Grown back, a reader or writer of page 4 waits for it to go.
        index.set_size(8 * PAGE_SIZE);
        let step = index.start_read(range(4, 1), &mut Attempt::new(), None);
        assert_eq!((step.read, step.fills.len()), (Read::Wait, 0));
        assert_eq!(index.frames(range(4, 1)), None);
        let plan = index.pages_for_write(4 * PAGE_SIZE, PAGE).expect("inside");
        assert_eq!(
            index.start_write(&plan, &mut Attempt::new()).read,
            Read::Wait
        );
        let done = index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        assert_eq!(
            done,
            WrittenBack {
                dropped: 2,
                dirty_left: 3,
                ..WrittenBack::default()
            }
        );
        assert_eq!(dirty(&index), [0, 1, 2]);
        assert_eq!(index.state(4), PageState::Absent);
        // The page is written anew from a zeroed frame.
        assert_eq!(write(&mut index, 4 * PAGE_SIZE, PAGE).dirtied, 1);
        // A failed writeback of a cut page loses what was below the cut.
        let mut index = filled(2);
        write(&mut index, 0, 2 * PAGE);
        let run = index.start_writeback(1, 64).expect("idle").expect("dirty");
        index.set_size(PAGE_SIZE + 1);
        let done = index.complete_writeback(run.pages, EIO).expect("in flight");
        assert_eq!((done.dropped, done.lost, done.dirty_left), (1, 1, 1));
    }

    #[test]
    fn invalidating_dirty_pages_counts_them() {
        let mut index = filled(4);
        write(&mut index, 0, PAGE);
        let run = index.start_writeback(0, 64).expect("idle").expect("dirty");
        write(&mut index, PAGE_SIZE, PAGE);
        let gone = index.invalidate(range(0, 4));
        // Page 0 is under writeback and stays; 1 was dirty; 2 and 3 were clean.
        assert_eq!((gone.dropped, gone.dirty, gone.stale), (3, 1, 0));
        assert_eq!(index.dirty_pages(), 1);
        index
            .complete_writeback(run.pages, Outcome::Done)
            .expect("in flight");
        assert_eq!(index.dirty_pages(), 0);
    }

    #[test]
    fn errors_describe_themselves() {
        let transition = PageState::Filling
            .apply(Event::Write)
            .expect_err("a filling page takes no write");
        let messages = [
            WriteError::Frames {
                expected: 2,
                got: 1,
            }
            .to_string(),
            WriteError::Resized.to_string(),
            WriteError::NotReady.to_string(),
            WriteError::Transition(transition).to_string(),
            WritebackError::Busy.to_string(),
            WritebackError::NoRoom.to_string(),
            WritebackError::NotInFlight.to_string(),
            WritebackError::Unframed { page: 3 }.to_string(),
            EvictError {
                page: 1,
                state: PageState::Dirty,
            }
            .to_string(),
        ];
        for (i, message) in messages.iter().enumerate() {
            assert!(!message.is_empty());
            assert!(!messages[..i].contains(message), "{message} is used twice");
        }
        assert!(core::error::Error::source(&WriteError::Transition(transition)).is_some());
    }
}
