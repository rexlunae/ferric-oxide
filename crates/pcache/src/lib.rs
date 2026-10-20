//! The interface and policies of the Ferric Oxide page cache (roadmap Phase 2.6, design in
//! `io/pagecache-design.md` in the project files).
//!
//! The kernel keeps a *cache object* for each file a file system chooses to cache: the file's
//! size and the frames holding its pages. A `read` on a file description attached to an object
//! is served from its pages with no IPC; a miss is a *fill* the file system performs, and dirty
//! pages go back to it as *writeback*. The file system owns the file's contents, size,
//! permissions at open, allocation and durability; the kernel owns only the memory and the
//! index.
//!
//! This crate is the one definition the kernel and file systems share, and the kernel's
//! policies, kept here so they are tested on the host:
//!
//! - [`abi`]: the `kernel.pcache` scheme's paths, the calls on queues and objects, and the
//!   fixed-size records the kernel puts on a file system's fill queue.
//! - [`page`]: the state of one cached page and the transitions fills, writes, writeback and
//!   truncation make.
//! - [`readahead`]: how far ahead of a reader the kernel fills, per open file description.
//! - [`copy`]: how the kernel copies a cached page to a reader through a small bounce buffer
//!   on its stack, so the copy is not 4K-aliased and never allocates.
//! - `object` (with the `alloc` feature): one object's page index, which says what a read
//!   needs (fills to queue, fills to wait for) and applies the file system's answers.
//! - `clock` (with the `alloc` feature): the two-list CLOCK replacement that picks pages to
//!   evict under memory pressure.
//! - `rmap` (with the `alloc` feature): an object's reverse map, from a file page to the
//!   mappings that show it, for unmapping on truncation and eviction.
//!
//! Nothing here touches memory or hardware: the kernel feeds these types the facts and acts on
//! their answers.
//!
//! References: Johnson and Shasha, *2Q: A Low Overhead High Performance Buffer Management
//! Replacement Algorithm* (VLDB 1994), and Jiang, Chen and Zhang, *CLOCK-Pro: An Effective
//! Improvement of the CLOCK Replacement* (USENIX ATC 2005), for the two-list CLOCK; POSIX.1-2017
//! for the file semantics the states serve.
//!
//! # Examples
//!
//! ```
//! use ferric_pcache::abi::{Generation, ObjectId, PageRange, Record, RecordPages};
//! use ferric_pcache::page::{Event, PageState};
//! use ferric_pcache::readahead::{Readahead, ReadaheadConfig};
//!
//! // A reader asks for pages 0..4 of a 100-page file; the kernel fills them and reads ahead.
//! let mut ahead = Readahead::new(ReadaheadConfig::new(4, 32).expect("a valid window"));
//! let wanted = PageRange::new(0, 4).expect("non-empty");
//! let extra = ahead.on_read(wanted, 100);
//! assert_eq!(extra, PageRange::new(4, 4));
//!
//! // The fill goes on the file system's queue as a record (at most 1024 pages each).
//! let pages = RecordPages::new(wanted).expect("4 pages fit one record");
//! let fill = Record::Fill {
//!     object: ObjectId::new(7),
//!     generation: Generation::new(1),
//!     pages,
//! };
//! assert_eq!(Record::from_bytes(&fill.to_bytes()), Ok(fill));
//!
//! // Each page moves from absent to filling to up to date.
//! let page = PageState::Absent.apply(Event::FillStart).and_then(|p| p.apply(Event::FillDone));
//! assert_eq!(page, Ok(PageState::Uptodate));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod abi;
#[cfg(feature = "alloc")]
pub mod clock;
pub mod copy;
#[cfg(feature = "alloc")]
pub mod object;
pub mod page;
pub mod readahead;
#[cfg(feature = "alloc")]
pub mod rmap;
