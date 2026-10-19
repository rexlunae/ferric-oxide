//! The single-producer, single-consumer queues, over ring memory that the other side can
//! write at any time.
//!
//! A queue is a power-of-two number of slots of [`Entry`] words and two free-running 64-bit
//! indices in the header: the producer's tail and the consumer's head. A slot is its index
//! modulo the slot count. The producer writes an entry's words with relaxed stores, then
//! publishes them with a release store of the tail; the consumer loads the tail with acquire
//! ordering, copies the entry out, and gives the slot back with a release store of the head.
//!
//! The two sides do not trust each other. Each keeps its own index in its [`Producer`] or
//! [`Consumer`] and only ever stores it to the header; it loads only the other side's index,
//! and an index that is behind its own or more than a queue's length ahead is
//! [`RingError::Corrupt`]. Every access is atomic, so a peer writing anything anywhere cannot
//! cause undefined behaviour, only bad entries, which are checked where they are used.
//!
//! The memory is anything that implements [`Memory`]: a slice of `AtomicU64` in user space,
//! the kernel's frames page by page.

use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::abi::{self, Layout, NotifyMode, QueueLayout};
use crate::entry::Entry;

/// Ring memory: 64-bit words reached by index.
pub trait Memory {
    /// The word at `index`, or `None` past the end.
    fn word(&self, index: usize) -> Option<&AtomicU64>;

    /// The number of words.
    fn len(&self) -> usize;

    /// Whether there are no words.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Memory for [AtomicU64] {
    fn word(&self, index: usize) -> Option<&AtomicU64> {
        self.get(index)
    }

    fn len(&self) -> usize {
        <[AtomicU64]>::len(self)
    }
}

/// Why a queue cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RingError {
    /// The other side's index is behind this side's or more than the queue's length ahead.
    Corrupt {
        /// The other side's index as read.
        peer: u64,
        /// This side's index.
        own: u64,
    },
    /// The memory is smaller than the layout says.
    OutOfBounds {
        /// The word that is missing.
        word: usize,
    },
    /// A consumer was asked to give back more entries than are pending.
    Overrun {
        /// Entries asked for.
        count: u32,
        /// Entries pending.
        pending: u32,
    },
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt { peer, own } => write!(
                f,
                "corrupt ring: the other side's index {peer} is out of range of ours, {own}"
            ),
            Self::OutOfBounds { word } => {
                write!(f, "the ring memory has no word {word}")
            }
            Self::Overrun { count, pending } => {
                write!(f, "cannot release {count} entries, {pending} are pending")
            }
        }
    }
}

impl core::error::Error for RingError {}

/// Why an entry was not pushed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushError {
    /// The queue is full.
    Full,
    /// The queue cannot be used.
    Ring(RingError),
}

impl From<RingError> for PushError {
    fn from(err: RingError) -> Self {
        Self::Ring(err)
    }
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("the queue is full"),
            Self::Ring(_) => f.write_str("the queue cannot be used"),
        }
    }
}

impl core::error::Error for PushError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Full => None,
            Self::Ring(err) => Some(err),
        }
    }
}

/// The word at `index` of `mem`.
pub(crate) fn word<M: Memory + ?Sized>(mem: &M, index: usize) -> Result<&AtomicU64, RingError> {
    mem.word(index)
        .ok_or(RingError::OutOfBounds { word: index })
}

/// The first word of the slot for `index` in `queue`.
fn slot_base<E: Entry>(queue: &QueueLayout, index: u64) -> usize {
    // `entries` is a power of two of at most 8192, so the masked index and the product fit.
    let slot = (index & (u64::from(queue.entries) - 1)) as usize;
    queue.base.saturating_add(slot.saturating_mul(E::WORDS))
}

/// How far `ahead` is ahead of `behind`, if that is within `entries`.
fn distance(ahead: u64, behind: u64, entries: u32) -> Option<u32> {
    ahead
        .checked_sub(behind)
        .filter(|&d| d <= u64::from(entries))
        .map(|d| d as u32)
}

/// The producing side of a queue.
#[derive(Debug)]
pub struct Producer<E> {
    queue: QueueLayout,
    /// The index after the last entry written.
    tail: u64,
    /// The index after the last entry published.
    published: u64,
    _entry: PhantomData<fn(E)>,
}

impl<E: Entry> Producer<E> {
    /// The producer of an empty `queue`, as a ring starts.
    pub const fn new(queue: QueueLayout) -> Self {
        Self {
            queue,
            tail: 0,
            published: 0,
            _entry: PhantomData,
        }
    }

    /// The queue's layout.
    pub const fn queue(&self) -> &QueueLayout {
        &self.queue
    }

    /// The index after the last entry written, published or not.
    pub const fn tail(&self) -> u64 {
        self.tail
    }

    /// The consumer's head, checked against this side's tail.
    fn head<M: Memory + ?Sized>(&self, mem: &M) -> Result<u64, RingError> {
        let head = word(mem, self.queue.head)?.load(Ordering::Acquire);
        // The consumer cannot be past what was published, nor more than a queue behind.
        match distance(self.published, head, self.queue.entries) {
            Some(_) => Ok(head),
            None => Err(RingError::Corrupt {
                peer: head,
                own: self.published,
            }),
        }
    }

    /// The slots free for new entries.
    ///
    /// # Errors
    ///
    /// [`RingError`] when the consumer's head is corrupt or the memory too small.
    pub fn free<M: Memory + ?Sized>(&self, mem: &M) -> Result<u32, RingError> {
        let head = self.head(mem)?;
        let used = self.tail.saturating_sub(head);
        Ok(u32::try_from(u64::from(self.queue.entries).saturating_sub(used)).unwrap_or(0))
    }

    /// Writes `entry` into the next slot without publishing it; [`Producer::publish`] makes it
    /// visible, so a batch is published with one store.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] when no slot is free, [`PushError::Ring`] when the queue cannot be
    /// used.
    pub fn stage<M: Memory + ?Sized>(&mut self, mem: &M, entry: &E) -> Result<(), PushError> {
        if self.free(mem)? == 0 {
            return Err(PushError::Full);
        }
        let base = slot_base::<E>(&self.queue, self.tail);
        for (i, &value) in entry.to_words().as_ref().iter().enumerate() {
            word(mem, base.saturating_add(i))?.store(value, Ordering::Relaxed);
        }
        self.tail = self.tail.wrapping_add(1);
        Ok(())
    }

    /// Publishes every staged entry to the consumer.
    ///
    /// # Errors
    ///
    /// [`RingError::OutOfBounds`] when the memory has no tail word.
    pub fn publish<M: Memory + ?Sized>(&mut self, mem: &M) -> Result<(), RingError> {
        word(mem, self.queue.tail)?.store(self.tail, Ordering::Release);
        self.published = self.tail;
        Ok(())
    }

    /// Writes `entry` and publishes it.
    ///
    /// # Errors
    ///
    /// As [`Producer::stage`] and [`Producer::publish`].
    pub fn push<M: Memory + ?Sized>(&mut self, mem: &M, entry: &E) -> Result<(), PushError> {
        self.stage(mem, entry)?;
        self.publish(mem)?;
        Ok(())
    }
}

/// A [`Producer`] whose free slots can be set aside before the entries exist, so that work
/// accepted now is sure of room for its completions later, whatever is posted meanwhile.
///
/// The kernel side of a ring takes a chain only once [`Reserving::try_reserve`] has set aside
/// a CQ slot for each of its SQEs, and posts the chain's CQEs with [`Reserving::post`] when it
/// has run, synchronously or on the ring's worker.
#[derive(Debug)]
pub struct Reserving<E> {
    producer: Producer<E>,
    /// Slots set aside and not posted yet.
    reserved: u32,
}

impl<E: Entry> Reserving<E> {
    /// An empty `queue` with nothing set aside.
    pub const fn new(queue: QueueLayout) -> Self {
        Self {
            producer: Producer::new(queue),
            reserved: 0,
        }
    }

    /// The slots set aside and not posted yet.
    pub const fn reserved(&self) -> u32 {
        self.reserved
    }

    /// Sets aside `count` slots if that many are free and not set aside already; returns
    /// whether it did.
    ///
    /// # Errors
    ///
    /// [`RingError`] when the consumer's head is corrupt or the memory too small.
    pub fn try_reserve<M: Memory + ?Sized>(
        &mut self,
        mem: &M,
        count: u32,
    ) -> Result<bool, RingError> {
        let free = self.producer.free(mem)?.saturating_sub(self.reserved);
        if free < count {
            return Ok(false);
        }
        self.reserved = self.reserved.saturating_add(count);
        Ok(true)
    }

    /// Gives back `count` slots set aside for work that will not post after all.
    pub fn unreserve(&mut self, count: u32) {
        self.reserved = self.reserved.saturating_sub(count);
    }

    /// Writes `entries` into slots set aside before and publishes them together. The slots are
    /// no longer set aside afterwards, even when this fails.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] when fewer slots are set aside than `entries` (nothing is written),
    /// [`PushError::Ring`] when the queue cannot be used.
    pub fn post<M: Memory + ?Sized>(&mut self, mem: &M, entries: &[E]) -> Result<(), PushError> {
        let count = u32::try_from(entries.len()).map_err(|_| PushError::Full)?;
        if count > self.reserved {
            return Err(PushError::Full);
        }
        self.reserved -= count;
        for entry in entries {
            self.producer.stage(mem, entry)?;
        }
        self.producer.publish(mem)?;
        Ok(())
    }
}

/// The consuming side of a queue.
#[derive(Debug)]
pub struct Consumer<E> {
    queue: QueueLayout,
    /// The index of the next entry to take.
    head: u64,
    _entry: PhantomData<fn() -> E>,
}

impl<E: Entry> Consumer<E> {
    /// The consumer of an empty `queue`, as a ring starts.
    pub const fn new(queue: QueueLayout) -> Self {
        Self {
            queue,
            head: 0,
            _entry: PhantomData,
        }
    }

    /// The queue's layout.
    pub const fn queue(&self) -> &QueueLayout {
        &self.queue
    }

    /// The index of the next entry to take.
    pub const fn head(&self) -> u64 {
        self.head
    }

    /// The entries the producer has published and this side has not taken.
    ///
    /// # Errors
    ///
    /// [`RingError`] when the producer's tail is corrupt or the memory too small.
    pub fn pending<M: Memory + ?Sized>(&self, mem: &M) -> Result<u32, RingError> {
        let tail = word(mem, self.queue.tail)?.load(Ordering::Acquire);
        distance(tail, self.head, self.queue.entries).ok_or(RingError::Corrupt {
            peer: tail,
            own: self.head,
        })
    }

    /// A copy of the pending entry `n` places after the head, or `None` when fewer than `n + 1`
    /// are pending. The entry stays in the queue.
    ///
    /// # Errors
    ///
    /// As [`Consumer::pending`].
    pub fn peek<M: Memory + ?Sized>(&self, mem: &M, n: u32) -> Result<Option<E>, RingError> {
        if n >= self.pending(mem)? {
            return Ok(None);
        }
        let base = slot_base::<E>(&self.queue, self.head.wrapping_add(u64::from(n)));
        let mut words = E::Words::default();
        for (i, value) in words.as_mut().iter_mut().enumerate() {
            *value = word(mem, base.saturating_add(i))?.load(Ordering::Relaxed);
        }
        Ok(Some(E::from_words(&words)))
    }

    /// Gives the next `count` pending entries' slots back to the producer.
    ///
    /// # Errors
    ///
    /// [`RingError::Overrun`] when fewer are pending, or as [`Consumer::pending`].
    pub fn release<M: Memory + ?Sized>(&mut self, mem: &M, count: u32) -> Result<(), RingError> {
        let pending = self.pending(mem)?;
        if count > pending {
            return Err(RingError::Overrun { count, pending });
        }
        self.head = self.head.wrapping_add(u64::from(count));
        word(mem, self.queue.head)?.store(self.head, Ordering::Release);
        Ok(())
    }

    /// Takes the next entry, if there is one.
    ///
    /// # Errors
    ///
    /// As [`Consumer::pending`].
    pub fn pop<M: Memory + ?Sized>(&mut self, mem: &M) -> Result<Option<E>, RingError> {
        let entry = self.peek(mem, 0)?;
        if entry.is_some() {
            self.release(mem, 1)?;
        }
        Ok(entry)
    }
}

/// The setup words of a ring's header, as its creator wrote them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderInfo {
    /// The magic word, [`abi::MAGIC`] for a ring.
    pub magic: u64,
    /// The format version.
    pub version: u64,
    /// SQ entries.
    pub sq_entries: u64,
    /// CQ entries.
    pub cq_entries: u64,
    /// The notify mode's number.
    pub notify: u64,
}

impl HeaderInfo {
    /// Whether this is the header of a ring of this crate's version with `layout` and
    /// `notify`.
    pub fn matches(&self, layout: &Layout, notify: NotifyMode) -> bool {
        *self
            == Self {
                magic: abi::MAGIC,
                version: abi::VERSION,
                sq_entries: u64::from(layout.sq_entries()),
                cq_entries: u64::from(layout.cq_entries()),
                notify: notify as u64,
            }
    }

    /// Reads the setup words of the header in `mem`.
    ///
    /// # Errors
    ///
    /// [`RingError::OutOfBounds`] when the memory has no header.
    pub fn read<M: Memory + ?Sized>(mem: &M) -> Result<Self, RingError> {
        let load = |i| word(mem, i).map(|w| w.load(Ordering::Acquire));
        Ok(Self {
            magic: load(abi::header::MAGIC)?,
            version: load(abi::header::VERSION)?,
            sq_entries: load(abi::header::SQ_ENTRIES)?,
            cq_entries: load(abi::header::CQ_ENTRIES)?,
            notify: load(abi::header::NOTIFY)?,
        })
    }

    /// Writes the setup words for a ring with `layout` and `notify` into `mem`, which must be
    /// zeroed (as fresh pages are), so every index starts at 0.
    ///
    /// # Errors
    ///
    /// [`RingError::OutOfBounds`] when the memory is smaller than `layout`.
    pub fn write<M: Memory + ?Sized>(
        mem: &M,
        layout: &Layout,
        notify: NotifyMode,
    ) -> Result<(), RingError> {
        word(mem, layout.words().saturating_sub(1))?;
        let store = |i, value| word(mem, i).map(|w| w.store(value, Ordering::Release));
        store(abi::header::VERSION, abi::VERSION)?;
        store(abi::header::SQ_ENTRIES, u64::from(layout.sq_entries()))?;
        store(abi::header::CQ_ENTRIES, u64::from(layout.cq_entries()))?;
        store(abi::header::NOTIFY, notify as u64)?;
        store(abi::header::MAGIC, abi::MAGIC)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;
    use crate::entry::{Cqe, Sqe};

    fn memory(layout: &Layout) -> Vec<AtomicU64> {
        (0..layout.words()).map(|_| AtomicU64::new(0)).collect()
    }

    #[test]
    fn reserved_slots_stay_free_for_later_posts() {
        let layout = Layout::new(4, 4).expect("valid");
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut cq = Reserving::<Cqe>::new(layout.cq());
        let mut rx = Consumer::<Cqe>::new(layout.cq());
        // Work taken first, to be posted last.
        assert_eq!(cq.try_reserve(mem, 3), Ok(true));
        assert_eq!(cq.try_reserve(mem, 2), Ok(false));
        assert_eq!(cq.try_reserve(mem, 1), Ok(true));
        assert_eq!(cq.reserved(), 4);
        assert_eq!(cq.try_reserve(mem, 0), Ok(true));
        assert_eq!(cq.try_reserve(mem, 1), Ok(false));
        cq.post(mem, &[Cqe::new(9, 0)]).expect("reserved");
        cq.post(mem, &[Cqe::new(1, 0), Cqe::new(2, 0), Cqe::new(3, 0)])
            .expect("reserved");
        assert_eq!(cq.reserved(), 0);
        let tags: Vec<u64> = (0..4)
            .map(|_| rx.pop(mem).expect("valid").expect("posted").user_data)
            .collect();
        assert_eq!(tags, [9, 1, 2, 3]);
        assert_eq!(cq.try_reserve(mem, 4), Ok(true));
    }

    #[test]
    fn posting_more_than_was_reserved_writes_nothing() {
        let layout = Layout::new(4, 4).expect("valid");
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut cq = Reserving::<Cqe>::new(layout.cq());
        assert_eq!(cq.try_reserve(mem, 1), Ok(true));
        assert_eq!(
            cq.post(mem, &[Cqe::new(1, 0), Cqe::new(2, 0)]),
            Err(PushError::Full)
        );
        assert_eq!(cq.reserved(), 1);
        assert_eq!(Consumer::<Cqe>::new(layout.cq()).pending(mem), Ok(0));
        cq.unreserve(1);
        assert_eq!(cq.reserved(), 0);
        assert_eq!(cq.try_reserve(mem, 4), Ok(true));
    }

    #[test]
    fn entries_go_through_in_order_and_wrap() {
        let layout = Layout::new(4, 0).expect("valid");
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut tx = Producer::<Sqe>::new(layout.sq());
        let mut rx = Consumer::<Sqe>::new(layout.sq());
        for round in 0..10u64 {
            for i in 0..4 {
                tx.push(mem, &Sqe::nop(round * 4 + i)).expect("room");
            }
            assert_eq!(tx.push(mem, &Sqe::nop(0)), Err(PushError::Full));
            assert_eq!(rx.pending(mem), Ok(4));
            for i in 0..4 {
                let sqe = rx.pop(mem).expect("valid").expect("pending");
                assert_eq!(sqe.user_data, round * 4 + i);
            }
            assert_eq!(rx.pop(mem), Ok(None));
            assert_eq!(tx.free(mem), Ok(4));
        }
    }

    #[test]
    fn staged_entries_are_invisible_until_published() {
        let layout = Layout::new(8, 0).expect("valid");
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut tx = Producer::<Cqe>::new(layout.cq());
        let rx = Consumer::<Cqe>::new(layout.cq());
        tx.stage(mem, &Cqe::new(1, 0)).expect("room");
        tx.stage(mem, &Cqe::new(2, 0)).expect("room");
        assert_eq!(rx.pending(mem), Ok(0));
        tx.publish(mem).expect("in bounds");
        assert_eq!(rx.pending(mem), Ok(2));
        assert_eq!(
            rx.peek(mem, 1).expect("valid").map(|c| c.user_data),
            Some(2)
        );
        assert_eq!(rx.peek(mem, 2), Ok(None));
    }

    #[test]
    fn a_tail_out_of_range_is_corrupt() {
        let layout = Layout::new(8, 0).expect("valid");
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut rx = Consumer::<Sqe>::new(layout.sq());
        mem[abi::header::SQ_TAIL].store(9, Ordering::Relaxed);
        assert_eq!(rx.pending(mem), Err(RingError::Corrupt { peer: 9, own: 0 }));
        mem[abi::header::SQ_TAIL].store(3, Ordering::Relaxed);
        rx.release(mem, 3).expect("pending");
        // A tail that moves backwards is as corrupt as one too far ahead.
        mem[abi::header::SQ_TAIL].store(2, Ordering::Relaxed);
        assert_eq!(rx.pending(mem), Err(RingError::Corrupt { peer: 2, own: 3 }));
        assert_eq!(rx.pop(mem), Err(RingError::Corrupt { peer: 2, own: 3 }));
    }

    #[test]
    fn a_head_out_of_range_is_corrupt() {
        let layout = Layout::new(8, 0).expect("valid");
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut tx = Producer::<Cqe>::new(layout.cq());
        // The consumer claims to have taken entries that were never published.
        mem[abi::header::CQ_HEAD].store(1, Ordering::Relaxed);
        assert_eq!(
            tx.push(mem, &Cqe::new(0, 0)),
            Err(PushError::Ring(RingError::Corrupt { peer: 1, own: 0 }))
        );
        mem[abi::header::CQ_HEAD].store(0, Ordering::Relaxed);
        tx.push(mem, &Cqe::new(0, 0)).expect("room");
        // The producer never reads back its own tail: scribbling on it changes nothing here.
        mem[abi::header::CQ_TAIL].store(1000, Ordering::Relaxed);
        assert_eq!(tx.free(mem), Ok(15));
    }

    #[test]
    fn release_refuses_more_than_pending() {
        let layout = Layout::new(8, 0).expect("valid");
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut tx = Producer::<Sqe>::new(layout.sq());
        let mut rx = Consumer::<Sqe>::new(layout.sq());
        tx.push(mem, &Sqe::nop(1)).expect("room");
        assert_eq!(
            rx.release(mem, 2),
            Err(RingError::Overrun {
                count: 2,
                pending: 1
            })
        );
    }

    #[test]
    fn short_memory_is_out_of_bounds() {
        let layout = Layout::new(8, 0).expect("valid");
        let mem: Vec<AtomicU64> = (0..600).map(|_| AtomicU64::new(0)).collect();
        let mem = &mem[..];
        let mut tx = Producer::<Cqe>::new(layout.cq());
        assert_eq!(
            tx.push(mem, &Cqe::new(0, 0)),
            Err(PushError::Ring(RingError::OutOfBounds {
                word: layout.cq().base
            }))
        );
        assert_eq!(
            HeaderInfo::write(mem, &layout, NotifyMode::Event),
            Err(RingError::OutOfBounds {
                word: layout.words() - 1
            })
        );
    }

    #[test]
    fn headers_round_trip() {
        let layout = Layout::new(16, 64).expect("valid");
        let mem = memory(&layout);
        HeaderInfo::write(&mem[..], &layout, NotifyMode::Event).expect("in bounds");
        let info = HeaderInfo::read(&mem[..]).expect("in bounds");
        assert!(info.matches(&layout, NotifyMode::Event));
        assert!(!info.matches(&layout, NotifyMode::SqPoll));
        assert_eq!(info.sq_entries, 16);
        assert_eq!(info.cq_entries, 64);
    }
}
