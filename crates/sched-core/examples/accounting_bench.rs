//! Host arithmetic comparison with the preceding kernel accounting.
//!
//! The legacy units/heuristic are retained only here to measure their cost.
//! This does not benchmark selection, locks, hardware events or dispatch latency.

use ferric_sched_core::{CpuActivity, FairClock, FairState, ScheduleReason, VirtualTime, Weight};
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

// The immediately preceding relative CPU-clock arithmetic, retained only for
// this host comparison. The kernel also stored its population separately.
struct PreviousClock {
    time: u128,
    denominator: u64,
    remainder: u128,
}

impl PreviousClock {
    fn account(&mut self, elapsed: Duration, weight: u64) {
        if self.denominator != weight {
            self.denominator = weight;
            self.remainder = 0;
        }
        let numerator = elapsed.as_nanos() * 1024 + self.remainder;
        self.remainder = numerator % u128::from(weight);
        self.time = self.time.saturating_add(numerator / u128::from(weight));
    }
}

fn clocks() {
    let weight = black_box(4096);
    let mut previous = PreviousClock {
        time: 0,
        denominator: 1,
        remainder: 0,
    };
    let start = Instant::now();
    for event in 0..ITERATIONS {
        let elapsed = black_box(Duration::from_nanos(1_000 + event % 12 * 1_000_000));
        previous.account(elapsed, weight);
        black_box(&previous);
    }
    let old = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;

    let mut measured = FairClock::new();
    measured.set_weight(0, weight);
    measured.set_activity(0, CpuActivity::Running);
    let mut now = 0;
    let start = Instant::now();
    for event in 0..ITERATIONS {
        now += u128::from(black_box(1_000 + event % 12 * 1_000_000));
        measured.settle(black_box(now));
        black_box(&measured);
    }
    let new = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;
    assert_eq!(measured.time().get(), previous.time);
    println!("cpu_clock weight={weight} previous_ns/event={old:.2} observed_ns/event={new:.2}");
    println!(
        "cpu_clock_bytes previous={} observed={} (previous population was separate)",
        std::mem::size_of::<PreviousClock>(),
        std::mem::size_of::<FairClock>()
    );
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
    clocks();
}
