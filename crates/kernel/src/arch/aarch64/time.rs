//! ARM physical-counter timekeeping and the optional UEFI wall-clock anchor.

use crate::{sync::CleanLockToken, time::NANOS_PER_SEC};

/// Initializes realtime from the bootloader's UEFI clock/counter sample.
/// A missing or invalid handoff leaves the device-tree RTC fallback intact.
pub(crate) fn init_boot_time(env: &[u8], token: &mut CleanLockToken) {
    match ferric_boot_time::BootTime::from_env(env) {
        Ok(Some(clock)) => {
            *crate::time::START.lock(token.token()) = clock.offset_ns();
            info!("Wall clock initialized from UEFI RTC");
        }
        Ok(None) => {
            warn!("No UEFI clock handoff; relying on device-tree RTC if present");
        }
        Err(err) => {
            warn!("Ignoring invalid UEFI clock handoff: {err}");
        }
    }
}

pub fn monotonic_absolute(_token: &mut CleanLockToken) -> u128 {
    //TODO: aarch64 generic timer counter
    let ticks: usize;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) ticks) };
    let freq: usize;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq) };

    ticks as u128 * NANOS_PER_SEC / freq as u128
}

pub fn monotonic_resolution() -> u128 {
    let freq: usize;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq) };

    NANOS_PER_SEC / freq as u128
}
