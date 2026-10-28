//! Targeted TLB shootdowns: what one round invalidates, and how a round reaches another CPU.
//!
//! A `Flusher` (`context/memory.rs`) collects the virtual pages whose translations it changed
//! into a [`TlbInvalidation`]. Up to [`TLB_SINGLE_PAGE_FLUSH_CEILING`] pages are invalidated one
//! by one; more, or a page the caller could not name, turn the round into a full flush.
//!
//! The pages reach the other CPUs through one [`TlbMailbox`] per target CPU, in its
//! `PercpuBlock`, next to the `wants_tlb_shootdown` flag. A requester first [posts] its
//! invalidation, then sets the flag (`Release`) and sends the IPI. The target swaps the flag off
//! (`Acquire`), [receives] what the mailbox holds, invalidates, and acknowledges. So every flag
//! set still gets exactly one handling and one acknowledgement, as before.
//!
//! # The mailbox protocol
//!
//! One state word describes the slots. Its low two bits are a tag, the next six the number of
//! ranges in the slots, and the rest a sequence number:
//!
//! - `EMPTY`: nothing pending. Only the target sets it, when it takes the content.
//! - `WRITING`: one requester owns the slots and is filling them. A requester gets there only
//!   from `EMPTY` without the stale-writer bit (below), so `WRITING` never hides an earlier
//!   request that was not taken, and no one else stores to the slots meanwhile.
//! - `READY`: the slots hold a page list for the target.
//! - `ALL`: the target must flush everything.
//!
//! A requester that finds the mailbox `WRITING` or `READY`, that is, occupied by a request the
//! target has not taken yet, does not merge into it: it upgrades it to `ALL` with one CAS, which
//! then covers both requests. A requester whose own request is a full flush also posts `ALL`.
//! So the content of the mailbox is always at least the union of every request posted since the
//! last take, and a receiver never does less than any pending requester needs.
//!
//! The requester displaced from `WRITING` may still be storing to the slots. So the upgrade
//! also sets a stale-writer bit, which the `ALL` keeps through its take (to `EMPTY`) and through
//! later posts (an `EMPTY` with the bit becomes `ALL`, not `WRITING`), until the displaced
//! requester clears it with a `Release` CAS after its last store. Without it, the displaced
//! stores could land on top of the next owner's list, and the target would invalidate the
//! wrong pages.
//!
//! A receiver that finds `EMPTY` or `WRITING` has nothing to take: every request posted before
//! the flag it is answering was set has been taken already, by an earlier handling on this CPU
//! (only the target takes from its mailbox). That handling has finished invalidating, unless
//! this handling interrupted it; the receiver counts nesting and does a full flush when nested.
//!
//! # Memory ordering
//!
//! - The requester's page-table writes and its slot stores come before its `Release` CAS that
//!   publishes `READY` (or `ALL`), and before its `Release` swap that sets the flag.
//! - The receiver's `Acquire` swap of the flag synchronizes with the requester that set it, so
//!   its `Acquire` load of the state sees that requester's post or a later state.
//! - A take reads the slots and then CASes `READY` to `EMPTY` with `AcqRel`. The slot loads cannot
//!   pass the CAS's release, and a requester that later claims the slots CASes `EMPTY` to
//!   `WRITING` with `Acquire`, reading from that CAS; so the taker never sees the next request's
//!   slot stores. If another take ran in between (a nested handler) and a new request was
//!   posted, the sequence number differs and the CAS fails, so the taker retries and never acts
//!   on a stale list. The sequence number is `usize::BITS - 8` bits wide, and an interrupted
//!   take would have to see that many posts wrap around for a false match.
//! - The receiver acknowledges with `Release` after invalidating; the requester waits for the
//!   acknowledgements before freeing any frame.
//!
//! [posts]: TlbMailbox::post
//! [receives]: TlbMailbox::receive

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// The most pages one shootdown round invalidates one at a time. A round that changes more
/// pages flushes the whole TLB instead, locally and on every target.
///
/// One `invlpg` (with KPTI and PCID, plus an `INVPCID` for the user PCID) costs a few hundred
/// cycles; a full flush costs about as much once, plus the refills of every entry it threw away.
/// Linux makes the same trade at 33 pages (`tlb_single_page_flush_ceiling`). Every slot of a
/// [`TlbMailbox`] holds one range, so this is also the mailbox's capacity.
pub const TLB_SINGLE_PAGE_FLUSH_CEILING: usize = 32;

/// Bits of a slot that hold a range's page count minus one.
const SLOT_COUNT_BITS: u32 = 6;
const SLOT_COUNT_MASK: usize = !(usize::MAX << SLOT_COUNT_BITS);
const _: () = assert!(TLB_SINGLE_PAGE_FLUSH_CEILING <= 1 << SLOT_COUNT_BITS);

/// The virtual pages one shootdown round invalidates.
///
/// Pages are page numbers (address divided by the page size). Contiguous pages are kept as one
/// range.
#[derive(Clone, Debug)]
pub struct TlbInvalidation {
    /// Ranges `[0..len]`, each encoded as `first << SLOT_COUNT_BITS | (count - 1)`.
    slots: [usize; TLB_SINGLE_PAGE_FLUSH_CEILING],
    len: usize,
    pages: usize,
    all: bool,
}

impl TlbInvalidation {
    /// An invalidation that flushes the whole TLB.
    pub const ALL: Self = Self {
        slots: [0; TLB_SINGLE_PAGE_FLUSH_CEILING],
        len: 0,
        pages: 0,
        all: true,
    };

    /// An invalidation of nothing.
    pub const fn new() -> Self {
        Self {
            slots: [0; TLB_SINGLE_PAGE_FLUSH_CEILING],
            len: 0,
            pages: 0,
            all: false,
        }
    }

    /// Whether this invalidates nothing.
    pub fn is_empty(&self) -> bool {
        !self.all && self.len == 0
    }

    /// Whether this flushes the whole TLB.
    pub fn is_all(&self) -> bool {
        self.all
    }

    /// Adds `count` pages from page number `first`. Once the pages exceed
    /// [`TLB_SINGLE_PAGE_FLUSH_CEILING`], or cannot be encoded, this becomes a full flush.
    pub fn add(&mut self, first: usize, count: usize) {
        if self.all || count == 0 {
            return;
        }
        let pages = self.pages.saturating_add(count);
        if pages > TLB_SINGLE_PAGE_FLUSH_CEILING || first > usize::MAX >> SLOT_COUNT_BITS {
            self.set_all();
            return;
        }
        self.pages = pages;

        if let Some(last) = self.len.checked_sub(1).and_then(|i| self.slots.get_mut(i)) {
            let (last_first, last_count) = decode(*last);
            if last_first.checked_add(last_count) == Some(first) {
                // `pages` bounds the sum.
                *last = encode(last_first, last_count.saturating_add(count));
                return;
            }
        }
        // `pages` bounds `len`, as every range holds at least one page.
        match self.slots.get_mut(self.len) {
            Some(slot) => {
                *slot = encode(first, count);
                self.len = self.len.saturating_add(1);
            }
            None => self.set_all(),
        }
    }

    /// Makes this a full flush.
    pub fn set_all(&mut self) {
        *self = Self::ALL;
    }

    /// The ranges to invalidate, as `(first page, page count)`, or `None` for a full flush.
    pub fn ranges(&self) -> Option<impl Iterator<Item = (usize, usize)> + '_> {
        (!self.all).then(|| self.slots.iter().take(self.len).map(|&slot| decode(slot)))
    }

    /// Calls `page` for every page to invalidate, or `all` once for a full flush.
    pub fn invalidate(&self, mut page: impl FnMut(usize), all: impl FnOnce()) {
        match self.ranges() {
            Some(ranges) => ranges
                .flat_map(|(first, count)| first..first.saturating_add(count))
                .for_each(&mut page),
            None => all(),
        }
    }
}

impl Default for TlbInvalidation {
    fn default() -> Self {
        Self::new()
    }
}

/// Encodes `count` (1 to [`TLB_SINGLE_PAGE_FLUSH_CEILING`]) pages from page number `first`,
/// which [`TlbInvalidation::add`] checked fits.
fn encode(first: usize, count: usize) -> usize {
    first << SLOT_COUNT_BITS | count.wrapping_sub(1)
}

fn decode(slot: usize) -> (usize, usize) {
    (
        slot >> SLOT_COUNT_BITS,
        (slot & SLOT_COUNT_MASK).wrapping_add(1),
    )
}

const TAG_BITS: u32 = 2;
const TAG_MASK: usize = (1 << TAG_BITS) - 1;
const TAG_EMPTY: usize = 0;
const TAG_WRITING: usize = 1;
const TAG_READY: usize = 2;
const TAG_ALL: usize = 3;
const LEN_SHIFT: u32 = TAG_BITS;
const LEN_MASK: usize = SLOT_COUNT_MASK;
const SEQ_SHIFT: u32 = TAG_BITS + SLOT_COUNT_BITS;
const _: () = assert!(TLB_SINGLE_PAGE_FLUSH_CEILING <= LEN_MASK);

/// In an `EMPTY` or `ALL` state (which have no length), the length field's lowest bit: a
/// requester displaced from `WRITING` may still be storing to the slots, so nobody may claim
/// them. At most one such writer exists, because claiming needs this bit clear; it clears the
/// bit itself when its stores are done.
const STALE_WRITER: usize = 1 << LEN_SHIFT;

const fn state(tag: usize, len: usize, seq: usize) -> usize {
    seq << SEQ_SHIFT | len << LEN_SHIFT | tag
}

/// The sequence number a new owner of the slots gives the state.
fn next_seq(current: usize) -> usize {
    (current >> SEQ_SHIFT).wrapping_add(1)
}

/// One CPU's incoming shootdown requests. See the [module docs](self) for the protocol.
#[derive(Debug)]
pub struct TlbMailbox {
    state: AtomicUsize,
    slots: [AtomicUsize; TLB_SINGLE_PAGE_FLUSH_CEILING],
    /// How many [`TlbMailbox::receive`] calls are running on the target CPU, which is the only
    /// one that changes it; interrupts nest them. `Relaxed` is enough for that.
    receiving: AtomicU32,
}

impl TlbMailbox {
    /// An empty mailbox.
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(state(TAG_EMPTY, 0, 0)),
            slots: [const { AtomicUsize::new(0) }; TLB_SINGLE_PAGE_FLUSH_CEILING],
            receiving: AtomicU32::new(0),
        }
    }

    /// Posts `inv` for the target CPU. Call it after the page-table changes, and before setting
    /// the target's request flag with `Release` ordering. Requesters on any CPU may post at once.
    pub fn post(&self, inv: &TlbInvalidation) {
        if inv.is_empty() {
            return;
        }
        let mut current = self.state.load(Ordering::Relaxed);
        loop {
            let stale = current & STALE_WRITER;
            let (tag, desired) = match current & TAG_MASK {
                TAG_ALL => return,
                TAG_EMPTY if !inv.is_all() && stale == 0 => {
                    (TAG_WRITING, state(TAG_WRITING, 0, next_seq(current)))
                }
                // A full flush, or slots a displaced writer may still be storing to.
                TAG_EMPTY => (TAG_ALL, state(TAG_ALL, 0, next_seq(current)) | stale),
                // The owner of the slots has not published them yet: one `ALL` covers its
                // request and this one, and the owner, displaced, must clear `STALE_WRITER`
                // before anyone else may claim the slots.
                TAG_WRITING => (TAG_ALL, state(TAG_ALL, 0, next_seq(current)) | STALE_WRITER),
                // A request the target has not taken yet: one `ALL` covers it and this one.
                _ => (TAG_ALL, state(TAG_ALL, 0, next_seq(current))),
            };
            match self.state.compare_exchange_weak(
                current,
                desired,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) if tag == TAG_WRITING => return self.fill(inv, desired),
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Fills the slots this requester owns since it moved the state to `writing`, and publishes
    /// them. If another requester displaced it meanwhile, clears `STALE_WRITER` instead, which
    /// lets the next requester claim the slots after these stores.
    fn fill(&self, inv: &TlbInvalidation, writing: usize) {
        for (slot, &range) in self.slots.iter().zip(inv.slots.iter().take(inv.len)) {
            slot.store(range, Ordering::Relaxed);
        }
        let ready = state(TAG_READY, inv.len, writing >> SEQ_SHIFT);
        let mut current = writing;
        loop {
            // `Release`: the next claimer's `Acquire` CAS orders these slot stores before its
            // own, and a receiver that takes `ready` sees them.
            let desired = if current == writing {
                ready
            } else {
                // Displaced: the `ALL` that displaced this request covers it. `STALE_WRITER`
                // stays set until here, through takes and later posts.
                current & !STALE_WRITER
            };
            match self.state.compare_exchange_weak(
                current,
                desired,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Takes what is pending, leaving the mailbox empty. Returns an empty invalidation if
    /// nothing is pending.
    fn take(&self) -> TlbInvalidation {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let taken = match current & TAG_MASK {
                TAG_EMPTY | TAG_WRITING => return TlbInvalidation::new(),
                TAG_ALL => TlbInvalidation::ALL,
                _ => {
                    let len = (current >> LEN_SHIFT) & LEN_MASK;
                    let mut inv = TlbInvalidation::new();
                    for slot in self.slots.iter().take(len) {
                        let (first, count) = decode(slot.load(Ordering::Relaxed));
                        inv.add(first, count);
                    }
                    inv
                }
            };
            // A displaced writer may still be storing to the slots: keep it marked. (Only `ALL`
            // can carry the mark; in `READY` the bit belongs to the length.)
            let stale = if taken.is_all() {
                current & STALE_WRITER
            } else {
                0
            };
            let empty = state(TAG_EMPTY, 0, current >> SEQ_SHIFT) | stale;
            match self
                .state
                .compare_exchange(current, empty, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return taken,
                Err(actual) => current = actual,
            }
        }
    }

    /// Handles one request on the target CPU: takes what is pending and calls `page` for every
    /// page to invalidate, or `all` once for a full flush. Call it after swapping the request
    /// flag off with `Acquire` ordering, and acknowledge after it returns.
    ///
    /// A call that interrupted another one on the same CPU always flushes everything: the outer
    /// call may have taken the request this one answers and not yet invalidated it.
    pub fn receive(&self, page: impl FnMut(usize), all: impl FnOnce()) {
        let nested = self.receiving.fetch_add(1, Ordering::Relaxed) > 0;
        let inv = self.take();
        if nested {
            all();
        } else if !inv.is_empty() {
            inv.invalidate(page, all);
        }
        self.receiving.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Default for TlbMailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::{sync::Arc, vec::Vec};

    fn pages(inv: &TlbInvalidation) -> Option<Vec<usize>> {
        let mut out = Vec::new();
        let mut all = false;
        inv.invalidate(|p| out.push(p), || all = true);
        (!all).then_some(out)
    }

    fn received(mailbox: &TlbMailbox) -> Option<Vec<usize>> {
        let mut out = Vec::new();
        let mut all = false;
        mailbox.receive(|p| out.push(p), || all = true);
        (!all).then_some(out)
    }

    fn of(ranges: &[(usize, usize)]) -> TlbInvalidation {
        let mut inv = TlbInvalidation::new();
        for &(first, count) in ranges {
            inv.add(first, count);
        }
        inv
    }

    #[test]
    fn contiguous_pages_merge_into_one_range() {
        let inv = of(&[(10, 1), (11, 2), (13, 1), (20, 1)]);
        assert_eq!(
            inv.ranges().unwrap().collect::<Vec<_>>(),
            [(10, 4), (20, 1)]
        );
        assert_eq!(pages(&inv).unwrap(), [10, 11, 12, 13, 20]);
    }

    #[test]
    fn ceiling_is_inclusive() {
        let inv = of(&[(0, TLB_SINGLE_PAGE_FLUSH_CEILING)]);
        assert_eq!(pages(&inv).unwrap().len(), TLB_SINGLE_PAGE_FLUSH_CEILING);

        let scattered: Vec<_> = (0..TLB_SINGLE_PAGE_FLUSH_CEILING)
            .map(|i| (i * 2, 1))
            .collect();
        let mut inv = of(&scattered);
        assert_eq!(pages(&inv).unwrap().len(), TLB_SINGLE_PAGE_FLUSH_CEILING);
        inv.add(1000, 1);
        assert!(inv.is_all());
        assert_eq!(pages(&inv), None);
    }

    #[test]
    fn large_range_or_unencodable_page_is_a_full_flush() {
        assert!(of(&[(0, TLB_SINGLE_PAGE_FLUSH_CEILING + 1)]).is_all());
        assert!(of(&[(usize::MAX, 1)]).is_all());
        assert!(of(&[(1, usize::MAX)]).is_all());
        let mut inv = of(&[(5, 1)]);
        inv.set_all();
        inv.add(6, 1);
        assert!(inv.is_all());
    }

    #[test]
    fn highest_encodable_page_round_trips() {
        let top = usize::MAX >> SLOT_COUNT_BITS;
        assert_eq!(pages(&of(&[(top, 1)])).unwrap(), [top]);
    }

    #[test]
    fn empty_mailbox_receives_nothing() {
        let mailbox = TlbMailbox::new();
        mailbox.post(&TlbInvalidation::new());
        assert_eq!(received(&mailbox).unwrap(), []);
    }

    #[test]
    fn posted_pages_are_received_once() {
        let mailbox = TlbMailbox::new();
        mailbox.post(&of(&[(7, 3)]));
        assert_eq!(received(&mailbox).unwrap(), [7, 8, 9]);
        assert_eq!(received(&mailbox).unwrap(), []);
        mailbox.post(&of(&[(1, 1)]));
        assert_eq!(received(&mailbox).unwrap(), [1]);
    }

    #[test]
    fn full_flush_is_received() {
        let mailbox = TlbMailbox::new();
        mailbox.post(&TlbInvalidation::ALL);
        assert_eq!(received(&mailbox), None);
        assert_eq!(received(&mailbox).unwrap(), []);
    }

    #[test]
    fn second_request_before_a_take_upgrades_to_all() {
        let mailbox = TlbMailbox::new();
        mailbox.post(&of(&[(1, 1)]));
        mailbox.post(&of(&[(2, 1)]));
        assert_eq!(received(&mailbox), None);
        assert_eq!(received(&mailbox).unwrap(), []);

        mailbox.post(&of(&[(1, 1)]));
        mailbox.post(&TlbInvalidation::ALL);
        mailbox.post(&of(&[(2, 1)]));
        assert_eq!(received(&mailbox), None);
    }

    #[test]
    fn request_during_another_requesters_fill_upgrades_to_all() {
        let mailbox = TlbMailbox::new();
        let first = of(&[(1, 1)]);
        // The first requester owns the slots but has not published them yet.
        let writing = state(TAG_WRITING, 0, 1);
        mailbox.state.store(writing, Ordering::Relaxed);
        // A receiver answering an earlier flag finds nothing to take and leaves the slots alone.
        assert_eq!(received(&mailbox).unwrap(), []);
        mailbox.post(&of(&[(2, 1)]));
        mailbox.fill(&first, writing);
        assert_eq!(received(&mailbox), None);
    }

    /// A requester displaced from `WRITING` may still be storing its list. Until it is done,
    /// nobody may claim the slots, or its late stores would overwrite the next owner's list and
    /// the target would invalidate the wrong page (what made
    /// `concurrent_requesters_are_all_covered` fail now and then).
    #[test]
    fn displaced_writer_keeps_the_slots_until_it_is_done() {
        let mailbox = TlbMailbox::new();
        let slow = of(&[(1, 1)]);
        // The slow requester owns the slots but has not stored anything yet.
        let writing = state(TAG_WRITING, 0, 1);
        mailbox.state.store(writing, Ordering::Relaxed);
        // A second requester displaces it, and the target takes the resulting `ALL`.
        mailbox.post(&of(&[(2, 1)]));
        assert_eq!(received(&mailbox), None);
        // A third requester must not claim the slots while the slow one may still store.
        mailbox.post(&of(&[(3, 1)]));
        assert_eq!(mailbox.state.load(Ordering::Relaxed) & TAG_MASK, TAG_ALL);
        // The slow requester's stores land now, then it lets go of the slots.
        mailbox.fill(&slow, writing);
        assert_eq!(mailbox.state.load(Ordering::Relaxed) & STALE_WRITER, 0);
        assert_eq!(received(&mailbox), None, "page 3 needs a full flush");
        // The slots are free again.
        mailbox.post(&of(&[(4, 1)]));
        assert_eq!(received(&mailbox).unwrap(), [4]);
    }

    #[test]
    fn displaced_writer_finishing_before_the_take_clears_its_mark() {
        let mailbox = TlbMailbox::new();
        let writing = state(TAG_WRITING, 0, 1);
        mailbox.state.store(writing, Ordering::Relaxed);
        mailbox.post(&of(&[(2, 1)]));
        mailbox.fill(&of(&[(1, 1)]), writing);
        assert_eq!(received(&mailbox), None);
        mailbox.post(&of(&[(5, 1)]));
        assert_eq!(received(&mailbox).unwrap(), [5]);
    }

    #[test]
    fn nested_receive_flushes_everything() {
        let mailbox = TlbMailbox::new();
        mailbox.post(&of(&[(4, 1)]));
        let mut inner = Some(Vec::new());
        let mut outer = Vec::new();
        mailbox.receive(
            |p| {
                // An interrupt arrives after the outer call took the list.
                mailbox.post(&of(&[(5, 1)]));
                inner = received(&mailbox);
                outer.push(p);
            },
            || panic!("the outer call has a page list"),
        );
        assert_eq!(outer, [4]);
        assert_eq!(inner, None);
        // A nested call with nothing pending must still flush: the outer one may not be done.
        mailbox.post(&of(&[(6, 1)]));
        let mut nested_empty = Some(Vec::new());
        mailbox.receive(
            |_| nested_empty = received(&mailbox),
            || panic!("the outer call has a page list"),
        );
        assert_eq!(nested_empty, None);
    }

    #[test]
    fn take_after_nested_take_and_repost_never_uses_a_stale_list() {
        let mailbox = TlbMailbox::new();
        mailbox.post(&of(&[(1, 1)]));
        let stale = mailbox.state.load(Ordering::Acquire);
        // A nested handler takes the list, and a new request with the same length is posted.
        assert_eq!(received(&mailbox).unwrap(), [1]);
        mailbox.post(&of(&[(2, 1)]));
        // The interrupted take's CAS with the state it loaded earlier must fail.
        let empty = state(TAG_EMPTY, 0, stale >> SEQ_SHIFT);
        assert!(mailbox
            .state
            .compare_exchange(stale, empty, Ordering::AcqRel, Ordering::Acquire)
            .is_err());
        assert_eq!(received(&mailbox).unwrap(), [2]);
    }

    /// Requesters on several threads each post one page and set the flag, as
    /// `shootdown_tlb_ipi` does; one receiver answers flags as the IPI handler does. Every page
    /// must be invalidated, one by one or by a full flush, before the requester's flag is
    /// answered.
    #[test]
    fn concurrent_requesters_are_all_covered() {
        use std::sync::{
            atomic::{AtomicBool, AtomicU64},
            Mutex,
        };

        const REQUESTERS: usize = 3;
        const ROUNDS: usize = if cfg!(miri) { 8 } else { 2000 };

        struct Target {
            mailbox: TlbMailbox,
            flag: AtomicBool,
            /// Set before the receiver looks at the flag, cleared after it invalidated.
            busy: AtomicBool,
            full_flushes: AtomicU64,
            /// Per requester, the last round whose page was invalidated one by one.
            covered: Mutex<[usize; REQUESTERS]>,
            done: AtomicBool,
        }
        let target = Arc::new(Target {
            mailbox: TlbMailbox::new(),
            flag: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            full_flushes: AtomicU64::new(0),
            covered: Mutex::new([usize::MAX; REQUESTERS]),
            done: AtomicBool::new(false),
        });

        let receiver = {
            let target = target.clone();
            std::thread::spawn(move || {
                while !target.done.load(Ordering::Acquire) {
                    target.busy.store(true, Ordering::SeqCst);
                    if target.flag.swap(false, Ordering::SeqCst) {
                        target.mailbox.receive(
                            |page| {
                                let (requester, round) = (page / ROUNDS, page % ROUNDS);
                                target.covered.lock().unwrap()[requester] = round;
                            },
                            || {
                                target.full_flushes.fetch_add(1, Ordering::Relaxed);
                            },
                        );
                    }
                    target.busy.store(false, Ordering::SeqCst);
                    std::hint::spin_loop();
                }
            })
        };

        let requesters: Vec<_> = (0..REQUESTERS)
            .map(|requester| {
                let target = target.clone();
                std::thread::spawn(move || {
                    for round in 0..ROUNDS {
                        let full_before = target.full_flushes.load(Ordering::Acquire);
                        target.mailbox.post(&of(&[(requester * ROUNDS + round, 1)]));
                        while target.flag.swap(true, Ordering::SeqCst) {
                            while target.flag.load(Ordering::Relaxed) {
                                std::hint::spin_loop();
                            }
                        }
                        // Wait until the flag set above has been answered: the receiver cleared
                        // it, and then was seen idle. `busy` was set before the swap that cleared
                        // the flag, so seeing it clear afterwards means that handling ended.
                        while target.flag.load(Ordering::SeqCst) {
                            std::hint::spin_loop();
                        }
                        while target.busy.load(Ordering::SeqCst) {
                            std::hint::spin_loop();
                        }
                        let by_page = target.covered.lock().unwrap()[requester] == round;
                        let by_full = target.full_flushes.load(Ordering::Acquire) > full_before;
                        assert!(
                            by_page || by_full,
                            "requester {requester} round {round} lost"
                        );
                    }
                })
            })
            .collect();
        for requester in requesters {
            requester.join().unwrap();
        }
        target.done.store(true, Ordering::Release);
        receiver.join().unwrap();
    }
}
