//! Ferric Oxide (`kernel.oneshot_timer`, issue #186): a calibrated TSC clocksource and a
//! per-CPU one-shot local APIC timer.
//!
//! This is the first step of tickless scheduling. It measures and validates the hardware
//! and nothing else: the PIT (or HPET) still drives time, the scheduler tick and timeouts,
//! exactly as without the option.
//!
//! - [`init_bsp`] measures the TSC's rate against the HPET main counter, or the PIT when
//!   there is no usable HPET, over several busy-wait windows with interrupts off
//!   (`ferric_clock::calibrate`). The result is [`tsc_scale`].
//! - [`init_cpu`] runs on every CPU once its contexts exist. It picks the CPU's backend:
//!   TSC-deadline mode when CPUID.01H:ECX\[24\] reports it, otherwise a one-shot countdown
//!   whose rate it measures against the TSC. It then runs `ferric_clock::selftest`'s
//!   series of one-shot deadlines, with interrupts on, and logs how late they fired. The
//!   timer is left disarmed afterwards.
//! - [`interrupt`] is the local APIC timer vector's handler (48, on every CPU with the
//!   option): it re-arms an early countdown for the same deadline, and otherwise hands the
//!   expiry to the self-test.
//!
//! The `sched.timer_calibrate`, `sched.timer_arm` and `sched.timer_fire` tracepoints
//! (`kernel.trace_sched`) record each step, so a capture shows every deadline, the value
//! programmed and how late it was served.
//!
//! Hardware reference: Intel SDM Vol. 3A, section 11.5.4 ("APIC Timer") and 11.5.4.1
//! ("TSC-Deadline Mode"); the HPET specification 1.0a for the main counter.

use core::{
    cell::Cell,
    num::NonZeroU64,
    sync::atomic::{fence, Ordering},
};

use ferric_clock::{
    calibrate::{self, estimate, estimate_against, CalibrationError},
    oneshot::{self, Backend, Fired, Program},
    selftest::{Next, SelfTest, Verdict},
    Calibrated, Hz, Sample, Scale, WrappingCounter, NANOS_PER_SEC,
};
use spin::Once;
use x86::msr::{wrmsr, IA32_TSC_DEADLINE};

use super::{local_apic, pit};
use crate::percpu::PercpuBlock;

/// The local APIC timer's interrupt vector, as the IDT sets it up.
const VECTOR: u32 = 48;
/// LVT timer mode bits (SDM Vol. 3A, Figure 11-8): one-shot is 00, TSC-deadline 10.
const LVT_ONE_SHOT: u32 = 0b00 << 17;
const LVT_TSC_DEADLINE: u32 = 0b10 << 17;
const LVT_MASKED: u32 = 1 << 16;
/// Divide configuration for divide-by-1 (SDM Vol. 3A, Figure 11-10).
const DIVIDE_BY_1: u32 = 0b1011;

/// Calibration windows and their length. Five 2 ms windows cost 10 ms of boot.
const WINDOWS: usize = 5;
const WINDOW_NS: u64 = 2_000_000;
/// The most the windows may disagree. A virtual CPU preempted by its host during one
/// window produces an outlier; 0.2% rejects that without failing on ordinary noise.
const MAX_SPREAD_PPM: u64 = 2_000;
/// How many reference readings a calibration window may take before the reference is
/// taken to be stopped: a 2 ms window needs a few thousand even where each read is a VM exit.
const MAX_SPINS: u32 = 5_000_000;
/// The least time between arming and firing (5 µs), so a deadline closer than the write
/// takes still interrupts instead of being missed.
const MIN_LEAD_NS: u64 = 5_000;
/// How long a CPU's self-test may take before it is abandoned as incomplete: well above
/// the series' 25 ms of deadlines, even on a slow emulator.
const SELFTEST_TIMEOUT_NS: u64 = 2 * NANOS_PER_SEC;

/// `sched.timer_calibrate`'s counter and reference codes.
const COUNTER_TSC: u64 = 1;
const COUNTER_LAPIC: u64 = 2;
const REFERENCE_HPET: u64 = 1;
const REFERENCE_PIT: u64 = 2;
const REFERENCE_TSC: u64 = 3;

/// The calibrated TSC, once [`init_bsp`] has measured it.
static TSC: Once<Scale> = Once::new();

/// The calibrated TSC, or `None` when calibration failed or has not run.
pub fn tsc_scale() -> Option<&'static Scale> {
    TSC.get()
}

/// This CPU's one-shot timer: its backend, the deadline it is armed for and the
/// self-test's progress. Only the owning CPU touches it, with interrupts off or from the
/// timer interrupt, hence `Cell`.
pub struct OneShotPercpu {
    backend: Cell<Option<Backend>>,
    /// The TSC deadline armed, or 0 when disarmed.
    deadline: Cell<u64>,
    selftest: Cell<Option<SelfTest>>,
}

impl OneShotPercpu {
    /// A CPU whose timer is not set up.
    pub const fn default() -> Self {
        Self {
            backend: Cell::new(None),
            deadline: Cell::new(0),
            selftest: Cell::new(None),
        }
    }
}

fn percpu() -> &'static OneShotPercpu {
    &PercpuBlock::current().misc_arch_info.oneshot
}

fn rdtsc() -> u64 {
    // SAFETY: RDTSC reads a counter; it has no side effects and every x86_64 CPU has it.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The counter the TSC is measured against.
enum Reference {
    /// The HPET's main counter.
    Hpet(super::hpet::MainCounter),
    /// PIT channel 0, which `pit::init` programmed with `pit::CHAN0_DIVISOR` because there
    /// is no usable HPET.
    Pit,
}

impl Reference {
    fn find() -> Self {
        let hpet = crate::acpi::ACPI_TABLE.hpet.read();
        match hpet.as_ref().and_then(super::hpet::main_counter) {
            Some(main) => Self::Hpet(main),
            None => Self::Pit,
        }
    }

    fn code(&self) -> u64 {
        match self {
            Self::Hpet(_) => REFERENCE_HPET,
            Self::Pit => REFERENCE_PIT,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Hpet(_) => "HPET",
            Self::Pit => "PIT",
        }
    }

    fn period_fs(&self) -> u64 {
        match self {
            Self::Hpet(main) => main.period_fs,
            Self::Pit => u64::try_from(pit::PERIOD_FS).unwrap_or(u64::MAX),
        }
    }

    /// The reference's raw reading.
    fn read(&self) -> u64 {
        match self {
            // SAFETY: `main.counter` is the HPET main counter in the linear map, which stays
            // mapped for the kernel's life (`hpet::main_counter`); reading it has no side
            // effects.
            Self::Hpet(main) => unsafe { main.counter.read_volatile() },
            // SAFETY: called on the BSP during boot with interrupts off, so nothing else
            // drives the PIT's command port; latching channel 0 has no other effect.
            Self::Pit => u64::from(unsafe { pit::read() }),
        }
    }

    /// Extends the reference past its wraps, from the reading `first`.
    fn extend(&self, first: u64) -> WrappingCounter {
        match self {
            Self::Hpet(main) if main.wide => WrappingCounter::full_width(first),
            Self::Hpet(_) => {
                WrappingCounter::new(NonZeroU64::new(1 << 32).expect("2^32 is not zero"), first)
            }
            Self::Pit => WrappingCounter::new(
                NonZeroU64::new(u64::from(pit::CHAN0_DIVISOR))
                    .expect("the PIT divisor is not zero"),
                first,
            ),
        }
    }

    /// Busy-waits at least `WINDOW_NS` of reference time and returns the TSC and reference
    /// readings at both ends. A reference that stops advancing ends the wait after
    /// `MAX_SPINS` readings, and the window then fails to calibrate.
    fn window(&self) -> (Sample, Sample) {
        let ticks = (u128::from(WINDOW_NS) * 1_000_000).div_ceil(u128::from(self.period_fs()));
        let ticks = u64::try_from(ticks).unwrap_or(u64::MAX);
        let mut elapsed = self.extend(self.read());
        let start = Sample {
            counter: rdtsc(),
            reference: 0,
        };
        for _ in 0..MAX_SPINS {
            // Read far more often than any reference wraps (the PIT's 4 ms is the shortest).
            let reference = elapsed.update(self.read());
            if reference >= ticks {
                return (
                    start,
                    Sample {
                        counter: rdtsc(),
                        reference,
                    },
                );
            }
            core::hint::spin_loop();
        }
        (
            start,
            Sample {
                counter: rdtsc(),
                reference: elapsed.total(),
            },
        )
    }
}

/// Measures the TSC against the HPET or PIT and keeps the result for [`tsc_scale`].
///
/// # Safety
///
/// Must run once, on the BSP, with interrupts disabled, after `init_noncore` set up the
/// HPET or programmed the PIT.
pub unsafe fn init_bsp() {
    let reference = Reference::find();
    let mut estimates = [Hz::new(1).expect("one is not zero"); WINDOWS];
    for estimate_slot in &mut estimates {
        let (start, end) = reference.window();
        match estimate(start, end, reference.period_fs()) {
            Ok(hz) => *estimate_slot = hz,
            Err(err) => return report_tsc_failure(err),
        }
    }
    match calibrate::calibrate(&mut estimates, MAX_SPREAD_PPM) {
        Ok(calibrated) => {
            report_tsc(&calibrated, &reference);
            TSC.call_once(|| Scale::new(calibrated.hz));
        }
        Err(err) => report_tsc_failure(err),
    }
}

fn report_tsc(calibrated: &Calibrated, reference: &Reference) {
    crate::trace::trace_event!(
        ferric_trace::kernel::SCHED_TIMER_CALIBRATE,
        COUNTER_TSC,
        calibrated.hz.get(),
        reference.code(),
        calibrated.spread_ppm,
    );
    let cpuid = crate::arch::cpuid::cpuid();
    let invariant = invariant_tsc();
    let reported = cpuid.get_tsc_info().and_then(|info| info.tsc_frequency());
    crate::info!(
        "oneshot: TSC {} from {} ({} windows, spread {} ppm, invariant {}, CPUID 15H says {:?})",
        calibrated.hz,
        reference.name(),
        calibrated.windows,
        calibrated.spread_ppm,
        invariant,
        reported,
    );
    if !invariant {
        crate::warn!(
            "oneshot: the TSC is not invariant; deadlines may drift with frequency changes"
        );
    }
}

fn report_tsc_failure(err: CalibrationError) {
    crate::warn!("oneshot: TSC calibration failed ({err}); one-shot timers stay off");
}

fn invariant_tsc() -> bool {
    use ferric_trace::clock::{invariant_tsc, INVARIANT_TSC_LEAF};
    let max_extended = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    let edx = if max_extended >= INVARIANT_TSC_LEAF {
        core::arch::x86_64::__cpuid(INVARIANT_TSC_LEAF).edx
    } else {
        0
    };
    invariant_tsc(max_extended, edx)
}

/// Sets up this CPU's one-shot timer and runs its self-test.
///
/// # Safety
///
/// Must run once per CPU, on that CPU, with interrupts disabled, after its local APIC and
/// its idle context exist (the self-test enables interrupts while it waits) and after
/// [`init_bsp`]. Returns with interrupts disabled.
pub unsafe fn init_cpu() {
    let Some(tsc) = tsc_scale() else {
        return;
    };
    let backend = match choose_backend(tsc) {
        Ok(backend) => backend,
        Err(err) => {
            crate::warn!(
                "oneshot: cpu {}: countdown calibration failed ({err})",
                crate::cpu_id().get()
            );
            return;
        }
    };
    percpu().backend.set(Some(backend));
    // SAFETY: the caller runs this with interrupts off on the CPU that owns the timer.
    let verdict = unsafe { run_selftest(tsc) };
    report_selftest(backend, verdict);
}

/// TSC-deadline mode when the CPU has it, otherwise a countdown measured against the TSC.
fn choose_backend(tsc: &Scale) -> Result<Backend, CalibrationError> {
    let cpuid = crate::arch::cpuid::cpuid();
    if cpuid
        .get_feature_info()
        .is_some_and(|info| info.has_tsc_deadline())
    {
        // SAFETY: the local APIC is set up; masking aside, the vector and mode are what the
        // handler expects, and nothing is armed until a deadline is written.
        unsafe { local_apic::the_local_apic().set_lvt_timer(VECTOR | LVT_TSC_DEADLINE) };
        // A deadline write must not pass the mode switch (SDM 11.5.4.1).
        fence(Ordering::SeqCst);
        return Ok(Backend::TscDeadline);
    }
    let timer_hz = calibrate_countdown(tsc.hz())?;
    Ok(Backend::Countdown { timer_hz })
}

/// Measures this CPU's local APIC countdown against the calibrated TSC.
fn calibrate_countdown(tsc_hz: Hz) -> Result<Hz, CalibrationError> {
    let window_ticks = Scale::new(tsc_hz).ns_to_ticks_ceil(WINDOW_NS);
    let mut estimates = [Hz::new(1).expect("one is not zero"); WINDOWS];
    for slot in &mut estimates {
        let (start, end) = countdown_window(window_ticks);
        *slot = estimate_against(start, end, tsc_hz)?;
    }
    let calibrated = calibrate::calibrate(&mut estimates, MAX_SPREAD_PPM)?;
    crate::trace::trace_event!(
        ferric_trace::kernel::SCHED_TIMER_CALIBRATE,
        COUNTER_LAPIC,
        calibrated.hz.get(),
        REFERENCE_TSC,
        calibrated.spread_ppm,
    );
    Ok(calibrated.hz)
}

/// Runs this CPU's countdown, masked, for `window_ticks` of TSC and returns the counts
/// elapsed (`!count` turns the down-counter from `u32::MAX` into an up-counter) and the TSC
/// at both ends. Leaves the countdown stopped and unmasked in one-shot mode, ready for
/// [`arm`].
fn countdown_window(window_ticks: u64) -> (Sample, Sample) {
    // SAFETY: this CPU's local APIC is set up. A masked countdown raises no interrupt, so
    // starting it and reading its count have no other effect; a zero initial count stops
    // it again before the mask is lifted.
    unsafe {
        let apic = local_apic::the_local_apic();
        apic.set_div_conf(DIVIDE_BY_1);
        apic.set_lvt_timer(VECTOR | LVT_ONE_SHOT | LVT_MASKED);
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
        apic.set_lvt_timer(VECTOR | LVT_ONE_SHOT);
        (start, end)
    }
}

/// Arms this CPU's timer for the TSC reading `deadline`.
fn arm(tsc: &Scale, deadline: u64) {
    let state = percpu();
    let Some(backend) = state.backend.get() else {
        return;
    };
    let now = rdtsc();
    let armed = oneshot::arm(
        backend,
        tsc,
        now,
        deadline,
        tsc.ns_to_ticks_ceil(MIN_LEAD_NS),
    );
    state.deadline.set(deadline);
    // SAFETY: this CPU's local APIC is set up in `backend`'s mode with the timer vector;
    // writing the deadline or count only schedules that interrupt on this CPU.
    unsafe {
        match armed.program {
            Program::TscDeadline(value) => wrmsr(IA32_TSC_DEADLINE, value),
            Program::Countdown(count) => local_apic::the_local_apic().set_init_count(count),
        }
    }
    crate::trace::trace_event!(
        ferric_trace::kernel::SCHED_TIMER_ARM,
        deadline,
        backend.code(),
        armed.program.value(),
        u64::from(armed.past) | (u64::from(armed.clamped) << 1),
    );
}

/// Stops this CPU's timer.
fn disarm() {
    let state = percpu();
    state.deadline.set(0);
    // SAFETY: as in `arm`; zero stops either mode.
    unsafe {
        match state.backend.get() {
            Some(Backend::TscDeadline) => wrmsr(IA32_TSC_DEADLINE, 0),
            Some(Backend::Countdown { .. }) => local_apic::the_local_apic().set_init_count(0),
            None => {}
        }
    }
}

/// The local APIC timer interrupt: re-arms an early countdown, otherwise records the
/// expiry and arms the self-test's next shot.
pub fn interrupt() {
    let state = percpu();
    let deadline = state.deadline.get();
    let Some(tsc) = tsc_scale() else {
        return;
    };
    if deadline == 0 {
        return;
    }
    let now = rdtsc();
    match oneshot::on_fire(now, deadline) {
        Fired::Early { remaining } => {
            // Only the tracepoint reads it.
            #[cfg(not(feature = "trace_sched"))]
            let _ = remaining;
            crate::trace::trace_event!(
                ferric_trace::kernel::SCHED_TIMER_FIRE,
                deadline,
                tsc.ticks_to_ns(remaining),
                1u64,
            );
            if let Some(mut test) = state.selftest.get() {
                test.record_early();
                state.selftest.set(Some(test));
            }
            arm(tsc, deadline);
        }
        Fired::Due { late } => {
            let late_ns = tsc.ticks_to_ns(late);
            crate::trace::trace_event!(
                ferric_trace::kernel::SCHED_TIMER_FIRE,
                deadline,
                late_ns,
                0u64,
            );
            state.deadline.set(0);
            let Some(mut test) = state.selftest.get() else {
                return;
            };
            let next = test.record(late_ns);
            state.selftest.set(Some(test));
            if let Next::Arm(interval_ns) = next {
                arm(tsc, now.saturating_add(tsc.ns_to_ticks_ceil(interval_ns)));
            }
        }
    }
}

/// Runs the self-test series and returns its result.
///
/// # Safety
///
/// As [`init_cpu`]: interrupts off on entry, on this CPU's idle context. Interrupts are
/// enabled while waiting and disabled again on return.
unsafe fn run_selftest(tsc: &Scale) -> Verdict {
    let state = percpu();
    state.selftest.set(Some(SelfTest::new()));
    let start = rdtsc();
    let timeout = tsc.ns_to_ticks_ceil(SELFTEST_TIMEOUT_NS);
    arm(
        tsc,
        start.saturating_add(tsc.ns_to_ticks_ceil(SelfTest::first_interval())),
    );
    loop {
        // SAFETY: the caller allows interrupts here; any interrupt (the timer, the PIT,
        // an IPI) ends the halt, and the handlers only touch their own state. Interrupts are
        // off again before the self-test's state is read.
        unsafe {
            crate::arch::interrupt::enable_and_halt();
            crate::arch::interrupt::disable();
        }
        let done = state.deadline.get() == 0;
        if done || rdtsc().wrapping_sub(start) > timeout {
            break;
        }
    }
    disarm();
    let verdict = state
        .selftest
        .get()
        .map_or(SelfTest::new().verdict(), |test| test.verdict());
    state.selftest.set(None);
    verdict
}

fn report_selftest(backend: Backend, verdict: Verdict) {
    let cpu = crate::cpu_id().get();
    if let Backend::Countdown { timer_hz } = backend {
        crate::info!("oneshot: cpu {cpu}: countdown {timer_hz}");
    }
    let line = SelftestLine {
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

/// The self-test's log line, which CI matches (`scripts/ferric-ci/check-oneshot-log.sh`).
struct SelftestLine {
    cpu: u32,
    backend: Backend,
    verdict: Verdict,
}

impl core::fmt::Display for SelftestLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self {
            cpu,
            backend,
            verdict,
        } = self;
        write!(
            f,
            "oneshot: cpu {cpu}: {} fired {}/{} early {} late min {} avg {} max {} ns",
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
