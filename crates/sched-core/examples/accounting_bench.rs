//! Host arithmetic comparison with the preceding kernel accounting.
//!
//! The legacy units/heuristic are retained only here to measure their cost.
//! This does not benchmark selection, locks, hardware events or dispatch latency.

use ferric_sched_core::{FairState, ScheduleReason, VirtualTime, Weight};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

const ITERATIONS: u64 = 2_000_000;
const SCALE: u128 = 1 << 30;

struct Legacy {
    runtime: u64,
    deadline: u64,
    remaining: u64,
}

impl Legacy {
    fn account(&mut self, elapsed_ns: u64, weight: u64) {
        let ticks = u128::from(elapsed_ns) * SCALE / 2_250_000;
        self.remaining = self.remaining.saturating_sub(ticks as u64);
        self.runtime += (ticks / u128::from(weight)) as u64;
        if u128::from(elapsed_ns) < 3 * 2_250_000 / 2 {
            self.runtime += self.remaining / weight;
            self.remaining = 0;
        }
        if self.remaining == 0 {
            self.remaining = 9 * SCALE as u64;
            self.deadline = self.runtime + (9 * SCALE / u128::from(weight)) as u64;
        }
    }
}

fn main() {
    println!(
        "state_bytes legacy={} measured={}",
        std::mem::size_of::<Legacy>(),
        std::mem::size_of::<FairState>()
    );
    println!("priority legacy_ns/event measured_ns/event");
    for priority in [0, 20, 39] {
        let weight = Weight::for_priority(priority).unwrap();
        let mut legacy = Legacy {
            runtime: 0,
            deadline: 0,
            remaining: 9 * SCALE as u64,
        };
        let start = Instant::now();
        for event in 0..ITERATIONS {
            let elapsed = black_box(1_000 + event % 12 * 1_000_000);
            legacy.account(elapsed, black_box(weight.get()));
            black_box(&legacy);
        }
        let old = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;
        let mut measured = FairState::new();
        measured.activate(VirtualTime::ZERO, weight);
        let start = Instant::now();
        for event in 0..ITERATIONS {
            let elapsed = black_box(1_000 + event % 12 * 1_000_000);
            let reason = black_box(if event % 12 == 0 {
                ScheduleReason::Yield
            } else {
                ScheduleReason::Preempt
            });
            measured.account(Duration::from_nanos(elapsed), black_box(weight), reason);
            black_box(&measured);
        }
        let new = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;
        println!("{priority} {old:.2} {new:.2}");
    }
}
