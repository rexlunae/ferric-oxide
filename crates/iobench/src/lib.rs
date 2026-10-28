//! Block I/O benchmark jobs described with fio's options, for Ferric Oxide's disk benchmark
//! (roadmap Phase 2, the I/O exit benchmark).
//!
//! The on-target runner (`recipes/tests/ferric-iobench`) times reads of the NVMe disk with
//! each of nvmed's queue policies. Everything here runs on the host too, so `cargo test`
//! covers it:
//!
//! - [`job`] parses a job from fio-style options (`--rw=randread --bs=4k --iodepth=32`),
//!   refuses what the runner does not do, and prints the fio command line that runs the
//!   same job on Linux. Ferric's `--ioengine=regbuf|ring` and `--align=page` choose the
//!   request path, to measure the per-request mapping of the read buffer.
//! - [`offsets`] is the order a job reads its blocks in: in sequence, or a seeded random
//!   permutation that reads every block once, as fio does with its random map.
//! - [`latency`] is a latency histogram with fio's bucket layout (64 linear buckets per
//!   power of two, under 1.6% error), and its percentiles.
//! - [`report`] turns a job's I/O count, time and latencies into IOPS, bandwidth and
//!   latency figures, and formats them as a `FERRIC_BENCH` line.
//!
//! The same-boot A/B gate between queue policies is `ferric_blkqueue::ab`; the runner feeds
//! it each round's IOPS.
//!
//! # Examples
//!
//! ```
//! use ferric_iobench::job::{Job, Rw};
//!
//! let job = Job::parse(["--rw=randread", "--bs=4k", "--iodepth=32"]).unwrap();
//! assert_eq!(job.rw, Rw::RandRead);
//! assert_eq!(job.bs, 4096);
//! assert_eq!(job.submitters(), 32);
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod job;
pub mod latency;
pub mod offsets;
pub mod report;
