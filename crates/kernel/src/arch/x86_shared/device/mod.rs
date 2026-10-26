use spin::Mutex;

use crate::memory::KernelMapper;
use core::cell::Cell;

/// Ferric Oxide: decoding the local APIC ID register.
pub mod apic_id;
pub mod cpu;
pub mod hpet;
pub mod ioapic;
pub mod local_apic;
// Ferric Oxide: the calibrated TSC and per-CPU one-shot timers (`kernel.oneshot_timer`).
#[cfg(all(target_arch = "x86_64", feature = "oneshot_timer"))]
pub mod oneshot;
pub mod pic;
pub mod pit;
pub mod serial;
#[cfg(feature = "system76_ec_debug")]
pub mod system76_ec;

#[cfg(feature = "x86_kvm_pv")]
pub mod tsc;

pub unsafe fn init() {
    unsafe {
        pic::init();
        local_apic::init(&mut KernelMapper::lock_rw());

        // Run here for the side effect of printing if KVM was used to avoid interleaved logs.
        #[cfg(feature = "x86_kvm_pv")]
        tsc::get_kvm_support();
    }
}
pub unsafe fn init_after_acpi() {
    // this will disable the IOAPIC if needed.
    //ioapic::init(mapper);
}

unsafe fn init_hpet() -> bool {
    if cfg!(not(feature = "acpi")) {
        return false;
    }

    unsafe {
        use crate::acpi::ACPI_TABLE;
        match *ACPI_TABLE.hpet.write() {
            Some(ref mut hpet) => {
                if cfg!(target_arch = "x86") {
                    //TODO: fix HPET on i686
                    crate::warn!("HPET found but implemented on i686");
                    return false;
                }
                hpet::init(hpet)
            }
            _ => false,
        }
    }
}

pub unsafe fn init_noncore() {
    unsafe {
        crate::debug!("Initializing system timer");

        #[cfg(feature = "x86_kvm_pv")]
        if tsc::init() {
            crate::debug!("TSC used as system clock source");
        }

        if init_hpet() {
            crate::debug!("HPET used as system timer");
        } else {
            pit::init();
            crate::debug!("PIT used as system timer");
        }

        #[cfg(all(target_arch = "x86_64", feature = "oneshot_timer"))]
        oneshot::init_bsp();

        crate::debug!("Finished initializing devices");
    }
}

pub unsafe fn init_ap() {
    unsafe {
        local_apic::init_ap();

        #[cfg(feature = "x86_kvm_pv")]
        tsc::init();
    }
}

pub struct ArchPercpuMisc {
    pub apic_id_opt: Cell<Option<local_apic::ApicId>>,
    #[cfg(feature = "x86_kvm_pv")]
    pub tsc_info: tsc::TscPercpu,
    pub idt: Mutex<Option<&'static mut crate::arch::idt::Idt>>,
    /// Ferric Oxide: this CPU's one-shot timer (`kernel.oneshot_timer`).
    #[cfg(all(target_arch = "x86_64", feature = "oneshot_timer"))]
    pub oneshot: oneshot::OneShotPercpu,
}

impl ArchPercpuMisc {
    pub const fn default() -> Self {
        Self {
            apic_id_opt: Cell::new(None),
            #[cfg(feature = "x86_kvm_pv")]
            tsc_info: tsc::TscPercpu::default(),
            idt: Mutex::new(None),
            #[cfg(all(target_arch = "x86_64", feature = "oneshot_timer"))]
            oneshot: oneshot::OneShotPercpu::default(),
        }
    }
}
