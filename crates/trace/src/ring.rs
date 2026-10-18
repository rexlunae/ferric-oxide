//! A single-producer, single-consumer ring of records.
//!
//! The ring is a power-of-two slice of `AtomicU64` words and three counters: `head` (the next
//! word the producer writes), `tail` (the next word the consumer reads) and `lost` (records the
//! producer dropped because the ring was full). `head` and `tail` only grow; a word's slot is its
//! position modulo the length, so a record can wrap around the end of the slice.
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

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::EventId;
use crate::record::{Header, MAX_RECORD_WORDS, WORD_BYTES};

/// A ring over the words in `D` (a slice, a `Box<[AtomicU64]>`, ...).
#[derive(Debug)]
pub struct Ring<D> {
    head: AtomicU64,
    tail: AtomicU64,
    lost: AtomicU64,
    mask: u64,
    data: D,
}

/// Why a slice of words cannot be a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RingError {
    /// The length is not a power of two.
    NotPowerOfTwo {
        /// The length in words.
        len: usize,
    },
    /// The ring could not hold the longest record.
    TooSmall {
        /// The length in words.
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
        }
    }
}

impl core::error::Error for RingError {}

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

impl<D: AsRef<[AtomicU64]>> Ring<D> {
    /// An empty ring over `data`, whose current contents are ignored.
    ///
    /// # Errors
    ///
    /// [`RingError::NotPowerOfTwo`] or [`RingError::TooSmall`] when `data`'s length is not a
    /// power of two of at least [`MAX_RECORD_WORDS`] words.
    pub fn new(data: D) -> Result<Self, RingError> {
        let len = data.as_ref().len();
        if !len.is_power_of_two() {
            return Err(RingError::NotPowerOfTwo { len });
        }
        if len < MAX_RECORD_WORDS {
            return Err(RingError::TooSmall { len });
        }
        Ok(Self {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            mask: len as u64 - 1,
            data,
        })
    }

    /// The ring's size in words.
    pub fn capacity(&self) -> usize {
        self.data.as_ref().len()
    }

    /// The words written and not yet read.
    pub fn pending(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        head.saturating_sub(tail) as usize
    }

    /// The records dropped so far, because the ring was full or because the producer could not
    /// write them (see [`Ring::count_lost`]).
    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    /// Counts a record the producer could not push for a reason of its own.
    pub fn count_lost(&self) {
        self.lost.fetch_add(1, Ordering::Relaxed);
    }

    /// The word slot for position `pos`.
    fn slot(&self, pos: u64) -> &AtomicU64 {
        // `mask` is the length minus one, so the index is always in bounds.
        &self.data.as_ref()[(pos & self.mask) as usize]
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
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
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
        self.head.store(head.wrapping_add(words), Ordering::Release);
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
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
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
        self.tail.fetch_max(peek.end, Ordering::Release);
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

/// Records [`Ring::peek`] copied out of a ring and has not consumed yet.
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
impl Ring<alloc::boxed::Box<[AtomicU64]>> {
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
