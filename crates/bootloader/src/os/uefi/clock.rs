//! Samples the UEFI RTC before boot services end, paired with the ARM counter.

use ferric_boot_time::{BootTime, FirmwareTime};
use uefi::{system::SystemTable, time::Time};

pub(super) fn sample(st: &SystemTable) -> Option<BootTime> {
    let mut time = Time::default();
    if let Err(err) = super::status_to_result((st.RuntimeServices.GetTime)(
        &mut time,
        core::ptr::null_mut(),
    )) {
        log::warn!("UEFI GetTime failed: {err:?}");
        return None;
    }
    let ticks: u64;
    let frequency: u64;
    // SAFETY: UEFI AArch64 runs at EL1 or EL2 with the physical counter enabled.
    // This is the same counter the kernel uses; ISB orders the time sample.
    unsafe {
        core::arch::asm!(
            "isb",
            "mrs {ticks}, cntpct_el0",
            "mrs {frequency}, cntfrq_el0",
            ticks = out(reg) ticks,
            frequency = out(reg) frequency,
            options(nostack, preserves_flags),
        );
    }
    if frequency == 0 {
        log::warn!("UEFI clock sample has zero counter frequency");
        return None;
    }
    let firmware = FirmwareTime {
        year: time.Year,
        month: time.Month,
        day: time.Day,
        hour: time.Hour,
        minute: time.Minute,
        second: time.Second,
        nanosecond: time.Nanosecond,
        // The upstream binding declares UINT16, but EFI_TIME specifies INT16.
        timezone: time.TimeZone as i16,
        daylight: time.Daylight,
    };
    let monotonic_ns = u128::from(ticks) * 1_000_000_000 / u128::from(frequency);
    match firmware
        .unix_nanos()
        .and_then(|ns| BootTime::from_sample(ns, monotonic_ns))
    {
        Ok(clock) => Some(clock),
        Err(err) => {
            log::warn!("Ignoring invalid UEFI clock: {err}");
            None
        }
    }
}
