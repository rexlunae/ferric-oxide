use crate::info;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use ferric_devicetree::Tree as Fdt;

pub mod cpu;
pub mod generic_timer;
pub mod irqchip;
pub mod psci;
pub mod rtc;
pub mod serial;

use crate::dtb::irqchip::IRQ_CHIP;
use irqchip::ic_for_chip;

pub static ROOT_IC_IDX: AtomicUsize = AtomicUsize::new(0);
pub static ROOT_IC_IDX_IS_SET: AtomicUsize = AtomicUsize::new(0);

unsafe fn init_root_ic(fdt: &Fdt) {
    unsafe {
        let is_set = ROOT_IC_IDX_IS_SET.load(Ordering::Relaxed);
        if is_set != 0 {
            let ic_idx = ROOT_IC_IDX.load(Ordering::Relaxed);
            crate::info!("Already selected {} as root ic", ic_idx);
            return;
        }

        let root_irqc_phandle = fdt
            .root()
            .property("interrupt-parent")
            .unwrap()
            .as_usize()
            .unwrap();
        let ic_idx = IRQ_CHIP
            .phandle_to_ic_idx(root_irqc_phandle as u32)
            .unwrap();
        crate::info!("select {} as root ic", ic_idx);
        ROOT_IC_IDX.store(ic_idx, Ordering::Release);
        ROOT_IC_IDX_IS_SET.store(1, Ordering::Release);
    }
}

pub unsafe fn init_devicetree(fdt: &Fdt) {
    unsafe {
        crate::info!("PSCI INIT");
        psci::init(fdt);
        crate::info!("CPU TOPOLOGY INIT");
        cpu::init_topology(fdt);
        crate::info!("IRQCHIP INIT");
        crate::dtb::irqchip::init(&fdt);
        init_root_ic(&fdt);
        crate::info!("GIT INIT");
        generic_timer::init(fdt);
        crate::info!("SERIAL INIT");
        serial::init(fdt);
        crate::info!("RTC INIT");
        rtc::init(fdt);
    }
}

pub struct ArchPercpuMisc {
    pub gic_target_mask: AtomicU8,
}

impl ArchPercpuMisc {
    pub const fn default() -> Self {
        Self {
            gic_target_mask: AtomicU8::new(0),
        }
    }
}
