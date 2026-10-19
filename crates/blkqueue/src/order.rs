//! Closing a descriptor only after its in-flight requests are done.
//!
//! A disk scheme that serves reads and writes as concurrent tasks may get a descriptor's
//! close while some of its requests are still running. Closing at once would release the
//! descriptor's state (its exclusive lock on the disk, its attached grants) while the device
//! still works for it, so a later open could see the lock gone before the writes landed.
//! [`CloseGate`] counts each descriptor's in-flight requests and holds a close back until the
//! last of them ends; the scheme then runs the closes [`take_ready`](CloseGate::take_ready)
//! hands it before it serves its next request, which is the first point anyone could observe
//! the difference.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// Per-descriptor in-flight counts and the closes waiting on them.
#[derive(Debug, Default)]
pub struct CloseGate {
    in_flight: BTreeMap<usize, usize>,
    deferred: BTreeSet<usize>,
    ready: Vec<usize>,
}

impl CloseGate {
    /// An empty gate.
    pub const fn new() -> Self {
        Self {
            in_flight: BTreeMap::new(),
            deferred: BTreeSet::new(),
            ready: Vec::new(),
        }
    }

    /// Counts a request on descriptor `id` as started.
    pub fn begin(&mut self, id: usize) {
        *self.in_flight.entry(id).or_insert(0) += 1;
    }

    /// Counts a request on descriptor `id` as done. When it was the last one and the
    /// descriptor's close is waiting, the close becomes ready.
    ///
    /// # Panics
    ///
    /// When no request on `id` is in flight (a `begin`/`end` mismatch, a bug in the caller).
    pub fn end(&mut self, id: usize) {
        let count = self
            .in_flight
            .get_mut(&id)
            .expect("CloseGate::end without a matching begin");
        *count -= 1;
        if *count == 0 {
            self.in_flight.remove(&id);
            if self.deferred.remove(&id) {
                self.ready.push(id);
            }
        }
    }

    /// Asks to close descriptor `id`: `true` when nothing is in flight on it, so the caller
    /// closes it now; `false` when the close waits for its requests and comes back from
    /// [`take_ready`](Self::take_ready).
    pub fn close(&mut self, id: usize) -> bool {
        if self.in_flight.contains_key(&id) {
            self.deferred.insert(id);
            false
        } else {
            true
        }
    }

    /// How many requests on descriptor `id` are in flight.
    pub fn in_flight(&self, id: usize) -> usize {
        self.in_flight.get(&id).copied().unwrap_or(0)
    }

    /// Takes the descriptors whose waiting close may now run, in the order they became ready.
    pub fn take_ready(&mut self) -> Vec<usize> {
        core::mem::take(&mut self.ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_without_requests_runs_at_once() {
        let mut gate = CloseGate::new();
        assert!(gate.close(3));
        assert!(gate.take_ready().is_empty());
    }

    #[test]
    fn close_waits_for_the_last_request() {
        let mut gate = CloseGate::new();
        gate.begin(3);
        gate.begin(3);
        assert!(!gate.close(3));
        gate.end(3);
        assert!(gate.take_ready().is_empty(), "one request still runs");
        assert_eq!(gate.in_flight(3), 1);
        gate.end(3);
        assert_eq!(gate.take_ready(), [3]);
        assert!(gate.take_ready().is_empty(), "a close is handed out once");
        assert_eq!(gate.in_flight(3), 0);
    }

    #[test]
    fn other_descriptors_do_not_hold_a_close() {
        let mut gate = CloseGate::new();
        gate.begin(1);
        gate.begin(2);
        assert!(!gate.close(1));
        assert!(gate.close(4));
        gate.end(2);
        assert!(gate.take_ready().is_empty());
        gate.end(1);
        assert_eq!(gate.take_ready(), [1]);
    }

    #[test]
    fn requests_ending_without_a_close_leave_nothing_ready() {
        let mut gate = CloseGate::new();
        gate.begin(5);
        gate.end(5);
        assert!(gate.take_ready().is_empty());
        assert!(gate.close(5));
    }

    #[test]
    #[should_panic(expected = "without a matching begin")]
    fn unmatched_end_panics() {
        CloseGate::new().end(7);
    }
}
