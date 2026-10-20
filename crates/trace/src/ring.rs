//! A single-producer, single-consumer ring of records.
//!
//! A ring is a power-of-two run of `AtomicU64` data words and three counters: `head` (the next
//! word the producer writes), `tail` (the next word the consumer reads) and `lost` (records the
//! producer dropped because the ring was full). `head` and `tail` only grow; a word's slot is its
//! position modulo the data length, so a record can wrap around the end.
//!
//! The producer writes a record's words with relaxed stores and then publishes them by storing
//! the new `head` with release ordering; it checks for room against `tail` loaded with acquire
//! ordering, so it never overwrites words the consumer has not finished copying. The consumer
//! loads `head` with acquire ordering, copies whole records, and stores the new `tail` with
//! release ordering; [`Ring::peek`] copies without storing `tail`, and [`Ring::commit`] stores it
//! once the copy has been passed on, so a reader that fails to deliver records keeps them.
//!
//! # When the ring is full
//!
//! What the producer does with a record that does not fit is the ring's [`WhenFull`] policy:
//!
//! - [`WhenFull::DropNewest`] (the default) drops the new record and counts it in `lost`. The
//!   consumer is then the only one that stores `tail`.
//! - [`WhenFull::OverwriteOldest`] makes the ring a flight recorder, which always holds the
//!   newest records, as a crash dump needs ([`crash`](crate::crash)): the producer moves `tail`
//!   past the oldest whole records, with a compare-and-swap, until the new one fits, counts
//!   them in `lost`, and only then writes over their words. Readers see the overwrite as `lost`
//!   growing and `tail` moving under them. `lost` is then an upper bound: a record a reader
//!   had copied and checked just before the producer overwrote it is delivered and counted.
//!
//! Either way a reader copies first and checks afterwards: after copying, [`Ring::peek`] (and
//! [`View::peek`]) loads `tail` again, and if it moved past the copy's start, the words before
//! the new `tail` may have been reused under the copy. Positions at or after the new `tail` were
//! not, so the copy keeps the records from there on, when the walk it made lands on that
//! position, and starts over at the new `tail` otherwise. So a copy never holds a torn record,
//! and a reader that falls behind an overwriting producer still gets the newest records instead
//! of retrying for ever. [`Ring::copy_latest`], the crash dump's copy of the newest records,
//! checks the same way, so a dump taken while other CPUs keep writing holds no record the
//! producer overwrote. With [`WhenFull::DropNewest`] and one reader, `tail` never moves under
//! the copy and the check never fires.
//!
//! There must be one producer and one consumer at a time. The kernel guarantees the first by
//! pushing only on the ring's own CPU with interrupts off, and the second with a lock. Breaking
//! the rule corrupts records, which the consumer detects and drops, but it is not undefined
//! behaviour: every access is atomic.
//!
//! # Storage
//!
//! Where the words live is a [`Storage`]:
//!
//! - [`Inline`] keeps the counters in the ring itself and the data in any slice of words
//!   ([`Ring::new`]).
//! - [`Shared`] keeps the counters too in the words, in a header of [`SHARED_HEADER_WORDS`]
//!   words ahead of the data ([`Ring::shared`]). This is the layout the kernel maps read-only
//!   into a reader (`fmap` of `cpu/<n>` in [`abi`](crate::abi)): the reader sees the counters
//!   and the records, and a [`View`] reads them without storing anything. It hands the words it
//!   has consumed back through the kernel, which checks the position with [`Ring::commit_to`]
//!   before storing `tail`. Since the view is not the consumer, another reader can consume
//!   the words it is copying and the producer can reuse them; [`View::peek`] checks `tail`
//!   after each copy and retries a copy it overtook.
//!
//! The header, in words: [`SHARED_MAGIC`] at [`MAGIC_WORD`], [`SHARED_VERSION`] at
//! [`VERSION_WORD`], the data length at [`DATA_WORDS_WORD`], and `head`, `tail` and `lost` at
//! [`HEAD_WORD`], [`TAIL_WORD`] and [`LOST_WORD`], each on a 64-byte line of its own, so the
//! producer's stores to `head` and the consumer's to `tail` do not share a cache line.
//!
//! A producer never trusts the counters it reads back, beyond what keeps its own accesses in
//! bounds: every slot is a position masked to the data length, so a scribbled counter can only
//! cost records (the consumer drops what does not decode), never memory. The kernel goes
//! further: it maps the shared words read-only, so it is their only writer, and the one value a
//! reader passes back, the end of what it consumed, goes through [`Ring::commit_to`], which
//! accepts only a record boundary between the kernel's own `tail` and `head`.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use crate::EventId;
use crate::record::{Header, MAX_RECORD_WORDS, WORD_BYTES};

/// Words, by index: the memory a [`Storage`] is made of.
///
/// Every slice of `AtomicU64` (an array, a `Vec`, a `Box<[AtomicU64]>`, a borrowed slice) is
/// one. Memory that is not one contiguous slice, such as a run of separately allocated pages,
/// implements it by hand.
pub trait Words {
    /// The number of words.
    fn len(&self) -> usize;

    /// Whether there are no words.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Word `index`.
    ///
    /// # Panics
    ///
    /// May panic when `index` is not below [`Words::len`]; a ring never asks for one that is
    /// not.
    fn word(&self, index: usize) -> &AtomicU64;
}

impl<T: AsRef<[AtomicU64]>> Words for T {
    fn len(&self) -> usize {
        self.as_ref().len()
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        &self.as_ref()[index]
    }
}

/// Where a ring's counters and data words live; see the [module docs](self#storage).
pub trait Storage {
    /// The next position the producer writes.
    fn head(&self) -> &AtomicU64;
    /// The next position the consumer reads.
    fn tail(&self) -> &AtomicU64;
    /// The records dropped.
    fn lost(&self) -> &AtomicU64;
    /// The number of data words, a power of two.
    fn data_len(&self) -> usize;
    /// Data word `index`, below [`Storage::data_len`].
    fn data(&self, index: usize) -> &AtomicU64;
}

/// Counters in the ring, data in a slice of words: [`Ring::new`].
#[derive(Debug)]
pub struct Inline<D> {
    head: AtomicU64,
    tail: AtomicU64,
    lost: AtomicU64,
    data: D,
}

impl<D: Words> Storage for Inline<D> {
    fn head(&self) -> &AtomicU64 {
        &self.head
    }
    fn tail(&self) -> &AtomicU64 {
        &self.tail
    }
    fn lost(&self) -> &AtomicU64 {
        &self.lost
    }
    fn data_len(&self) -> usize {
        self.data.len()
    }
    fn data(&self, index: usize) -> &AtomicU64 {
        self.data.word(index)
    }
}

/// [`SHARED_MAGIC`]: the first word of a shared ring, `b"FOTRACE1"` as a little-endian word.
pub const SHARED_MAGIC: u64 = u64::from_le_bytes(*b"FOTRACE1");

/// The version of the shared layout, at [`VERSION_WORD`].
pub const SHARED_VERSION: u64 = 1;

/// The words of a shared ring's header, ahead of its data: one 4 KiB page.
pub const SHARED_HEADER_WORDS: usize = 512;

/// The header word holding [`SHARED_MAGIC`].
pub const MAGIC_WORD: usize = 0;
/// The header word holding [`SHARED_VERSION`].
pub const VERSION_WORD: usize = 1;
/// The header word holding the number of data words.
pub const DATA_WORDS_WORD: usize = 2;
/// The header word holding `head`.
pub const HEAD_WORD: usize = 8;
/// The header word holding `tail`.
pub const TAIL_WORD: usize = 16;
/// The header word holding `lost`.
pub const LOST_WORD: usize = 24;

/// Counters and data both in one run of words, [`SHARED_HEADER_WORDS`] of header first:
/// [`Ring::shared`], and the words a [`View`] reads.
#[derive(Debug)]
pub struct Shared<W> {
    words: W,
    data_len: usize,
}

impl<W: Words> Shared<W> {
    /// The words, header included.
    pub fn words(&self) -> &W {
        &self.words
    }
}

impl<W: Words> Storage for Shared<W> {
    fn head(&self) -> &AtomicU64 {
        self.words.word(HEAD_WORD)
    }
    fn tail(&self) -> &AtomicU64 {
        self.words.word(TAIL_WORD)
    }
    fn lost(&self) -> &AtomicU64 {
        self.words.word(LOST_WORD)
    }
    fn data_len(&self) -> usize {
        self.data_len
    }
    fn data(&self, index: usize) -> &AtomicU64 {
        self.words.word(SHARED_HEADER_WORDS + index)
    }
}

/// A ring over the storage `S`; see the [module docs](self).
#[derive(Debug)]
pub struct Ring<S> {
    mask: u64,
    when_full: WhenFull,
    storage: S,
}

/// What the producer does with a record that does not fit; see the
/// [module docs](self#when-the-ring-is-full).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhenFull {
    /// Drop the new record and count it as lost.
    #[default]
    DropNewest,
    /// Drop the oldest records, counting them as lost, until the new one fits: a flight
    /// recorder.
    OverwriteOldest,
}

/// Why words cannot be a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RingError {
    /// The number of data words is not a power of two.
    NotPowerOfTwo {
        /// The number of data words.
        len: usize,
    },
    /// The ring could not hold the longest record.
    TooSmall {
        /// The number of data words.
        len: usize,
    },
    /// Shared words without room for the header.
    NoHeader {
        /// The number of words.
        len: usize,
    },
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RingError::NotPowerOfTwo { len } => {
                write!(
                    f,
                    "a ring of {len} words: the length must be a power of two"
                )
            }
            RingError::TooSmall { len } => write!(
                f,
                "a ring of {len} words cannot hold a record of {MAX_RECORD_WORDS} words"
            ),
            RingError::NoHeader { len } => write!(
                f,
                "{len} words cannot hold a shared ring's {SHARED_HEADER_WORDS}-word header"
            ),
        }
    }
}

impl core::error::Error for RingError {}

/// Checks that `len` data words can be a ring, and returns its mask.
fn data_mask(len: usize) -> Result<u64, RingError> {
    if !len.is_power_of_two() {
        return Err(RingError::NotPowerOfTwo { len });
    }
    if len < MAX_RECORD_WORDS {
        return Err(RingError::TooSmall { len });
    }
    Ok(len as u64 - 1)
}

/// Why a record was not pushed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushError {
    /// The ring had no room; the record was counted in [`Ring::lost`].
    Full,
    /// The record had more than [`MAX_ARGS`](crate::MAX_ARGS) arguments.
    TooManyArgs,
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PushError::Full => "the ring is full",
            PushError::TooManyArgs => "a record has at most MAX_ARGS arguments",
        })
    }
}

impl core::error::Error for PushError {}

/// Why [`Ring::commit_to`] refused a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitError {
    /// The position is past `head`, in words the producer has not published.
    BeyondHead {
        /// The position asked for.
        end: u64,
        /// `head` when it was checked.
        head: u64,
    },
    /// The position is not the end of a record, counting from `tail`.
    NotBoundary {
        /// The position asked for.
        end: u64,
    },
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::BeyondHead { end, head } => {
                write!(f, "position {end} is past the ring's head {head}")
            }
            CommitError::NotBoundary { end } => {
                write!(f, "position {end} is not the end of a record")
            }
        }
    }
}

impl core::error::Error for CommitError {}

impl<D: Words> Ring<Inline<D>> {
    /// An empty ring over `data`, whose current contents are ignored.
    ///
    /// # Errors
    ///
    /// [`RingError::NotPowerOfTwo`] or [`RingError::TooSmall`] when `data`'s length is not a
    /// power of two of at least [`MAX_RECORD_WORDS`] words.
    pub fn new(data: D) -> Result<Self, RingError> {
        let mask = data_mask(data.len())?;
        Ok(Self {
            mask,
            when_full: WhenFull::default(),
            storage: Inline {
                head: AtomicU64::new(0),
                tail: AtomicU64::new(0),
                lost: AtomicU64::new(0),
                data,
            },
        })
    }
}

impl<W: Words> Ring<Shared<W>> {
    /// An empty shared ring over `words`: writes the header, with the counters at zero, and
    /// ignores the rest of the current contents. The data are the words after the
    /// [`SHARED_HEADER_WORDS`]-word header.
    ///
    /// # Errors
    ///
    /// [`RingError::NoHeader`] when there are no more words than the header, and as
    /// [`Ring::new`] for the data words.
    pub fn shared(words: W) -> Result<Self, RingError> {
        let len = words.len();
        let data_len = len
            .checked_sub(SHARED_HEADER_WORDS)
            .filter(|&data| data > 0)
            .ok_or(RingError::NoHeader { len })?;
        let mask = data_mask(data_len)?;
        for (index, value) in [
            (VERSION_WORD, SHARED_VERSION),
            (DATA_WORDS_WORD, data_len as u64),
            (HEAD_WORD, 0),
            (TAIL_WORD, 0),
            (LOST_WORD, 0),
        ] {
            words.word(index).store(value, Ordering::Relaxed);
        }
        // A reader that sees the magic sees the rest of the header.
        words
            .word(MAGIC_WORD)
            .store(SHARED_MAGIC, Ordering::Release);
        Ok(Self {
            mask,
            when_full: WhenFull::default(),
            storage: Shared { words, data_len },
        })
    }

    /// The words, header included: what the kernel maps into a reader.
    pub fn words(&self) -> &W {
        self.storage.words()
    }

    /// Moves `tail` to `end`, a position a reader got from a [`View`]'s [`Peek::end`] and
    /// passed back. Only the consumer may call this. A position at or below `tail` leaves the
    /// ring as it is, as [`Ring::commit`] does with an older peek.
    ///
    /// # Errors
    ///
    /// [`CommitError::BeyondHead`] when `end` is past `head`, and [`CommitError::NotBoundary`]
    /// when walking the records from `tail` steps over `end` instead of landing on it. Nothing
    /// is consumed then. A walk that meets a record that does not decode (only possible if two
    /// producers raced) accepts `end` and counts one lost record, as [`Ring::commit`] does
    /// with a corrupt peek, whose end is past the bad record.
    pub fn commit_to(&self, end: u64) -> Result<(), CommitError> {
        loop {
            let tail = self.storage.tail().load(Ordering::Acquire);
            if end <= tail {
                return Ok(());
            }
            let head = self.storage.head().load(Ordering::Acquire);
            if end > head {
                return Err(CommitError::BeyondHead { end, head });
            }
            let walk = self.walk_to(tail, end);
            // Pairs with the release fence in `Ring::push`: an overwriting producer moves `tail`
            // before it reuses the words the walk read, so an unchanged `tail` means they were
            // whole. Otherwise walk again from the new `tail`.
            fence(Ordering::Acquire);
            if self.storage.tail().load(Ordering::Relaxed) != tail {
                continue;
            }
            match walk {
                Walk::Boundary => {}
                // The reader met the same undecodable record and stopped there, with a
                // corrupt peek: drop what it covers, as [`Ring::commit`] does.
                Walk::Corrupt => self.count_lost(),
                Walk::Over => return Err(CommitError::NotBoundary { end }),
            }
            self.storage.tail().fetch_max(end, Ordering::Release);
            return Ok(());
        }
    }
}

/// Where walking the records from one position towards another ends.
enum Walk {
    /// On the position.
    Boundary,
    /// At a record that does not decode, before the position.
    Corrupt,
    /// Past the position: it is inside a record.
    Over,
}

impl<S: Storage> Ring<S> {
    /// The ring's size in data words.
    pub fn capacity(&self) -> usize {
        self.storage.data_len()
    }

    /// The words written and not yet read.
    pub fn pending(&self) -> usize {
        let tail = self.storage.tail().load(Ordering::Acquire);
        let head = self.storage.head().load(Ordering::Acquire);
        head.saturating_sub(tail) as usize
    }

    /// The records dropped so far, because the ring was full or because the producer could not
    /// write them (see [`Ring::count_lost`]).
    pub fn lost(&self) -> u64 {
        self.storage.lost().load(Ordering::Relaxed)
    }

    /// Counts a record the producer could not push for a reason of its own.
    pub fn count_lost(&self) {
        self.storage.lost().fetch_add(1, Ordering::Relaxed);
    }

    /// The word slot for position `pos`.
    fn slot(&self, pos: u64) -> &AtomicU64 {
        // `mask` is the data length minus one, so the index is always in bounds.
        self.storage.data((pos & self.mask) as usize)
    }

    /// The ring with the policy `when_full` for records that do not fit.
    #[must_use]
    pub fn when_full(mut self, when_full: WhenFull) -> Self {
        self.when_full = when_full;
        self
    }

    /// The ring's policy for records that do not fit.
    pub fn full_policy(&self) -> WhenFull {
        self.when_full
    }

    /// Walks the records from `from` towards `to`.
    fn walk_to(&self, from: u64, to: u64) -> Walk {
        let mut pos = from;
        while pos < to {
            let Ok(header) = Header::decode(self.slot(pos).load(Ordering::Relaxed)) else {
                return Walk::Corrupt;
            };
            pos = pos.saturating_add(header.words() as u64);
        }
        if pos == to {
            Walk::Boundary
        } else {
            Walk::Over
        }
    }

    /// Moves `tail` past the oldest whole records until `words` more fit before `head`, and
    /// counts them as lost. Only the producer may call this.
    fn make_room(&self, head: u64, words: u64) {
        // `data_mask` checked that the ring holds the longest record, so this is at most `head`.
        let needed = head
            .wrapping_add(words)
            .saturating_sub(self.capacity() as u64);
        let mut tail = self.storage.tail().load(Ordering::Acquire);
        while tail < needed {
            // Only this producer writes records, so the headers from `tail` on are whole.
            let mut pos = tail;
            let mut dropped = 0;
            while pos < needed {
                match Header::decode(self.slot(pos).load(Ordering::Relaxed)) {
                    Ok(header) => pos = pos.saturating_add(header.words() as u64),
                    // Not a record (only possible if two producers raced): drop everything.
                    Err(_) => pos = head,
                }
                // A scribbled length cannot take `tail` past `head`.
                pos = pos.min(head);
                dropped += 1;
            }
            // The consumer may commit meanwhile; then count from where it stopped. The release
            // fence in `Ring::push` orders this store before the stores that reuse the words.
            match self.storage.tail().compare_exchange_weak(
                tail,
                pos,
                Ordering::Relaxed,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.storage.lost().fetch_add(dropped, Ordering::Relaxed);
                    return;
                }
                Err(now) => tail = now,
            }
        }
    }

    /// Appends the record of `event` at `timestamp` with `args`. Only the producer may call this.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] when the ring has no room and drops new records (the record is
    /// counted as lost), and [`PushError::TooManyArgs`] when there are more than
    /// [`MAX_ARGS`](crate::MAX_ARGS) arguments. A ring that overwrites its oldest records is
    /// never full.
    pub fn push(&self, event: EventId, timestamp: u64, args: &[u64]) -> Result<(), PushError> {
        let header = Header::new(event, args.len()).ok_or(PushError::TooManyArgs)?;
        let words = header.words() as u64;
        // Only the producer stores `head`, so a relaxed load sees its own last store.
        let head = self.storage.head().load(Ordering::Relaxed);
        let tail = self.storage.tail().load(Ordering::Acquire);
        if head.wrapping_sub(tail).saturating_add(words) > self.capacity() as u64 {
            match self.when_full {
                WhenFull::DropNewest => {
                    self.count_lost();
                    return Err(PushError::Full);
                }
                WhenFull::OverwriteOldest => self.make_room(head, words),
            }
        }
        // Pairs with the acquire fence in `Ring::peek_checked`: a reader that sees one of the
        // stores below then sees a `tail` at least this one (the consumer's, or the one
        // `make_room` just stored), so it knows the words may be reused under it. On x86_64
        // this emits no instruction.
        fence(Ordering::Release);
        self.slot(head).store(header.encode(), Ordering::Relaxed);
        self.slot(head.wrapping_add(1))
            .store(timestamp, Ordering::Relaxed);
        for (pos, &arg) in (head.wrapping_add(2)..).zip(args) {
            self.slot(pos).store(arg, Ordering::Relaxed);
        }
        self.storage
            .head()
            .store(head.wrapping_add(words), Ordering::Release);
        Ok(())
    }

    /// Copies as many whole records as fit into `out`, as little-endian words, without consuming
    /// them. Only the consumer may call this; [`Ring::commit`] the result to consume what it
    /// holds, or drop it to leave the records for the next read (for example when passing them on
    /// fails).
    ///
    /// A record that does not decode (only possible if two producers raced) ends the copy, and the
    /// commit drops everything pending, counting one lost record.
    ///
    /// # Errors
    ///
    /// [`ReadError::BufferTooSmall`] when records are pending but the first one does not fit in
    /// `out`. An empty ring is not an error: it gives an empty [`Peek`].
    pub fn peek(&self, out: &mut [u8]) -> Result<Peek, ReadError> {
        self.peek_checked(0, out)
    }

    /// [`Ring::peek_from`] at `from` or `tail`, whichever is later, checked against `tail`
    /// moving under the copy; see the [module docs](self#when-the-ring-is-full).
    fn peek_checked(&self, from: u64, out: &mut [u8]) -> Result<Peek, ReadError> {
        loop {
            let start = from.max(self.storage.tail().load(Ordering::Acquire));
            let copy = self.peek_from(start, out);
            // Pairs with the release fence in `Ring::push`: if a copied word was rewritten,
            // the `tail` that allowed the rewrite is visible now.
            fence(Ordering::Acquire);
            let tail = self.storage.tail().load(Ordering::Relaxed);
            if tail <= start {
                return copy;
            }
            if let Some(kept) = copy.ok().and_then(|copy| keep_from(start, tail, out, copy)) {
                return Ok(kept);
            }
        }
    }

    /// [`Ring::peek`] from position `tail`.
    fn peek_from(&self, tail: u64, out: &mut [u8]) -> Result<Peek, ReadError> {
        let head = self.storage.head().load(Ordering::Acquire);
        let mut pos = tail;
        let mut written = 0;
        while pos < head {
            let Some(header) = Header::decode(self.slot(pos).load(Ordering::Relaxed))
                .ok()
                .filter(|header| pos.saturating_add(header.words() as u64) <= head)
            else {
                return Ok(Peek {
                    len: written,
                    end: head,
                    verified_end: pos,
                    corrupt: true,
                });
            };
            let len = header.words() * WORD_BYTES;
            let Some(dst) = out.get_mut(written..written + len) else {
                if written == 0 {
                    return Err(ReadError::BufferTooSmall { needed: len });
                }
                break;
            };
            for (chunk, word_pos) in dst.chunks_exact_mut(WORD_BYTES).zip(pos..) {
                chunk.copy_from_slice(&self.slot(word_pos).load(Ordering::Relaxed).to_le_bytes());
            }
            pos += header.words() as u64;
            written += len;
        }
        Ok(Peek {
            len: written,
            end: pos,
            verified_end: pos,
            corrupt: false,
        })
    }

    /// Consumes the records `peek` copied, so the producer can reuse their words. Only the
    /// consumer may call this, with the result of its latest [`Ring::peek`]; an older result
    /// never moves the ring back.
    pub fn commit(&self, peek: Peek) {
        if peek.corrupt {
            self.count_lost();
        }
        self.storage.tail().fetch_max(peek.end, Ordering::Release);
    }

    /// Copies the newest whole records that fit in `out`, oldest first, to the start of `out`
    /// without consuming them, and returns the number of words copied. Any CPU may call it; it
    /// is meant for a crash dump, taken while the producer may still be running. A record the
    /// producer has not published is not copied, and a record that does not decode ends the
    /// walk with what came before it.
    ///
    /// The producer can reuse the oldest words while they are copied: a consumer commits, or a
    /// [`WhenFull::OverwriteOldest`] ring makes room. `tail` is read again after the copy, as
    /// [`Ring::peek`] does. When it moved into the copy, the records before it are dropped and
    /// the rest moved to the start of `out`; when it moved past the copy, or only past records
    /// the walk had to read to find the first one kept, the copy is made again, up to
    /// [`COPY_LATEST_TRIES`] times, after which nothing is copied. A copy therefore never
    /// holds a record the producer rewrote under it, and is never out of order; only its last
    /// record can be cut short when the producer kept rewriting the words the walk went
    /// through. It never reads out of bounds and does not allocate.
    pub fn copy_latest(&self, out: &[AtomicU64]) -> usize {
        for _ in 0..COPY_LATEST_TRIES {
            let tail = self.storage.tail().load(Ordering::Acquire);
            let (start, end) = self.latest(tail, out.len() as u64);
            for (slot, pos) in out.iter().zip(start..end) {
                slot.store(self.slot(pos).load(Ordering::Relaxed), Ordering::Relaxed);
            }
            // Pairs with the release fence in `Ring::push`: if a copied word was rewritten, the
            // `tail` that allowed the rewrite is visible now.
            fence(Ordering::Acquire);
            let now = self.storage.tail().load(Ordering::Relaxed);
            if now == tail {
                return (end - start) as usize;
            }
            // `tail` moved in whole records, so `now` starts one. The words from it on were
            // read before the producer could reuse them, and so were the headers the walk read
            // there; the headers before it may not have been, so `start` is only known to be a
            // record when `now` is at or after it.
            if (start..end).contains(&now) {
                let skip = (now - start) as usize;
                let kept = (end - now) as usize;
                // Forwards, so each word is read before anything is stored over it.
                for (to, from) in out.iter().zip(&out[skip..skip + kept]) {
                    to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
                }
                return kept;
            }
        }
        0
    }

    /// The positions `start..end` of the newest whole records from `tail` that fit in `max`
    /// words, by walking their headers.
    fn latest(&self, tail: u64, max: u64) -> (u64, u64) {
        let head = self.storage.head().load(Ordering::Acquire);
        let words_at = |pos: u64| {
            Header::decode(self.slot(pos).load(Ordering::Relaxed))
                .ok()
                .map(|header| header.words() as u64)
        };
        // Walk the records from `tail`, keeping `start` on the oldest record that still fits
        // with every newer one in `max` words.
        let (mut start, mut end) = (tail, tail);
        while end < head {
            let Some(words) = words_at(end).filter(|&words| end + words <= head) else {
                break;
            };
            end += words;
            while end - start > max {
                match words_at(start) {
                    Some(words) if start + words <= end => start += words,
                    // The words at `start` changed under the walk: keep nothing older.
                    _ => start = end,
                }
            }
        }
        (start, end)
    }

    /// [`Ring::peek`] and [`Ring::commit`] in one step: moves as many whole records as fit into
    /// `out` and returns the number of bytes written, 0 only when the ring is empty (or held only
    /// a corrupt record, which is dropped). Only the consumer may call this.
    ///
    /// # Errors
    ///
    /// As [`Ring::peek`]; nothing is consumed then.
    pub fn read(&self, out: &mut [u8]) -> Result<usize, ReadError> {
        let peek = self.peek(out)?;
        let len = peek.len();
        self.commit(peek);
        Ok(len)
    }
}

/// How many times [`Ring::copy_latest`] copies again when the producer reused words under the
/// copy, before it gives up and copies nothing.
pub const COPY_LATEST_TRIES: usize = 4;

/// The part of `copy`, made from position `start` into `out`, from position `tail` on, moved
/// to the front of `out`: the records whose words were not reused, if the copy's walk lands on
/// `tail` and there are any.
fn keep_from(start: u64, tail: u64, out: &mut [u8], copy: Peek) -> Option<Peek> {
    let mut pos = start;
    let mut offset = 0;
    while pos < tail {
        let word = out.get(offset..offset + WORD_BYTES)?;
        let header = Header::decode(u64::from_le_bytes(word.try_into().ok()?)).ok()?;
        pos = pos.saturating_add(header.words() as u64);
        offset += header.words() * WORD_BYTES;
    }
    if pos != tail || offset >= copy.len {
        return None;
    }
    out.copy_within(offset..copy.len, 0);
    Some(Peek {
        len: copy.len - offset,
        ..copy
    })
}

/// A reader's view of a shared ring it may only read, such as the kernel's mapping of a CPU's
/// ring: it copies records without storing anything, and the reader hands [`Peek::end`] back to
/// whoever owns `tail`.
#[derive(Debug)]
pub struct View<W> {
    ring: Ring<Shared<W>>,
}

/// Why words are not a shared ring a [`View`] can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ViewError {
    /// The first word is not [`SHARED_MAGIC`].
    BadMagic,
    /// The layout version is not [`SHARED_VERSION`].
    Version {
        /// The version found.
        found: u64,
    },
    /// The header's data length does not match the words, or cannot be a ring.
    BadLength {
        /// The data length in the header.
        data_words: u64,
        /// The words after the header.
        available: usize,
    },
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewError::BadMagic => f.write_str("not a shared trace ring (bad magic)"),
            ViewError::Version { found } => write!(
                f,
                "shared trace ring version {found}, this reader knows {SHARED_VERSION}"
            ),
            ViewError::BadLength {
                data_words,
                available,
            } => write!(
                f,
                "a shared trace ring of {data_words} data words in {available} words"
            ),
        }
    }
}

impl core::error::Error for ViewError {}

impl<W: Words> View<W> {
    /// A view of the shared ring in `words`, after checking its header.
    ///
    /// # Errors
    ///
    /// [`ViewError`] when the header is not one this reader understands.
    pub fn new(words: W) -> Result<Self, ViewError> {
        if words.len() <= SHARED_HEADER_WORDS
            || words.word(MAGIC_WORD).load(Ordering::Acquire) != SHARED_MAGIC
        {
            return Err(ViewError::BadMagic);
        }
        let found = words.word(VERSION_WORD).load(Ordering::Relaxed);
        if found != SHARED_VERSION {
            return Err(ViewError::Version { found });
        }
        let data_words = words.word(DATA_WORDS_WORD).load(Ordering::Relaxed);
        let available = words.len() - SHARED_HEADER_WORDS;
        let bad = ViewError::BadLength {
            data_words,
            available,
        };
        if data_words != available as u64 {
            return Err(bad);
        }
        let mask = data_mask(available).map_err(|_| bad)?;
        Ok(Self {
            ring: Ring {
                mask,
                // A view never pushes.
                when_full: WhenFull::DropNewest,
                storage: Shared {
                    words,
                    data_len: available,
                },
            },
        })
    }

    /// Copies as many whole records as fit into `out`, starting at position `from` (the
    /// [`Peek::end`] of the previous copy, or [`View::tail`] at first). Nothing is stored. A
    /// `from` below the ring's `tail` starts at `tail`.
    ///
    /// The view is not the ring's consumer, so the consumer can move `tail` while it copies
    /// (another reader draining the ring through the kernel, say), as can a producer that
    /// overwrites its oldest records, and the producer can then reuse the words being copied.
    /// A word at a position is reused only once `tail` has passed that position, so after
    /// copying the view loads `tail` again and, if it moved past the start, keeps the records
    /// from the new `tail` on, or starts over there when its walk does not land on it (as a
    /// seqlock reader retries). A returned copy therefore never mixes two records, and never
    /// holds records consumed or overwritten before it was checked. Records consumed after the check
    /// can still reach both readers: a reader that needs each record exactly once must be the
    /// ring's only reader.
    ///
    /// # Errors
    ///
    /// As [`Ring::peek`], for a copy that `tail` did not overtake.
    pub fn peek(&self, from: u64, out: &mut [u8]) -> Result<Peek, ReadError> {
        self.ring.peek_checked(from, out)
    }

    /// The ring's `tail`: where the owner will let the producer reuse words from.
    pub fn tail(&self) -> u64 {
        self.ring.storage.tail().load(Ordering::Acquire)
    }

    /// The ring's `head`.
    pub fn head(&self) -> u64 {
        self.ring.storage.head().load(Ordering::Acquire)
    }

    /// The records the producer dropped.
    pub fn lost(&self) -> u64 {
        self.ring.lost()
    }

    /// The ring's size in data words.
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }
}

/// Records [`Ring::peek`] or [`View::peek`] copied out of a ring and has not consumed yet.
#[derive(Debug)]
#[must_use = "the records stay in the ring until the peek is committed"]
pub struct Peek {
    len: usize,
    end: u64,
    verified_end: u64,
    corrupt: bool,
}

impl Peek {
    /// The number of bytes copied.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing was copied.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The position after the last word copied: what to pass to [`Ring::commit_to`] (through
    /// the kernel, for a mapped ring) to consume the copy, and where the next [`View::peek`]
    /// starts.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// The position after the last whole record copied, counted from the start this peek
    /// actually used. It equals [`Peek::end`] unless the copy is corrupt, when it is where
    /// the record that does not decode starts. A reader that keeps a corrupt copy's records
    /// but not the corruption commits to this position, not to one worked out from a later
    /// load of `tail`, which another reader may have moved since.
    pub fn verified_end(&self) -> u64 {
        self.verified_end
    }

    /// Whether the copy stopped at a record that does not decode.
    pub fn is_corrupt(&self) -> bool {
        self.corrupt
    }
}

/// Why records could not be read from a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadError {
    /// The next record is longer than the buffer; it stays in the ring.
    BufferTooSmall {
        /// The record's length in bytes. A buffer of
        /// [`MAX_RECORD_WORDS`] `*` [`WORD_BYTES`] bytes always holds one.
        needed: usize,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::BufferTooSmall { needed } => {
                write!(f, "the next record needs a buffer of {needed} bytes")
            }
        }
    }
}

impl core::error::Error for ReadError {}

#[cfg(feature = "alloc")]
impl Ring<Inline<alloc::boxed::Box<[AtomicU64]>>> {
    /// An empty ring of `words` words on the heap.
    ///
    /// # Errors
    ///
    /// As [`Ring::new`].
    pub fn with_capacity(words: usize) -> Result<Self, RingError> {
        Ring::new((0..words).map(|_| AtomicU64::new(0)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Records;
    use std::vec::Vec;

    fn words(n: usize) -> Vec<AtomicU64> {
        (0..n).map(|_| AtomicU64::new(0)).collect()
    }

    #[test]
    fn sizes_are_checked() {
        assert_eq!(
            Ring::new(words(24)).map(|_| ()),
            Err(RingError::NotPowerOfTwo { len: 24 })
        );
        assert_eq!(
            Ring::new(words(8)).map(|_| ()),
            Err(RingError::TooSmall { len: 8 })
        );
        assert!(Ring::new(words(16)).is_ok());
    }

    /// [`Ring::copy_latest`] into `max` words: the words copied.
    fn latest<S: Storage>(ring: &Ring<S>, max: usize) -> Vec<u64> {
        let out = words(max);
        let n = ring.copy_latest(&out);
        out[..n].iter().map(|w| w.load(Ordering::Relaxed)).collect()
    }

    /// The timestamps of the records in `words`.
    fn timestamps(words: &[u64]) -> Vec<u64> {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        byte_timestamps(&bytes)
    }

    fn byte_timestamps(bytes: &[u8]) -> Vec<u64> {
        Records::new(bytes)
            .map(|r| r.expect("whole records").timestamp())
            .collect()
    }

    #[test]
    fn copy_latest_keeps_the_newest_records() {
        let ring = Ring::new(words(32)).expect("valid");
        // Three-word records; read some so the newest wrap round the end of the data.
        for t in 0..8 {
            ring.push(EventId::new(1), t, &[t]).expect("room");
        }
        let mut buf = [0u8; 192];
        assert_eq!(ring.read(&mut buf), Ok(192));
        for t in 8..14 {
            ring.push(EventId::new(1), t, &[t]).expect("room");
        }
        let pending = ring.pending();
        let copied = latest(&ring, 10);
        assert_eq!(copied.len(), 9);
        assert_eq!(timestamps(&copied), [11, 12, 13]);
        // Nothing was consumed.
        assert_eq!(ring.pending(), pending);
        let copied = latest(&ring, 64);
        assert_eq!(copied.len(), pending);
        assert_eq!(timestamps(&copied), [8, 9, 10, 11, 12, 13]);
        assert!(latest(&ring, 2).is_empty(), "no record fits");
    }

    #[test]
    fn copy_latest_stops_at_a_corrupt_record() {
        let ring = Ring::new(words(32)).expect("valid");
        ring.push(EventId::new(1), 1, &[]).expect("room");
        ring.push(EventId::new(1), 2, &[]).expect("room");
        ring.push(EventId::new(1), 3, &[]).expect("room");
        // Scribble over the second record's header.
        ring.slot(2).store(u64::MAX, Ordering::Relaxed);
        let copied = latest(&ring, 64);
        assert_eq!(copied.len(), 2);
        assert_eq!(timestamps(&copied), [1]);
    }

    #[test]
    fn push_and_read() {
        let ring = Ring::new(words(64)).expect("valid");
        ring.push(EventId::new(1), 5, &[1, 2, 3]).expect("room");
        ring.push(EventId::new(2), 6, &[]).expect("room");
        assert_eq!(ring.pending(), 7);
        assert_eq!(
            ring.push(EventId::new(0), 0, &[0; 9]),
            Err(PushError::TooManyArgs)
        );

        let mut buf = [0u8; 256];
        let len = ring.read(&mut buf).expect("fits");
        assert_eq!(len, 56);
        assert_eq!(ring.pending(), 0);
        let records: Vec<_> = Records::new(&buf[..len])
            .collect::<Result<_, _>>()
            .expect("valid");
        assert_eq!(records[0].args().collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(records[1].timestamp(), 6);
        assert_eq!(ring.read(&mut buf), Ok(0));
    }

    #[test]
    fn full_ring_drops_and_counts() {
        let ring = Ring::new(words(16)).expect("valid");
        // 6 words each: two fit, the third does not.
        for i in 0..3 {
            let res = ring.push(EventId::new(0), i, &[0; 4]);
            assert_eq!(res.is_ok(), i < 2, "push {i}");
        }
        assert_eq!(ring.lost(), 1);
        ring.count_lost();
        assert_eq!(ring.lost(), 2);
    }

    #[test]
    fn overwriting_ring_keeps_the_newest_records() {
        let ring = Ring::new(words(16))
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        assert_eq!(ring.full_policy(), WhenFull::OverwriteOldest);
        // 6 words each: two fit, so each push from the third on drops the oldest.
        for i in 0..5 {
            assert_eq!(ring.push(EventId::new(0), i, &[i; 4]), Ok(()), "push {i}");
        }
        assert_eq!(ring.lost(), 3);
        let mut buf = [0u8; 256];
        let len = ring.read(&mut buf).expect("fits");
        assert_eq!(byte_timestamps(&buf[..len]), [3, 4]);
        // The ring empties as usual, and then takes records without dropping any.
        assert_eq!(ring.read(&mut buf), Ok(0));
        ring.push(EventId::new(0), 5, &[]).expect("room");
        assert_eq!(ring.lost(), 3);
    }

    #[test]
    fn overwriting_ring_drops_whole_records_of_any_length() {
        let ring = Ring::new(words(16))
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        // 2, 4, 6 and 3 words: 15 of 16.
        ring.push(EventId::new(0), 0, &[]).expect("room");
        ring.push(EventId::new(0), 1, &[1, 1]).expect("room");
        ring.push(EventId::new(0), 2, &[2; 4]).expect("room");
        ring.push(EventId::new(0), 3, &[3]).expect("room");
        assert_eq!(ring.lost(), 0);
        // 10 words: the first three records (12 words) make room for it, the fourth stays.
        ring.push(EventId::new(0), 4, &[4; 8]).expect("room");
        assert_eq!(ring.lost(), 3);
        assert_eq!(ring.pending(), 13);
        let mut buf = [0u8; 256];
        let len = ring.read(&mut buf).expect("fits");
        assert_eq!(byte_timestamps(&buf[..len]), [3, 4]);
    }

    #[test]
    fn copy_latest_of_an_overwriting_ring() {
        let ring = Ring::new(words(32))
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        for i in 0..100 {
            ring.push(EventId::new(1), i, &[i, i]).expect("room");
        }
        let copied = latest(&ring, 12);
        assert_eq!(copied.len(), 12);
        assert_eq!(timestamps(&copied), [97, 98, 99]);
    }

    /// Two rings over the same shared words: one that copies through [`Hooked`], whose `hook`
    /// pushes on the other (the producer, a flight recorder) partway through the copy.
    fn overtaken_copy(at: usize, pushes: u64, max: usize) -> (Vec<u64>, u64) {
        let mem = words(SHARED_HEADER_WORDS + 16);
        let producer = std::sync::OnceLock::new();
        let reader = Ring::shared(Hooked {
            words: &mem[..],
            at,
            fired: core::sync::atomic::AtomicBool::new(false),
            hook: || {
                let producer: &Ring<Shared<&[AtomicU64]>> = producer.get().expect("made");
                for t in 4..4 + pushes {
                    producer.push(EventId::new(1), t, &[t, t]).expect("room");
                }
            },
        })
        .expect("valid");
        let ring = producer.get_or_init(|| {
            Ring::shared(&mem[..])
                .expect("valid")
                .when_full(WhenFull::OverwriteOldest)
        });
        // Four 4-word records fill the 16 data words.
        for t in 0..4 {
            ring.push(EventId::new(1), t, &[t, t]).expect("room");
        }
        (latest(&reader, max), ring.lost())
    }

    /// The producer overwrites the oldest record while it is copied (after its header, before
    /// its timestamp): the copy drops it instead of keeping its old header with the new
    /// record's timestamp, which would put timestamp 4 first.
    #[test]
    fn copy_latest_drops_what_the_producer_overwrote() {
        let (copied, lost) = overtaken_copy(SHARED_HEADER_WORDS + 1, 1, 16);
        assert_eq!(lost, 1);
        assert_eq!(copied.len(), 12);
        assert_eq!(timestamps(&copied), [1, 2, 3]);
    }

    /// The producer overwrites every record under the copy: the copy is made again, of the
    /// records that are there now.
    #[test]
    fn copy_latest_copies_again_when_overtaken() {
        let (copied, lost) = overtaken_copy(SHARED_HEADER_WORDS + 1, 4, 16);
        assert_eq!(lost, 4);
        assert_eq!(timestamps(&copied), [4, 5, 6, 7]);
    }

    /// The producer overwrites only records older than those the copy keeps, which the walk
    /// read to find the first one kept: the copy is made again.
    #[test]
    fn copy_latest_rewalks_when_the_records_before_it_change() {
        // Room for two records: the copy starts at the third record's header (data word 8).
        let (copied, lost) = overtaken_copy(SHARED_HEADER_WORDS + 9, 1, 8);
        assert_eq!(lost, 1);
        assert_eq!(timestamps(&copied), [3, 4]);
    }

    /// The consumer's position is a record boundary below the overwriting producer's new
    /// `tail`: committing it changes nothing.
    #[test]
    fn commit_to_after_an_overwrite() {
        let mem = words(SHARED_HEADER_WORDS + 16);
        let ring = Ring::shared(&mem[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        for i in 0..4 {
            ring.push(EventId::new(1), i, &[i, i]).expect("room");
        }
        let view = View::new(&mem[..]).expect("valid header");
        let mut buf = [0u8; 256];
        let peek = view.peek(0, &mut buf).expect("fits");
        assert_eq!(peek.end(), 16);
        // Two more records overwrite the first two, which moves `tail` to 8.
        ring.push(EventId::new(1), 4, &[4, 4]).expect("room");
        ring.push(EventId::new(1), 5, &[5, 5]).expect("room");
        assert_eq!(view.tail(), 8);
        assert_eq!(ring.commit_to(4), Ok(()));
        assert_eq!(view.tail(), 8);
        assert_eq!(ring.commit_to(peek.end()), Ok(()));
        assert_eq!(view.tail(), 16);
        let peek = view.peek(16, &mut buf).expect("fits");
        assert_eq!(byte_timestamps(&buf[..peek.len()]), [4, 5]);
    }

    /// An overwriting producer reuses the first record's words while a view copies them: the
    /// view keeps the records after it, which are whole.
    #[test]
    fn view_keeps_what_an_overwrite_left() {
        let mem = words(SHARED_HEADER_WORDS + 16);
        let ring = Ring::shared(&mem[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        // Four 4-word records fill the 16 data words.
        for i in 0..4 {
            ring.push(EventId::new(1), i, &[i, i]).expect("room");
        }
        let view = View::new(Hooked {
            words: &mem[..],
            // The first record's timestamp: its header is copied, the rest is not yet.
            at: SHARED_HEADER_WORDS + 1,
            fired: core::sync::atomic::AtomicBool::new(false),
            hook: || ring.push(EventId::new(1), 4, &[4, 4]).expect("room"),
        })
        .expect("valid header");
        let mut buf = [0u8; 256];
        let peek = view.peek(0, &mut buf).expect("fits");
        assert!(!peek.is_corrupt());
        assert_eq!(peek.end(), 16);
        assert_eq!(byte_timestamps(&buf[..peek.len()]), [1, 2, 3]);
        assert_eq!(view.lost(), 1);
    }

    #[test]
    fn reads_whole_records_only() {
        let ring = Ring::new(words(64)).expect("valid");
        ring.push(EventId::new(0), 1, &[9; 4]).expect("room");
        ring.push(EventId::new(0), 2, &[9; 4]).expect("room");
        // Room for one 48-byte record and a half.
        let mut buf = [0u8; 72];
        assert_eq!(ring.read(&mut buf), Ok(48));
        // A buffer too small for the next record is an error, not an empty ring, and the
        // record stays.
        assert_eq!(
            ring.read(&mut buf[..47]),
            Err(ReadError::BufferTooSmall { needed: 48 })
        );
        assert_eq!(ring.pending(), 6);
        assert_eq!(ring.read(&mut buf), Ok(48));
        assert_eq!(ring.read(&mut buf[..1]), Ok(0));
    }

    #[test]
    fn uncommitted_peeks_keep_records() {
        let ring = Ring::new(words(64)).expect("valid");
        ring.push(EventId::new(1), 7, &[1, 2]).expect("room");
        let mut buf = [0u8; 128];
        // A reader whose copy-out failed drops its peek: the record is still there.
        let peek = ring.peek(&mut buf).expect("fits");
        assert_eq!(peek.len(), 32);
        drop(peek);
        assert_eq!(ring.pending(), 4);
        assert_eq!(ring.lost(), 0);

        let mut again = [0u8; 128];
        let peek = ring.peek(&mut again).expect("fits");
        assert_eq!(again[..32], buf[..32]);
        let stale = ring.peek(&mut buf).expect("fits");
        ring.commit(peek);
        assert_eq!(ring.pending(), 0);
        // Committing an older peek again never moves the ring back.
        ring.commit(stale);
        assert_eq!(ring.pending(), 0);
        assert_eq!(ring.read(&mut buf), Ok(0));
    }

    #[test]
    fn records_wrap_around_the_end() {
        let ring = Ring::new(words(16)).expect("valid");
        let mut buf = [0u8; 128];
        for i in 0..100u64 {
            ring.push(EventId::new(3), i, &[i, !i, i * 3])
                .expect("room");
            let len = ring.read(&mut buf).expect("fits");
            let record = Records::new(&buf[..len])
                .next()
                .expect("one record")
                .expect("valid");
            assert_eq!(record.timestamp(), i);
            assert_eq!(record.args().collect::<Vec<_>>(), [i, !i, i * 3]);
        }
        assert_eq!(ring.lost(), 0);
    }

    #[test]
    fn corrupt_contents_are_dropped() {
        let data = words(16);
        let ring = Ring::new(&data[..]).expect("valid");
        ring.push(EventId::new(0), 1, &[]).expect("room");
        // A second producer scribbling over the header.
        data[0].store(u64::MAX, Ordering::Relaxed);
        let mut buf = [0u8; 128];
        assert_eq!(ring.read(&mut buf), Ok(0));
        assert_eq!(ring.pending(), 0);
        assert_eq!(ring.lost(), 1);
    }

    #[test]
    fn shared_ring_writes_its_header() {
        let mem = words(SHARED_HEADER_WORDS + 64);
        for word in &mem {
            word.store(0xdead, Ordering::Relaxed);
        }
        let ring = Ring::shared(&mem[..]).expect("valid");
        assert_eq!(mem[MAGIC_WORD].load(Ordering::Relaxed), SHARED_MAGIC);
        assert_eq!(mem[VERSION_WORD].load(Ordering::Relaxed), SHARED_VERSION);
        assert_eq!(mem[DATA_WORDS_WORD].load(Ordering::Relaxed), 64);
        assert_eq!(ring.capacity(), 64);
        assert_eq!(ring.pending(), 0);
        ring.push(EventId::new(1), 9, &[4]).expect("room");
        assert_eq!(mem[HEAD_WORD].load(Ordering::Relaxed), 3);
        assert_eq!(mem[TAIL_WORD].load(Ordering::Relaxed), 0);
        // The data start after the header.
        assert_eq!(mem[SHARED_HEADER_WORDS + 1].load(Ordering::Relaxed), 9);
        assert_eq!(ring.words().len(), SHARED_HEADER_WORDS + 64);
    }

    #[test]
    fn shared_sizes_are_checked() {
        let err = |n| Ring::shared(words(n)).map(|_| ()).expect_err("invalid");
        assert_eq!(err(0), RingError::NoHeader { len: 0 });
        assert_eq!(
            err(SHARED_HEADER_WORDS),
            RingError::NoHeader {
                len: SHARED_HEADER_WORDS
            }
        );
        assert_eq!(
            err(SHARED_HEADER_WORDS + 24),
            RingError::NotPowerOfTwo { len: 24 }
        );
        assert_eq!(err(SHARED_HEADER_WORDS + 8), RingError::TooSmall { len: 8 });
    }

    #[test]
    fn view_reads_without_consuming() {
        let mem = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&mem[..]).expect("valid");
        ring.push(EventId::new(1), 5, &[1, 2, 3]).expect("room");
        ring.push(EventId::new(2), 6, &[]).expect("room");
        let view = View::new(&mem[..]).expect("valid header");
        assert_eq!(view.capacity(), 64);
        assert_eq!((view.tail(), view.head()), (0, 7));

        let mut buf = [0u8; 256];
        let peek = view.peek(view.tail(), &mut buf).expect("fits");
        assert_eq!((peek.len(), peek.end()), (56, 7));
        assert!(!peek.is_corrupt());
        let records: Vec<_> = Records::new(&buf[..peek.len()])
            .collect::<Result<_, _>>()
            .expect("valid");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].args().collect::<Vec<_>>(), [1, 2, 3]);
        // Nothing was stored: the records are still pending, and a peek from the end is empty.
        assert_eq!(ring.pending(), 7);
        assert!(view.peek(peek.end(), &mut buf).expect("empty").is_empty());

        ring.commit_to(peek.end()).expect("a boundary");
        assert_eq!((ring.pending(), view.tail()), (0, 7));
    }

    #[test]
    fn view_peek_starts_at_tail_at_least() {
        let mem = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&mem[..]).expect("valid");
        ring.push(EventId::new(1), 1, &[]).expect("room");
        ring.push(EventId::new(2), 2, &[]).expect("room");
        ring.commit_to(2).expect("a boundary");
        let view = View::new(&mem[..]).expect("valid header");
        let mut buf = [0u8; 64];
        let peek = view.peek(0, &mut buf).expect("fits");
        assert_eq!((peek.len(), peek.end()), (16, 4));
    }

    #[test]
    fn commit_to_checks_the_position() {
        let mem = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&mem[..]).expect("valid");
        ring.push(EventId::new(1), 1, &[7, 7]).expect("room");
        ring.push(EventId::new(2), 2, &[]).expect("room");
        assert_eq!(
            ring.commit_to(7),
            Err(CommitError::BeyondHead { end: 7, head: 6 })
        );
        for inside in [1, 3, 5] {
            assert_eq!(
                ring.commit_to(inside),
                Err(CommitError::NotBoundary { end: inside })
            );
        }
        assert_eq!(ring.pending(), 6);
        ring.commit_to(4).expect("a boundary");
        assert_eq!(ring.pending(), 2);
        // Old positions are no-ops, never moving the ring back.
        ring.commit_to(0).expect("old");
        ring.commit_to(4).expect("old");
        assert_eq!(ring.pending(), 2);
        ring.commit_to(6).expect("a boundary");
        assert_eq!((ring.pending(), ring.lost()), (0, 0));
    }

    #[test]
    fn commit_to_drops_corrupt_records() {
        let mem = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&mem[..]).expect("valid");
        ring.push(EventId::new(1), 1, &[]).expect("room");
        ring.push(EventId::new(1), 2, &[]).expect("room");
        mem[SHARED_HEADER_WORDS].store(u64::MAX, Ordering::Relaxed);
        let view = View::new(&mem[..]).expect("valid header");
        let mut buf = [0u8; 64];
        let peek = view.peek(0, &mut buf).expect("stops");
        assert!(peek.is_corrupt());
        assert_eq!((peek.len(), peek.end()), (0, 4));
        ring.commit_to(peek.end()).expect("dropped");
        assert_eq!((ring.pending(), ring.lost()), (0, 1));
    }

    #[test]
    fn a_corrupt_peek_reports_where_its_verified_records_end() {
        let mem = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&mem[..]).expect("valid");
        for ts in 1..=3 {
            ring.push(EventId::new(1), ts, &[]).expect("room");
        }
        // The second two-word record no longer decodes.
        mem[SHARED_HEADER_WORDS + 2].store(u64::MAX, Ordering::Relaxed);
        let view = View::new(&mem[..]).expect("valid header");
        let mut buf = [0u8; 64];
        let peek = view.peek(0, &mut buf).expect("stops");
        assert!(peek.is_corrupt());
        assert_eq!((peek.len(), peek.end(), peek.verified_end()), (16, 6, 2));
        // A whole copy's verified end is its end.
        let mem = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&mem[..]).expect("valid");
        ring.push(EventId::new(1), 1, &[]).expect("room");
        let peek = View::new(&mem[..])
            .expect("valid")
            .peek(0, &mut buf)
            .expect("copies");
        assert_eq!((peek.end(), peek.verified_end()), (2, 2));
    }

    #[test]
    fn view_checks_the_header() {
        let mem = words(SHARED_HEADER_WORDS + 64);
        assert_eq!(View::new(&mem[..]).map(|_| ()), Err(ViewError::BadMagic));
        let _ring = Ring::shared(&mem[..]).expect("valid");
        assert!(View::new(&mem[..]).is_ok());
        // Fewer words than the header says.
        assert_eq!(
            View::new(&mem[..SHARED_HEADER_WORDS + 32]).map(|_| ()),
            Err(ViewError::BadLength {
                data_words: 64,
                available: 32
            })
        );
        assert_eq!(
            View::new(&mem[..SHARED_HEADER_WORDS]).map(|_| ()),
            Err(ViewError::BadMagic)
        );
        mem[VERSION_WORD].store(2, Ordering::Relaxed);
        assert_eq!(
            View::new(&mem[..]).map(|_| ()),
            Err(ViewError::Version { found: 2 })
        );
    }

    #[test]
    fn view_follows_records_around_the_end() {
        let mem = words(SHARED_HEADER_WORDS + 16);
        let ring = Ring::shared(&mem[..]).expect("valid");
        let view = View::new(&mem[..]).expect("valid header");
        let mut buf = [0u8; 128];
        for i in 0..100u64 {
            ring.push(EventId::new(3), i, &[i, !i, i * 3])
                .expect("room");
            let peek = view.peek(view.tail(), &mut buf).expect("fits");
            let record = Records::new(&buf[..peek.len()])
                .next()
                .expect("one record")
                .expect("valid");
            assert_eq!(record.timestamp(), i);
            assert_eq!(record.args().collect::<Vec<_>>(), [i, !i, i * 3]);
            ring.commit_to(peek.end()).expect("a boundary");
        }
        assert_eq!(ring.lost(), 0);
    }

    /// Words that run `hook` the first time word `at` is read, to act between two loads of a
    /// copy.
    struct Hooked<'a, F: Fn()> {
        words: &'a [AtomicU64],
        at: usize,
        fired: core::sync::atomic::AtomicBool,
        hook: F,
    }

    impl<F: Fn()> Words for Hooked<'_, F> {
        fn len(&self) -> usize {
            self.words.len()
        }

        fn word(&self, index: usize) -> &AtomicU64 {
            if index == self.at && !self.fired.swap(true, Ordering::Relaxed) {
                (self.hook)();
            }
            &self.words[index]
        }
    }

    /// Another reader drains the ring while a view copies it, and the producer reuses the
    /// words: the view retries instead of returning a record made of two.
    #[test]
    fn view_retries_when_overtaken() {
        let mem = words(SHARED_HEADER_WORDS + 16);
        let ring = Ring::shared(&mem[..]).expect("valid");
        // Two 6-word records: 12 of the 16 data words.
        ring.push(EventId::new(1), 10, &[1, 1, 1, 1]).expect("room");
        ring.push(EventId::new(2), 20, &[2, 2, 2, 2]).expect("room");
        let view = View::new(Hooked {
            words: &mem[..],
            // The first record's last argument: the copy is half done.
            at: SHARED_HEADER_WORDS + 5,
            fired: core::sync::atomic::AtomicBool::new(false),
            hook: || {
                let mut drained = [0u8; 256];
                assert_eq!(ring.read(&mut drained), Ok(96));
                // Positions 12..24 wrap over the first record's words.
                ring.push(EventId::new(3), 30, &[3, 3, 3, 3]).expect("room");
                ring.push(EventId::new(4), 40, &[4, 4, 4, 4]).expect("room");
            },
        })
        .expect("valid header");
        let mut buf = [0u8; 256];
        let peek = view.peek(0, &mut buf).expect("fits");
        assert!(!peek.is_corrupt());
        assert_eq!(peek.end(), 24);
        let records: Vec<_> = Records::new(&buf[..peek.len()])
            .collect::<Result<_, _>>()
            .expect("valid");
        let seen: Vec<_> = records
            .iter()
            .map(|r| (r.event(), r.timestamp(), r.args().collect::<Vec<_>>()))
            .collect();
        assert_eq!(
            seen,
            [
                (EventId::new(3), 30, std::vec![3; 4]),
                (EventId::new(4), 40, std::vec![4; 4]),
            ]
        );
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn heap_ring() {
        let ring = Ring::with_capacity(1024).expect("valid");
        assert_eq!(ring.capacity(), 1024);
        assert!(Ring::with_capacity(1000).is_err());
    }

    /// A producer thread pushes sequenced records as fast as it can while a consumer drains them:
    /// every record arrives whole and in order, and the gaps add up to the lost count.
    #[test]
    fn concurrent_producer_and_consumer() {
        producer_and_consumer(WhenFull::DropNewest);
    }

    /// As `concurrent_producer_and_consumer`, with the producer overwriting the records the
    /// consumer has not read yet.
    #[test]
    fn concurrent_overwriting_producer_and_consumer() {
        producer_and_consumer(WhenFull::OverwriteOldest);
    }

    /// A mapped reader (a [`View`] that commits through [`Ring::commit_to`]) keeps up with a
    /// producer that overwrites what it has not read: every record it gets is whole and in
    /// order, and commits it passes back are accepted.
    #[test]
    fn concurrent_overwriting_producer_and_view() {
        const RECORDS: u64 = 300_000;
        let mem = words(SHARED_HEADER_WORDS + 1024);
        let ring = Ring::shared(&mem[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        let view = View::new(&mem[..]).expect("valid header");
        let done = core::sync::atomic::AtomicBool::new(false);
        let seen = std::thread::scope(|s| {
            s.spawn(|| {
                for seq in 0..RECORDS {
                    let n = (seq % 9) as usize;
                    let args = [seq, !seq, seq, seq, seq, seq, seq, seq];
                    ring.push(EventId::new(1), seq, &args[..n])
                        .expect("an overwriting ring is never full");
                }
                done.store(true, Ordering::Release);
            });
            let mut buf = [0u8; 2048];
            let (mut seen, mut next, mut from) = (0u64, 0u64, 0u64);
            loop {
                let finished = done.load(Ordering::Acquire);
                let peek = view
                    .peek(from, &mut buf)
                    .expect("a 2 KiB buffer holds any record");
                for record in Records::new(&buf[..peek.len()]) {
                    let record = record.expect("whole, valid records");
                    let seq = record.timestamp();
                    assert!(seq >= next, "out of order: {seq} after {next}");
                    assert_eq!(record.arg_count(), (seq % 9) as usize);
                    if let Some(a) = record.arg(1) {
                        assert_eq!(a, !seq);
                    }
                    next = seq + 1;
                    seen += 1;
                }
                ring.commit_to(peek.end())
                    .expect("a record boundary within head");
                from = peek.end();
                if finished && peek.is_empty() {
                    break;
                }
            }
            seen
        });
        assert!(seen > 0);
        assert!(seen + ring.lost() >= RECORDS);
    }

    fn producer_and_consumer(when_full: WhenFull) {
        const RECORDS: u64 = 1_000_000;
        let ring = Ring::new(words(1024)).expect("valid").when_full(when_full);
        let done = core::sync::atomic::AtomicBool::new(false);

        let (seen, gaps) = std::thread::scope(|s| {
            s.spawn(|| {
                for seq in 0..RECORDS {
                    // Vary the length so records straddle the end at every offset.
                    let n = (seq % 9) as usize;
                    let args = [seq, !seq, seq ^ 0x5a5a, seq, seq, seq, seq, seq];
                    let _ = ring.push(EventId::new((seq % 7) as u16), seq, &args[..n]);
                }
                done.store(true, Ordering::Release);
            });

            let mut buf = [0u8; 4096];
            let mut seen = 0u64;
            let mut gaps = 0u64;
            let mut next = 0u64;
            loop {
                let finished = done.load(Ordering::Acquire);
                let len = ring
                    .read(&mut buf)
                    .expect("a 4 KiB buffer holds any record");
                for record in Records::new(&buf[..len]) {
                    let record = record.expect("whole, valid records");
                    let seq = record.timestamp();
                    assert!(seq >= next, "out of order: {seq} after {next}");
                    gaps += seq - next;
                    next = seq + 1;
                    seen += 1;
                    assert_eq!(record.event(), EventId::new((seq % 7) as u16));
                    assert_eq!(record.arg_count(), (seq % 9) as usize);
                    if let Some(a) = record.arg(1) {
                        assert_eq!(a, !seq);
                    }
                }
                if finished && len == 0 {
                    break;
                }
            }
            gaps += RECORDS - next;
            (seen, gaps)
        });

        match when_full {
            WhenFull::OverwriteOldest => {
                // A record overwritten after the consumer checked its copy is both seen and
                // counted as lost.
                assert!(seen + ring.lost() >= RECORDS);
                assert!(gaps <= ring.lost());
            }
            _ => {
                assert_eq!(seen + ring.lost(), RECORDS);
                assert_eq!(gaps, ring.lost());
            }
        }
        assert!(seen > 0);
    }
}
