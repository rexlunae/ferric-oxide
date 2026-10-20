//! The Phase 2 disk benchmark for Ferric Oxide: fio-style read jobs against the NVMe disk,
//! with each of nvmed's queue policies.
//!
//! Run without arguments, it runs the default jobs ([`DEFAULT_JOBS`]):
//!
//! - `randread-4k-qd1`: 4 KiB random reads, one at a time (latency).
//! - `randread-4k-qd32`: 4 KiB random reads, 32 in flight (IOPS).
//! - `read-128k-qd1`: 128 KiB sequential reads, one at a time (bandwidth).
//!
//! With arguments, it runs the one job they describe, in fio's option syntax
//! (`ferric-iobench --rw=randread --bs=4k --iodepth=8`; `ferric_iobench::job` lists the
//! options).
//!
//! Each job reads its `io_size` bytes from the start of the largest partition of the NVMe
//! disk scheme, each block once, with `iodepth × numjobs` threads that each keep one `pread`
//! in flight. When nvmed has per-CPU queue pairs, every round runs the job once per queue
//! policy (`MainQueue`, the one-queue path; `Adaptive`, the default; `Workers`), alternating
//! their order between rounds, and `ferric_blkqueue::ab` compares `Adaptive` against
//! `MainQueue` round by round, as the `nvme-read-4k-t<N>` benchmark does.
//!
//! Output, one line per policy and one for the comparison:
//!
//! ```text
//! FERRIC_BENCH iobench-randread-4k-qd32 policy=main rounds=3 ios=… iops=… mib_s=… lat_…_ns=…
//! FERRIC_BENCH iobench-randread-4k-qd32 ab_baseline_iops=… ab_candidate_iops=… ab_ratio=…
//!   ab_tolerance=… ab_rounds=3 ab=pass
//! ```
//!
//! Without per-CPU queue pairs there is one path, `policy=single`, and `ab=none`. Each line's
//! figures cover every round of that policy. It prints `FERRIC_TEST_SKIP iobench: <why>` and
//! exits zero when there is no NVMe disk, `FERRIC_TEST_FAIL iobench: <why>` and exits
//! non-zero when a job cannot run, and `FERRIC_TEST_PASS iobench` when every job ran.

#[cfg(target_os = "redox")]
mod redox;
#[cfg_attr(not(target_os = "redox"), allow(dead_code))]
mod round;

/// The jobs a run without arguments runs, in fio's option syntax.
#[cfg_attr(not(target_os = "redox"), allow(dead_code))]
const DEFAULT_JOBS: [&str; 3] = [
    "--rw=randread --bs=4k --iodepth=1 --io_size=4m",
    "--rw=randread --bs=4k --iodepth=32 --io_size=8m",
    "--rw=read --bs=128k --iodepth=1 --io_size=32m",
];

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP iobench: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_jobs_parse() {
        for job in DEFAULT_JOBS {
            ferric_iobench::job::Job::parse(job.split_whitespace()).unwrap();
        }
    }
}
