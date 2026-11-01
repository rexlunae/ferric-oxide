//! Absolute-clock timeouts and their next monotonic wakeup.
//!
//! This is the time-scheme part of the Phase S shared deadline service. It
//! preserves separate monotonic and realtime clock domains: changing the wall
//! clock changes the projection of realtime timers, never their stored dates or
//! monotonic timers. The caller serializes queue mutation and hardware rearming,
//! obtains a consistent [`ClockSnapshot`], and dispatches returned events outside
//! its queue lock. This module neither programs hardware nor invokes callbacks.
//!
//! The clock/event separation follows the published [Zephyr timer contract] and
//! [Ferric scheduling design]. No other operating system implementation is used.
//! Unlike the wrapping, hardware-facing [`crate::Nanos`], [`Timestamp`] preserves
//! the full `u128` nanosecond range of Ferric's time-scheme interface.
//!
//! [Zephyr timer contract]: https://docs.zephyrproject.org/latest/kernel/services/timing/system_timer_drivers.html
//! [Ferric scheduling design]: https://github.com/rexlunae/ferric-oxide/issues/187
//!
//! # Examples
//!
//! ```
//! use ferric_timed::timeout::{Clock, ClockSnapshot, TimeoutQueue, Timestamp};
//!
//! let mut queue = TimeoutQueue::new();
//! let registration = queue.insert(Clock::Realtime, Timestamp::new(1_020), "alarm")?;
//! let clocks = ClockSnapshot::new(Timestamp::new(10), Timestamp::new(1_000));
//! assert_eq!(queue.next_deadline(clocks), Some(Timestamp::new(30)));
//! assert_eq!(queue.cancel(registration), Some("alarm"));
//! assert!(queue.is_empty());
//! # Ok::<(), ferric_timed::timeout::InsertError>(())
//! ```

use alloc::collections::BinaryHeap;
use core::{
    cmp::{Ordering, Reverse},
    fmt, mem,
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
};

// Shared by every queue, including queues dropped and created at the same address.
// Exhaustion is permanent rather than recycling a live or stale token's identity.
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(0);

fn next_registration(counter: &AtomicU64) -> Result<u64, InsertError> {
    counter
        .fetch_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |next| {
            next.checked_add(1)
        })
        .map_err(|_| InsertError::IdentifiersExhausted)
}

struct Entry<T> {
    key: (Timestamp, u64),
    value: T,
}

impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<T> Eq for Entry<T> {}

impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Entry<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

type Heap<T> = BinaryHeap<Reverse<Entry<T>>>;

/// Non-wrapping absolute nanoseconds in a time-scheme clock domain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(u128);

impl Timestamp {
    /// Constructs an absolute nanosecond timestamp.
    pub const fn new(nanos: u128) -> Self {
        Self(nanos)
    }

    /// Returns the absolute nanosecond count.
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// The clock against which an absolute timeout expires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    /// Time elapsed since the monotonic epoch.
    Monotonic,
    /// Wall-clock time, affected by realtime offset changes.
    Realtime,
}

/// Two readings taken with one monotonic sample and a consistent realtime offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSnapshot {
    monotonic: Timestamp,
    realtime: Timestamp,
}

impl ClockSnapshot {
    /// Pairs the monotonic reading and its corresponding realtime reading.
    pub const fn new(monotonic: Timestamp, realtime: Timestamp) -> Self {
        Self {
            monotonic,
            realtime,
        }
    }

    fn now(self, clock: Clock) -> Timestamp {
        match clock {
            Clock::Monotonic => self.monotonic,
            Clock::Realtime => self.realtime,
        }
    }

    fn project(self, clock: Clock, at: Timestamp) -> Timestamp {
        let remaining = at.0.saturating_sub(self.now(clock).0);
        Timestamp(self.monotonic.0.saturating_add(remaining))
    }
}

/// An opaque registration identifying exactly one insertion into its queue.
///
/// Registration identities are unique across all queues and are never recycled.
/// Passing a token to another queue, or cancelling a retired token, returns
/// `None` without removing an unrelated insertion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registration {
    clock: Clock,
    at: Timestamp,
    sequence: u64,
}

/// Why a timeout could not be admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InsertError {
    /// The process-wide registration identity counter is exhausted; it never wraps.
    IdentifiersExhausted,
}

impl fmt::Display for InsertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("timeout registration identifiers exhausted")
    }
}

impl core::error::Error for InsertError {}

/// Ordered timeout storage with independent monotonic and realtime queues.
///
/// Earliest-deadline lookup inspects two heap heads. Expiry retains heap
/// capacity and neither allocates nor deallocates storage, even on the last
/// entry. Insertion may grow storage. Cancellation rebuilds the affected heap
/// in place in O(n), and retains its capacity too. Queue destruction frees it.
/// Queue owners supply synchronization and dispatch returned values themselves.
/// There is no internal clock, periodic polling, hardware state or callback.
pub struct TimeoutQueue<T> {
    monotonic: Heap<T>,
    realtime: Heap<T>,
}

impl<T> Default for TimeoutQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TimeoutQueue<T> {
    /// Constructs an empty queue without allocating.
    pub const fn new() -> Self {
        Self {
            monotonic: BinaryHeap::new(),
            realtime: BinaryHeap::new(),
        }
    }

    /// Returns the number of pending timeouts across both clocks.
    pub fn len(&self) -> usize {
        self.monotonic.len() + self.realtime.len()
    }

    /// Returns whether neither clock has a pending timeout.
    pub fn is_empty(&self) -> bool {
        self.monotonic.is_empty() && self.realtime.is_empty()
    }

    fn queue(&self, clock: Clock) -> &Heap<T> {
        match clock {
            Clock::Monotonic => &self.monotonic,
            Clock::Realtime => &self.realtime,
        }
    }

    fn queue_mut(&mut self, clock: Clock) -> &mut Heap<T> {
        match clock {
            Clock::Monotonic => &mut self.monotonic,
            Clock::Realtime => &mut self.realtime,
        }
    }

    /// Registers a timeout, retaining every insertion even at an identical date.
    ///
    /// Allocation failure follows the allocator's policy. On an admission error,
    /// `value` is dropped without adding an entry.
    ///
    /// # Errors
    /// Returns [`InsertError::IdentifiersExhausted`] when the process-wide
    /// identity source is exhausted. It never wraps or recycles identities.
    pub fn insert(
        &mut self,
        clock: Clock,
        at: Timestamp,
        value: T,
    ) -> Result<Registration, InsertError> {
        let sequence = next_registration(&NEXT_REGISTRATION)?;
        self.queue_mut(clock).push(Reverse(Entry {
            key: (at, sequence),
            value,
        }));
        Ok(Registration {
            clock,
            at,
            sequence,
        })
    }

    /// Removes a pending registration from this queue, returning its value.
    /// Returns `None` for another queue's token or after this registration has
    /// fired or been cancelled. Takes O(n) without allocating or freeing storage.
    pub fn cancel(&mut self, registration: Registration) -> Option<T> {
        let queue = self.queue_mut(registration.clock);
        let index = queue
            .iter()
            .position(|entry| entry.0.key == (registration.at, registration.sequence))?;
        let mut entries = mem::take(queue).into_vec();
        let Reverse(entry) = entries.swap_remove(index);
        *queue = BinaryHeap::from(entries);
        Some(entry.value)
    }

    /// Cancels all values selected by `predicate`, returning their count.
    ///
    /// Takes O(n), retains heap capacity and does not allocate storage. Removed
    /// values are dropped immediately; callers must account for their destructors
    /// and any work in `predicate`. A kernel can use this on final owner close,
    /// serialized with admission, without retaining a second list of tokens.
    ///
    /// # Panics
    /// Propagates a panic from `predicate` or a removed value's destructor.
    pub fn cancel_where(&mut self, mut predicate: impl FnMut(&T) -> bool) -> usize {
        let before = self.len();
        self.monotonic.retain(|entry| !predicate(&entry.0.value));
        self.realtime.retain(|entry| !predicate(&entry.0.value));
        before - self.len()
    }

    /// Returns the next required wakeup in monotonic nanoseconds.
    ///
    /// A due timeout projects to `clocks`' current monotonic time. Realtime
    /// projection is recomputed on every call, so wall-clock offset changes do
    /// not alter monotonic timers or require rewriting realtime entries.
    /// Projection beyond the timestamp range saturates at `u128::MAX`.
    /// Returns `None` when no timeout exists.
    pub fn next_deadline(&self, clocks: ClockSnapshot) -> Option<Timestamp> {
        [Clock::Monotonic, Clock::Realtime]
            .into_iter()
            .filter_map(|clock| {
                self.queue(clock)
                    .peek()
                    .map(|entry| clocks.project(clock, entry.0.key.0))
            })
            .min()
    }

    /// Returns the latest projected monotonic deadline for diagnostics.
    ///
    /// This scans all entries in O(n), unlike [`Self::next_deadline`]'s two
    /// head checks. It is not intended for hardware rearming or IRQ paths.
    pub fn latest_deadline(&self, clocks: ClockSnapshot) -> Option<Timestamp> {
        [Clock::Monotonic, Clock::Realtime]
            .into_iter()
            .flat_map(|clock| {
                self.queue(clock)
                    .iter()
                    .map(move |entry| clocks.project(clock, entry.0.key.0))
            })
            .max()
    }

    /// Removes one due timeout, with monotonic timeouts considered first.
    ///
    /// Equal dates within a clock are returned in insertion order. A second
    /// consumer cannot receive an entry already removed under the owner's lock.
    /// The caller invokes any callback only after releasing that lock. Removal
    /// retains storage even when the queue becomes empty, so it never calls the
    /// allocator. Ownership of `T` transfers to the caller; IRQ bindings must
    /// still account for callbacks and destructors of returned values.
    pub fn pop_due(&mut self, clocks: ClockSnapshot) -> Option<T> {
        for clock in [Clock::Monotonic, Clock::Realtime] {
            let queue = self.queue_mut(clock);
            if queue
                .peek()
                .is_some_and(|entry| entry.0.key.0 <= clocks.now(clock))
            {
                return queue.pop().map(|entry| entry.0.value);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    fn clocks(mono: u128, real: u128) -> ClockSnapshot {
        ClockSnapshot::new(Timestamp::new(mono), Timestamp::new(real))
    }

    #[test]
    fn an_empty_queue_needs_no_interrupt() {
        let mut queue = TimeoutQueue::<()>::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.next_deadline(clocks(10, 1_000)), None);
        assert_eq!(queue.latest_deadline(clocks(10, 1_000)), None);
        assert_eq!(queue.pop_due(clocks(u128::MAX, u128::MAX)), None);
    }

    #[test]
    fn mixed_clocks_choose_the_earliest_monotonic_wakeup() {
        let mut queue = TimeoutQueue::new();
        queue
            .insert(Clock::Monotonic, Timestamp::new(40), 'm')
            .unwrap();
        queue
            .insert(Clock::Realtime, Timestamp::new(1_020), 'r')
            .unwrap();
        assert_eq!(
            queue.next_deadline(clocks(10, 1_000)),
            Some(Timestamp::new(30))
        );
        assert_eq!(
            queue.latest_deadline(clocks(10, 1_000)),
            Some(Timestamp::new(40))
        );
        assert_eq!(queue.pop_due(clocks(29, 1_019)), None);
        assert_eq!(queue.pop_due(clocks(30, 1_020)), Some('r'));
        assert_eq!(
            queue.latest_deadline(clocks(30, 1_020)),
            Some(Timestamp::new(40))
        );
        assert_eq!(
            queue.next_deadline(clocks(30, 1_020)),
            Some(Timestamp::new(40))
        );
        assert_eq!(queue.pop_due(clocks(40, 1_030)), Some('m'));
        assert!(queue.is_empty());
        assert_eq!(queue.latest_deadline(clocks(40, 1_030)), None);
    }

    #[test]
    fn realtime_rebases_change_only_realtime_projections() {
        let mut queue = TimeoutQueue::new();
        let mono = queue
            .insert(Clock::Monotonic, Timestamp::new(100), 'm')
            .unwrap();
        queue
            .insert(Clock::Realtime, Timestamp::new(1_050), 'r')
            .unwrap();
        assert_eq!(
            queue.next_deadline(clocks(10, 1_000)),
            Some(Timestamp::new(60))
        );
        assert_eq!(
            queue.latest_deadline(clocks(10, 1_000)),
            Some(Timestamp::new(100))
        );
        assert_eq!(
            queue.next_deadline(clocks(20, 900)),
            Some(Timestamp::new(100))
        );
        assert_eq!(
            queue.latest_deadline(clocks(20, 900)),
            Some(Timestamp::new(170))
        );
        assert_eq!(
            queue.next_deadline(clocks(30, 1_060)),
            Some(Timestamp::new(30))
        );
        assert_eq!(queue.pop_due(clocks(30, 1_060)), Some('r'));
        assert_eq!(
            queue.next_deadline(clocks(30, 1_060)),
            Some(Timestamp::new(100))
        );
        assert_eq!(queue.cancel(mono), Some('m'));
    }

    #[test]
    fn same_date_registrations_remain_distinct_and_ordered() {
        let mut queue = TimeoutQueue::new();
        let ids: Vec<_> = (0..4)
            .map(|n| {
                queue
                    .insert(Clock::Monotonic, Timestamp::new(10), n)
                    .unwrap()
            })
            .collect();
        assert_eq!(queue.cancel(ids[1]), Some(1));
        let values: Vec<_> = core::iter::from_fn(|| queue.pop_due(clocks(10, 100))).collect();
        assert_eq!(values, vec![0, 2, 3]);
        for id in ids {
            assert_eq!(queue.cancel(id), None);
        }
    }

    #[test]
    fn cancelling_the_earliest_selects_the_next_live_entry() {
        let mut queue = TimeoutQueue::new();
        let first = queue
            .insert(Clock::Realtime, Timestamp::new(1_010), ())
            .unwrap();
        queue
            .insert(Clock::Monotonic, Timestamp::new(50), ())
            .unwrap();
        assert_eq!(queue.cancel(first), Some(()));
        assert_eq!(
            queue.next_deadline(clocks(10, 1_000)),
            Some(Timestamp::new(50))
        );
        assert_eq!(queue.cancel(first), None);
    }

    #[test]
    fn stale_cancellation_cannot_remove_a_later_registration() {
        let mut queue = TimeoutQueue::new();
        let old = queue
            .insert(Clock::Monotonic, Timestamp::new(5), 'a')
            .unwrap();
        assert_eq!(queue.pop_due(clocks(5, 100)), Some('a'));
        queue
            .insert(Clock::Monotonic, Timestamp::new(5), 'b')
            .unwrap();
        assert_eq!(queue.cancel(old), None);
        assert_eq!(queue.pop_due(clocks(5, 100)), Some('b'));
    }

    #[test]
    fn dates_beyond_the_hardware_u64_range_are_preserved() {
        let mut queue = TimeoutQueue::new();
        let far = u128::from(u64::MAX) + 123;
        queue
            .insert(Clock::Monotonic, Timestamp::new(far), ())
            .unwrap();
        assert_eq!(queue.pop_due(clocks(far - 1, 0)), None);
        assert_eq!(queue.next_deadline(clocks(0, 0)), Some(Timestamp::new(far)));
        assert_eq!(queue.pop_due(clocks(far, 0)), Some(()));
    }

    #[test]
    fn unrepresentable_projection_saturates_without_wrapping_due() {
        let mut queue = TimeoutQueue::new();
        queue
            .insert(Clock::Realtime, Timestamp::new(u128::MAX), ())
            .unwrap();
        assert_eq!(
            queue.next_deadline(clocks(10, 0)),
            Some(Timestamp::new(u128::MAX))
        );
        assert_eq!(queue.pop_due(clocks(10, 0)), None);
        assert_eq!(queue.pop_due(clocks(10, u128::MAX)), Some(()));
    }

    #[test]
    fn exhaustion_never_reuses_an_identity() {
        let identities = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_registration(&identities), Ok(u64::MAX - 1));
        for _ in 0..3 {
            assert_eq!(
                next_registration(&identities),
                Err(InsertError::IdentifiersExhausted)
            );
        }
    }

    #[test]
    fn tokens_cannot_cancel_another_queue_even_at_the_same_date() {
        let mut first = TimeoutQueue::new();
        let mut second = TimeoutQueue::new();
        let a = first
            .insert(Clock::Monotonic, Timestamp::new(10), 'a')
            .unwrap();
        let b = second
            .insert(Clock::Monotonic, Timestamp::new(10), 'b')
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(second.cancel(a), None);
        assert_eq!(first.cancel(b), None);
        assert_eq!(second.cancel(b), Some('b'));
        drop(second);
        let mut replacement = TimeoutQueue::new();
        replacement
            .insert(Clock::Monotonic, Timestamp::new(10), 'c')
            .unwrap();
        assert_eq!(replacement.cancel(b), None);
        assert_eq!(replacement.pop_due(clocks(10, 0)), Some('c'));
        assert_eq!(first.cancel(a), Some('a'));
    }

    #[test]
    fn final_owner_cancellation_preserves_other_owners_and_ordering() {
        let mut queue = TimeoutQueue::new();
        for owner in 0..4 {
            for clock in [Clock::Monotonic, Clock::Realtime] {
                for date in 0..8 {
                    queue
                        .insert(clock, Timestamp::new(date), (owner, date))
                        .unwrap();
                }
            }
        }
        assert_eq!(queue.cancel_where(|&(owner, _)| owner == 1), 16);
        assert_eq!(queue.cancel_where(|&(owner, _)| owner == 1), 0);
        assert_eq!(queue.len(), 48);
        let mut expired = 0;
        while let Some((owner, date)) = queue.pop_due(clocks(8, 8)) {
            assert_ne!(owner, 1);
            assert!(date < 8);
            expired += 1;
        }
        assert_eq!(expired, 48);
    }

    #[test]
    fn multiple_consumers_under_a_shared_lock_deliver_each_entry_once() {
        let mut queue = TimeoutQueue::new();
        for n in 0..256 {
            queue
                .insert(
                    if n % 2 == 0 {
                        Clock::Monotonic
                    } else {
                        Clock::Realtime
                    },
                    Timestamp::new(n),
                    n,
                )
                .unwrap();
        }
        let queue = Arc::new(std::sync::Mutex::new(queue));
        let results = Arc::new(std::sync::Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let queue = Arc::clone(&queue);
                let results = Arc::clone(&results);
                scope.spawn(move || {
                    loop {
                        let value = queue.lock().unwrap().pop_due(clocks(256, 256));
                        let Some(value) = value else { break };
                        results.lock().unwrap().push(value);
                    }
                });
            }
        });
        let mut results = results.lock().unwrap();
        results.sort_unstable();
        assert_eq!(*results, (0..256).collect::<Vec<_>>());
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn mixed_insertions_cancellations_and_clock_changes_match_the_pending_set() {
        use alloc::collections::BTreeSet;

        let mut queue = TimeoutQueue::new();
        let mut pending = Vec::new();
        for value in 0..300 {
            let clock = if value % 3 == 0 {
                Clock::Realtime
            } else {
                Clock::Monotonic
            };
            let at = (value * 37 % 101) as u128 + if clock == Clock::Realtime { 1_000 } else { 0 };
            let registration = queue.insert(clock, Timestamp::new(at), value).unwrap();
            pending.push((clock, at, value, registration));
        }
        pending.retain(|&(_, _, value, registration)| {
            if value % 7 == 0 {
                assert_eq!(queue.cancel(registration), Some(value));
                false
            } else {
                true
            }
        });

        for (mono, real) in [
            (0, 1_000),
            (10, 1_010),
            (20, 800),
            (30, 1_040),
            (100, 1_200),
        ] {
            let mut expected = BTreeSet::new();
            pending.retain(|&(clock, at, value, _)| {
                let due = at <= if clock == Clock::Realtime { real } else { mono };
                if due {
                    expected.insert(value);
                }
                !due
            });
            let mut actual = BTreeSet::new();
            while let Some(value) = queue.pop_due(clocks(mono, real)) {
                assert!(actual.insert(value), "a timeout was delivered twice");
            }
            assert_eq!(actual, expected);
            assert_eq!(queue.len(), pending.len());
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn removal_transfers_ownership_and_drops_each_value_once() {
        struct Owned(Arc<AtomicUsize>);
        impl Drop for Owned {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut queue = TimeoutQueue::new();
        let first = queue
            .insert(
                Clock::Monotonic,
                Timestamp::new(10),
                Owned(Arc::clone(&drops)),
            )
            .unwrap();
        queue
            .insert(
                Clock::Realtime,
                Timestamp::new(10),
                Owned(Arc::clone(&drops)),
            )
            .unwrap();
        queue
            .insert(
                Clock::Monotonic,
                Timestamp::new(20),
                Owned(Arc::clone(&drops)),
            )
            .unwrap();
        let cancelled = queue.cancel(first).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(cancelled);
        drop(queue.pop_due(clocks(10, 10)));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        drop(queue);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }
}
