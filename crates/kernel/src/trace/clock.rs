//! Ferric Oxide (`kernel.trace`, roadmap T3d): the clock behind record timestamps.
//!
//! [`choice`] picks the clock once, the first time it is asked (when `kmain` makes the
//! rings, before any record is written), with `ferric_trace::clock::choose`: the TSC when
//! CPUID `0x8000_0007` says it is invariant, otherwise the KVM paravirtual clock, otherwise
//! a 64-bit HPET main counter, otherwise the TSC anyway with a warning. [`now`] reads it
//! for each record. The `kernel.trace` scheme's `clock` file serves the choice, so decoders
//! know what a timestamp counts.
//!
//! [`now`] runs inside tracepoints, with interrupts off, so none of the readers takes a lock
//! or logs: the KVM clock is read with `tsc::pvclock_ns`, which keeps no state, and the HPET
//! counter through the address found at selection.

use core::{
    ptr,
    sync::atomic::{AtomicPtr, AtomicU8, Ordering},
};

use ferric_trace::clock::{self, Choice, Clock, Offered, INVARIANT_TSC_LEAF};
use spin::Once;

/// The clock [`choice`] picked, once it has.
static CHOICE: Once<Choice> = Once::new();

/// [`CHOICE`]'s clock as a `Clock::raw`, for [`now`]. The TSC until the choice is made, which
/// happens before the first ring exists, so no record is written with another clock.
static CLOCK: AtomicU8 = AtomicU8::new(Clock::Tsc.raw());

/// The HPET main counter in the kernel's linear map, when [`CHOICE`] is the HPET.
static HPET_COUNTER: AtomicPtr<u64> = AtomicPtr::new(ptr::null_mut());

/// The timestamp clock, picked and logged on the first call.
pub fn choice() -> Choice {
    *CHOICE.call_once(select)
}

/// The clock timestamps come from now, without picking one: [`choice`]'s line once it has
/// run, and the TSC with an unknown period before, which is what [`now`] reads until then.
/// The panic handler uses it for the crash dump, where picking and logging is not wanted.
pub fn current() -> clock::ClockLine {
    CHOICE.get().map_or(
        clock::ClockLine {
            clock: Clock::Tsc,
            period_fs: None,
        },
        |choice| choice.line(),
    )
}

/// Picks the clock from what this machine offers and logs why.
fn select() -> Choice {
    let max_extended = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    let edx = if max_extended >= INVARIANT_TSC_LEAF {
        core::arch::x86_64::__cpuid(INVARIANT_TSC_LEAF).edx
    } else {
        0
    };
    let hpet = hpet_counter();
    let choice = clock::choose(Offered {
        invariant_tsc: clock::invariant_tsc(max_extended, edx),
        kvm_clock: kvm_clock(),
        hpet_period_fs: hpet.map(|(_, period_fs)| period_fs),
    });
    if let (Clock::Hpet, Some((counter, _))) = (choice.clock, hpet) {
        HPET_COUNTER.store(counter.cast_mut(), Ordering::Relaxed);
    }
    CLOCK.store(choice.clock.raw(), Ordering::Release);
    if choice.reliable {
        crate::info!("trace: clock {} ({})", choice.clock, choice.reason());
    } else {
        crate::warn!("trace: clock {} ({})", choice.clock, choice.reason());
    }
    choice
}

/// Whether this CPU has a KVM paravirtual clock. The other CPUs set theirs up as they start,
/// under the same KVM feature check.
fn kvm_clock() -> bool {
    #[cfg(feature = "x86_kvm_pv")]
    return crate::arch::device::tsc::pvclock_ns().is_some();
    #[cfg(not(feature = "x86_kvm_pv"))]
    false
}

/// The HPET main counter and its tick length, when it can serve as the clock.
fn hpet_counter() -> Option<(*const u64, u64)> {
    if cfg!(not(feature = "acpi")) {
        return None;
    }
    let hpet = crate::acpi::ACPI_TABLE.hpet.read();
    crate::arch::device::hpet::trace_counter(hpet.as_ref()?)
}

/// The timestamp for a record now, from the chosen clock.
pub fn now() -> u64 {
    match Clock::from_raw(CLOCK.load(Ordering::Relaxed)) {
        Some(Clock::KvmClock) => kvm_ns(),
        Some(Clock::Hpet) => hpet_ticks(),
        _ => tsc(),
    }
}

/// The TSC.
fn tsc() -> u64 {
    crate::arch::misc::rdtsc()
}

/// The KVM clock in nanoseconds. Every CPU sets its clock up before it runs anything that
/// could record, so the zero for a CPU without one is not expected.
fn kvm_ns() -> u64 {
    #[cfg(feature = "x86_kvm_pv")]
    return crate::arch::device::tsc::pvclock_ns()
        .map_or(0, |ns| u64::try_from(ns).unwrap_or(u64::MAX));
    #[cfg(not(feature = "x86_kvm_pv"))]
    tsc()
}

/// The HPET main counter, in its ticks.
fn hpet_ticks() -> u64 {
    let counter = HPET_COUNTER.load(Ordering::Relaxed);
    if counter.is_null() {
        return 0;
    }
    // SAFETY: `counter` is the HPET's 64-bit main counter in the linear map, which stays mapped
    // for the kernel's life (`device::hpet::trace_counter`); reading it has no side effects.
    unsafe { counter.read_volatile() }
}
