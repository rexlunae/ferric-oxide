//! Ferric Oxide (tickless scheduling, #186, `kernel.time.one_shot`): the monotonic clock
//! sources.
//!
//! `monotonic_absolute` must keep advancing while the periodic PIT interrupt is masked
//! (the tickless work stops it), so it no longer depends on `time::OFFSET` advancing: a
//! *continuous* source is selected once, by [`init`], and validated against the others:
//!
//! 1. **The KVM paravirtual clock**, when `x86_kvm_pv` is built in and the vCPU set it
//!    up: KVM's master clock, already monotonic, per-vCPU but from one master, so
//!    comparable across CPUs.
//! 2. **The invariant TSC, calibrated at boot**: invariant means it does not stop on idle
//!    and runs at one rate; the calibration gives that rate.
//! 3. **The HPET main counter**, whose tick length the capabilities register gives; the
//!    64-bit counter does not wrap in practice (the previous code had the same TODO for
//!    rollover).
//! 4. **The PIT fallback** (a machine with neither an invariant TSC nor a usable HPET):
//!    the previous `OFFSET + interpolation` path, unchanged. On such a machine the clock
//!    still depends on the periodic interrupt; that is reported at boot and the tickless
//!    one-shot work stays limited to the continuous sources.
//!
//! The TSC is calibrated whether or not it is invariant, because the one-shot timer
//! (`arch::timer`) programs its deadlines in TSC ticks either way. The calibration
//! (`ferric_timed::calibrate`) measures the TSC against the KVM clock when available, else
//! the HPET main counter (32- or 64-bit), else PIT channel 0, over several short windows
//! with interrupts off, takes the median and refuses windows that disagree (a vCPU
//! preempted during one window). The result is logged and recorded by the
//! `sched.timer_calibrate` tracepoint.
//!
//! `time::OFFSET` remains, advanced by the PIT handler, and is what the fallback reads; the
//! realtime offset contract (`time::realtime = time::START + monotonic`) is untouched. The
//! portable arithmetic (rate conversions, calibration, deadline ordering, wrap) lives in
//! `ferric_timed`.

use core::num::NonZeroU64;

use ferric_timed::{
    calibrate::{self, estimate, CalibrationError, Sample, WrappingCounter},
    rate::TicksPerSecond,
};
use spin::Once;

use super::super::device::{hpet, pit};
use crate::sync::CleanLockToken;

/// The leaf that reports an invariant TSC.
const INVARIANT_TSC_LEAF: u32 = 0x8000_0007;

/// Calibration windows and their length. Five 2 ms windows cost 10 ms of boot.
const WINDOWS: usize = 5;
const WINDOW_NS: u64 = 2_000_000;
/// The most the windows may disagree. A virtual CPU preempted by its host during one
/// window produces an outlier; 0.2% rejects that without failing on ordinary noise.
pub(crate) const MAX_SPREAD_PPM: u64 = 2_000;
/// How many reference readings a calibration window may take before the reference is
/// taken to be stopped: a 2 ms window needs a few thousand even where each read is a VM
/// exit.
const MAX_SPINS: u32 = 5_000_000;

/// `sched.timer_calibrate`'s counter and reference codes.
pub(crate) const COUNTER_TSC: u64 = 1;
const REFERENCE_HPET: u64 = 1;
const REFERENCE_PIT: u64 = 2;
const REFERENCE_PVCLOCK: u64 = 4;

/// The chosen continuous source.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// The KVM paravirtual clock.
    #[cfg(feature = "x86_kvm_pv")]
    Kvm,
    /// The invariant TSC at the calibrated rate.
    Tsc(TicksPerSecond),
    /// The HPET main counter at its ACPI-reported period.
    Hpet,
    /// No continuous source: the previous OFFSET + interpolation path.
    PitFallback,
}

static SOURCE: Once<Source> = Once::new();
/// The calibrated TSC rate, used by the one-shot timer (whose deadlines are in TSC ticks)
/// even when the clock source itself is the KVM clock or the HPET.
static TSC_RATE: Once<Option<TicksPerSecond>> = Once::new();
/// Whether the TSC is invariant, read once (CPUID is a VM exit under a hypervisor).
static TSC_INVARIANT: Once<bool> = Once::new();

/// Calibrates the TSC and selects the clock source, logging both.
///
/// # Safety
///
/// Must run once, on the BSP, with interrupts disabled, after the HPET was set up or the
/// PIT programmed (`device::init_noncore`), and before anything reads the monotonic clock.
pub unsafe fn init() {
    let _ = tsc_rate();
    let _ = source();
}

/// The clock source, picked and logged on the first call.
fn source() -> &'static Source {
    SOURCE.call_once(select)
}

/// The TSC's calibrated rate, or `None` when calibration failed (the one-shot timer then
/// reports unsupported).
pub fn tsc_rate() -> Option<TicksPerSecond> {
    *TSC_RATE.call_once(calibrate_tsc)
}

fn rdtsc() -> u64 {
    // SAFETY: RDTSC reads a counter; it has no side effects and every x86_64 CPU has it.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The counter the TSC is measured against.
enum Reference {
    /// The KVM paravirtual clock, in nanoseconds.
    #[cfg(feature = "x86_kvm_pv")]
    Pvclock,
    /// The HPET's main counter.
    Hpet(hpet::MainCounter),
    /// PIT channel 0, which `pit::init` programmed with `pit::CHAN0_DIVISOR` because there
    /// is no usable HPET.
    Pit,
}

impl Reference {
    fn find() -> Self {
        #[cfg(feature = "x86_kvm_pv")]
        if super::super::device::tsc::monotonic_absolute().is_some() {
            return Self::Pvclock;
        }
        let hpet = crate::acpi::ACPI_TABLE.hpet.read();
        match hpet.as_ref().and_then(hpet::main_counter) {
            Some(main) => Self::Hpet(main),
            None => Self::Pit,
        }
    }

    fn code(&self) -> u64 {
        match self {
            #[cfg(feature = "x86_kvm_pv")]
            Self::Pvclock => REFERENCE_PVCLOCK,
            Self::Hpet(_) => REFERENCE_HPET,
            Self::Pit => REFERENCE_PIT,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "x86_kvm_pv")]
            Self::Pvclock => "kvm pvclock",
            Self::Hpet(_) => "hpet",
            Self::Pit => "pit",
        }
    }

    fn period_fs(&self) -> u64 {
        match self {
            #[cfg(feature = "x86_kvm_pv")]
            Self::Pvclock => 1_000_000,
            Self::Hpet(main) => main.period_fs,
            Self::Pit => u64::try_from(pit::PERIOD_FS).unwrap_or(u64::MAX),
        }
    }

    /// The reference's raw reading.
    fn read(&self) -> u64 {
        match self {
            // A pvclock that steps backward reads 0 here, which stalls the window and fails
            // the calibration rather than skewing it.
            #[cfg(feature = "x86_kvm_pv")]
            Self::Pvclock => super::super::device::tsc::monotonic_absolute()
                .and_then(|ns| u64::try_from(ns).ok())
                .unwrap_or(0),
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
            #[cfg(feature = "x86_kvm_pv")]
            Self::Pvclock => WrappingCounter::full_width(first),
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

/// Measures the TSC against the KVM clock, the HPET or the PIT, logging the result.
fn calibrate_tsc() -> Option<TicksPerSecond> {
    let reference = Reference::find();
    let result = (|| {
        let mut estimates = [TicksPerSecond::new(1).expect("one is not zero"); WINDOWS];
        for slot in &mut estimates {
            let (start, end) = reference.window();
            *slot = estimate(start, end, reference.period_fs())?;
        }
        calibrate::calibrate(&mut estimates, MAX_SPREAD_PPM)
    })();
    let calibrated = match result {
        Ok(calibrated) => calibrated,
        Err(err) => {
            report_failure(err, &reference);
            return None;
        }
    };
    crate::trace::trace_event!(
        ferric_trace::kernel::SCHED_TIMER_CALIBRATE,
        COUNTER_TSC,
        calibrated.hz.get(),
        reference.code(),
        calibrated.spread_ppm,
    );
    let invariant = tsc_invariant();
    crate::info!(
        "time: tsc calibrated at {} against the {} ({} windows, spread {} ppm, invariant {})",
        calibrated.hz,
        reference.name(),
        calibrated.windows,
        calibrated.spread_ppm,
        invariant,
    );
    Some(calibrated.hz)
}

fn report_failure(err: CalibrationError, reference: &Reference) {
    crate::warn!(
        "time: tsc calibration failed against the {} ({err}); no tsc clock or one-shot timer",
        reference.name()
    );
}

/// Picks the clock source from what this machine offers and logs why.
fn select() -> Source {
    // The KVM paravirtual clock first: KVM guarantees monotonicity.
    #[cfg(feature = "x86_kvm_pv")]
    if super::super::device::tsc::monotonic_absolute().is_some() {
        crate::info!("time: clock source kvm pvclock");
        return Source::Kvm;
    }

    if let Some(rate) = tsc_clock_rate() {
        crate::info!("time: clock source invariant tsc ({rate})");
        return Source::Tsc(rate);
    }

    if let Some((_, period_fs)) = hpet_cached() {
        crate::info!("time: clock source hpet (period {} fs)", period_fs);
        return Source::Hpet;
    }

    crate::warn!(
        "time: no continuous clock source (no KVM clock, no invariant TSC, no usable HPET); \
         the clock depends on the periodic PIT interrupt"
    );
    Source::PitFallback
}

/// Whether this CPU's TSC is invariant (CPUID 0x8000_0007 EDX bit 8).
fn tsc_invariant() -> bool {
    *TSC_INVARIANT.call_once(|| {
        let max_extended = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        if max_extended < INVARIANT_TSC_LEAF {
            return false;
        }
        let edx = core::arch::x86_64::__cpuid(INVARIANT_TSC_LEAF).edx;
        edx & (1 << 8) != 0
    })
}

/// The calibrated TSC rate when the TSC is invariant, so it can be a clock source.
fn tsc_clock_rate() -> Option<TicksPerSecond> {
    tsc_rate().filter(|_| tsc_invariant())
}

/// The HPET main counter's address in the linear map and its tick length, resolved once
/// (under the ACPI lock) and cached, so the interrupt paths can read the counter without
/// taking the ACPI lock (the same pattern `trace::clock` uses for its fallback clock).
static HPET_CLOCK: Once<Option<(usize, u64)>> = Once::new();

/// The HPET counter's address and its period, or `None` when the ACPI table has no usable
/// HPET (64-bit counter, enabled, known period). The address is kept as `usize` so the
/// `Once` payload is `Send + Sync` without an unsafe impl; the reads cast it back and are
/// unsafe only in the dereference.
fn hpet_cached() -> Option<(*const u64, u64)> {
    (*HPET_CLOCK.call_once(|| {
        if cfg!(feature = "acpi")
            && let Some(ref hpet) = *crate::acpi::ACPI_TABLE.hpet.read()
        {
            hpet::trace_counter(hpet).map(|(counter, period)| (counter as usize, period))
        } else {
            None
        }
    }))
    .map(|(address, period)| (address as *const u64, period))
}

#[allow(clippy::arithmetic_side_effects)] // unit conversion with u128 intermediates
/// The HPET main counter, as nanoseconds since the HPET started.
fn hpet_ns() -> u128 {
    let (counter, period_fs) = hpet_cached().expect("the selected clock has an HPET");
    // SAFETY: the mapped register outlives the kernel; reading it has no side
    // effects.
    let ticks = unsafe { counter.read_volatile() };
    (ticks as u128 * period_fs as u128) / 1_000_000
}

/// The last continuous-clock value this source returned, so a source hiccup (the KVM
/// clock briefly stepping backward while it re-bases its page, for example) can be
/// clamped instead of panicking or jumping the clock back.
static LAST_CONTINUOUS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// When the KVM clock hiccups (its page re-bases and every later read is behind the last
/// value, so its scale is over), the handover offset bridges the last good pvclock value
/// onto the fallback scale: after the handover, `time = fallback_time + KVM_HANDOVER`
/// continues without freezing or jumping backward. 0 means no handover happened yet.
static KVM_HANDOVER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The continuous sources without the KVM clock: the calibrated invariant TSC, else the
/// HPET, else the `OFFSET + interpolation` fallback. Used as the KVM clock's degradation
/// path (which cannot consult the selected source: on a KVM clock hiccup the selection is
/// still Kvm).
#[allow(clippy::arithmetic_side_effects)] // unit conversion with u128 intermediates
fn continuous_without_kvm(token: &mut CleanLockToken) -> u128 {
    if let Some(rate) = tsc_clock_rate() {
        u128::from(rate.nanos_for_ticks(rdtsc()))
    } else if hpet_cached().is_some() {
        hpet_ns()
    } else {
        let offset = { *crate::time::OFFSET.read(token.token()) };
        offset + super::hpet_or_pit()
    }
}

/// The monotonic time in nanoseconds: continuous while the periodic interrupt is masked,
/// comparable across CPUs, from the source [`init`] picked.
pub fn monotonic_absolute(token: &mut CleanLockToken) -> u128 {
    let ns = match source() {
        #[cfg(feature = "x86_kvm_pv")]
        Source::Kvm => match super::super::device::tsc::monotonic_absolute() {
            Some(ns) => ns,
            None => {
                // The pvclock stepped backward (its page re-based): its scale is over. The
                // first failure bridges the scales, then the fallback clock carries on with
                // the offset; the clamped value frozen here is exactly the last good pvclock
                // value.
                let offset = KVM_HANDOVER.load(core::sync::atomic::Ordering::Relaxed);
                if offset == 0 {
                    let fallback_now = continuous_without_kvm(token);
                    let last = LAST_CONTINUOUS.load(core::sync::atomic::Ordering::Relaxed);
                    KVM_HANDOVER.store(
                        last.saturating_sub(fallback_now as u64),
                        core::sync::atomic::Ordering::Relaxed,
                    );
                    u128::from(last)
                } else {
                    continuous_without_kvm(token).saturating_add(u128::from(offset))
                }
            }
        },
        _ => continuous_without_kvm(token).saturating_add(u128::from(
            KVM_HANDOVER.load(core::sync::atomic::Ordering::Relaxed),
        )),
    };
    // Clamp against the last returned value: the sources are continuous, but their
    // handover and the KVM clock's page re-basing can produce a brief backward step, which
    // would turn wake timers into long sleeps. Clamping freezes the clock for that step
    // instead (bounded by it).
    let last = LAST_CONTINUOUS.load(core::sync::atomic::Ordering::Relaxed);
    let ns = ns.max(u128::from(last));
    LAST_CONTINUOUS.store(ns as u64, core::sync::atomic::Ordering::Relaxed);
    ns
}

/// The resolution of [`monotonic_absolute`], in nanoseconds.
pub fn monotonic_resolution() -> u128 {
    match source() {
        #[cfg(feature = "x86_kvm_pv")]
        Source::Kvm => 1,
        Source::Tsc(_) => 1,
        Source::Hpet => {
            let (_, period_fs) = hpet_cached().expect("the selected clock has an HPET");
            (period_fs as u128) / 1_000_000
        }
        Source::PitFallback => pit::PERIOD_FS / 1_000_000,
    }
}
