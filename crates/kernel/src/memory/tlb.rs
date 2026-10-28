//! Targeted TLB shootdowns: what one round invalidates, and how a round reaches another CPU.
//!
//! A `Flusher` (`context/memory.rs`) collects the virtual pages whose translations it changed
//! into a [`TlbInvalidation`]. Up to [`TLB_SINGLE_PAGE_FLUSH_CEILING`] pages are invalidated one
//! by one; more, or a page the caller could not name, turn the round into a full flush.
//!
//! The pages reach the other CPUs through one [`TlbMailbox`] per target CPU, in its
//! `PercpuBlock`, next to the `wants_tlb_shootdown` flag. A requester first [posts] its
//! invalidation, which gives it a [`TlbAck`], then sets the flag (`Release`) and sends the IPI
//! unless the flag was already set. The target swaps the flag off (`Acquire`), [receives] what the
//! mailbox holds, invalidates, and acknowledges.
//!
//! # Acknowledgements
//!
//! Each post takes a ticket from the mailbox's request counter, and the target acknowledges by
//! advancing the mailbox's handled counter. A requester waits until every target it posted to
//! has handled its own ticket ([`TlbAcks`]), so nothing but that target's handling of that
//! request, or a later one, can end the wait: not a CPU that left the address space, not an
//! answer to someone else's request, and not a CPU the requester never asked. A CPU that leaves
//! an address space acknowledges like any other handling, by taking its mailbox and
//! invalidating first.
//!
//! A receiver loads the request counter, with `Acquire`, before it takes the mailbox, and
//! publishes that value as handled once it has invalidated. Tickets up to that value belong to
//! posts that happened before the take, so the take covered them (or an earlier take on this CPU
//! did, which has finished invalidating, or this handling is nested and flushed everything).
//! Posts with later tickets are not acknowledged by this handling; their flag set is answered by
//! the next one. A flag that is already set when a requester sets it has an interrupt on the way
//! and a handling still to come that swaps it off after this requester's swap, so the requester
//! need not send another interrupt or wait for the flag to clear.
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
//! A requester that finds `ALL` already pending still writes it back with a CAS: only a
//! read-modify-write is sure to see the latest state, and only a store publishes the requester's
//! page-table writes to the take that clears that `ALL`.
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
//!   publishes `READY` or `ALL` (or rewrites an `ALL` already pending), and before its `Release`
//!   swap that sets the flag. Every post therefore writes the state, so the take that clears it
//!   reads from the post or from a later read-modify-write in its release sequence.
//! - The receiver's `Acquire` swap of the flag synchronizes with the requester that set it, so
//!   its `Acquire` load of the state sees that requester's post or a later state.
//! - A take reads the slots and then CASes `READY` to `EMPTY` with `AcqRel`. The slot loads cannot
//!   pass the CAS's release, and a requester that later claims the slots CASes `EMPTY` to
//!   `WRITING` with `Acquire`, reading from that CAS; so the taker never sees the next request's
//!   slot stores. If another take ran in between (a nested handler) and a new request was
//!   posted, the sequence number differs and the CAS fails, so the taker retries and never acts
//!   on a stale list. The sequence number is `usize::BITS - 8` bits wide, and an interrupted
//!   take would have to see that many posts wrap around for a false match.
//! - A requester takes its ticket with a `Release` increment after its post. The receiver's
//!   `Acquire` load of the counter reads that increment or a later one, which continues its
//!   release sequence, so the post happens before the take.
//! - The receiver advances the handled counter with `Release` after invalidating; the requester's
//!   `Acquire` load of it comes before it frees any frame.
//!
//! [posts]: TlbMailbox::post
//! [receives]: TlbMailbox::receive

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::cpu_set::{LogicalCpuId, MAX_CPU_COUNT};

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
    /// The last ticket handed to a requester.
    requested: AtomicUsize,
    /// The last ticket the target has handled: it invalidated what every post up to it asked for.
    handled: AtomicUsize,
}

impl TlbMailbox {
    /// An empty mailbox.
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(state(TAG_EMPTY, 0, 0)),
            slots: [const { AtomicUsize::new(0) }; TLB_SINGLE_PAGE_FLUSH_CEILING],
            receiving: AtomicU32::new(0),
            requested: AtomicUsize::new(0),
            handled: AtomicUsize::new(0),
        }
    }

    /// Posts `inv` for the target CPU. Call it after the page-table changes, and before setting
    /// the target's request flag with `Release` ordering. Requesters on any CPU may post at once.
    ///
    /// The returned [`TlbAck`] tells when the target has handled this request.
    pub fn post(&self, inv: &TlbInvalidation) -> TlbAck<'_> {
        self.post_content(inv);
        // `Release`: a receiver that reads this ticket, or a later one, sees the post above.
        let ticket = self
            .requested
            .fetch_add(1, Ordering::Release)
            .wrapping_add(1);
        TlbAck {
            mailbox: self,
            ticket,
        }
    }

    fn post_content(&self, inv: &TlbInvalidation) {
        if inv.is_empty() {
            return;
        }
        let mut current = self.state.load(Ordering::Relaxed);
        loop {
            let stale = current & STALE_WRITER;
            let (tag, desired) = match current & TAG_MASK {
                // A pending `ALL` covers this request, but only if the target has not taken it
                // yet. A plain load may return an `ALL` that was already taken, and it would not
                // order this requester's page-table writes before the take; the CAS reads the
                // latest state and joins the release sequence the take reads from.
                TAG_ALL => (TAG_ALL, current),
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
    ///
    /// It then acknowledges every request whose ticket was handed out before it started.
    pub fn receive(&self, page: impl FnMut(usize), all: impl FnOnce()) {
        let nested = self.receiving.fetch_add(1, Ordering::Relaxed) > 0;
        // Before the take: every ticket up to this one belongs to a post the take will see.
        let requested = self.requested.load(Ordering::Acquire);
        let inv = self.take();
        if nested {
            all();
        } else if !inv.is_empty() {
            inv.invalidate(page, all);
        }
        self.receiving.fetch_sub(1, Ordering::Relaxed);
        self.acknowledge(requested);
    }

    /// Records that every ticket up to `ticket` has been handled. A nested handling may have
    /// published a later ticket already, so this never moves the counter back.
    fn acknowledge(&self, ticket: usize) {
        let mut current = self.handled.load(Ordering::Relaxed);
        while ticket_after(ticket, current) {
            match self.handled.compare_exchange_weak(
                current,
                ticket,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn has_handled(&self, ticket: usize) -> bool {
        !ticket_after(ticket, self.handled.load(Ordering::Acquire))
    }
}

/// Whether ticket `a` was handed out after ticket `b`. Tickets wrap; two that are compared are
/// never `isize::MAX` requests apart.
fn ticket_after(a: usize, b: usize) -> bool {
    (a.wrapping_sub(b) as isize) > 0
}

/// One request posted to a [`TlbMailbox`]: done once the target has handled it.
#[derive(Clone, Copy, Debug)]
pub struct TlbAck<'a> {
    mailbox: &'a TlbMailbox,
    ticket: usize,
}

impl TlbAck<'_> {
    /// Whether the target has invalidated what this request, and every earlier one to the same
    /// target, asked for.
    pub fn is_handled(&self) -> bool {
        self.mailbox.has_handled(self.ticket)
    }
}

/// The requests one shootdown round posted, at most one per target CPU, that the requester waits
/// for before it frees frames.
#[derive(Debug)]
pub struct TlbAcks<'a> {
    pending: [Option<TlbAck<'a>>; MAX_CPU_COUNT as usize],
    count: usize,
}

impl<'a> TlbAcks<'a> {
    /// No pending requests.
    pub const fn new() -> Self {
        Self {
            pending: [None; MAX_CPU_COUNT as usize],
            count: 0,
        }
    }

    /// Waits for `ack`, the request posted to `cpu`, as well. A later request to the same CPU
    /// replaces an earlier one, which it covers.
    ///
    /// # Errors
    ///
    /// Returns `ack` back if `cpu` is not below [`MAX_CPU_COUNT`], which no CPU with a per-CPU
    /// block is; the caller must then wait for it itself.
    pub fn record(&mut self, cpu: LogicalCpuId, ack: TlbAck<'a>) -> Result<(), TlbAck<'a>> {
        let Some(slot) = self.pending.get_mut(cpu.get() as usize) else {
            return Err(ack);
        };
        if slot.replace(ack).is_none() {
            self.count = self.count.saturating_add(1);
        }
        Ok(())
    }

    /// Forgets the requests that have been handled. Returns how many are left.
    pub fn poll(&mut self) -> usize {
        if self.count != 0 {
            for slot in &mut self.pending {
                if slot.is_some_and(|ack| ack.is_handled()) {
                    *slot = None;
                    self.count = self.count.saturating_sub(1);
                }
            }
        }
        self.count
    }
}

impl Default for TlbAcks<'_> {
    fn default() -> Self {
        Self::new()
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
    /// the target would invalidate the wrong page. This once made
    /// `concurrent_requesters_are_all_covered` fail now and then; `post_content` now joins the
    /// take's release sequence through the `TAG_ALL` CAS, so it does not flake.
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

    fn cpu(id: u32) -> LogicalCpuId {
        LogicalCpuId::new(id)
    }

    #[test]
    fn ack_waits_for_the_targets_own_handling() {
        let mailbox = TlbMailbox::new();
        let ack = mailbox.post(&of(&[(3, 1)]));
        assert!(!ack.is_handled());
        assert_eq!(received(&mailbox).unwrap(), [3]);
        assert!(ack.is_handled());
        // Later requests are not covered by the earlier handling.
        let later = mailbox.post(&of(&[(4, 1)]));
        assert!(ack.is_handled());
        assert!(!later.is_handled());
    }

    /// Issue #264: the requester asked a CPU that then left the address space (and answered in
    /// its switch hook), and a CPU that stayed. Under the shared counter the leaver was not
    /// counted but its answer was, so one answer ended the wait before the CPU that stayed had
    /// invalidated anything.
    #[test]
    fn leaver_answer_does_not_end_the_wait_for_another_cpu() {
        let leaver = TlbMailbox::new();
        let stayer = TlbMailbox::new();
        let inv = of(&[(9, 1)]);
        let mut acks = TlbAcks::new();
        acks.record(cpu(1), leaver.post(&inv)).unwrap();
        acks.record(cpu(2), stayer.post(&inv)).unwrap();
        assert_eq!(acks.poll(), 2);

        // The leaver answers in `switch_arch_hook`, after it cleared its bit.
        assert_eq!(received(&leaver).unwrap(), [9]);
        assert_eq!(
            acks.poll(),
            1,
            "the CPU that stayed has not invalidated yet"
        );

        assert_eq!(received(&stayer).unwrap(), [9]);
        assert_eq!(acks.poll(), 0);
    }

    /// Issue #264: a CPU answering someone else's request (another address space's round, or
    /// the shadow-stack broadcast) must not count for this round, whichever CPU it is.
    #[test]
    fn answers_to_unrelated_requests_do_not_count() {
        let target = TlbMailbox::new();
        let bystander = TlbMailbox::new();
        let mut acks = TlbAcks::new();

        // The target answered an unrelated request before this round posted.
        let _ = target.post(&TlbInvalidation::ALL);
        assert_eq!(received(&target), None);
        acks.record(cpu(0), target.post(&of(&[(5, 1)]))).unwrap();
        // Another CPU answers a broadcast, possibly while running this address space.
        let _ = bystander.post(&TlbInvalidation::ALL);
        assert_eq!(received(&bystander), None);
        assert_eq!(acks.poll(), 1);

        assert_eq!(received(&target).unwrap(), [5]);
        assert_eq!(acks.poll(), 0);
    }

    /// A handling that loaded the request counter before a post may still take that post, but
    /// must not acknowledge it: the next handling, the one its flag set asks for, does.
    #[test]
    fn handling_that_began_before_a_post_does_not_acknowledge_it() {
        let mailbox = TlbMailbox::new();
        let earlier = mailbox.post(&of(&[(1, 1)]));
        // The handling starts: it loads the counter...
        let requested = mailbox.requested.load(Ordering::Acquire);
        // ...a new request arrives before its take...
        let ack = mailbox.post(&of(&[(2, 1)]));
        // ...and the take covers both.
        assert!(mailbox.take().is_all());
        mailbox.acknowledge(requested);
        assert!(earlier.is_handled());
        assert!(!ack.is_handled());
        assert_eq!(received(&mailbox).unwrap(), []);
        assert!(ack.is_handled());
    }

    #[test]
    fn nested_acknowledgement_is_not_undone_by_the_outer_one() {
        let mailbox = TlbMailbox::new();
        let outer_ack = mailbox.post(&of(&[(4, 1)]));
        let mut inner_ack = None;
        mailbox.receive(
            |_| {
                let ack = mailbox.post(&of(&[(5, 1)]));
                assert_eq!(received(&mailbox), None);
                assert!(ack.is_handled());
                inner_ack = Some(ack);
            },
            || panic!("the outer call has a page list"),
        );
        // The outer call loaded an older ticket and acknowledged it after the inner one.
        assert!(outer_ack.is_handled());
        assert!(inner_ack.unwrap().is_handled());
    }

    #[test]
    fn tickets_wrap_around() {
        let mailbox = TlbMailbox::new();
        mailbox.requested.store(usize::MAX - 1, Ordering::Relaxed);
        mailbox.handled.store(usize::MAX - 1, Ordering::Relaxed);
        let before_wrap = mailbox.post(&of(&[(1, 1)]));
        let after_wrap = mailbox.post(&of(&[(2, 1)]));
        assert!(!before_wrap.is_handled() && !after_wrap.is_handled());
        assert_eq!(received(&mailbox), None);
        assert!(before_wrap.is_handled() && after_wrap.is_handled());
        assert_eq!(mailbox.handled.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn later_request_to_the_same_cpu_replaces_the_earlier() {
        let mailbox = TlbMailbox::new();
        let mut acks = TlbAcks::new();
        acks.record(cpu(3), mailbox.post(&of(&[(1, 1)]))).unwrap();
        acks.record(cpu(3), mailbox.post(&of(&[(2, 1)]))).unwrap();
        assert_eq!(acks.poll(), 1);
        let _ = received(&mailbox);
        assert_eq!(acks.poll(), 0);
        let out_of_range = mailbox.post(&of(&[(3, 1)]));
        assert!(acks.record(cpu(MAX_CPU_COUNT), out_of_range).is_err());
    }

    /// Several requesters each run shootdown rounds to every target, as `Flusher::flush` does,
    /// and wait for their own acknowledgements while serving their own mailboxes. Targets answer
    /// at arbitrary times, as a CPU does when it leaves an address space or reaches a spin
    /// point. When a round's wait ends, every target must have invalidated the round's page,
    /// one by one or by a full flush that began after it was posted.
    #[test]
    fn rounds_end_only_after_every_target_invalidated() {
        use std::sync::{atomic::AtomicBool, Mutex};

        const CPUS: usize = 4;
        const REQUESTERS: usize = 2;
        const ROUNDS: usize = if cfg!(miri) { 6 } else { 1000 };

        struct Cpu {
            mailbox: TlbMailbox,
            flag: AtomicBool,
            /// Pages invalidated one by one.
            pages: Mutex<std::collections::BTreeSet<usize>>,
            /// Counts posts as they begin, before the page-table changes would be visible.
            posts_begun: AtomicUsize,
            /// `posts_begun` at every full flush, which covers every post begun before its take.
            /// Read after the take, it may also include posts begun just after it, so this check
            /// is exact for page lists and lenient for full flushes. (A full flush can cover a
            /// post before that post's ticket exists, when another requester upgraded it.)
            full_flush_from: Mutex<Vec<usize>>,
        }
        impl Cpu {
            fn serve(&self) {
                if self.flag.swap(false, Ordering::Acquire) {
                    self.mailbox.receive(
                        |page| {
                            self.pages.lock().unwrap().insert(page);
                        },
                        || {
                            let begun = self.posts_begun.load(Ordering::SeqCst);
                            self.full_flush_from.lock().unwrap().push(begun);
                        },
                    );
                }
            }
            fn covered(&self, page: usize, post: usize) -> bool {
                self.pages.lock().unwrap().contains(&page)
                    || self
                        .full_flush_from
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|&begun| post < begun)
            }
        }

        // CPUs 0..REQUESTERS are requesters; all CPUs are targets of the others.
        let cpus: Arc<Vec<Cpu>> = Arc::new(
            (0..CPUS)
                .map(|_| Cpu {
                    mailbox: TlbMailbox::new(),
                    flag: AtomicBool::new(false),
                    pages: Mutex::new(Default::default()),
                    posts_begun: AtomicUsize::new(0),
                    full_flush_from: Mutex::new(Vec::new()),
                })
                .collect(),
        );
        let done = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicUsize::new(0));

        let targets: Vec<_> = (REQUESTERS..CPUS)
            .map(|me| {
                let (cpus, done) = (cpus.clone(), done.clone());
                std::thread::spawn(move || {
                    while !done.load(Ordering::Acquire) {
                        cpus[me].serve();
                        std::thread::yield_now();
                    }
                })
            })
            .collect();

        let requesters: Vec<_> = (0..REQUESTERS)
            .map(|me| {
                let (cpus, finished) = (cpus.clone(), finished.clone());
                std::thread::spawn(move || {
                    for round in 0..ROUNDS {
                        let page = (me * ROUNDS + round) * 2;
                        let mut posted = Vec::new();
                        let mut acks = TlbAcks::new();
                        for (id, target) in cpus.iter().enumerate() {
                            if id == me {
                                continue;
                            }
                            let post = target.posts_begun.fetch_add(1, Ordering::SeqCst);
                            let ack = target.mailbox.post(&of(&[(page, 1)]));
                            posted.push((id, post));
                            acks.record(cpu(id as u32), ack).unwrap();
                            // The interrupt is implicit: targets poll their flags.
                            target.flag.swap(true, Ordering::Release);
                        }
                        while acks.poll() != 0 {
                            // A requester is a target of the other requesters.
                            cpus[me].serve();
                            std::thread::yield_now();
                        }
                        for (id, post) in posted {
                            assert!(
                                cpus[id].covered(page, post),
                                "requester {me} round {round}: CPU {id} acknowledged early"
                            );
                        }
                    }
                    // Keep answering the requesters that are not done yet.
                    finished.fetch_add(1, Ordering::AcqRel);
                    while finished.load(Ordering::Acquire) < REQUESTERS {
                        cpus[me].serve();
                        std::thread::yield_now();
                    }
                })
            })
            .collect();

        for requester in requesters {
            requester.join().unwrap();
        }
        done.store(true, Ordering::Release);
        for target in targets {
            target.join().unwrap();
        }
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
