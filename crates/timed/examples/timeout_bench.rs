//! Compares the pre-migration time-scheme scan with ordered timeout storage.
//!
//! Run with `cargo run --release -p ferric-timed --features alloc --example
//! timeout_bench`. This is a single-threaded queue microbenchmark, not evidence
//! of kernel latency, timer hardware behavior or an end-to-end speedup.

use std::{collections::VecDeque, hint::black_box, time::Instant};

use ferric_timed::timeout::{Clock, ClockSnapshot, TimeoutQueue, Timestamp};

const REALTIME_OFFSET: u128 = 100_000;
const CHECKS: usize = 128;

trait Pending: Default {
    fn insert(&mut self, clock: Clock, at: u128, value: usize);
    fn expire(&mut self, monotonic: u128, realtime: u128) -> usize;
}

#[derive(Default)]
struct Legacy(VecDeque<(Clock, u128, usize)>);

impl Pending for Legacy {
    fn insert(&mut self, clock: Clock, at: u128, value: usize) {
        self.0.push_back((clock, at, value));
    }

    fn expire(&mut self, monotonic: u128, realtime: u128) -> usize {
        let mut i = 0;
        let mut sum = 0;
        while let Some(&(clock, at, _)) = self.0.get(i) {
            let now = match clock {
                Clock::Monotonic => monotonic,
                Clock::Realtime => realtime,
            };
            if now >= at {
                if let Some((_, _, value)) = self.0.swap_remove_back(i) {
                    sum += value + 1;
                }
            } else {
                i += 1;
            }
        }
        sum
    }
}

impl Pending for TimeoutQueue<usize> {
    fn insert(&mut self, clock: Clock, at: u128, value: usize) {
        self.insert(clock, Timestamp::new(at), value)
            .expect("a fresh benchmark queue receives at most 4096 registrations");
    }

    fn expire(&mut self, monotonic: u128, realtime: u128) -> usize {
        let clocks = ClockSnapshot::new(Timestamp::new(monotonic), Timestamp::new(realtime));
        let mut sum = 0;
        while let Some(value) = self.pop_due(clocks) {
            sum += value + 1;
        }
        sum
    }
}

fn measure<Q: Pending>(name: &str, timers: usize, rounds: usize) {
    let mut insertion = 0;
    let mut checking = 0;
    let mut expiration = 0;
    for _ in 0..rounds {
        let mut queue = Q::default();
        let start = Instant::now();
        for value in 0..timers {
            let (clock, offset) = if value % 2 == 0 {
                (Clock::Monotonic, 0)
            } else {
                (Clock::Realtime, REALTIME_OFFSET)
            };
            queue.insert(clock, offset + value as u128 * 64 + 1, value);
        }
        insertion += start.elapsed().as_nanos();

        let start = Instant::now();
        for _ in 0..CHECKS {
            assert_eq!(black_box(&mut queue).expire(0, REALTIME_OFFSET), 0);
        }
        checking += start.elapsed().as_nanos();

        let start = Instant::now();
        let end = timers as u128 * 64 + 1;
        let result = black_box(&mut queue).expire(end, REALTIME_OFFSET + end);
        expiration += start.elapsed().as_nanos();
        assert_eq!(result, timers * (timers + 1) / 2);
        assert_eq!(queue.expire(end, REALTIME_OFFSET + end), 0);
    }
    println!(
        "backend={name} timers={timers} rounds={rounds} insert_ns_per_timer={:.2} idle_check_ns_per_pass={:.2} expire_ns_per_timer={:.2}",
        insertion as f64 / (rounds * timers) as f64,
        checking as f64 / (rounds * CHECKS) as f64,
        expiration as f64 / (rounds * timers) as f64,
    );
}

fn main() {
    for timers in [1, 64, 4096] {
        let rounds = (131_072 / timers).clamp(16, 10_000);
        measure::<Legacy>("legacy-scan", timers, rounds);
        measure::<TimeoutQueue<usize>>("ordered", timers, rounds);
    }
}
