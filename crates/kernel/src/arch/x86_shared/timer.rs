//! Ferric Oxide (tickless scheduling, #186): the per-CPU one-shot scheduler
//! timer.
//!
//! Each CPU picks a backend for its local APIC timer in [`init_cpu`]
//! (Intel SDM Vol. 3A, section 11.5.4, "APIC Timer"):
//!
//! - **TSC-deadline mode** when CPUID.01H:ECX bit 24 says the CPU has it:
//!   writing a deadline in TSC ticks to IA32_TSC_DEADLINE arms the timer to
//!   fire once when the TSC reaches it, and writing 0 disarms it. One MSR
//!   write is the whole reprogram, so the rearm protocol's races reduce to
//!   "recheck the clock after programming".
//! - **A one-shot countdown** otherwise, whose rate this CPU measures against
//!   the calibrated TSC at boot. The 32-bit count limits one countdown to a
//!   few seconds, so a distant deadline is armed as the longest countdown and
//!   the handler re-arms it when it ends early.
//!
//! Deadlines are kept in monotonic nanoseconds (the [`time::monotonic`]
//! scale) and turned into a TSC deadline *relative to now* when programmed,
//! so the timer is right whichever continuous source the clock uses (the KVM
//! clock and the HPET are not on the TSC's epoch). The portable arithmetic
//! is host-tested in `ferric_timed`: the [`ArmedDeadline`] cell ("which
//! deadline is armed, is the new one earlier, is it due"), the TSC
//! conversions (`rate`), the backend's programming and the early/due
//! classification (`oneshot`), and the selftest's lateness series
//! (`selftest`).
//!
//! The kernel's timer handler and the rearm run on the owning CPU with
//! interrupts off, so the per-CPU state needs no further locking (the atomics
//! still make the ordering explicit).
//!
//! With the feature on, two boot selftests run:
//!
//! - [`init_cpu`], on every CPU, arms [`ferric_timed::selftest::SHOTS`]
//!   deadlines from 50 µs to 5 ms and logs how late they fired
//!   (`timer: cpu N: <backend> fired F/E ...`, which CI checks on every CPU).
//! - [`selftest`], on the BSP, arms the timer for a second and reports the
//!   measured elapsed time, arms an already-due deadline and checks it fires
//!   at once, and checks a disarmed timer does not fire.
//!
//! The `sched.timer_calibrate`, `sched.timer_arm` and `sched.timer_fire`
//! tracepoints (`kernel.trace_sched`) record each calibration, each
//! programmed deadline and how late each interrupt served it.
//!
//! [`ArmedDeadline`]: ferric_timed::ArmedDeadline

use core::sync::atomic::{fence, Ordering};

use ferric_timed::{
    calibrate::{self, estimate_against, CalibrationError, Sample},
    oneshot::{self, Backend, Fired, Program},
    rate::TicksPerSecond,
    selftest::{Next, SelfTest, Verdict},
    Nanos,
};
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
/// The LVT entry for a one-shot countdown at `TIMER_VECTOR`: mode 00b.
const LVT_ONE_SHOT: u32 = TIMER_VECTOR as u32;
/// The LVT mask bit.
const LVT_MASKED: u32 = 1 << 16;
/// Divide configuration for divide-by-1 (SDM Vol. 3A, Figure 11-10).
const DIVIDE_BY_1: u32 = 0b1011;
/// How long the selftest waits for a disarmed timer to stay quiet.
const SELFTEST_QUIET_MS: u32 = 200;
/// The least time between programming and firing (5 µs), so a deadline closer
/// than the write takes still interrupts instead of being missed.
const MIN_LEAD_NS: u64 = 5_000;
/// How long a CPU's lateness series may take before it is abandoned as
/// incomplete: well above the series' ~30 ms of deadlines, even on a slow
/// emulator.
const SERIES_TIMEOUT_NS: u64 = 2_000_000_000;
/// Countdown calibration windows and their length (10 ms of boot per CPU).
const COUNTDOWN_WINDOWS: usize = 5;
const COUNTDOWN_WINDOW_NS: u64 = 2_000_000;
/// `sched.timer_calibrate`'s counter and reference codes for the countdown.
const COUNTER_LAPIC: u64 = 2;
const REFERENCE_TSC: u64 = 3;

fn rdtsc() -> u64 {
    // SAFETY: RDTSC reads a counter; it has no side effects and every x86_64
    // CPU has it.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// This CPU's one-shot backend, or `None` when [`init_cpu`] found none (no
/// calibrated TSC, or a countdown that would not calibrate).
pub fn backend() -> Option<Backend> {
    PercpuBlock::current().one_shot.backend.get()
}

/// Sets up this CPU's one-shot timer and runs its lateness series.
///
/// # Safety
///
/// Must run once per CPU, on that CPU, with interrupts disabled, after its
/// local APIC and its idle context exist (the series enables interrupts
/// while it waits) and after `arch::time::init` calibrated the TSC. Returns with
/// interrupts disabled.
pub unsafe fn init_cpu() {
    let cpu = crate::cpu_id().get();
    let Some(rate) = arch::time::tsc_rate() else {
        crate::warn!("timer: cpu {cpu}: no one-shot timer (no calibrated tsc)");
        return;
    };
    let backend = match choose_backend(rate) {
        Ok(backend) => backend,
        Err(err) => {
            crate::warn!(
                "timer: cpu {cpu}: no one-shot timer (countdown calibration failed: {err})"
            );
            return;
        }
    };
    if let Backend::Countdown { timer_hz } = backend {
        crate::info!("timer: cpu {cpu}: lapic countdown at {timer_hz}");
    }
    PercpuBlock::current().one_shot.backend.set(Some(backend));
    // SAFETY: the caller runs this with interrupts off on the CPU that owns
    // the timer, on its idle context.
    let verdict = unsafe { run_series() };
    report_series(cpu, backend, verdict);
}

/// TSC-deadline mode when the CPU has it, otherwise a countdown measured
/// against the TSC.
fn choose_backend(rate: TicksPerSecond) -> Result<Backend, CalibrationError> {
    let ecx = core::arch::x86_64::__cpuid(1).ecx;
    if ecx & (1 << 24) != 0 {
        return Ok(Backend::TscDeadline);
    }
    let timer_hz = calibrate_countdown(rate)?;
    Ok(Backend::Countdown { timer_hz })
}

/// Measures this CPU's local APIC countdown against the calibrated TSC.
fn calibrate_countdown(rate: TicksPerSecond) -> Result<TicksPerSecond, CalibrationError> {
    let window_ticks = rate.ticks_for_nanos_ceil(COUNTDOWN_WINDOW_NS).get();
    let mut estimates = [TicksPerSecond::new(1).expect("one is not zero"); COUNTDOWN_WINDOWS];
    for slot in &mut estimates {
        let (start, end) = countdown_window(window_ticks);
        *slot = estimate_against(start, end, rate)?;
    }
    let calibrated = calibrate::calibrate(&mut estimates, arch::time::MAX_SPREAD_PPM)?;
    crate::trace::trace_event!(
        ferric_trace::kernel::SCHED_TIMER_CALIBRATE,
        COUNTER_LAPIC,
        calibrated.hz.get(),
        REFERENCE_TSC,
        calibrated.spread_ppm,
    );
    Ok(calibrated.hz)
}

/// Runs this CPU's countdown, masked, for `window_ticks` of TSC and returns
/// the counts elapsed (`!count` turns the down-counter from `u32::MAX` into
/// an up-counter) and the TSC at both ends. Leaves the countdown stopped.
fn countdown_window(window_ticks: u64) -> (Sample, Sample) {
    // SAFETY: this CPU's local APIC is set up. A masked countdown raises no
    // interrupt, so starting it and reading its count have no other effect; a
    // zero initial count stops it again.
    unsafe {
        let apic = the_local_apic();
        apic.set_div_conf(DIVIDE_BY_1);
        apic.set_lvt_timer(LVT_ONE_SHOT | LVT_MASKED);
        apic.set_init_count(u32::MAX);
        let start = Sample {
            counter: u64::from(!apic.cur_count()),
            reference: rdtsc(),
        };
        while rdtsc().wrapping_sub(start.reference) < window_ticks {
            core::hint::spin_loop();
        }
        let end = Sample {
            counter: u64::from(!apic.cur_count()),
            reference: rdtsc(),
        };
        apic.set_init_count(0);
        (start, end)
    }
}

/// Arms the per-CPU one-shot timer for `deadline` (monotonic nanoseconds, the
/// [`time::monotonic`] scale), if it is earlier than what is armed.
///
/// Returns [`ArmResult::Reprogrammed`] when the hardware was (re)programmed
/// for this deadline, [`ArmResult::Covered`] when an equal or earlier
/// deadline is already armed, and [`ArmResult::Unsupported`] when this CPU
/// has no one-shot timer. An already-due deadline is never discarded: it
/// fires immediately ([`ArmResult::Fired`]).
pub fn arm(deadline: Nanos) -> ArmResult {
    let Some(backend) = backend() else {
        return ArmResult::Unsupported;
    };
    let percpu = PercpuBlock::current();
    if !percpu.one_shot.armed.arm(deadline) {
        // A covered deadline may still be due (the armed one was due); the
        // handler's due recheck fires it. The rearm decision only needs to
        // know no reprogramming happened.
        return ArmResult::Covered;
    }
    // SAFETY: the monotonic clock takes no lock that the caller can hold:
    // the continuous sources read hardware counters, and the fallback reads
    // `time::OFFSET` the way every other caller does.
    let now = Nanos::new(time::monotonic(&mut unsafe { CleanLockToken::new() }) as u64);
    if now.at_or_past(deadline) {
        // Past-deadline handling: never discard an already-due event. The
        // handler's interrupt may be in flight; the fired flag makes the
        // delivery idempotent.
        percpu
            .one_shot
            .late_ns
            .set(now.elapsed_since(deadline).get());
        percpu.one_shot.fired.store(true, Ordering::Release);
        return ArmResult::Fired;
    }
    let rate = arch::time::tsc_rate().expect("a CPU with a one-shot backend has a TSC rate");
    let tsc_now = rdtsc();
    let tsc_deadline = tsc_now.saturating_add(
        rate.ticks_for_nanos_ceil(deadline.elapsed_since(now).get())
            .get(),
    );
    program(backend, rate, tsc_now, tsc_deadline);
    ArmResult::Reprogrammed
}

/// Programs the hardware for the TSC reading `tsc_deadline`, read at
/// `tsc_now`, and records it for the handler.
fn program(backend: Backend, rate: TicksPerSecond, tsc_now: u64, tsc_deadline: u64) {
    let percpu = PercpuBlock::current();
    let armed = oneshot::arm(
        backend,
        rate,
        tsc_now,
        tsc_deadline,
        rate.ticks_for_nanos_ceil(MIN_LEAD_NS).get(),
    );
    percpu.one_shot.tsc_deadline.set(tsc_deadline);
    percpu.one_shot.fired.store(false, Ordering::Release);
    // SAFETY: the local APIC is mapped by the APIC init; the LVT entry is a
    // normal register write, and IA32_TSC_DEADLINE a kernel-owned MSR that
    // exists when the backend is TSC-deadline. Either write only schedules
    // this CPU's timer interrupt.
    unsafe {
        let apic = the_local_apic();
        if percpu.one_shot.previous_lvt.get() == u32::MAX {
            percpu.one_shot.previous_lvt.set(apic.lvt_timer());
        }
        match armed.program {
            Program::TscDeadline(value) => {
                apic.set_lvt_timer(LVT_TSC_DEADLINE);
                // A deadline write must not pass the mode switch (SDM
                // 11.5.4.1).
                fence(Ordering::SeqCst);
                wrmsr(IA32_TSC_DEADLINE, value);
            }
            Program::Countdown(count) => {
                apic.set_div_conf(DIVIDE_BY_1);
                apic.set_lvt_timer(LVT_ONE_SHOT);
                apic.set_init_count(count);
            }
        }
    }
    crate::trace::trace_event!(
        ferric_trace::kernel::SCHED_TIMER_ARM,
        percpu.one_shot.armed.armed().map_or(0, Nanos::get),
        backend.code(),
        armed.program.value(),
        u64::from(armed.past) | (u64::from(armed.clamped) << 1),
    );
}

/// Disarms the per-CPU one-shot timer (only if it was armed; a covered arm
/// that never programmed leaves no event to cancel), restoring the LVT entry
/// the periodic paths expect (the profiling timer, for example).
pub fn disarm() {
    let percpu = PercpuBlock::current();
    if percpu.one_shot.armed.disarm() {
        // SAFETY: zero disarms either mode: IA32_TSC_DEADLINE, a
        // kernel-owned MSR that exists when the backend is TSC-deadline, or
        // the countdown's initial count. The LVT entry goes back to its
        // previous value after the timer is disarmed.
        unsafe {
            match percpu.one_shot.backend.get() {
                Some(Backend::TscDeadline) => wrmsr(IA32_TSC_DEADLINE, 0),
                Some(Backend::Countdown { .. }) => the_local_apic().set_init_count(0),
                None => {}
            }
            let previous = percpu.one_shot.previous_lvt.get();
            if previous != u32::MAX {
                the_local_apic().set_lvt_timer(previous);
            }
        }
    }
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
    /// This CPU has no one-shot timer.
    Unsupported,
}

/// The handler for the local APIC timer interrupt.
///
/// A delivery with nothing armed (disarmed, or covered) is stale and changes
/// nothing. Otherwise the TSC says whether the deadline passed: a
/// TSC-deadline interrupt always has, while a countdown that was clamped to
/// its 32-bit range (or ran fast) ends early and is re-armed for the same
/// deadline. A due deadline sets the fired flag for the switch path and the
/// selftest, with its lateness. Only the TSC is read here, never the clock
/// source.
///
/// Called with interrupts off, from the IDT entry.
pub fn handle_fire() {
    let percpu = PercpuBlock::current();
    percpu.one_shot.entries.fetch_add(1, Ordering::Relaxed);
    let Some(deadline) = percpu.one_shot.armed.armed() else {
        return;
    };
    let (Some(backend), Some(rate)) = (percpu.one_shot.backend.get(), arch::time::tsc_rate())
    else {
        return;
    };
    let tsc_deadline = percpu.one_shot.tsc_deadline.get();
    let now = rdtsc();
    match oneshot::on_fire(now, tsc_deadline) {
        Fired::Early { remaining } => {
            // Only the tracepoint reads them.
            #[cfg(not(feature = "trace_sched"))]
            let _ = (remaining, deadline);
            crate::trace::trace_event!(
                ferric_trace::kernel::SCHED_TIMER_FIRE,
                deadline.get(),
                rate.nanos_for_ticks(remaining),
                1u64,
            );
            percpu
                .one_shot
                .early
                .set(percpu.one_shot.early.get().saturating_add(1));
            program(backend, rate, now, tsc_deadline);
        }
        Fired::Due { late } => {
            let late_ns = rate.nanos_for_ticks(late);
            #[cfg(not(feature = "trace_sched"))]
            let _ = deadline;
            crate::trace::trace_event!(
                ferric_trace::kernel::SCHED_TIMER_FIRE,
                deadline.get(),
                late_ns,
                0u64,
            );
            percpu.one_shot.late_ns.set(late_ns);
            percpu.one_shot.fired.store(true, Ordering::Release);
        }
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

/// Runs this CPU's lateness series and returns its result.
///
/// # Safety
///
/// As [`init_cpu`]: interrupts off on entry, on this CPU's idle context.
/// Interrupts are enabled while waiting and disabled again on return.
unsafe fn run_series() -> Verdict {
    let percpu = PercpuBlock::current();
    let rate = arch::time::tsc_rate().expect("a CPU with a one-shot backend has a TSC rate");
    let mut series = SelfTest::new();
    let start = rdtsc();
    let timeout = rate.ticks_for_nanos_ceil(SERIES_TIMEOUT_NS).get();
    let mut timed_out = false;
    percpu.one_shot.early.set(0);
    while let Next::Arm(interval_ns) = series.next() {
        // SAFETY: see `arm`.
        let now = time::monotonic(&mut unsafe { CleanLockToken::new() }) as u64;
        let deadline = Nanos::new(now.saturating_add(interval_ns));
        if arm(deadline) == ArmResult::Unsupported {
            break;
        }
        while !fired() {
            if rdtsc().wrapping_sub(start) > timeout {
                timed_out = true;
                break;
            }
            // Spin with interrupts open rather than halt: a halted CPU only
            // reaches the timeout check when some interrupt arrives, and on a
            // CPU whose one-shot timer never fires (with no periodic tick or
            // IPI to fall back on) the boot would stall.
            // SAFETY: the caller allows interrupts here, and the handlers only
            // touch their own state. Interrupts are off again before the
            // fired flag is read.
            unsafe {
                arch::interrupt::enable_and_nop();
                arch::interrupt::disable();
            }
            core::hint::spin_loop();
        }
        if timed_out {
            break;
        }
        take_fired();
        series.record(percpu.one_shot.late_ns.get());
        disarm();
    }
    disarm();
    series.record_early(percpu.one_shot.early.get());
    series.verdict()
}

fn report_series(cpu: u32, backend: Backend, verdict: Verdict) {
    let line = SeriesLine {
        cpu,
        backend,
        verdict,
    };
    if verdict.complete() {
        crate::info!("{line}");
    } else {
        crate::warn!("{line} (incomplete)");
    }
}

/// The lateness series' log line, which CI matches
/// (`scripts/ferric-ci/check-oneshot-log.sh`).
struct SeriesLine {
    cpu: u32,
    backend: Backend,
    verdict: Verdict,
}

impl core::fmt::Display for SeriesLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self {
            cpu,
            backend,
            verdict,
        } = self;
        write!(
            f,
            "timer: cpu {cpu}: {} fired {}/{} early {} late min {} avg {} max {} ns",
            backend.name(),
            verdict.fired,
            verdict.expected,
            verdict.early,
            verdict.late_min_ns,
            verdict.late_avg_ns,
            verdict.late_max_ns,
        )
    }
}

/// The boot selftest (`kernel.time.one_shot`): validates the one-shot timer
/// and reports the measurements the tickless work compares against. Runs once
/// on the BSP, after the APs are up and before the bootstrap context exists,
/// with interrupts off; the waits open them for one instruction at a time, so
/// the PIT keeps ticking underneath.
pub fn selftest(token: &mut CleanLockToken) {
    let Some(backend) = backend() else {
        crate::info!("timer: one-shot selftest skipped: no one-shot timer on this cpu");
        return;
    };
    let rate = arch::time::tsc_rate().expect("a CPU with a one-shot backend has a TSC rate");
    crate::info!(
        "timer: one-shot selftest: {} backend, tsc rate {rate}",
        backend.name()
    );

    // Round 1: arm for one second, wait for the fire, measure the elapsed
    // monotonic time against the requested deadline.
    let start = time::monotonic(token);
    let deadline = Nanos::new((start as u64).saturating_add(1_000_000_000));
    match arm(deadline) {
        ArmResult::Reprogrammed => {}
        other => {
            crate::warn!("timer: one-shot selftest round 1 arm: {other:?}");
            return;
        }
    }
    // Bound the wait by the raw TSC (the clock source is not involved in the
    // wait itself) and a hard spin count, so a wedged timer is reported
    // instead of stalling the boot.
    let start_ticks = rdtsc();
    let give_up_ticks = rate.ticks_for_nanos(5_000_000_000).get();
    let mut spins = 0u64;
    while spins < 2_000_000_000 && !fired() {
        // Open interrupts for one instruction only: the rest of the selftest
        // takes the log and serial spinlocks, and a PIT tick that lands while
        // one is held can switch this CPU away from the holder for good.
        // SAFETY: the timer interrupt needs a window to arrive; its handler
        // only touches this CPU's one-shot state, and interrupts are off again
        // before the fired flag is read.
        unsafe {
            arch::interrupt::enable_and_nop();
            arch::interrupt::disable();
        }
        spins = spins.saturating_add(1);
        if rdtsc().wrapping_sub(start_ticks) >= give_up_ticks {
            break;
        }
    }
    if !fired() {
        let entries = PercpuBlock::current()
            .one_shot
            .entries
            .load(Ordering::Relaxed);
        // SAFETY: reading the LVT shows what the hardware latched.
        let lvt = unsafe { the_local_apic().lvt_timer() };
        crate::warn!(
            "timer: one-shot selftest round 1: no fire (wedged): {entries} handler entries, lvt read-back {lvt:#x}"
        );
        disarm();
        return;
    }
    take_fired();
    let elapsed = time::monotonic(token).saturating_sub(start);
    crate::info!(
        "timer: one-shot selftest round 1: fired after {elapsed} ns (wanted 1000000000, error {})",
        (elapsed as i128).saturating_sub(1_000_000_000)
    );
    disarm();

    // Round 2: an already-due deadline must fire at once, never be discarded.
    let now = time::monotonic(token);
    let past = Nanos::new(now.saturating_sub(1) as u64);
    match arm(past) {
        ArmResult::Fired => {
            take_fired();
            crate::info!("timer: one-shot selftest round 2: a due deadline fired at once");
        }
        other => {
            crate::warn!("timer: one-shot selftest round 2: expected Fired, got {other:?}");
        }
    }
    disarm();

    // Round 3: a disarmed timer does not fire.
    let deadline = Nanos::new((time::monotonic(token) as u64).saturating_add(1_000_000_000));
    if arm(deadline) == ArmResult::Reprogrammed {
        disarm();
        let quiet_until = rdtsc().wrapping_add(
            rate.ticks_for_nanos(u64::from(SELFTEST_QUIET_MS) * 1_000_000)
                .get(),
        );
        loop {
            // SAFETY: as in round 1, interrupts are open for one instruction.
            unsafe {
                arch::interrupt::enable_and_nop();
                arch::interrupt::disable();
            }
            core::hint::spin_loop();
            if rdtsc() >= quiet_until {
                break;
            }
            if fired() {
                crate::warn!("timer: one-shot selftest round 3: a disarmed timer fired!");
                take_fired();
                return;
            }
        }
        crate::info!("timer: one-shot selftest round 3: a disarmed timer stayed quiet");
    }
    disarm();
}
