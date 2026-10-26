//! <https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/software-developers-hpet-spec-1-0a.pdf>

use super::pit;
use crate::acpi::hpet::Hpet;
use core::time::Duration;

const LEG_RT_CNF: u64 = 2;
const ENABLE_CNF: u64 = 1;

const TN_VAL_SET_CNF: u64 = 0x40;
const TN_TYPE_CNF: u64 = 0x08;
const TN_INT_ENB_CNF: u64 = 0x04;

pub(crate) const CAPABILITY_OFFSET: usize = 0x00;
const GENERAL_CONFIG_OFFSET: usize = 0x10;
const GENERAL_INTERRUPT_OFFSET: usize = 0x20;
pub(crate) const MAIN_COUNTER_OFFSET: usize = 0xF0;
// const NUM_TIMER_CAP_MASK: u64 = 0x0f00;
const LEG_RT_CAP: u64 = 0x8000;
const T0_CONFIG_CAPABILITY_OFFSET: usize = 0x100;
pub(crate) const T0_COMPARATOR_OFFSET: usize = 0x108;

const PER_INT_CAP: u64 = 0x10;

pub unsafe fn init(hpet: &mut Hpet) -> bool {
    unsafe {
        crate::debug!("HPET @ {:#x}", { hpet.base_address.address });
        debug_caps(hpet);

        crate::trace!("HPET Before Init");
        debug_config(hpet);

        // Disable HPET
        {
            let mut config_word = hpet.read_u64(GENERAL_CONFIG_OFFSET);
            config_word &= !(LEG_RT_CNF | ENABLE_CNF);
            hpet.write_u64(GENERAL_CONFIG_OFFSET, config_word);
        }

        let capability = hpet.read_u64(CAPABILITY_OFFSET);
        if capability & LEG_RT_CAP == 0 {
            crate::warn!("HPET missing capability LEG_RT_CAP");
            return false;
        }

        let period_fs = capability >> 32;
        let divisor = (pit::RATE as u64 * 1_000_000) / period_fs;

        let t0_capabilities = hpet.read_u64(T0_CONFIG_CAPABILITY_OFFSET);
        if t0_capabilities & PER_INT_CAP == 0 {
            crate::warn!("HPET T0 missing capability PER_INT_CAP");
            return false;
        }

        let counter = hpet.read_u64(MAIN_COUNTER_OFFSET);

        let t0_config_word: u64 = TN_VAL_SET_CNF | TN_TYPE_CNF | TN_INT_ENB_CNF;
        hpet.write_u64(T0_CONFIG_CAPABILITY_OFFSET, t0_config_word);
        // set accumulator value
        hpet.write_u64(T0_COMPARATOR_OFFSET, counter + divisor);
        // set interval
        hpet.write_u64(T0_COMPARATOR_OFFSET, divisor);

        // Enable interrupts from the HPET
        {
            let mut config_word: u64 = hpet.read_u64(GENERAL_CONFIG_OFFSET);
            config_word |= LEG_RT_CNF | ENABLE_CNF;
            hpet.write_u64(GENERAL_CONFIG_OFFSET, config_word);
        }

        crate::trace!("HPET After Init");
        debug_config(hpet);

        true
    }
}

unsafe fn debug_caps(hpet: &mut Hpet) {
    unsafe {
        let capability = hpet.read_u64(CAPABILITY_OFFSET);
        crate::trace!("  caps: {:#x}", capability);
        crate::trace!(
            "    clock period: {:?}",
            Duration::from_nanos((capability >> 32) / 1_000_000)
        );
        crate::trace!(
            "    ID: {:#x} revision: {}",
            (capability >> 16) as u16,
            capability as u8
        );
        crate::trace!(
            "    LEG_RT_CAP: {} COUNT_SIZE_CAP: {}",
            capability & (1 << 15) == (1 << 15),
            capability & (1 << 13) == (1 << 13)
        );
        // The NUM_TIM_CAP field contains the index of the last timer.
        // Add 1 to get the amount of timers.
        crate::trace!("    timers: {}", ((capability >> 8) as u8 & 0x1F) + 1);

        let t0_capabilities = hpet.read_u64(T0_CONFIG_CAPABILITY_OFFSET);
        crate::trace!(
            "  T0 interrupt routing: {:#x}",
            (t0_capabilities >> 32) as u32
        );
    }
}

unsafe fn debug_config(hpet: &mut Hpet) {
    unsafe {
        let config_word = hpet.read_u64(GENERAL_CONFIG_OFFSET);
        crate::trace!("  config: {:#x}", config_word);

        let interrupt_status = hpet.read_u64(GENERAL_INTERRUPT_OFFSET);
        crate::trace!("  interrupt status: {:#x}", interrupt_status);

        let counter = hpet.read_u64(MAIN_COUNTER_OFFSET);
        crate::trace!("  counter: {:#x}", counter);

        let t0_capabilities = hpet.read_u64(T0_CONFIG_CAPABILITY_OFFSET);
        crate::trace!("  T0 flags: {:#x}", t0_capabilities as u32);

        let t0_comparator = hpet.read_u64(T0_COMPARATOR_OFFSET);
        crate::trace!("  T0 comparator: {:#x}", t0_comparator);
    }
}

/// Ferric Oxide: bit 13 of the capabilities register, COUNT_SIZE_CAP: the main counter is 64
/// bits wide.
#[cfg(any(feature = "trace", feature = "time_one_shot"))]
const COUNT_SIZE_CAP: u64 = 1 << 13;

/// Ferric Oxide: the HPET's main counter, where both `kernel.trace`'s fallback clock
/// (`trace::clock`, through [`trace_counter`]) and the TSC calibration
/// (`kernel.time.one_shot`, `arch::time`) read it without the ACPI table's lock.
#[cfg(all(
    any(feature = "trace", feature = "time_one_shot"),
    target_arch = "x86_64"
))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct MainCounter {
    /// The main counter in the kernel's linear map, which stays mapped for the kernel's life.
    pub(crate) counter: *const u64,
    /// The counter's tick in femtoseconds.
    pub(crate) period_fs: u64,
    /// The counter is 64 bits wide; otherwise only the low 32 bits count, and they wrap
    /// within minutes.
    pub(crate) wide: bool,
}

/// Ferric Oxide: the HPET's main counter, when the HPET is enabled and reports a period.
#[cfg(all(
    any(feature = "trace", feature = "time_one_shot"),
    target_arch = "x86_64"
))]
pub(crate) fn main_counter(hpet: &Hpet) -> Option<MainCounter> {
    // SAFETY: the ACPI code mapped the HPET's registers when it found the table; reading the
    // capabilities and configuration registers has no side effects.
    let (capability, config) = unsafe {
        (
            hpet.read_u64(CAPABILITY_OFFSET),
            hpet.read_u64(GENERAL_CONFIG_OFFSET),
        )
    };
    let period_fs = capability >> 32;
    let address = hpet.base_address.address as usize + MAIN_COUNTER_OFFSET;
    let counter = <crate::memory::RmmA as crate::memory::RmmArch>::phys_to_virt(
        crate::memory::PhysicalAddress::new(address),
    );
    (config & ENABLE_CNF != 0 && period_fs != 0).then_some(MainCounter {
        counter: counter.data() as *const u64,
        period_fs,
        wide: capability & COUNT_SIZE_CAP != 0,
    })
}

/// Ferric Oxide: the HPET's main counter as a clock (`kernel.trace`'s fallback clock in
/// `trace::clock`, and a `kernel.time.one_shot` clock source in `arch::time`): its address
/// in the kernel's linear map and its tick length in femtoseconds, when the HPET is enabled
/// and the counter is 64 bits wide (a 32-bit one wraps within minutes).
#[cfg(all(
    any(feature = "trace", feature = "time_one_shot"),
    target_arch = "x86_64"
))]
pub(crate) fn trace_counter(hpet: &Hpet) -> Option<(*const u64, u64)> {
    main_counter(hpet)
        .filter(|main| main.wide)
        .map(|main| (main.counter, main.period_fs))
}
