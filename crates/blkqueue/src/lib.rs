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

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod ab;
pub mod abi;

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
