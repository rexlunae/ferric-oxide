//! The file system's side of writeback (with the `alloc` feature): which of its objects have
//! dirty pages and since when, and the pass that writes one object's dirty pages back.
//!
//! The kernel keeps the dirty pages; the file system decides when they go to the file. It
//! learns that an object has dirty pages from a [`crate::abi::Record::Writeback`], notes it
//! in [`DirtyObjects`] with the time, and writes the object back with [`flush`]:
//!
//! - on `fsync` of the file, before it returns;
//! - before it reads or changes the file itself (a write it serves, a truncation, a mapping)
//!   and before it closes the object, so it never works from bytes older than the cache's;
//! - once the object has been dirty for [`DEFAULT_AGE_MS`], oldest first
//!   ([`DirtyObjects::due`]);
//! - at once, every object oldest first, on an urgent record: the queue is past its dirty
//!   limit ([`DEFAULT_MAX_DIRTY_PAGES`]).
//!
//! A run record ([`crate::abi::WritebackReason::Run`]) asks for less: a sequential writer has
//! dirtied a run of pages, and [`flush_run`] writes that one run back at once, from the
//! record's first page, while the writer goes on filling the cache. The kernel posts the next
//! once the writer has dirtied another run, so writeback of a large write overlaps the write
//! and `fsync` finds little left; the object stays on [`DirtyObjects`] for the rest.
//!
//! A [`flush`] pass walks the object from page 0 in page order, one run of dirty pages at a
//! time ([`crate::abi::Request::TakeDirty`]), each from the end of the one before, and writes
//! each run before it takes the next. It ends when no dirty page is left ahead of it, so it
//! writes every page that was dirty when it started and is not kept going by a writer behind
//! it: what that writer dirtied is left for the next pass, and the pass reports it
//! ([`Flushed::dirty_left`]) so the object stays on the list. A run the file system could not
//! write stays dirty in the kernel, and the pass goes on with the rest and reports the
//! failure, which an `fsync` returns as `EIO`.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::num::NonZeroU16;

use crate::abi::{Outcome, RecordPages};

/// How long an object may stay dirty before the file system writes it back, in
/// milliseconds: 5 seconds, as the design (`io/pagecache-design.md`, §4) has it.
pub const DEFAULT_AGE_MS: u64 = 5_000;

/// The queue's dirty limit a file system gives the kernel by default
/// ([`crate::abi::Request::SetWriteback`]): 8192 pages, 32 MiB.
pub const DEFAULT_MAX_DIRTY_PAGES: u32 = 8192;

/// The pages a [`flush`] pass takes at most at once: 1024, 4 MiB, the longest run the kernel
/// hands out ([`crate::abi::MAX_RECORD_PAGES`]), so a pass makes as few round trips and
/// file-system writes as it can. With 256 pages, a 32 MiB sequential write and its `fsync`
/// were slower through the cache than straight to RedoxFS.
pub const FLUSH_PAGES: u32 = crate::abi::MAX_RECORD_PAGES;

/// Objects with dirty pages, each with the time it became dirty (`T`, any ordered time such
/// as an `Instant` or a count of milliseconds), oldest first.
#[derive(Clone, Debug)]
pub struct DirtyObjects<K, T> {
    since: BTreeMap<K, T>,
    by_age: BTreeSet<(T, K)>,
}

impl<K: Ord + Copy, T: Ord + Copy> Default for DirtyObjects<K, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Copy, T: Ord + Copy> DirtyObjects<K, T> {
    /// No object is dirty.
    pub const fn new() -> Self {
        Self {
            since: BTreeMap::new(),
            by_age: BTreeSet::new(),
        }
    }

    /// The number of dirty objects.
    pub fn len(&self) -> usize {
        self.since.len()
    }

    /// Whether no object is dirty.
    pub fn is_empty(&self) -> bool {
        self.since.is_empty()
    }

    /// When `key` became dirty, if it is.
    pub fn since(&self, key: K) -> Option<T> {
        self.since.get(&key).copied()
    }

    /// `key` has dirty pages as of `at`. An object already dirty keeps its earlier time, so
    /// a file written to all the time is still written back within the age limit.
    pub fn dirtied(&mut self, key: K, at: T) {
        if let alloc::collections::btree_map::Entry::Vacant(entry) = self.since.entry(key) {
            entry.insert(at);
            self.by_age.insert((at, key));
        }
    }

    /// `key` has no dirty page any more (or is gone); returns whether it had.
    pub fn cleaned(&mut self, key: K) -> bool {
        match self.since.remove(&key) {
            Some(at) => {
                self.by_age.remove(&(at, key));
                true
            }
            None => false,
        }
    }

    /// The oldest dirty object and when it became dirty.
    pub fn oldest(&self) -> Option<(K, T)> {
        self.by_age.first().map(|&(at, key)| (key, at))
    }

    /// The objects dirty since `before` or earlier, oldest first: those to write back now,
    /// when `before` is the time an age limit ago.
    pub fn due(&self, before: T) -> Vec<K> {
        self.by_age
            .iter()
            .take_while(|(at, _)| *at <= before)
            .map(|&(_, key)| key)
            .collect()
    }

    /// Every dirty object, oldest first: what an urgent record writes back.
    pub fn all(&self) -> Vec<K> {
        self.by_age.iter().map(|&(_, key)| key).collect()
    }
}

/// What a [`flush`] pass works with: the kernel's object, and the file its pages go to.
pub trait Flush {
    /// Why a call on the object failed.
    type Error;

    /// Takes the next run of dirty pages at or after page `from`
    /// ([`crate::abi::Request::TakeDirty`]), its bytes kept for [`Flush::write`]; `None`
    /// when no page there is dirty.
    ///
    /// # Errors
    ///
    /// The call's error; the pass stops.
    fn take(&mut self, from: u64) -> Result<Option<RecordPages>, Self::Error>;

    /// Writes the run just taken to the file, up to the file's end.
    ///
    /// # Errors
    ///
    /// The errno of a write that failed; the pages stay dirty and the pass goes on.
    fn write(&mut self, pages: RecordPages) -> Result<(), NonZeroU16>;

    /// Tells the kernel how the writeback of `pages` ended
    /// ([`crate::abi::Request::WrittenBack`]), and returns the object's dirty pages left.
    ///
    /// # Errors
    ///
    /// The call's error; the pass stops.
    fn answer(&mut self, pages: RecordPages, outcome: Outcome) -> Result<u64, Self::Error>;
}

/// What a [`flush`] pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Pages written to the file.
    pub pages: u64,
    /// Runs taken, written or not.
    pub runs: u32,
    /// The errno of the first run the file system could not write; those pages are still
    /// dirty.
    pub failed: Option<NonZeroU16>,
    /// The object's dirty pages when the pass ended: written to behind it, or not written.
    pub dirty_left: u64,
}

/// Writes the dirty pages of one object back, in page order, as the module docs say.
///
/// # Errors
///
/// The first error of a call on the object ([`Flush::take`], [`Flush::answer`]): the pass
/// stops there.
pub fn flush<F: Flush>(target: &mut F) -> Result<Flushed, F::Error> {
    let mut done = Flushed::default();
    let mut from = 0;
    while let Some(pages) = flush_one(target, from, &mut done)? {
        // Runs end at a page before `u64::MAX`, the end of the last possible run.
        from = pages.pages().end();
    }
    Ok(done)
}

/// Writes back one run of dirty pages, the next at or after page `from`: what a run record
/// asks for (see the module docs). Nothing is written when no page there is dirty any more.
/// [`Flushed::dirty_left`] is the object's dirty pages after it, so the caller knows whether
/// the object is still dirty; it is 0 when nothing was taken, though pages before `from`
/// may still be dirty.
///
/// # Errors
///
/// The error of a call on the object ([`Flush::take`], [`Flush::answer`]).
pub fn flush_run<F: Flush>(target: &mut F, from: u64) -> Result<Flushed, F::Error> {
    let mut done = Flushed::default();
    flush_one(target, from, &mut done)?;
    Ok(done)
}

/// Takes, writes and answers the next run at or after `from`, adding it to `done`; returns
/// its pages, or `None` when nothing there was dirty.
fn flush_one<F: Flush>(
    target: &mut F,
    from: u64,
    done: &mut Flushed,
) -> Result<Option<RecordPages>, F::Error> {
    let Some(pages) = target.take(from)? else {
        return Ok(None);
    };
    done.runs += 1;
    let outcome = match target.write(pages) {
        Ok(()) => {
            done.pages += u64::from(pages.pages().count());
            Outcome::Done
        }
        Err(errno) => {
            done.failed.get_or_insert(errno);
            Outcome::Failed(errno)
        }
    };
    done.dirty_left = target.answer(pages, outcome)?;
    Ok(Some(pages))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::abi::{PAGE_SIZE, PageRange};
    use crate::object::{Attempt, PageIndex, Read, WritebackError};
    use std::vec;

    #[test]
    fn objects_are_due_oldest_first_and_keep_their_first_time() {
        let mut dirty: DirtyObjects<u32, u64> = DirtyObjects::new();
        assert!(dirty.is_empty());
        dirty.dirtied(7, 100);
        dirty.dirtied(3, 250);
        dirty.dirtied(9, 50);
        // Dirtied again later: still due from its first time.
        dirty.dirtied(7, 900);
        assert_eq!(dirty.len(), 3);
        assert_eq!(dirty.since(7), Some(100));
        assert_eq!(dirty.oldest(), Some((9, 50)));
        assert_eq!(dirty.due(49), []);
        assert_eq!(dirty.due(100), [9, 7]);
        assert_eq!(dirty.all(), [9, 7, 3]);
        assert!(dirty.cleaned(9));
        assert!(!dirty.cleaned(9));
        assert_eq!(dirty.due(1_000), [7, 3]);
        // Clean, then dirty again: a new time.
        assert!(dirty.cleaned(7));
        dirty.dirtied(7, 900);
        assert_eq!(dirty.all(), [3, 7]);
        // Two objects dirtied at the same time are both there.
        dirty.dirtied(1, 250);
        assert_eq!(dirty.due(250), [1, 3]);
    }

    /// An object in the kernel ([`PageIndex`]) and the file it writes back to: page number to
    /// the frame (a value) the file holds there.
    struct Model {
        index: PageIndex<u64>,
        file: BTreeMap<u64, u64>,
        /// The run taken, with its frames: what the file system got from `TakeDirty`.
        taken: Option<Vec<u64>>,
        /// Pages whose write fails.
        failing: BTreeSet<u64>,
        /// Order the pages were written in.
        order: Vec<u64>,
        /// Called after each run is written, before the answer: a writer running meanwhile.
        meanwhile: fn(&mut PageIndex<u64>, u32),
        runs: u32,
    }

    impl Model {
        fn new(pages: u32) -> Self {
            let mut index = PageIndex::new();
            index.set_size(u64::from(pages) * PAGE_SIZE);
            Self {
                index,
                file: BTreeMap::new(),
                taken: None,
                failing: BTreeSet::new(),
                order: Vec::new(),
                meanwhile: |_, _| {},
                runs: 0,
            }
        }
    }

    /// Writes page `page` of `index` whole, as the kernel would, with frame `value`.
    fn dirty(index: &mut PageIndex<u64>, page: u64, value: u64) {
        let plan = index
            .pages_for_write(page * PAGE_SIZE, PAGE_SIZE as usize)
            .expect("inside the file");
        assert_eq!(
            index.start_write(&plan, &mut Attempt::new()).read,
            Read::Ready
        );
        index.write(&plan, vec![value]).expect("ready");
    }

    impl Flush for Model {
        type Error = WritebackError;

        fn take(&mut self, from: u64) -> Result<Option<RecordPages>, WritebackError> {
            let max = 3;
            let run = self.index.start_writeback(from, max)?;
            Ok(run.map(|run| {
                self.taken = Some(run.frames);
                run.pages
            }))
        }

        fn write(&mut self, pages: RecordPages) -> Result<(), NonZeroU16> {
            let frames = self.taken.take().expect("a run was taken");
            let range = pages.pages();
            let failed = (range.first()..range.end()).any(|p| self.failing.contains(&p));
            self.runs += 1;
            (self.meanwhile)(&mut self.index, self.runs);
            if failed {
                return Err(NonZeroU16::new(28).expect("ENOSPC"));
            }
            for (page, frame) in (range.first()..range.end()).zip(frames) {
                self.file.insert(page, frame);
                self.order.push(page);
            }
            Ok(())
        }

        fn answer(&mut self, pages: RecordPages, outcome: Outcome) -> Result<u64, WritebackError> {
            let done = self.index.complete_writeback(pages, outcome)?;
            Ok(done.dirty_left as u64)
        }
    }

    #[test]
    fn a_pass_writes_every_dirty_page_in_order_and_leaves_them_clean() {
        let mut model = Model::new(32);
        for page in [20, 3, 4, 5, 6, 30, 0] {
            dirty(&mut model.index, page, 1000 + page);
        }
        let flushed = flush(&mut model).expect("no call fails");
        assert_eq!(
            flushed,
            Flushed {
                pages: 7,
                // 0, 3..6 (three at most), 6, 20, 30.
                runs: 5,
                failed: None,
                dirty_left: 0,
            }
        );
        assert_eq!(model.order, [0, 3, 4, 5, 6, 20, 30]);
        assert_eq!(model.file.get(&20), Some(&1020));
        assert_eq!(model.index.dirty_pages(), 0);
        // Every page is clean: reclaim may take them all now.
        assert!(
            [0, 3, 4, 5, 6, 20, 30]
                .iter()
                .all(|&p| model.index.evictable(p))
        );
        // Nothing dirty: a pass takes nothing.
        assert_eq!(flush(&mut model), Ok(Flushed::default()));
    }

    #[test]
    fn a_writer_behind_the_pass_does_not_keep_it_going() {
        let mut model = Model::new(64);
        for page in 0..12 {
            dirty(&mut model.index, page, page);
        }
        // After every run, the writer dirties page 0 again, and the page the pass is on.
        model.meanwhile = |index, run| {
            dirty(index, 0, 500 + u64::from(run));
            let on = u64::from(run - 1) * 3;
            dirty(index, on, 600 + u64::from(run));
        };
        let flushed = flush(&mut model).expect("no call fails");
        // Four runs of three pages, then nothing dirty ahead: it stops.
        assert_eq!(flushed.runs, 4);
        assert_eq!(flushed.pages, 12);
        // The file has every page as it was when its run was taken.
        assert_eq!(model.file.get(&0), Some(&0));
        assert_eq!(model.file.get(&3), Some(&3));
        assert_eq!(model.file.get(&9), Some(&9));
        // What was written behind it is still dirty, with the newest bytes, for the next pass.
        assert_eq!(flushed.dirty_left, 4);
        assert_eq!(model.index.frame(0), Some(&504));
        assert_eq!(model.index.frame(9), Some(&604));
        model.meanwhile = |_, _| {};
        let again = flush(&mut model).expect("no call fails");
        assert_eq!((again.pages, again.dirty_left), (4, 0));
        assert_eq!(model.file.get(&0), Some(&504));
    }

    #[test]
    fn a_failed_run_stays_dirty_and_the_pass_goes_on() {
        let mut model = Model::new(16);
        for page in [1, 2, 8, 9] {
            dirty(&mut model.index, page, page);
        }
        model.failing.insert(2);
        let flushed = flush(&mut model).expect("no call fails");
        assert_eq!(flushed.failed, NonZeroU16::new(28));
        assert_eq!((flushed.runs, flushed.pages, flushed.dirty_left), (2, 2, 2));
        assert_eq!(model.order, [8, 9]);
        assert!(!model.index.evictable(1) && !model.index.evictable(2));
        // Once the file system can write again, the next pass writes them.
        model.failing.clear();
        let again = flush(&mut model).expect("no call fails");
        assert_eq!((again.pages, again.failed, again.dirty_left), (2, None, 0));
        assert_eq!(model.order, [8, 9, 1, 2]);
    }

    #[test]
    fn a_run_writes_one_run_from_its_page() {
        let mut model = Model::new(16);
        for page in [1, 5, 6, 7, 8, 12] {
            dirty(&mut model.index, page, 100 + page);
        }
        // From page 4: the next run there, 5..8 (three at most in the model), and no more.
        let flushed = flush_run(&mut model, 4).expect("no call fails");
        assert_eq!(
            flushed,
            Flushed {
                pages: 3,
                runs: 1,
                failed: None,
                dirty_left: 3,
            }
        );
        assert_eq!(model.order, [5, 6, 7]);
        // Past every dirty page: nothing taken or written.
        assert_eq!(flush_run(&mut model, 13), Ok(Flushed::default()));
        // A failed run stays dirty and says why.
        model.failing.insert(8);
        let failed = flush_run(&mut model, 8).expect("no call fails");
        assert_eq!(
            (failed.failed, failed.pages, failed.dirty_left),
            (NonZeroU16::new(28), 0, 3)
        );
    }

    #[test]
    fn a_pass_stops_at_the_first_failed_call() {
        let mut model = Model::new(4);
        dirty(&mut model.index, 1, 1);
        // A run under writeback already (another pass that never answered): take refuses.
        let _ = model.index.start_writeback(0, 1).expect("idle");
        assert_eq!(flush(&mut model), Err(WritebackError::Busy));
        let range = PageRange::new(1, 1).expect("valid");
        assert_eq!(model.index.dirty_pages(), 1);
        assert_eq!(model.index.frames(range), Some(vec![1]));
    }
}
