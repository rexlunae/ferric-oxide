//! [`Observer`], a reader of a shared ring that keeps its own position and stores nothing.

use core::fmt;
use core::sync::atomic::{Ordering, fence};

use super::{
    POLICY_WORD, Peek, ReadError, Ring, Shared, Storage, ViewError, WhenFull, Words, keep_from,
};
use crate::record::MAX_RECORD_WORDS;

/// How many times [`Observer::observe`] copies again after the producer overwrote the start
/// of its copy, before it gives up on what the ring holds and resumes at `head`.
pub const OBSERVE_TRIES: usize = 8;

/// A reader of a shared ring that keeps its own position and stores nothing, for a host
/// that reads a guest's rings out of its RAM ([`host`](crate::host)).
///
/// A [`View`](super::View) starts each copy at the ring's `tail` when `tail` is ahead of it,
/// because the `tail` it reads belongs to the reader it hands positions back to. An observer
/// has no such reader: other readers (inside the guest) may consume the ring meanwhile, moving
/// `tail` freely, and what they consume is still there for the observer until the producer
/// overwrites it. So it starts where its own last copy ended, and tells words the producer has
/// overwritten from words someone else has read by the producer's positions alone:
///
/// - A push that starts at `head` writes the slots of positions `head - capacity` up to
///   `head + MAX_RECORD_WORDS - capacity` (one record at most); positions from there on hold
///   what was written there. Every push before it ended below `head`. So, with `head` loaded
///   after a copy, every copied word from position `head + MAX_RECORD_WORDS - capacity` on
///   is the one written at that position: the producer stores a record's words after a
///   release fence, so a copy that read any word of a later push also sees (after its own
///   acquire fence) the `head` that push started at.
/// - Before a push of a ring that overwrites its oldest records
///   ([`WhenFull::OverwriteOldest`], as its [`POLICY_WORD`] says) overwrites any word, the
///   producer moves the ring's `oldest` past the records the word belongs to, whatever any
///   reader has consumed, and the same release fence orders that store before the
///   overwriting ones. Only the producer stores `oldest`. So every copied word from the
///   `oldest` loaded after the copy on is whole, too. A ring that drops new records
///   ([`WhenFull::DropNewest`]) reuses only words a reader consumed, so for it `tail` is that
///   bound instead, and the producer does not maintain `oldest` (keeping its push cheap).
///
/// A copy that starts at or after the lower of the two bounds is whole. One that does not was
/// overtaken: the observer fell behind by more than the ring holds. It keeps the records from
/// the new `oldest` (a record boundary) on, when its walk lands there, and otherwise copies
/// again from `oldest`. A copy never starts before the `oldest` loaded ahead of it, so an
/// observer that fell behind resumes at the oldest record the ring still holds, consumed by a
/// guest reader or not. What it passed over is reported as [`Observed::skipped`]: exactly the
/// words the producer overwrote before the observer read them.
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
    /// copied: the producer overwrote them first. What other readers consumed is never
    /// skipped.
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

    /// The ring's `tail`: what readers in the guest have consumed up to. An observer does not
    /// depend on it.
    pub fn tail(&self) -> u64 {
        let tail = self.ring.storage.tail().load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        tail
    }

    /// The ring's `oldest`: the first record the producer has not overwritten, where an
    /// observer that fell behind resumes. The records from it up to [`Observer::head`] are what
    /// an observer starting at 0 can still read.
    pub fn oldest(&self) -> u64 {
        let oldest = self.ring.storage.oldest().load(Ordering::Relaxed);
        // Pairs with the release fence in `Ring::push`.
        fence(Ordering::Acquire);
        oldest
    }

    /// The producer's policy for records that do not fit, from the ring's [`POLICY_WORD`].
    pub fn full_policy(&self) -> WhenFull {
        WhenFull::from_word(
            self.ring
                .storage
                .words
                .word(POLICY_WORD)
                .load(Ordering::Acquire),
        )
    }

    /// The bound below which the producer may have reused words, loaded after a copy: the
    /// ring's `oldest` when it overwrites, which only the producer moves; its `tail` when it
    /// drops new records, since it then reuses only words a reader consumed.
    fn reuse_bound(&self, policy: WhenFull) -> u64 {
        match policy {
            WhenFull::OverwriteOldest => self.ring.storage.oldest().load(Ordering::Relaxed),
            WhenFull::DropNewest => self.ring.storage.tail().load(Ordering::Relaxed),
        }
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
        let policy = self.full_policy();
        for _ in 0..OBSERVE_TRIES {
            let head = self.head();
            if from > head {
                return Err(ObserveError::Ahead { from, head });
            }
            let start = match policy {
                // `oldest` is exact: never start before the oldest record still whole.
                WhenFull::OverwriteOldest => self.oldest().clamp(from, head),
                // `tail` is only a bound: records a reader consumed may still be whole, so
                // start at `tail` only once `from` may have been reused.
                WhenFull::DropNewest if from < first_whole(head, capacity) => {
                    self.tail().clamp(from, head)
                }
                WhenFull::DropNewest => from,
            };
            let copy = self.ring.copy_records(start, head, out);
            // Pairs with the release fence in `Ring::push`: a copied word of a later push
            // makes that push's `head`, `tail` and `oldest` visible now.
            fence(Ordering::Acquire);
            let head_after = self.ring.storage.head().load(Ordering::Relaxed);
            let oldest_after = self.reuse_bound(policy);
            if start >= first_whole(head_after, capacity).min(oldest_after) {
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
                .and_then(|copy| keep_from(start, oldest_after, out, copy))
            {
                return Ok(Observed {
                    start: oldest_after,
                    skipped: oldest_after - from,
                    peek,
                });
            }
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
    use crate::ring::tests::Hooked;
    use crate::ring::{OLDEST_WORD, SHARED_HEADER_WORDS, TAIL_WORD, WhenFull};
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

    /// The reviewer's case: forty 3-word records into 64 words, then a reader in the guest
    /// drains the ring. An observer that starts at 0 has been overtaken; it resumes at the
    /// oldest record the producer has not overwritten, not at the drained `tail`, and gets
    /// the 21 records the ring still holds.
    #[test]
    fn an_overtaken_observer_resumes_at_oldest_after_a_drain() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        for seq in 0..40 {
            push(&ring, seq);
        }
        let mut buf = [0u8; 1024];
        assert_eq!(ring.read(&mut buf), Ok(21 * 24));
        assert_eq!((observer.tail(), observer.head()), (120, 120));
        // The last push overwrote up to position 56: the first record kept starts at 57.
        assert_eq!(observer.oldest(), 57);
        let (seqs, end, skipped) = drain(&observer, 0);
        assert_eq!(seqs, (19..40).collect::<Vec<_>>());
        assert_eq!((end, skipped), (120, 57));
    }

    /// Readers in the guest consume between two polls of an observer that the producer
    /// overtakes in between: the second poll still gets every record the ring holds.
    #[test]
    fn consumption_between_polls_hides_nothing() {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        let observer = Observer::new(&memory[..]).expect("a shared ring");
        let mut buf = [0u8; 1024];
        for seq in 0..10 {
            push(&ring, seq);
        }
        let (first, from, _) = drain(&observer, 0);
        assert_eq!(first, (0..10).collect::<Vec<_>>());
        assert_eq!(from, 30);
        // The guest drains, the producer laps the observer, the guest drains again.
        ring.read(&mut buf).expect("fits");
        for seq in 10..35 {
            push(&ring, seq);
            if seq % 4 == 0 {
                ring.read(&mut buf).expect("fits");
            }
        }
        ring.read(&mut buf).expect("fits");
        assert_eq!(observer.tail(), observer.head());
        let oldest = observer.oldest();
        assert_eq!(oldest, 105 - 63);
        let (seqs, end, skipped) = drain(&observer, from);
        assert_eq!(seqs, (14..35).collect::<Vec<_>>());
        assert_eq!((end, skipped), (105, oldest - from));
        // With no overwrite since, a poll from the end is empty and skips nothing.
        let (seqs, _, skipped) = drain(&observer, end);
        assert!(seqs.is_empty());
        assert_eq!(skipped, 0);
    }

    /// Fills a 64-word ring with 21 records, which a guest reader drains, and observes it
    /// from 0 through words that push two more records (overwriting the first two, positions
    /// 0..6) when data word `at` is read: the copy's start, the words skipped and the
    /// sequence numbers copied.
    fn overtaken_after_a_drain(at: usize) -> (u64, u64, Vec<u64>) {
        let memory = words(SHARED_HEADER_WORDS + 64);
        let ring = Ring::shared(&memory[..])
            .expect("valid")
            .when_full(WhenFull::OverwriteOldest);
        for seq in 0..21 {
            push(&ring, seq);
        }
        let mut buf = [0u8; 1024];
        assert_eq!(ring.read(&mut buf), Ok(21 * 24));
        let observer = Observer::new(Hooked {
            words: &memory[..],
            at: SHARED_HEADER_WORDS + at,
            fired: AtomicBool::new(false),
            // Positions 63..69 wrap to slots 63 and 0..5.
            hook: || {
                push(&ring, 21);
                push(&ring, 22);
            },
        })
        .expect("a shared ring");
        let mut out = [0u8; 1024];
        let copy = observer.observe(0, &mut out).expect("observes");
        assert!(!copy.is_corrupt());
        assert_eq!(memory[OLDEST_WORD].load(Ordering::Relaxed), 6);
        assert_eq!(memory[TAIL_WORD].load(Ordering::Relaxed), 63);
        (copy.start(), copy.skipped(), seqs(&out[..copy.len()]))
    }

    /// The producer overwrites the first two records after the copy read them: the observer
    /// keeps the rest of the copy, from the new `oldest`, though the drained `tail` is ahead.
    #[test]
    fn an_overwrite_behind_the_copy_keeps_from_oldest() {
        let (start, skipped, seqs) = overtaken_after_a_drain(10);
        assert_eq!((start, skipped), (6, 6));
        assert_eq!(seqs, (2..21).collect::<Vec<_>>());
    }

    /// The producer overwrites the first record while the copy reads it: the observer copies
    /// again from the new `oldest`, and gets the new records too.
    #[test]
    fn an_overwrite_under_the_copy_copies_again_from_oldest() {
        let (start, skipped, seqs) = overtaken_after_a_drain(2);
        assert_eq!((start, skipped), (6, 6));
        assert_eq!(seqs, (2..23).collect::<Vec<_>>());
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
    fn a_scribbled_counter_cannot_send_the_observer_past_head() {
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
        // Nor can a scribbled `oldest`.
        memory[OLDEST_WORD].store(u64::MAX, Ordering::Relaxed);
        let copy = observer.observe(0, &mut buf).expect("observes");
        assert!(copy.end() <= observer.head());
    }

    /// A producer on another thread overwrites the ring as fast as it can while the observer
    /// follows it: every copy holds whole records in order, and a gap in the sequence only
    /// comes with words reported skipped.
    #[test]
    fn a_racing_producer_never_tears_a_copy() {
        race(false);
    }

    /// As `a_racing_producer_never_tears_a_copy`, with a reader in the guest draining the
    /// ring on a third thread, moving `tail` past records the observer has not read: it
    /// still gets every record the producer did not overwrite first.
    #[test]
    fn a_racing_producer_and_consumer_never_tear_a_copy() {
        race(true);
    }

    fn race(consume: bool) {
        // Miri explores weak-memory orderings too, slowly: fewer records keep it short.
        const RECORDS: u64 = if cfg!(miri) { 2_000 } else { 200_000 };
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
            if consume {
                scope.spawn(|| {
                    let mut buf = [0u8; 64 * WORD_BYTES];
                    while !done.load(Ordering::Acquire) {
                        ring.read(&mut buf).expect("fits");
                    }
                });
            }
            let mut buf = [0u8; 256 * WORD_BYTES];
            let (mut from, mut last) = (0, None::<u64>);
            loop {
                let finished = done.load(Ordering::Acquire);
                let copy = observer.observe(from, &mut buf).expect("observes");
                assert!(!copy.is_corrupt(), "never torn");
                assert!(copy.start() >= from);
                assert_eq!(copy.skipped(), copy.start() - from);
                let copied = seqs(&buf[..copy.len()]);
                if let Some(&first) = copied.first() {
                    // Three words a record: the skip is exactly the records missing.
                    assert_eq!(
                        first * 3,
                        copy.start(),
                        "the copy starts at its first record"
                    );
                }
                for seq in copied {
                    if let Some(prev) = last {
                        assert!(seq > prev, "in order: {prev} then {seq}");
                    }
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
