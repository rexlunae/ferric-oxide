//! Shared request lifetime accounting across dispatch and worker threads.
//!
//! A claim moves with a queued request and stays live until response or drop.
//! Both inline futures and direct-response workers use the same count.
//!
//! ```
//! use ferric_blkqueue::activity::Activity;
//! let requests = Activity::default();
//! let inline = requests.claim();
//! let worker = requests.claim();
//! drop(inline);
//! assert_eq!(requests.count(), 1);
//! drop(worker);
//! assert_eq!(requests.count(), 0);
//! ```

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Shared activity count; cloning a counter does not create a request claim.
#[derive(Clone, Default)]
pub struct Activity(Arc<AtomicUsize>);

impl Activity {
    /// Current queued and running request count.
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    /// Counts one request until the returned claim is dropped, on any thread.
    pub fn claim(&self) -> Claim {
        self.0.fetch_add(1, Ordering::AcqRel);
        Claim(Arc::clone(&self.0))
    }
}

/// A request's activity ownership, transferred with its queued/running work.
///
/// Dropping it after completion, failed delivery or abandonment removes exactly
/// one claim. It is deliberately not Clone.
pub struct Claim(Arc<AtomicUsize>);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn worker_claim_survives_inline_completion_and_failed_delivery() {
        let activity = Activity::default();
        let inline = activity.claim();
        let worker = activity.claim();
        assert_eq!(activity.count(), 2);
        drop(inline);
        assert_eq!(activity.count(), 1);
        std::thread::spawn(move || drop(worker)).join().unwrap();
        assert_eq!(activity.count(), 0);
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let returned = tx.send(activity.claim()).unwrap_err();
        assert_eq!(activity.count(), 1);
        drop(returned);
        assert_eq!(activity.count(), 0);
    }
}
