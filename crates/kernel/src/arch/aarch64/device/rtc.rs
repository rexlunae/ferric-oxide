use crate::{dtb::translate_mmio_address, sync::CleanLockToken, time};
use core::ptr::read_volatile;

static RTC_DR: usize = 0x000;

pub unsafe fn init(fdt: &ferric_devicetree::Tree) {
    if let Some(node) = fdt.find_compatible(&["arm,pl031"]) {
        match node
            .reg()
            .and_then(|mut iter| iter.next())
            .and_then(|region| translate_mmio_address(fdt, &node, &region))
        {
            Some(phys) => {
                let mut rtc = Pl031rtc { phys };
                let mut token = unsafe { CleanLockToken::new() };
                // `realtime` adds the counter to START, so the anchor is the
                // RTC minus the counter read beside it. Storing the RTC epoch
                // alone would count the counter's uptime a second time.
                let counter = crate::arch::time::monotonic_absolute(&mut token);
                let seconds = rtc.time();
                let rtc_ns = (seconds as u128) * time::NANOS_PER_SEC;
                crate::info!("PL031 RTC at {:#x}, wall clock {seconds} s", rtc.phys);
                *time::START.lock(token.token()) = rtc_ns.saturating_sub(counter);
            }
            None => {
                crate::warn!("No PL031 RTC registers");
            }
        }
    } else {
        crate::warn!("No PL031 RTC found");
    }
}

struct Pl031rtc {
    pub phys: usize,
}

impl Pl031rtc {
    unsafe fn read(&self, reg: usize) -> u32 {
        unsafe { read_volatile((crate::PHYS_OFFSET + self.phys + reg) as *const u32) }
    }

    pub fn time(&mut self) -> u64 {
        let seconds = unsafe { self.read(RTC_DR) } as u64;
        seconds
    }
}
