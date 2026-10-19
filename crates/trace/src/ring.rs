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
//! once the copy has been passed on, so a reader that fails to deliver records keeps them. A full
//! ring drops the new record (it does not overwrite old ones, which would
//! need the producer to move `tail`).
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
//!   before storing `tail`.
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
use core::sync::atomic::{AtomicU64, Ordering};

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
    storage: S,
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
        let tail = self.storage.tail().load(Ordering::Relaxed);
        if end <= tail {
            return Ok(());
        }
        let head = self.storage.head().load(Ordering::Acquire);
        if end > head {
            return Err(CommitError::BeyondHead { end, head });
        }
        let mut pos = tail;
        while pos < end {
            let Ok(header) = Header::decode(self.slot(pos).load(Ordering::Relaxed)) else {
                // The reader met the same undecodable record and stopped there, with a
                // corrupt peek: drop what it covers, as [`Ring::commit`] does.
                self.count_lost();
                break;
            };
            pos = pos.saturating_add(header.words() as u64);
            if pos > end {
                return Err(CommitError::NotBoundary { end });
            }
        }
        self.storage.tail().fetch_max(end, Ordering::Release);
        Ok(())
    }
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

    /// Appends the record of `event` at `timestamp` with `args`. Only the producer may call this.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] when the ring has no room (the record is counted as lost), and
    /// [`PushError::TooManyArgs`] when there are more than [`MAX_ARGS`](crate::MAX_ARGS)
    /// arguments.
    pub fn push(&self, event: EventId, timestamp: u64, args: &[u64]) -> Result<(), PushError> {
        let header = Header::new(event, args.len()).ok_or(PushError::TooManyArgs)?;
        let words = header.words() as u64;
        // Only the producer stores `head`, so a relaxed load sees its own last store.
        let head = self.storage.head().load(Ordering::Relaxed);
        let tail = self.storage.tail().load(Ordering::Acquire);
        if head.wrapping_sub(tail).saturating_add(words) > self.capacity() as u64 {
            self.count_lost();
            return Err(PushError::Full);
        }
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
        // Only the consumer stores `tail`.
        let tail = self.storage.tail().load(Ordering::Relaxed);
        self.peek_from(tail, out)
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
    /// # Errors
    ///
    /// As [`Ring::peek`].
    pub fn peek(&self, from: u64, out: &mut [u8]) -> Result<Peek, ReadError> {
        self.ring.peek_from(from.max(self.tail()), out)
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
        const RECORDS: u64 = 1_000_000;
        let ring = Ring::new(words(1024)).expect("valid");
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

        assert_eq!(seen + ring.lost(), RECORDS);
        assert_eq!(gaps, ring.lost());
        assert!(seen > 0);
    }
}
