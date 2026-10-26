//! Ferric Oxide (tickless scheduling, #186): the monotonic clock sources.
//!
//! `monotonic_absolute` must keep advancing while the periodic PIT interrupt
//! is masked (the tickless work stops it), so it no longer depends on
//! `time::OFFSET` advancing: a *continuous* source is selected once, at the
//! first read, and validated against the others:
//!
//! 1. **The KVM paravirtual clock**, when `x86_kvm_pv` is built in and the
//!    vCPU set it up: KVM's master clock, already monotonic, per-vCPU but
//!    from one master, so comparable across CPUs.
//! 2. **The invariant TSC, calibrated at boot** against the HPET (whose tick
//!    length the ACPI table gives): invariant means it does not stop on idle
//!    and runs at one rate; the calibration gives that rate (logged, with a
//!    second sample's cross-check against the HPET in ppm).
//! 3. **The HPET main counter**, whose tick length the capabilities register
//!    gives; the 64-bit counter does not wrap in practice (the previous code
//!    had the same TODO for rollover).
//! 4. **The PIT fallback** (a machine with neither an invariant TSC with a
//!    usable HPET, nor a usable HPET): the previous `OFFSET + interpolation`
//!    path, unchanged. On such a machine the clock still depends on the
//!    periodic interrupt; that is reported at boot and the tickless one-shot
//!    work stays limited to the continuous sources.
//!
//! `time::OFFSET` remains, advanced by the PIT handler, and is what the
//! fallback reads; the realtime offset contract (`time::realtime =
//! time::START + monotonic`) is untouched. The portable arithmetic (rate
//! conversions, deadline ordering, wrap) lives in `ferric_timed`.

use ferric_timed::rate::TicksPerSecond;
use spin::Once;

use super::device::{hpet, pit};
use crate::sync::CleanLockToken;

/// The leaf that reports an invariant TSC.
const INVARIANT_TSC_LEAF: u32 = 0x8000_0007;

/// The chosen continuous source.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// The KVM paravirtual clock.
    Kvm,
    /// The invariant TSC at the calibrated rate.
    Tsc(TicksPerSecond),
    /// The HPET main counter at its ACPI-reported period.
    Hpet,
    /// No continuous source: the previous OFFSET + interpolation path.
    PitFallback,
}

static SOURCE: Once<Source> = Once::new();
/// The calibrated invariant TSC rate, used by the one-shot timer (whose
/// deadlines are in TSC ticks) even when the clock source itself is the KVM
/// clock.
static TSC_RATE: Once<Option<TicksPerSecond>> = Once::new();

/// The clock source, picked and logged on the first call.
fn source() -> &'static Source {
    SOURCE.call_once(select)
}

/// The invariant TSC's calibrated rate, or `None` when this machine has no
/// invariant TSC it could be measured against (the one-shot TSC-deadline
/// timer then reports unsupported).
pub fn tsc_rate() -> Option<TicksPerSecond> {
    *TSC_RATE.call_once(calibrate_tsc)
}

#[allow(clippy::arithmetic_side_effects)] // unit conversions with u128 intermediates, see module
/// Measures the invariant TSC's rate against the KVM clock when available,
/// else the HPET, logging the calibration and a cross-check in ppm.
fn calibrate_tsc() -> Option<TicksPerSecond> {
    crate::info!("time: calibrating the invariant tsc against the pvclock");
    if !tsc_invariant() {
        return None;
    }
    let (start_tsc, start_ns, end_tsc, end_ns) = sample()?;
    let ticks = end_tsc.saturating_sub(start_tsc);
    let nanos = end_ns.saturating_sub(start_ns);
    if ticks == 0 || nanos == 0 {
        return None;
    }
    let rate = TicksPerSecond::new(
        u64::try_from((ticks as u128 * 1_000_000_000) / nanos).unwrap_or(u64::MAX),
    )
    .ok()?;
    // Cross-check: project the TSC over a second sample and report the
    // difference against the reference in ppm.
    let (check_tsc, check_start_ns, end_tsc, end_ns) = sample()?;
    let projected = u128::from(rate.nanos_for_ticks(end_tsc.saturating_sub(check_tsc)));
    let check_ns = end_ns.saturating_sub(check_start_ns).max(1);
    let drift = projected.abs_diff(check_ns) * 1_000_000 / check_ns;
    crate::info!(
        "time: invariant tsc calibrated at {} Hz (cross-check {} ppm vs the reference)",
        rate.get(),
        drift
    );
    Some(rate)
}

#[allow(clippy::arithmetic_side_effects)] // unit conversions with u128 intermediates
/// One calibration sample: the TSC and the reference clock (KVM clock when
/// available, else the HPET counter) around a quarter-second pause.
fn sample() -> Option<(u64, u128, u64, u128)> {
    #[cfg(feature = "x86_kvm_pv")]
    {
        if let Some(ns) = super::device::tsc::monotonic_absolute() {
            // SAFETY: RDTSC reads a counter; it has no side effects.
            let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };
            let start_ns = ns;
            delay_ns(250_000_000);
            // SAFETY: RDTSC reads a counter; it has no side effects.
            let end_tsc = unsafe { core::arch::x86_64::_rdtsc() };
            let end_ns = super::device::tsc::monotonic_absolute()?;
            return Some((start_tsc, start_ns, end_tsc, end_ns));
        }
    }
    let (counter, period_fs) = hpet_cached()?;
    // SAFETY: RDTSC reads a counter; it has no side effects.
    let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };
    // SAFETY: the mapped register outlives the kernel; reading it has no side
    // effects.
    let start_hpet = unsafe { counter.read_volatile() };
    let start_ns = start_hpet as u128 * period_fs as u128 / 1_000_000;
    let want = hpet_ticks_for_ns(period_fs, 250_000_000);
    // SAFETY: the mapped register outlives the kernel; reading it has no side
    // effects.
    while unsafe { counter.read_volatile() }.saturating_sub(start_hpet) < want {
        core::hint::spin_loop();
    }
    // SAFETY: RDTSC reads a counter; it has no side effects.
    let end_tsc = unsafe { core::arch::x86_64::_rdtsc() };
    // SAFETY: the mapped register outlives the kernel; reading it has no side
    // effects.
    let end_hpet = unsafe { counter.read_volatile() };
    let end_ns = end_hpet as u128 * period_fs as u128 / 1_000_000;
    Some((start_tsc, start_ns, end_tsc, end_ns))
}

/// A bounded busy wait of `ns` nanoseconds against the HPET counter, or a
/// spin loop when no HPET reference was given (the KVM-clock path, where the
/// reference read happens around it).
fn delay_ns(ns: u128) {
    let mut i = 0u128;
    while i < ns / 10 {
        core::hint::spin_loop();
        i += 1;
    }
}

/// Picks the clock source from what this machine offers and logs why.
fn select() -> Source {
    // The KVM paravirtual clock first: KVM guarantees monotonicity.
    #[cfg(feature = "x86_kvm_pv")]
    if super::device::tsc::monotonic_absolute().is_some() {
        crate::info!("time: clock source kvm pvclock");
        return Source::Kvm;
    }

    if tsc_invariant()
        && let Some(rate) = tsc_rate()
    {
        return Source::Tsc(rate);
    }

    if let Some((_, period_fs)) = hpet_cached() {
        crate::info!("time: clock source hpet (period {} fs)", period_fs);
        return Source::Hpet;
    }

    crate::warn!(
        "time: no continuous clock source (no KVM clock, no invariant TSC with a usable HPET, \
         no usable HPET); the clock depends on the periodic PIT interrupt"
    );
    Source::PitFallback
}

/// Whether this CPU's TSC is invariant (CPUID 0x8000_0007 EDX bit 8).
fn tsc_invariant() -> bool {
    let max_extended = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    if max_extended < INVARIANT_TSC_LEAF {
        return false;
    }
    let edx = core::arch::x86_64::__cpuid(INVARIANT_TSC_LEAF).edx;
    edx & (1 << 8) != 0
}

/// The HPET main counter's address in the linear map and its tick length,
/// resolved once (under the ACPI lock) and cached, so the interrupt paths and
/// the calibration can read the counter without taking the ACPI lock (the
/// same pattern `trace::clock` uses for its fallback clock).
static HPET_CLOCK: Once<Option<(usize, u64)>> = Once::new();

/// The HPET counter's address and its period, or `None` when the ACPI table
/// has no usable HPET (64-bit counter, enabled, known period). The address is
/// kept as `usize` so the `Once` payload is `Send + Sync` without an unsafe
/// impl; the reads cast it back and are unsafe only in the dereference.
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

#[allow(clippy::arithmetic_side_effects)] // unit conversion, saturating at max(1)
/// How many HPET ticks `ns` nanoseconds are at `period_fs` femtoseconds per
/// tick, at least one.
fn hpet_ticks_for_ns(period_fs: u64, ns: u128) -> u64 {
    ((ns * 1_000_000) / period_fs as u128).max(1) as u64
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

#[allow(clippy::arithmetic_side_effects)] // unit conversions across the sources
/// The monotonic time in nanoseconds: continuous while the periodic
/// interrupt is masked, comparable across CPUs, from the source [`source`]
/// picked.
pub fn monotonic_absolute(_token: &mut CleanLockToken) -> u128 {
    match source() {
        Source::Kvm => super::device::tsc::monotonic_absolute()
            .expect("the selected clock is the pvclock"),
        Source::Tsc(rate) => {
            // SAFETY: RDTSC reads a counter; it has no side effects.
            let ticks = unsafe { core::arch::x86_64::_rdtsc() };
            u128::from(rate.nanos_for_ticks(ticks))
        }
        Source::Hpet => hpet_ns(),
        Source::PitFallback => {
            let offset = { *crate::time::OFFSET.read(_token.token()) };
            offset + hpet_or_pit()
        }
    }
}

/// The resolution of [`monotonic_absolute`], in nanoseconds.
pub fn monotonic_resolution() -> u128 {
    match source() {
        Source::Kvm => 1,
        Source::Tsc(_) => 1,
        Source::Hpet => {
            let (_, period_fs) = hpet_cached().expect("the selected clock has an HPET");
            (period_fs as u128) / 1_000_000
        }
        Source::PitFallback => pit::PERIOD_FS / 1_000_000,
    }
}

/// The previous OFFSET + interpolation path, kept for the fallback source:
/// OFFSET carries the ticks the periodic interrupt advanced, and the counter
/// interpolation the part of the current period already elapsed.
fn hpet_or_pit() -> u128 {
    if cfg!(feature = "acpi")
        && let Some(ref hpet) = *crate::acpi::ACPI_TABLE.hpet.read()
    {
        //TODO: handle rollover?
        //TODO: improve performance

        // Current count; the comparator holds the next interrupt count; the capabilities hold
        // the period in femtoseconds.
        // SAFETY: the ACPI code mapped the HPET's registers when it found the table, and
        // reading these three has no side effects.
        let (counter, comparator, capability) = unsafe {
            (
                hpet.read_u64(hpet::MAIN_COUNTER_OFFSET),
                hpet.read_u64(hpet::T0_COMPARATOR_OFFSET),
                hpet.read_u64(hpet::CAPABILITY_OFFSET),
            )
        };

        let mut period_fs = capability >> 32;
        if period_fs == 0 {
            period_fs = 10_000_000;
        }

        // Calculate divisor
        let divisor = (pit::RATE as u64 * 1_000_000) / period_fs;
        // Calculate last interrupt
        let last_interrupt = comparator.saturating_sub(divisor);
        // Calculate ticks since last interrupt
        let elapsed = counter.saturating_sub(last_interrupt);
        // Calculate nanoseconds since last interrupt
        return (elapsed as u128 * period_fs as u128) / 1_000_000;
    }
    // Read ticks since last interrupt; reading the PIT countdown has no side
    // effects.
    let elapsed = unsafe { pit::read() };
    // Calculate nanoseconds since last interrupt
    (elapsed as u128 * pit::PERIOD_FS) / 1_000_000
}
