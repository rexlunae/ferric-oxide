//! [`Observer`], a reader of a shared ring that keeps its own position and stores nothing.

use core::fmt;
use core::sync::atomic::{Ordering, fence};

use super::{Peek, ReadError, Ring, Shared, Storage, ViewError, Words, keep_from};
use crate::record::MAX_RECORD_WORDS;

/// How many times [`Observer::observe`] copies again after the producer overwrote the start
/// of its copy, before it gives up on what the ring holds and resumes at `head`.
pub const OBSERVE_TRIES: usize = 8;

/// A reader of a shared ring that keeps its own position and stores nothing, for a host
/// that reads a guest's rings out of its RAM ([`host`](crate::host)).
///
/// A [`View`](super::View) starts each copy at the ring's `tail` when `tail` is ahead of it,
/// because the `tail` it reads belongs to the reader it hands positions back to. An observer
/// has no such reader: other readers (inside the guest) may consume the ring meanwhile, and
/// what they consume is still there for the observer. So it starts where its own last copy
/// ended, and tells words the producer has overwritten from words someone else has read by
/// the producer's position alone:
///
/// - A push that starts at `head` writes the slots of positions `head - capacity` up to
///   `head + MAX_RECORD_WORDS - capacity` (one record at most); positions from there on hold
///   what was written there. Every push before it ended below `head`. So, with `head` loaded
///   after a copy, every copied word from position `head + MAX_RECORD_WORDS - capacity` on
///   is the one written at that position: the producer stores a record's words after a
///   release fence, so a copy that read any word of a later push also sees (after its own
///   acquire fence) the `head` that push started at.
/// - The producer also moves `tail` past the words it is about to reuse before it reuses
///   them ([`WhenFull::OverwriteOldest`](super::WhenFull::OverwriteOldest)), and with
///   [`WhenFull::DropNewest`](super::WhenFull::DropNewest) it reuses only words a reader
///   moved `tail` past; so every copied word from the `tail` loaded after the copy on is
///   whole, too.
///
/// A copy that starts at or after the lower of the two bounds is whole. One that does not was
/// overtaken: the observer fell behind by more than the ring holds. It keeps the records
/// from the new `tail` (a record boundary) on, when its walk lands there, and otherwise copies
/// again from `tail`, as [`View::peek`](super::View::peek) does. What it passed over is
/// reported as [`Observed::skipped`].
///
/// That recovery takes `tail` as the oldest record still whole, which holds when only the
/// producer moves it. A reader that consumes the ring moves it to records it has read but
/// the producer has not overwritten, and an overtaken observer then skips those too. So an
/// observer that may fall behind should be the ring's only reader; the kernel refuses
/// `cpu/<n>` reads and commits of its host-visible rings for that reason.
///
/// Every load is relaxed, with acquire fences where ordering matters, because the memory may
/// be mapped read-only, where only relaxed atomic loads are allowed.
#[derive(Debug)]
pub struct Observer<W> {
    ring: Ring<Shared<W>>,
}

/// Records [`Observer::observe`] copied.
#[derive(Debug)]
pub struct Observed {
    start: u64,
    skipped: u64,
    peek: Peek,
}

impl Observed {
    /// The number of bytes copied.
    pub fn len(&self) -> usize {
        self.peek.len()
    }

    /// Whether nothing was copied.
    pub fn is_empty(&self) -> bool {
        self.peek.is_empty()
    }

    /// The position of the first record copied.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// The position after the last whole record copied: where the next
    /// [`Observer::observe`] starts. For a corrupt copy it is where the record that does not
    /// decode starts.
    pub fn end(&self) -> u64 {
        self.peek.verified_end()
    }

    /// The words between the position asked for and [`Observed::start`] that were not
    /// copied: the producer overwrote them first (or, once the observer had fallen behind,
    /// another reader had consumed them).
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Whether the copy stopped at a record that does not decode, which only a producer that
    /// breaks the ring's rules leaves. Nothing after it can be split into records: resume at
    /// [`Observer::head`].
    pub fn is_corrupt(&self) -> bool {
        self.peek.is_corrupt()
    }
}

/// Why [`Observer::observe`] copied nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObserveError {
    /// The position asked for is past the ring's `head`: the ring was made again (the guest
    /// booted again) or the position is from another ring.
    Ahead {
        /// The position asked for.
        from: u64,
        /// The ring's `head`.
        head: u64,
    },
    /// As [`Ring::peek`].
    Read(ReadError),
}

impl fmt::Display for ObserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObserveError::Ahead { from, head } => {
                write!(f, "position {from} is past the ring's head {head}")
            }
            ObserveError::Read(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl core::error::Error for ObserveError {}

impl<W: Words> Observer<W> {
    /// An observer of the shared ring in `words`, after checking its header.
    ///
    /// # Errors
    ///
    /// [`ViewError`] when the header is not one this reader understands.
    pub fn new(words: W) -> Result<Self, ViewError> {
        Ok(Self {
            ring: Ring::read_shared(words)?,
        })
    }

    /// The ring's `head`: the position after the last record the producer published.
    pub fn head(&self) -> u64 {
        let head = self.ring.storage.head().load(Ordering::Relaxed);
        // Pairs with the release store of `head` in `Ring::push`.
        fence(Ordering::Acquire);
        head
    }

    /// The ring's `tail`.
    pub fn tail(&self) -> u64 {
        let tail = self.ring.storage.tail().load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        tail
    }

    /// The records the producer dropped or overwrote.
    pub fn lost(&self) -> u64 {
        self.ring.lost()
    }

    /// The ring's size in data words.
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }

    /// Copies as many whole records as fit into `out`, from position `from` (0, or the
    /// [`Observed::end`] of the previous copy), and checks the copy against the producer
    /// overwriting it; see [`Observer`]. Nothing is stored.
    ///
    /// When the producer has overwritten records at `from`, the copy starts at the first
    /// record still whole, and [`Observed::skipped`] counts the words passed over. When it
    /// overtakes [`OBSERVE_TRIES`] copies in a row, nothing is copied and the result
    /// resumes at `head`, skipping everything.
    ///
    /// # Errors
    ///
    /// [`ObserveError::Ahead`] when `from` is past `head`, and [`ObserveError::Read`] when the
    /// first record does not fit in `out`.
    pub fn observe(&self, from: u64, out: &mut [u8]) -> Result<Observed, ObserveError> {
        let capacity = self.ring.capacity() as u64;
        let mut from_tail = false;
        for _ in 0..OBSERVE_TRIES {
            let head = self.head();
            if from > head {
                return Err(ObserveError::Ahead { from, head });
            }
            let start = if from_tail || from < first_whole(head, capacity) {
                self.tail().clamp(from, head)
            } else {
                from
            };
            let copy = self.ring.copy_records(start, head, out);
            // Pairs with the release fence in `Ring::push`: a copied word of a later push
            // makes that push's `head` and `tail` visible now.
            fence(Ordering::Acquire);
            let head_after = self.ring.storage.head().load(Ordering::Relaxed);
            let tail_after = self.ring.storage.tail().load(Ordering::Relaxed);
            if start >= first_whole(head_after, capacity).min(tail_after) {
                return copy
                    .map(|peek| Observed {
                        start,
                        skipped: start - from,
                        peek,
                    })
                    .map_err(ObserveError::Read);
            }
            if let Some(peek) = copy
                .ok()
                .and_then(|copy| keep_from(start, tail_after, out, copy))
            {
                return Ok(Observed {
                    start: tail_after,
                    skipped: tail_after - from,
                    peek,
                });
            }
            from_tail = true;
        }
        let head = self.head();
        Ok(Observed {
            start: head,
            skipped: head - from.min(head),
            peek: Peek {
                len: 0,
                end: head,
                verified_end: head,
                corrupt: false,
            },
        })
    }
}

/// The first position no push can be overwriting while the ring's `head` is `head`.
fn first_whole(head: u64, capacity: u64) -> u64 {
    head.saturating_add(MAX_RECORD_WORDS as u64)
        .saturating_sub(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventId;
    use crate::record::{Records, WORD_BYTES};
    use crate::ring::{SHARED_HEADER_WORDS, TAIL_WORD, WhenFull};
    use core::sync::atomic::{AtomicBool, AtomicU64};
    use std::vec::Vec;

    fn words(n: usize) -> Vec<AtomicU64> {
        (0..n).map(|_| AtomicU64::new(0)).collect()
    }

    /// The first argument of each record in `bytes`.
    fn seqs(bytes: &[u8]) -> Vec<u64> {
        Records::new(bytes)
            .map(|r| {
                r.expect("whole records")
                    .args()
                    .next()
                    .expect("an argument")
            })
            .collect()
    }

    /// Observes from `from` until a copy is empty: the sequence numbers, the end and the
    /// words skipped.
    fn drain<W: Words>(observer: &Observer<W>, mut from: u64) -> (Vec<u64>, u64, u64) {
        let mut buf = [0u8; 4096];
        let (mut all, mut skipped) = (Vec::new(), 0);
        loop {
            let copy = observer.observe(from, &mut buf).expect("observes");
            skipped += copy.skipped();
            if copy.is_empty() {
                return (all, copy.end(), skipped);
            }
            all.extend(seqs(&buf[..copy.len()]));
            from = copy.end();
        }
    }

    fn push(ring: &Ring<Shared<&[AtomicU64]>>, seq: u64) {
        // Three-word records: header, timestamp, sequence number.
        ring.push(EventId::new(1), seq, &[seq]).expect("room");
    }

    #[test]
    fn follows_through_wrap_around() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        let mut from = 0;
        let mut seen = Vec::new();
        // 15 records of 3 words per round: the second round wraps round the 64 words.
        for round in 0..4 {
            for seq in round * 15..(round + 1) * 15 {
                push(&ring, seq);
            }
            let (seqs, end, skipped) = drain(&observer, from);
            assert_eq!(skipped, 0, "round {round} kept up");
            seen.extend(seqs);
            from = end;
        }
        assert_eq!(seen, (0..60).collect::<Vec<_>>());
        assert_eq!(from, 180);
        assert_eq!(observer.head(), 180);
    }

    #[test]
    fn another_readers_tail_does_not_hide_records() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        for seq in 0..10 {
            push(&ring, seq);
        }
        // A reader in the guest consumes everything before the observer looks.
        let mut buf = [0u8; 512];
        assert_eq!(ring.read(&mut buf), Ok(240));
        assert_eq!(observer.tail(), 30);
        let (seqs, end, skipped) = drain(&observer, 0);
        assert_eq!(seqs, (0..10).collect::<Vec<_>>());
        assert_eq!((end, skipped), (30, 0));
    }

    #[test]
    fn overwritten_records_are_skipped() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        for seq in 0..5 {
            push(&ring, seq);
        }
        let (first, from, _) = drain(&observer, 0);
        assert_eq!(first, [0, 1, 2, 3, 4]);
        // 40 records (120 words) into 64 words, unobserved: the oldest are overwritten.
        for seq in 5..45 {
            push(&ring, seq);
        }
        let (seqs, end, skipped) = drain(&observer, from);
        let oldest = *seqs.first().expect("records");
        assert_eq!(
            seqs,
            (oldest..45).collect::<Vec<_>>(),
            "no gap after the skip"
        );
        assert_eq!(skipped, (oldest - 5) * 3, "the skip is in whole records");
        assert!(seqs.len() >= 18, "most of the ring is kept: {seqs:?}");
        assert_eq!(end, 135);
        assert!(ring.lost() > 0);
    }

    #[test]
    fn a_position_past_head_is_refused() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..]).expect("valid");
        push(&ring, 1);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        let mut buf = [0u8; 64];
        assert_eq!(
            observer.observe(4, &mut buf).map(|o| o.len()),
            Err(ObserveError::Ahead { from: 4, head: 3 })
        );
        assert_eq!(
            observer.observe(0, &mut buf[..8]).map(|o| o.len()),
            Err(ObserveError::Read(ReadError::BufferTooSmall { needed: 24 }))
        );
    }

    #[test]
    fn a_corrupt_record_is_reported() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..]).expect("valid");
        for seq in 0..3 {
            push(&ring, seq);
        }
        // Scribble over the second record's header.
        memory[SHARED_HEADER_WORDS + 3].store(u64::MAX, Ordering::Relaxed);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        let mut buf = [0u8; 512];
        let copy = observer.observe(0, &mut buf).expect("observes");
        assert!(copy.is_corrupt());
        assert_eq!((copy.len(), copy.end()), (24, 3));
        assert_eq!(seqs(&buf[..copy.len()]), [0]);
    }

    #[test]
    fn a_scribbled_tail_cannot_send_the_observer_past_head() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        for seq in 0..40 {
            push(&ring, seq);
        }
        memory[TAIL_WORD].store(u64::MAX, Ordering::Relaxed);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        let mut buf = [0u8; 512];
        let copy = observer.observe(0, &mut buf).expect("observes");
        assert!(copy.end() <= observer.head());
    }

    /// A producer on another thread overwrites the ring as fast as it can while the observer
    /// follows it: every copy holds whole records in order, and a gap in the sequence only
    /// comes with words reported skipped.
    #[test]
    fn a_racing_producer_never_tears_a_copy() {
        const RECORDS: u64 = 200_000;
        let memory = words(SHARED_HEADER_WORDS + 256);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for seq in 0..RECORDS {
                    push(&ring, seq);
                }
                done.store(true, Ordering::Release);
            });
            let mut buf = [0u8; 256 * WORD_BYTES];
            let (mut from, mut last) = (0, None::<u64>);
            let mut skipped_since_last = false;
            loop {
                let finished = done.load(Ordering::Acquire);
                let copy = observer.observe(from, &mut buf).expect("observes");
                assert!(!copy.is_corrupt(), "never torn");
                assert!(copy.start() >= from);
                skipped_since_last |= copy.skipped() > 0;
                for seq in seqs(&buf[..copy.len()]) {
                    if let Some(prev) = last {
                        assert!(seq > prev, "in order: {prev} then {seq}");
                        assert!(
                            skipped_since_last || seq == prev + 1,
                            "a gap without a skip: {prev} then {seq}"
                        );
                    }
                    skipped_since_last = false;
                    last = Some(seq);
                }
                from = copy.end();
                if finished && copy.is_empty() {
                    break;
                }
            }
            assert_eq!(from, observer.head(), "followed to the end");
            assert!(last.is_some(), "records were delivered");
        });
    }
}
