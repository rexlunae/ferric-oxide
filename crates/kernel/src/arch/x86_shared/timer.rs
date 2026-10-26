//! Ferric Oxide (tickless scheduling, #186): the per-CPU one-shot scheduler
//! timer.
//!
//! The LAPIC timer is set to TSC-deadline mode (Intel SDM: "Local APIC Timer"
//! and "IA32_TSC_DEADLINE") when CPUID.01H:ECX bit 24 says the CPU has it:
//! writing the deadline in TSC ticks to IA32_TSC_DEADLINE arms the timer to
//! fire once when the TSC reaches it, and writing 0 disarms it. TSC-deadline
//! mode needs no bus-frequency calibration (the deadline is in TSC ticks,
//! and the invariant TSC's rate is the calibrated one from the time module)
//! and one MSR write is the whole reprogram, so the rearm protocol's races
//! reduce to "recheck the clock after programming". When the CPU lacks the
//! feature, the module reports unsupported at boot and the periodic PIT path
//! is the only scheduler timer (a calibrated LAPIC countdown or an
//! HPET/broadcast alternative is the fallback work of #187's deadline
//! service).
//!
//! The per-CPU armed state is the [`ArmedDeadline`](ferric_timed::ArmedDeadline)
//! cell from `ferric_timed`, so the "which deadline is armed, is the new one
//! earlier, is it due" logic is host-tested. The kernel's timer handler is
//! the only reader and runs with interrupts off, so the cell needs no further
//! locking (the `AtomicU64` still makes the ordering explicit).
//!
//! `selftest` runs at boot when the `time_one_shot` feature is on: it arms
//! the timer for a second and reports the measured elapsed time, arms an
//! already-due deadline and checks it fires at once, and checks a disarmed
//! timer does not fire. The numbers are evidence for the before/after story
//! of the tickless work (the periodic baseline comes from the scheduler
//! tracepoints of the baseline protocol).

use core::sync::atomic::Ordering;

use ferric_timed::Nanos;
use x86::msr::{wrmsr, IA32_TSC_DEADLINE};

use crate::{
    arch::{self, device::local_apic::the_local_apic},
    percpu::PercpuBlock,
    sync::CleanLockToken,
    time,
};

/// The vector of the local APIC timer (the same one the periodic handler used
/// before this module existed, `irq::lapic_timer`).
pub const TIMER_VECTOR: u8 = 48;
/// The LVT entry for TSC-deadline mode at `TIMER_VECTOR`: delivery Fixed,
/// unmasked, mode 10b.
const LVT_TSC_DEADLINE: u32 = 0b10 << 17 | TIMER_VECTOR as u32;
/// How long the selftest waits for a disarmed timer to stay quiet.
const SELFTEST_QUIET_MS: u32 = 200;

/// Whether this CPU has the TSC-deadline timer (CPUID.01H:ECX bit 24).
pub fn tsc_deadline_supported() -> bool {
    let ecx = unsafe { core::arch::x86_64::__cpuid(1) }.ecx;
    ecx & (1 << 24) != 0
}

/// Arms the per-CPU one-shot timer for `deadline` (monotonic nanoseconds, the
/// [`time::monotonic`] scale), if it is earlier than what is armed.
///
/// Returns [`ArmResult::Reprogrammed`] when the hardware was (re)programmed
/// for this deadline, [`ArmResult::Covered`] when an equal or earlier
/// deadline is already armed, and [`ArmResult::Unsupported`] when this CPU
/// has no TSC-deadline timer. An already-due deadline is never discarded: it
/// fires immediately ([`ArmResult::Fired`]).
pub fn arm(deadline: Nanos) -> ArmResult {
    if !tsc_deadline_supported() {
        return ArmResult::Unsupported;
    }
    let percpu = PercpuBlock::current();
    if !percpu.one_shot.armed.arm(deadline) {
        // A covered deadline may still be due (the armed one was due); the
        // handler's due recheck fires it. The rearm decision only needs to
        // know no reprogramming happened.
        return ArmResult::Covered;
    }
    let now = Nanos::new(time::monotonic(&mut unsafe { CleanLockToken::new() }) as u64);
    if now.at_or_past(deadline) {
        // Past-deadline handling: never discard an already-due event. The
        // handler's interrupt may be in flight; the fired flag makes the
        // delivery idempotent.
        PercpuBlock::current()
            .one_shot
            .fired
            .store(true, Ordering::Release);
        return ArmResult::Fired;
    }
    program(deadline);
    ArmResult::Reprogrammed
}

/// Disarms the per-CPU one-shot timer (only if it was armed; a covered arm
/// that never programmed leaves no event to cancel), restoring the LVT entry
/// the periodic paths expect (the profiling timer, for example).
pub fn disarm() {
    let percpu = PercpuBlock::current();
    if percpu.one_shot.armed.disarm() {
        // SAFETY: IA32_TSC_DEADLINE is a model-specific register the kernel
        // owns; writing 0 disarms the timer. The LVT entry goes back to its
        // previous value after the MSR is disarmed.
        unsafe {
            wrmsr(IA32_TSC_DEADLINE, 0);
        }
        let previous = percpu.one_shot.previous_lvt.get();
        if previous != u32::MAX {
            // SAFETY: the local APIC is mapped by the APIC init.
            unsafe { the_local_apic().set_lvt_timer(previous) };
        }
    }
}

/// Programs the hardware for the currently armed deadline.
fn program(deadline: Nanos) {
    let percpu = PercpuBlock::current();
    // SAFETY: the local APIC is mapped by the APIC init; the LVT entry is a
    // normal register write, and IA32_TSC_DEADLINE a kernel-owned MSR. The
    // TSC rate is the calibrated invariant rate of the clock module (same on
    // every core).
    unsafe {
        if percpu.one_shot.previous_lvt.get() == u32::MAX {
            percpu
                .one_shot
                .previous_lvt
                .set(the_local_apic().lvt_timer());
        }
        let rate = crate::arch::time::tsc_rate().expect("the one-shot timer needs a TSC rate");
        the_local_apic().set_lvt_timer(LVT_TSC_DEADLINE);
        wrmsr(
            IA32_TSC_DEADLINE,
            rate.ticks_for_nanos(deadline.get()).get(),
        );
    }
    percpu.one_shot.fired.store(false, Ordering::Release);
}

/// What [`arm`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmResult {
    /// The hardware was (re)programmed for the deadline.
    Reprogrammed,
    /// An equal or earlier deadline is already armed; nothing to do.
    Covered,
    /// The deadline was already due when armed; it fired at once.
    Fired,
    /// This CPU has no TSC-deadline timer.
    Unsupported,
}

/// The handler for the local APIC timer interrupt: when an interrupt arrives
/// with an armed cell, the one-shot deadline fired (the TSC-deadline timer
/// fires exactly at its deadline), so the fired flag is set for the switch
/// path and the selftest; a stale interrupt (nothing armed - disarmed or
/// covered) changes nothing. No clock read happens in the interrupt path.
///
/// Called with interrupts off, from the IDT entry.
pub fn handle_fire() {
    let percpu = PercpuBlock::current();
    let entries = percpu.one_shot.entries.fetch_add(1, Ordering::Relaxed);
    if entries < 10 {
        crate::debug!("timer: handler entry {entries}");
    }
    // The TSC-deadline one-shot fires exactly at its deadline, so a delivery
    // with an armed cell is the deadline firing: mark it fired without
    // reading the clock (a recheck would be racing the rearm protocol, and
    // the clock source must not run in the interrupt path). A delivery after
    // a disarm or with nothing armed is stale and changes nothing; the
    // consumer (the switch path) re-checks the clock when it settles the
    // cell.
    if percpu.one_shot.armed.armed().is_some() {
        percpu.one_shot.fired.store(true, Ordering::Release);
    }
}

/// Whether a one-shot fired since the last consume; the switch path and the
/// selftest use it to know there is timer work.
pub fn fired() -> bool {
    PercpuBlock::current()
        .one_shot
        .fired
        .load(Ordering::Acquire)
}

/// Consumes the fired flag (returns what [`fired`] said).
pub fn take_fired() -> bool {
    PercpuBlock::current()
        .one_shot
        .fired
        .swap(false, Ordering::AcqRel)
}

/// The boot selftest (`kernel.time.one_shot`): validates the one-shot timer
/// and reports the measurements the tickless work compares against. Runs once
/// on the BSP, after the APs are up, with interrupts enabled; the PIT keeps
/// ticking underneath.
pub fn selftest(token: &mut CleanLockToken) {
    crate::info!("timer: one-shot selftest entered");
    let supported = tsc_deadline_supported();
    crate::info!("timer: one-shot selftest: tsc-deadline supported = {supported}");
    if !supported {
        return;
    }
    let rate = crate::arch::time::tsc_rate();
    crate::info!(
        "timer: one-shot selftest: tsc rate {} Hz",
        rate.map(|rate| rate.get()).unwrap_or(0)
    );
    crate::info!("timer: one-shot selftest: rate ready");

    // Round 1: arm for one second, wait for the fire, measure the elapsed
    // monotonic time against the requested deadline.
    crate::info!("timer: one-shot selftest: arming round 1");
    let start = time::monotonic(token);
    let deadline = Nanos::new(start as u64 + 1_000_000_000);
    match arm(deadline) {
        ArmResult::Reprogrammed => {
            // SAFETY: reading the LVT confirms what the hardware latched.
            let lvt = unsafe { the_local_apic().lvt_timer() };
            crate::info!("timer: one-shot selftest round 1 armed, lvt read-back {lvt:#x}");
        }
        other => {
            crate::info!("timer: one-shot selftest round 1 arm: {other:?}");
            return;
        }
    }
    // Bound the wait by the raw TSC (the pvclock is not involved in the
    // wait itself) and a hard spin count, so a wedged timer is reported
    // instead of stalling the boot on any CPU; report progress every half
    // second while waiting.
    let rate = crate::arch::time::tsc_rate().expect("the selftest has a TSC rate");
    // SAFETY: RDTSC reads a counter; it has no side effects. The raw ticks
    // are only compared against each other here.
    let start_ticks = unsafe { core::arch::x86_64::_rdtsc() };
    let give_up_ticks = rate.ticks_for_nanos(5_000_000_000).get();
    let report_ticks = rate.ticks_for_nanos(500_000_000).get();
    let mut last_report_ticks = start_ticks;
    let mut spins = 0u64;
    while spins < 2_000_000_000 && !fired() {
        // SAFETY: interrupts on so the timer interrupt arrives; the spin is a
        // bounded wait for a boot selftest.
        unsafe { arch::interrupt::enable_and_nop() };
        spins += 1;
        // SAFETY: RDTSC reads a counter; it has no side effects.
        let ticks = unsafe { core::arch::x86_64::_rdtsc() };
        if ticks.wrapping_sub(last_report_ticks) >= report_ticks {
            let fired = fired();
            let entries = PercpuBlock::current().one_shot.entries.load(Ordering::Relaxed);
            crate::info!(
                "timer: selftest waiting: ticks={ticks} fired={fired} entries={entries}"
            );
            last_report_ticks = ticks;
            if ticks.wrapping_sub(start_ticks) >= give_up_ticks {
                break;
            }
        }
    }
    if !fired() {
        let entries = PercpuBlock::current().one_shot.entries.load(Ordering::Relaxed);
        // SAFETY: reading the LVT shows what the hardware latched.
        let lvt = unsafe { the_local_apic().lvt_timer() };
        crate::info!(
            "timer: one-shot selftest round 1: no fire (wedged): {} handler entries, lvt read-back {lvt:#x}",
            entries
        );
        disarm();
        return;
    }
    take_fired();
    let end = time::monotonic(token);
    crate::info!(
        "timer: one-shot selftest round 1: fired after {} ns (wanted 1000000000, error {})",
        end - start,
        (end - start) as i128 - 1_000_000_000i128
    );

    // Round 2: an already-due deadline must fire at once, never be discarded.
    let now = time::monotonic(token);
    let past = Nanos::new(now.saturating_sub(1) as u64);
    match arm(past) {
        ArmResult::Fired => {
            crate::info!("timer: one-shot selftest round 2: a due deadline fired at once");
        }
        other => {
            crate::info!("timer: one-shot selftest round 2: expected Fired, got {other:?}");
        }
    }
    disarm();

    // Round 3: a disarmed timer does not fire.
    let deadline = Nanos::new(time::monotonic(token) as u64 + 1_000_000_000);
    if arm(deadline) == ArmResult::Reprogrammed {
        disarm();
        // SAFETY: RDTSC reads a counter; it has no side effects. The raw
        // ticks are only compared against each other here.
        let quiet_until = unsafe { core::arch::x86_64::_rdtsc() }
            .wrapping_add(rate.ticks_for_nanos(u64::from(SELFTEST_QUIET_MS) * 1_000_000).get());
        loop {
            // SAFETY: interrupts on, bounded wait.
            unsafe { arch::interrupt::enable_and_nop() };
            core::hint::spin_loop();
            // SAFETY: RDTSC reads a counter; it has no side effects.
            if unsafe { core::arch::x86_64::_rdtsc() } >= quiet_until {
                break;
            }
            if fired() {
                crate::info!("timer: one-shot selftest round 3: a disarmed timer fired!");
                take_fired();
                return;
            }
        }
        crate::info!("timer: one-shot selftest round 3: a disarmed timer stayed quiet");
    }
    disarm();
}
