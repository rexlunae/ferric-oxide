//! Per-CPU disk queue routing (roadmap Phase 2.4, per-core NVMe queues).
//!
//! A disk driver with one I/O queue pair per CPU (nvmed with `drivers.nvme_percore`) can run
//! each request on the queue pair of the thread that received it, or hand it to a worker
//! thread pinned to another CPU, at the cost of two cross-CPU hand-offs. A hand-off only pays
//! when requests overlap, so the default [`QueuePolicy::Adaptive`] runs a request on the
//! receiving thread's own queue pair when nothing else is in flight, and on a worker
//! otherwise.
//!
//! - [`QueuePolicy`] and [`Route`] are the choice and its outcome, with
//!   [`QueuePolicy::route`] the one place the rule lives.
//! - [`abi`] is the one definition of the call that sets a disk's policy, shared by
//!   driver-block and its clients (the same-boot benchmark compares the policies with it).
//! - [`ab`] summarises that benchmark's rounds and holds its pass rule.
//! - [`ids`] reads the finished jobs' ids back from the workers' pipe, keeping a part of an
//!   id that one read splits for the next, and reserves the id a worker reports as it exits.
//! - `openers` (with the `alloc` feature) records which process opened each disk
//!   descriptor, so only that process may set the policy through it.
//! - `order` (with the `alloc` feature) holds a descriptor's close back until its in-flight
//!   requests are done, for a scheme that serves requests concurrently.
//! - `slot` (with the `std` feature) is a job's result slot between the thread that waits
//!   and the worker that runs it, which settles once, done or abandoned.
//!
//! # Examples
//!
//! ```
//! use ferric_blkqueue::{QueuePolicy, Route};
//!
//! assert_eq!(QueuePolicy::Adaptive.route(0), Route::Main);
//! assert_eq!(QueuePolicy::Adaptive.route(1), Route::Worker);
//! assert_eq!(QueuePolicy::MainQueue.route(3), Route::Main);
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod ab;
pub mod abi;
pub mod ids;
#[cfg(feature = "alloc")]
pub mod openers;
#[cfg(feature = "alloc")]
pub mod order;
#[cfg(feature = "std")]
pub mod slot;

/// How a disk driver with per-CPU queue pairs places requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum QueuePolicy {
    /// On the receiving thread's queue pair when no other request is in flight, on a worker
    /// otherwise.
    #[default]
    Adaptive = 0,
    /// Always on the receiving thread's queue pair, as a driver with one I/O queue does.
    MainQueue = 1,
    /// Always on a worker.
    Workers = 2,
}

impl QueuePolicy {
    /// The policy numbered `raw` in the [`abi`], or `None` for an unknown number.
    pub const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            0 => Some(Self::Adaptive),
            1 => Some(Self::MainQueue),
            2 => Some(Self::Workers),
            _ => None,
        }
    }

    /// Where a new request runs, given how many requests are already in flight (on any
    /// queue pair).
    pub const fn route(self, in_flight: usize) -> Route {
        match self {
            Self::Adaptive if in_flight == 0 => Route::Main,
            Self::Adaptive | Self::Workers => Route::Worker,
            Self::MainQueue => Route::Main,
        }
    }

    /// Whether the disk scheme should serve requests concurrently under this policy.
    ///
    /// Only [`Workers`](Self::Workers) does. Measured in the same-boot A/B on QEMU, sending
    /// overlapping requests to workers lost to queueing them on the receiving thread's pair
    /// (two-thread reads at 0.78 of the single queue, four-thread at 0.67): each hand-off
    /// costs two cross-CPU wake-ups, and the emulated controller serves its queues from one
    /// thread. So [`Adaptive`](Self::Adaptive) and [`MainQueue`](Self::MainQueue) are served
    /// one call at a time, which keeps every request on the receiving thread's pair.
    pub const fn serves_concurrently(self) -> bool {
        matches!(self, Self::Workers)
    }
}

/// Where one request runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    /// On the queue pair of the thread that received the request.
    Main,
    /// On a worker thread's queue pair.
    Worker,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_workers_policy_serves_concurrently() {
        assert!(QueuePolicy::Workers.serves_concurrently());
        assert!(!QueuePolicy::Adaptive.serves_concurrently());
        assert!(!QueuePolicy::MainQueue.serves_concurrently());
    }

    #[test]
    fn adaptive_hands_off_only_overlapping_requests() {
        assert_eq!(QueuePolicy::Adaptive.route(0), Route::Main);
        for in_flight in 1..8 {
            assert_eq!(QueuePolicy::Adaptive.route(in_flight), Route::Worker);
        }
    }

    #[test]
    fn fixed_policies_ignore_load() {
        for in_flight in 0..8 {
            assert_eq!(QueuePolicy::MainQueue.route(in_flight), Route::Main);
            assert_eq!(QueuePolicy::Workers.route(in_flight), Route::Worker);
        }
    }

    #[test]
    fn raw_numbers_round_trip() {
        for policy in [
            QueuePolicy::Adaptive,
            QueuePolicy::MainQueue,
            QueuePolicy::Workers,
        ] {
            assert_eq!(QueuePolicy::from_raw(policy as u64), Some(policy));
        }
        assert_eq!(QueuePolicy::from_raw(3), None);
        assert_eq!(QueuePolicy::default(), QueuePolicy::Adaptive);
    }
}
