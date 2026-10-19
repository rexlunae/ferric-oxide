//! EXPERIMENT (ci/percore-latency, never merged): wake-to-run latency of contexts, split by
//! whether the waker ran on another CPU and by what made the target CPU reschedule.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use crate::context::Context;

const CPUS: usize = 64;
static IN_TICK: [AtomicBool; CPUS] = [const { AtomicBool::new(false) }; CPUS];

/// local/remote x idle/tick/voluntary
const BINS: usize = 6;
static COUNT: [AtomicU64; BINS] = [const { AtomicU64::new(0) }; BINS];
static SUM: [AtomicU64; BINS] = [const { AtomicU64::new(0) }; BINS];
static MAX: [AtomicU64; BINS] = [const { AtomicU64::new(0) }; BINS];
static OVER_1M: [AtomicU64; BINS] = [const { AtomicU64::new(0) }; BINS];
static TSC_PER_US: AtomicU64 = AtomicU64::new(2500);
static LAST_IPI: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];
static LAST_IPI_NONEMPTY: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];
static LAST_SCHED: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];
static PREV_SCHED: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];

fn cpu() -> usize {
    crate::cpu_id().get() as usize % CPUS
}

pub fn ipi_drain(nonempty: bool) {
    let now = tsc();
    LAST_IPI[cpu()].store(now, Relaxed);
    if nonempty {
        LAST_IPI_NONEMPTY[cpu()].store(now, Relaxed);
    }
}

static MASK: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];
static PREV_MASK: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];

/// Records why the scheduler passed a context over in this schedule call.
pub fn skip(bit: u64) {
    MASK[cpu()].fetch_or(bit, Relaxed);
}

pub fn schedule_entry() {
    let mask = MASK[cpu()].swap(0, Relaxed);
    PREV_MASK[cpu()].store(mask, Relaxed);
    let now = tsc();
    let last = LAST_SCHED[cpu()].swap(now, Relaxed);
    PREV_SCHED[cpu()].store(last, Relaxed);
}

static SLOW_PRINTED: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static LAST_TSC: AtomicU64 = AtomicU64::new(0);
static LAST_NS: AtomicU64 = AtomicU64::new(0);
const NAMES: [&str; BINS] = ["l-idle", "l-tick", "l-vol", "r-idle", "r-tick", "r-vol"];

pub fn tsc() -> u64 {
    // SAFETY: rdtsc has no preconditions.
    unsafe { core::arch::x86_64::_rdtsc() }
}

pub fn set_in_tick(on: bool) {
    IN_TICK[crate::cpu_id().get() as usize % CPUS].store(on, Relaxed);
}

pub fn picked(next: &mut Context, prev: &Context, prev_is_idle: bool, now_ns: u128) {
    let woke = core::mem::replace(&mut next.wake_tsc, 0);
    if woke == 0 || !next.userspace {
        return;
    }
    let now = tsc();
    let lat = now.saturating_sub(woke);
    let reason = if prev_is_idle {
        0
    } else if IN_TICK[crate::cpu_id().get() as usize % CPUS].load(Relaxed) {
        1
    } else {
        2
    };
    let bin = reason + if next.wake_remote { 3 } else { 0 };
    COUNT[bin].fetch_add(1, Relaxed);
    SUM[bin].fetch_add(lat, Relaxed);
    MAX[bin].fetch_max(lat, Relaxed);
    // ~1 ms at the CI hosts' ~2-3 GHz TSC; the summary prints the TSC rate to be exact.
    if lat > TSC_PER_US.load(Relaxed) * 1000 {
        OVER_1M[bin].fetch_add(1, Relaxed);
    }
    if lat > TSC_PER_US.load(Relaxed) * 1000
        && next.name.contains("nvmed")
        && SLOW_PRINTED.fetch_add(1, Relaxed) < 400
    {
        let per_us = TSC_PER_US.load(Relaxed);
        println!(
            "WL slow cpu{} {}us ipi{} ipine{} psched{} m{:x} {} r{} prev={}/{} next={}/{}",
            crate::cpu_id().get(),
            lat / per_us,
            now.saturating_sub(LAST_IPI[cpu()].load(Relaxed)) / per_us,
            now.saturating_sub(LAST_IPI_NONEMPTY[cpu()].load(Relaxed)) / per_us,
            now.saturating_sub(PREV_SCHED[cpu()].load(Relaxed)) / per_us,
            PREV_MASK[cpu()].load(Relaxed),
            NAMES[bin],
            u8::from(next.wake_remote),
            prev.name,
            prev.prio,
            next.name,
            next.prio
        );
    }
    let total = TOTAL.fetch_add(1, Relaxed) + 1;
    if total % 5_000 == 0 {
        let ns = now_ns as u64;
        let dt = now.saturating_sub(LAST_TSC.swap(now, Relaxed));
        let dns = ns.saturating_sub(LAST_NS.swap(ns, Relaxed)).max(1);
        let per_us = (dt.saturating_mul(1000) / dns).max(1);
        TSC_PER_US.store(per_us, Relaxed);
        println!("WAKELAT n={total} tsc_per_us={per_us}");
        for b in 0..BINS {
            let c = COUNT[b].swap(0, Relaxed);
            let s = SUM[b].swap(0, Relaxed);
            let m = MAX[b].swap(0, Relaxed);
            let o = OVER_1M[b].swap(0, Relaxed);
            if c > 0 {
                println!("WL {} {c} {} {} {o}", NAMES[b], s / c / per_us, m / per_us);
            }
        }
    }
}
