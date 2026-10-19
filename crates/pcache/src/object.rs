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
//!    to queue ([`Read::Wait`]). The reader waits and calls it again, as a
//!    [`Attempt::Retry`], until the pages are ready or a fill it waited for failed
//!    ([`Read::Failed`]).
//! 3. [`PageIndex::frames`] hands out the frames of ready pages, for the copy.
//!
//! [`PageIndex::complete_fill`] applies the file system's answer, and
//! [`PageIndex::fail_all`] fails every outstanding fill when the file system goes away. A
//! failed page goes back to [`PageState::Absent`] marked failed: a reader that waited for it
//! gets [`Read::Failed`] (the kernel's `EIO`), and the next new read fills it again.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;

use crate::abi::{Outcome, PAGE_SIZE, PageRange, RecordPages};
use crate::page::{Event, PageState, TransitionError};

/// Whether a reader is asking for its pages for the first time or again after waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// The first look at the pages: a page whose last fill failed is filled again.
    First,
    /// After waiting for fills: a page whose fill failed fails the read.
    Retry,
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
        fills: Vec<RecordPages>,
    },
    /// A fill the reader waited for failed.
    Failed,
}

/// One page of the index.
#[derive(Clone, Debug)]
struct Entry<F> {
    state: PageState,
    /// The frame holding the page, while the page is readable.
    frame: Option<F>,
    /// The last fill failed (the page is `Absent`).
    failed: bool,
}

impl<F> Entry<F> {
    const fn absent() -> Self {
        Self {
            state: PageState::Absent,
            frame: None,
            failed: false,
        }
    }
}

/// The pages of one cache object.
#[derive(Clone, Debug)]
pub struct PageIndex<F> {
    size: u64,
    /// Pages that are not plain absent ones: absent pages are not stored, unless their last
    /// fill failed.
    pages: BTreeMap<u64, Entry<F>>,
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

    /// Sets the file's size. Pages wholly past the new end are dropped, unless a fill of them
    /// is outstanding: those stay until it is answered, and reads never reach them, being cut
    /// at the size. Returns how many pages were dropped.
    pub fn set_size(&mut self, size: u64) -> usize {
        self.size = size;
        let end = self.file_pages();
        let past: Vec<u64> = self
            .pages
            .range(end..)
            .filter(|(_, entry)| !entry.state.busy())
            .map(|(&page, _)| page)
            .collect();
        for page in &past {
            self.pages.remove(page);
        }
        past.len()
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

    /// Looks at `pages` for a read: see [`Read`]. Absent pages (failed ones too, on the
    /// [`Attempt::First`] look) become [`PageState::Filling`], in records of at most
    /// [`crate::abi::MAX_RECORD_PAGES`] pages.
    pub fn start_read(&mut self, pages: PageRange, attempt: Attempt) -> Read {
        if attempt == Attempt::Retry && self.any_failed(pages) {
            return Read::Failed;
        }
        let fills = self.start_fills(pages);
        let waiting = (pages.first()..pages.end()).any(|page| self.state(page).busy());
        if fills.is_empty() && !waiting {
            Read::Ready
        } else {
            Read::Wait { fills }
        }
    }

    /// Starts fills of the absent pages of `pages` that no reader waits for yet (readahead),
    /// and returns their records. Failed pages are filled again.
    pub fn prefetch(&mut self, pages: PageRange) -> Vec<RecordPages> {
        self.start_fills(pages)
    }

    /// Whether a page of `pages` is absent because its last fill failed.
    fn any_failed(&self, pages: PageRange) -> bool {
        self.pages
            .range(pages.first()..pages.end())
            .any(|(_, entry)| entry.failed)
    }

    /// Marks the absent pages of `pages` filling, and returns them as records.
    fn start_fills(&mut self, pages: PageRange) -> Vec<RecordPages> {
        let mut fills = Vec::new();
        let mut run: Option<(u64, u64)> = None;
        for page in pages.first()..pages.end() {
            let entry = self.pages.entry(page).or_insert_with(Entry::absent);
            // Pages being filled or holding data are left as they are.
            if let Ok(next) = entry.state.apply(Event::FillStart) {
                entry.state = next;
                entry.failed = false;
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

    /// Checks that every page of `pages` is being filled, so an answer naming them may be
    /// applied.
    ///
    /// # Errors
    ///
    /// [`FillError::NotFilling`] for the first page that is not.
    pub fn check_filling(&self, pages: RecordPages) -> Result<(), FillError> {
        let range = pages.pages();
        match (range.first()..range.end()).find(|&page| self.state(page) != PageState::Filling) {
            Some(page) => Err(FillError::NotFilling { page }),
            None => Ok(()),
        }
    }

    /// Applies the file system's answer to a fill of `pages`. A [`Outcome::Done`] answer needs
    /// `frames`, one for each page in order, holding its data; a failed one marks the pages
    /// failed and drops any `frames` given.
    ///
    /// # Errors
    ///
    /// [`FillError::NotFilling`] when a page is not being filled (nothing changes), and
    /// [`FillError::Frames`] when a done answer does not have one frame per page.
    pub fn complete_fill(
        &mut self,
        pages: RecordPages,
        outcome: Outcome,
        frames: Vec<F>,
    ) -> Result<(), FillError> {
        self.check_filling(pages)?;
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
                    self.transition(page, Event::FillDone)?;
                    if let Some(entry) = self.pages.get_mut(&page) {
                        entry.frame = Some(frame);
                    }
                }
            }
            Outcome::Failed(_) => {
                for page in range.first()..range.end() {
                    self.fail(page)?;
                }
            }
        }
        Ok(())
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

    /// Applies `FillFailed` to `page` and marks it failed.
    fn fail(&mut self, page: u64) -> Result<(), FillError> {
        self.transition(page, Event::FillFailed)?;
        if let Some(entry) = self.pages.get_mut(&page) {
            entry.failed = true;
            entry.frame = None;
        }
        Ok(())
    }

    /// Applies `event` to `page`.
    fn transition(&mut self, page: u64, event: Event) -> Result<(), FillError> {
        let entry = self.pages.entry(page).or_insert_with(Entry::absent);
        entry.state = entry.state.apply(event).map_err(FillError::Transition)?;
        Ok(())
    }
}

/// Adds the run of pages `first..end`, if any, to `fills`, cut into records.
fn push_run(fills: &mut Vec<RecordPages>, run: Option<(u64, u64)>) {
    if let Some(pages) = run.and_then(|(first, end)| PageRange::from_bounds(first, end)) {
        fills.extend(RecordPages::split(pages));
    }
}

/// Why a fill answer was not applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FillError {
    /// The answer names a page that is not being filled.
    NotFilling {
        /// The first such page.
        page: u64,
    },
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
            Self::NotFilling { page } => write!(f, "page {page} is not being filled"),
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
        assert_eq!(
            index.start_read(range(2, 3), Attempt::First),
            Read::Wait {
                fills: vec![record(2, 3)]
            }
        );
        assert_eq!(index.state(3), PageState::Filling);
        assert_eq!(index.frames(range(2, 3)), None);
        // A second reader of overlapping pages waits for the same fill and starts one only
        // for the page nobody fills.
        assert_eq!(
            index.start_read(range(4, 2), Attempt::First),
            Read::Wait {
                fills: vec![record(5, 1)]
            }
        );
        index
            .complete_fill(record(2, 3), Outcome::Done, frames(2, 3))
            .expect("pages 2..5 are filling");
        assert_eq!(index.start_read(range(2, 3), Attempt::Retry), Read::Ready);
        assert_eq!(index.frames(range(2, 3)), Some(vec![2, 3, 4]));
        assert_eq!(
            index.start_read(range(4, 2), Attempt::Retry),
            Read::Wait { fills: vec![] }
        );
        assert_eq!(index.resident(), 3);
    }

    #[test]
    fn fills_are_cut_into_records_around_present_pages() {
        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(3000 * PAGE_SIZE);
        let Read::Wait { fills } = index.start_read(range(5, 1), Attempt::First) else {
            panic!("page 5 is absent");
        };
        assert_eq!(fills, [record(5, 1)]);
        let Read::Wait { fills } = index.start_read(range(0, 2100), Attempt::First) else {
            panic!("pages are absent");
        };
        assert_eq!(
            fills,
            [
                record(0, 5),
                record(6, MAX_RECORD_PAGES),
                record(6 + 1024, MAX_RECORD_PAGES),
                record(6 + 2048, 46)
            ]
        );
    }

    #[test]
    fn a_failed_fill_fails_its_waiters_and_is_retried_by_new_readers() {
        let mut index = index();
        let _ = index.start_read(range(0, 2), Attempt::First);
        index
            .complete_fill(record(0, 2), EIO, Vec::new())
            .expect("pages 0..2 are filling");
        assert_eq!(index.state(0), PageState::Absent);
        assert_eq!(index.start_read(range(0, 2), Attempt::Retry), Read::Failed);
        // A read that had not waited fills again; its retry then waits, not fails.
        assert_eq!(
            index.start_read(range(1, 1), Attempt::First),
            Read::Wait {
                fills: vec![record(1, 1)]
            }
        );
        assert_eq!(index.start_read(range(0, 1), Attempt::Retry), Read::Failed);
        assert_eq!(
            index.start_read(range(1, 1), Attempt::Retry),
            Read::Wait { fills: vec![] }
        );
    }

    #[test]
    fn answers_must_match_outstanding_fills() {
        let mut index = index();
        assert_eq!(
            index.complete_fill(record(0, 1), Outcome::Done, frames(0, 1)),
            Err(FillError::NotFilling { page: 0 })
        );
        let _ = index.start_read(range(0, 2), Attempt::First);
        // Too many pages, the wrong number of frames, then the right answer once only.
        assert_eq!(
            index.complete_fill(record(0, 3), Outcome::Done, frames(0, 3)),
            Err(FillError::NotFilling { page: 2 })
        );
        assert_eq!(
            index.complete_fill(record(0, 2), Outcome::Done, frames(0, 1)),
            Err(FillError::Frames {
                expected: 2,
                got: 1
            })
        );
        assert_eq!(index.state(0), PageState::Filling);
        assert_eq!(
            index.complete_fill(record(0, 2), Outcome::Done, frames(0, 2)),
            Ok(())
        );
        assert_eq!(
            index.complete_fill(record(0, 2), Outcome::Done, frames(0, 2)),
            Err(FillError::NotFilling { page: 0 })
        );
    }

    #[test]
    fn prefetch_fills_only_absent_pages() {
        let mut index = index();
        let _ = index.start_read(range(0, 2), Attempt::First);
        index
            .complete_fill(record(0, 2), Outcome::Done, frames(0, 2))
            .expect("filling");
        let _ = index.start_read(range(4, 1), Attempt::First);
        assert_eq!(index.prefetch(range(0, 8)), [record(2, 2), record(5, 3)]);
        assert_eq!(index.prefetch(range(0, 8)), []);
    }

    #[test]
    fn closing_the_queue_fails_every_outstanding_fill() {
        let mut index = index();
        let _ = index.start_read(range(0, 3), Attempt::First);
        let _ = index.prefetch(range(5, 2));
        assert_eq!(index.fail_all(), 5);
        assert_eq!(index.start_read(range(0, 1), Attempt::Retry), Read::Failed);
        assert_eq!(index.fail_all(), 0);
    }

    #[test]
    fn shrinking_drops_pages_past_the_end_but_not_busy_ones() {
        let mut index = index();
        let _ = index.start_read(range(0, 10), Attempt::First);
        index
            .complete_fill(record(0, 8), Outcome::Done, frames(0, 8))
            .expect("filling");
        assert_eq!(index.set_size(4 * PAGE_SIZE), 4);
        assert_eq!(index.resident(), 4);
        // Pages 8 and 9 are still being filled; their answer is still taken.
        assert_eq!(index.state(9), PageState::Filling);
        assert_eq!(
            index.complete_fill(record(8, 2), Outcome::Done, frames(8, 2)),
            Ok(())
        );
        assert_eq!(index.pages_for_read(4 * PAGE_SIZE, 10), None);
    }

    #[test]
    fn errors_describe_themselves() {
        let transition = PageState::Absent
            .apply(Event::FillDone)
            .expect_err("absent pages are not filled");
        let messages = [
            FillError::NotFilling { page: 1 }.to_string(),
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
