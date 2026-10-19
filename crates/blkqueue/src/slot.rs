//! A job's result slot, shared between the thread that waits for it and the worker that
//! runs it, which settles exactly once: with the result, or abandoned when the worker drops
//! the job without finishing it (its thread panicked, or it was never received).
//!
//! The worker holds the job through a [`Claim`]. Completing the claim stores the result;
//! dropping it any other way marks the slot abandoned, so a waiter never needs to guess from
//! reference counts whether a result is still coming.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

const PENDING: u8 = 0;
const DONE: u8 = 1;
const ABANDONED: u8 = 2;

/// Where a job stands, as its waiter sees it.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome<T> {
    /// The worker has not settled it yet.
    Pending,
    /// The worker finished it; the result, the first time it is taken.
    Done(Option<T>),
    /// The worker dropped it without finishing it.
    Abandoned,
}

/// The shared slot.
#[derive(Debug)]
pub struct JobSlot<T> {
    state: AtomicU8,
    result: Mutex<Option<T>>,
}

impl<T> JobSlot<T> {
    /// A pending slot and the claim the worker settles it through.
    pub fn new() -> (Arc<Self>, Claim<T>) {
        let slot = Arc::new(Self {
            state: AtomicU8::new(PENDING),
            result: Mutex::new(None),
        });
        let claim = Claim {
            slot: Some(Arc::clone(&slot)),
        };
        (slot, claim)
    }

    /// Whether the slot is settled, either way (the worker no longer touches the job).
    pub fn is_settled(&self) -> bool {
        self.state.load(Ordering::Acquire) != PENDING
    }

    /// Whether the worker finished the job, without taking its result.
    pub fn is_done(&self) -> bool {
        self.state.load(Ordering::Acquire) == DONE
    }

    /// The job's outcome; a [`Outcome::Done`] result is taken out of the slot.
    pub fn take(&self) -> Outcome<T> {
        match self.state.load(Ordering::Acquire) {
            PENDING => Outcome::Pending,
            DONE => Outcome::Done(
                self.result
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take(),
            ),
            _ => Outcome::Abandoned,
        }
    }
}

/// The worker's hold on a job: [`complete`](Self::complete) it, or drop it to abandon the
/// job.
#[derive(Debug)]
pub struct Claim<T> {
    slot: Option<Arc<JobSlot<T>>>,
}

impl<T> Claim<T> {
    /// Stores the job's result and settles the slot as done.
    pub fn complete(mut self, result: T) {
        if let Some(slot) = self.slot.take() {
            *slot.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
            slot.state.store(DONE, Ordering::Release);
        }
    }
}

impl<T> Drop for Claim<T> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.state.store(ABANDONED, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_job_hands_its_result_out_once() {
        let (slot, claim) = JobSlot::new();
        assert_eq!(slot.take(), Outcome::Pending);
        assert!(!slot.is_settled());
        claim.complete(7);
        assert!(slot.is_settled() && slot.is_done());
        assert_eq!(slot.take(), Outcome::Done(Some(7)));
        assert_eq!(slot.take(), Outcome::Done(None));
    }

    #[test]
    fn dropped_claim_abandons_the_job() {
        let (slot, claim) = JobSlot::<u32>::new();
        drop(claim);
        assert!(slot.is_settled() && !slot.is_done());
        assert_eq!(slot.take(), Outcome::Abandoned);
    }

    #[test]
    fn a_worker_that_panics_abandons_its_job() {
        let (slot, claim) = JobSlot::<u32>::new();
        let worker = std::thread::spawn(move || {
            let _held = claim;
            panic!("worker dies mid-job");
        });
        assert!(worker.join().is_err());
        assert_eq!(slot.take(), Outcome::Abandoned);
    }

    #[test]
    fn other_holders_do_not_keep_the_job_pending() {
        // The waiter and a watchdog both hold the slot; the worker's exit still settles it.
        let (slot, claim) = JobSlot::<u32>::new();
        let watchdog = Arc::clone(&slot);
        drop(claim);
        assert_eq!(Arc::strong_count(&slot), 2);
        assert_eq!(watchdog.take(), Outcome::Abandoned);
    }
}
