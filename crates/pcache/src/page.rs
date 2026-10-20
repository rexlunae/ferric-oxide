//! The state of one cached page, and the transitions that change it.
//!
//! ```text
//!            FillStart            FillDone
//!   Absent ───────────▶ Filling ───────────▶ Uptodate ◀─────────────────┐
//!     ▲                    │ FillFailed         │ Write                 │ WritebackDone
//!     └────────────────────┘                    ▼                       │
//!                                             Dirty ──WritebackStart──▶ Writeback
//!                                               ▲                       │  │ Write: redirtied
//!                                               └── WritebackFailed ────┘  │
//!                                               └── WritebackDone, redirtied
//! ```
//!
//! `Discard` (truncation, or a file system that wrote the file itself) and `Evict` (reclaim)
//! take a page back to `Absent`. Neither may lose data or race an operation in progress, so
//! `Evict` takes only clean pages, and neither takes a page that is being filled or written
//! back: the caller waits for that to finish and tries again.
//!
//! Only `Uptodate`, `Dirty` and `Writeback` pages hold the file's data ([`PageState::readable`]);
//! a reader of an `Absent` page starts a fill, and a reader of a `Filling` page waits.

use core::fmt;

/// The state of a cached page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PageState {
    /// No frame, or a frame whose contents are not the file's.
    Absent,
    /// A fill is outstanding; readers wait for it.
    Filling,
    /// The file's data, the same as on the file system.
    Uptodate,
    /// The file's data, changed since it was last written back.
    Dirty,
    /// Being written back. `redirtied` is set when it was written to meanwhile, so it stays
    /// dirty once the writeback is done.
    Writeback {
        /// Written to since the writeback started.
        redirtied: bool,
    },
}

/// Something that happens to a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    /// A fill of the page was put on the queue.
    FillStart,
    /// The file system filled the page.
    FillDone,
    /// The fill failed.
    FillFailed,
    /// The page was written to: through a writable mapping, or by a `write` the kernel served.
    Write,
    /// A writeback of the page was put on the queue.
    WritebackStart,
    /// The file system wrote the page back.
    WritebackDone,
    /// The writeback failed; the page keeps its data and stays dirty.
    WritebackFailed,
    /// The page is no longer the file's: truncated away, or the file system changed the file
    /// itself. Its data, dirty or not, is dropped.
    Discard,
    /// Reclaim takes the page's frame.
    Evict,
}

impl PageState {
    /// The state after `event`.
    ///
    /// # Errors
    ///
    /// [`TransitionError`] when `event` cannot happen in this state: a fill of a page that is
    /// not absent, a write to a page whose data is not there, a writeback of a clean page, a
    /// discard or eviction of a page being filled or written back, an eviction of a dirty
    /// page, or an answer to an operation that is not in progress.
    pub const fn apply(self, event: Event) -> Result<Self, TransitionError> {
        use Event as E;
        use PageState as S;
        let next = match (self, event) {
            (S::Absent, E::FillStart) => S::Filling,
            (S::Filling, E::FillDone) => S::Uptodate,
            (S::Filling, E::FillFailed) => S::Absent,
            (S::Uptodate | S::Dirty, E::Write) => S::Dirty,
            (S::Writeback { .. }, E::Write) => S::Writeback { redirtied: true },
            (S::Dirty, E::WritebackStart) => S::Writeback { redirtied: false },
            (S::Writeback { redirtied: false }, E::WritebackDone) => S::Uptodate,
            (S::Writeback { redirtied: true }, E::WritebackDone) => S::Dirty,
            (S::Writeback { .. }, E::WritebackFailed) => S::Dirty,
            (S::Absent | S::Uptodate | S::Dirty, E::Discard) => S::Absent,
            (S::Absent | S::Uptodate, E::Evict) => S::Absent,
            _ => {
                return Err(TransitionError { from: self, event });
            }
        };
        Ok(next)
    }

    /// Whether the page holds the file's data, so a reader may copy it.
    pub const fn readable(self) -> bool {
        matches!(self, Self::Uptodate | Self::Dirty | Self::Writeback { .. })
    }

    /// Whether the page has data the file system may not have yet (a writeback in progress
    /// can still fail).
    pub const fn dirty(self) -> bool {
        matches!(self, Self::Dirty | Self::Writeback { .. })
    }

    /// Whether reclaim may take the page's frame now.
    pub const fn evictable(self) -> bool {
        matches!(self, Self::Absent | Self::Uptodate)
    }

    /// Whether a fill or writeback is outstanding, so the page must not be dropped yet.
    pub const fn busy(self) -> bool {
        matches!(self, Self::Filling | Self::Writeback { .. })
    }
}

/// An [`Event`] that cannot happen in a [`PageState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionError {
    /// The page's state.
    pub from: PageState,
    /// What was asked.
    pub event: Event,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "page cache: {:?} cannot happen to a {:?} page",
            self.event, self.from
        )
    }
}

impl core::error::Error for TransitionError {}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    const STATES: [PageState; 6] = [
        PageState::Absent,
        PageState::Filling,
        PageState::Uptodate,
        PageState::Dirty,
        PageState::Writeback { redirtied: false },
        PageState::Writeback { redirtied: true },
    ];

    const EVENTS: [Event; 9] = [
        Event::FillStart,
        Event::FillDone,
        Event::FillFailed,
        Event::Write,
        Event::WritebackStart,
        Event::WritebackDone,
        Event::WritebackFailed,
        Event::Discard,
        Event::Evict,
    ];

    fn run(events: &[Event]) -> Result<PageState, TransitionError> {
        events
            .iter()
            .try_fold(PageState::Absent, |state, &event| state.apply(event))
    }

    #[test]
    fn a_fill_makes_a_page_readable() {
        assert_eq!(
            run(&[Event::FillStart, Event::FillDone]),
            Ok(PageState::Uptodate)
        );
        assert_eq!(
            run(&[Event::FillStart, Event::FillFailed]),
            Ok(PageState::Absent)
        );
        assert!(!PageState::Filling.readable());
        assert!(PageState::Uptodate.readable());
    }

    #[test]
    fn a_write_during_writeback_is_not_lost() {
        let start = [
            Event::FillStart,
            Event::FillDone,
            Event::Write,
            Event::WritebackStart,
        ];
        assert_eq!(run(&start), Ok(PageState::Writeback { redirtied: false }));
        let mut redirtied: Vec<Event> = start.to_vec();
        redirtied.extend([Event::Write, Event::WritebackDone]);
        assert_eq!(run(&redirtied), Ok(PageState::Dirty));
        let mut clean: Vec<Event> = start.to_vec();
        clean.push(Event::WritebackDone);
        assert_eq!(run(&clean), Ok(PageState::Uptodate));
        let mut failed: Vec<Event> = start.to_vec();
        failed.push(Event::WritebackFailed);
        assert_eq!(run(&failed), Ok(PageState::Dirty));
    }

    #[test]
    fn dirty_and_busy_pages_are_never_evicted() {
        for state in STATES {
            let evicted = state.apply(Event::Evict);
            assert_eq!(evicted.is_ok(), state.evictable(), "{state:?}");
            assert!(!(state.dirty() && state.evictable()), "{state:?}");
            if state.busy() {
                assert!(state.apply(Event::Discard).is_err(), "{state:?}");
            }
        }
    }

    #[test]
    fn data_is_lost_only_by_discard() {
        // Every transition out of a state with unwritten data keeps it, except a discard.
        for state in STATES {
            for event in EVENTS {
                if let Ok(next) = state.apply(event) {
                    let written = state == PageState::Writeback { redirtied: false }
                        && event == Event::WritebackDone;
                    if state.dirty() && !next.dirty() {
                        assert!(
                            event == Event::Discard || written,
                            "{state:?} --{event:?}--> {next:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn answers_without_an_operation_are_refused() {
        for (state, event) in [
            (PageState::Absent, Event::FillDone),
            (PageState::Uptodate, Event::FillFailed),
            (PageState::Uptodate, Event::WritebackDone),
            (PageState::Dirty, Event::WritebackFailed),
            (PageState::Uptodate, Event::FillStart),
            (PageState::Absent, Event::Write),
            (PageState::Uptodate, Event::WritebackStart),
        ] {
            assert_eq!(
                state.apply(event),
                Err(TransitionError { from: state, event })
            );
        }
    }
}
