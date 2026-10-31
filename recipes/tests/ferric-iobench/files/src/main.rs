//! The Phase 2 disk benchmark for Ferric Oxide: fio-style read jobs against the NVMe disk,
//! with each of nvmed's queue policies, and file jobs through the file system and the page
//! cache.
//!
//! Run without arguments, it runs the default jobs ([`DEFAULT_JOBS`]):
//!
//! - `randread-4k-qd1`: 4 KiB random reads, one at a time (latency).
//! - `randread-4k-qd32`: 4 KiB random reads, 32 in flight (IOPS).
//! - `read-128k-qd1`: 128 KiB sequential reads, one at a time (bandwidth).
//! - `file-randwrite-4k-qd1`: 4 KiB random overwrites of a file in `/tmp`, one at a time,
//!   then one `fsync` (the cached-write path).
//! - `file-write-128k-qd1`: 128 KiB sequential overwrites of that file, then one `fsync`.
//!
//! A file job (`--filename`) runs each round once through RedoxFS alone (`path=fs`, an
//! `O_FSYNC` descriptor) and once through the page cache (`path=cache`), and compares the
//! two in the same boot; `file.rs` has the details. Its lines have `path=` where a disk job's
//! have `policy=`.
//!
//! With arguments, it runs the one job they describe, in fio's option syntax
//! (`ferric-iobench --rw=randread --bs=4k --iodepth=8`; `ferric_iobench::job` lists the
//! options).
//!
//! `--ioengine` and `--align` choose the request path (`engine`; "Request paths" in
//! `ferric_iobench::job`): `pread` into a heap buffer (the default), `pread` into a
//! page-aligned buffer (`--align=page`), reads through a registered-buffer grant with no
//! per-request mapping (`--ioengine=regbuf`), or reads as entries on a kernel ring
//! (`--ioengine=ring`). A path that needs a feature the system was built without
//! (`kernel.regbuf`, `drivers.grant_dma` and an IOMMU, or `kernel.rings`) fails the run with
//! a message naming the feature. Paths other than the default add a suffix to the job's name
//! (`iobench-randread-4k-qd1-regbuf`).
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
//! exits zero when there is no NVMe disk and no file job, `FERRIC_TEST_FAIL iobench: <why>`
//! and exits non-zero when a job cannot run, and `FERRIC_TEST_PASS iobench` when every job
//! ran.
//!
//! On a kernel with `kernel.tlb_stats` (`/scheme/sys/tlb`), it also prints, per policy, how
//! much TLB maintenance each read cost, from the counters' growth over the rounds' timed
//! parts (`ferric_trace::tlb::PerOp`): local flushes, shootdowns, shootdown IPIs, CR3
//! loads, system calls and IOTLB flushes per read, and targets, pages and acknowledgement
//! wait per shootdown. The counters are system-wide, so whatever else runs meanwhile
//! (nvmed, the file system daemons) is included, which for disk reads is the point:
//!
//! ```text
//! FERRIC_BENCH iobench-randread-4k-qd32 tlb_policy=main kpti=on pcid=on tlb_ops=… flushes_per_op=…
//! ```

#[cfg(target_os = "redox")]
mod engine;
#[cfg(target_os = "redox")]
mod file;
#[cfg(target_os = "redox")]
mod redox;

/// The jobs a run without arguments runs, in fio's option syntax.
#[cfg_attr(not(target_os = "redox"), allow(dead_code))]
const DEFAULT_JOBS: [&str; 5] = [
    "--rw=randread --bs=4k --iodepth=1 --io_size=4m",
    "--rw=randread --bs=4k --iodepth=32 --io_size=8m",
    "--rw=read --bs=128k --iodepth=1 --io_size=32m",
    "--filename=/tmp/ferric-iobench --rw=randwrite --bs=4k --iodepth=1 --io_size=4m --end_fsync=1",
    "--filename=/tmp/ferric-iobench --rw=write --bs=128k --iodepth=1 --io_size=32m --end_fsync=1",
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
            ferric_iobench::job::Spec::parse(job.split_whitespace()).unwrap();
        }
    }
}
