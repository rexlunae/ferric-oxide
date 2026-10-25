//! The scheduler baseline's busy workload (`ferric-sched-burn`).
//!
//! Runs one CPU-bound loop for `--seconds N` (default 10): a tight spin that makes no
//! system calls, so the scheduler's periodic tick is the only thing that ever preempts
//! it. The scheduler-baseline image (config/ferric-sched-bench.toml) runs it while the
//! kernel trace is on and attributes the capture's `sched.switch`, `sched.tick` and
//! `sched.ipi_pit` records to a single greedy best-effort context.

use std::process::ExitCode;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let seconds = std::env::args()
        .position(|arg| arg == "--seconds")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut spins = 0u64;
    println!("FERRIC_BURN_BEGIN seconds={seconds}");
    while Instant::now() < deadline {
        for _ in 0..1_000_000 {
            std::hint::spin_loop();
        }
        spins += 1_000_000;
    }
    println!("FERRIC_BURN_END seconds={seconds} spins={spins}");
    ExitCode::SUCCESS
}
