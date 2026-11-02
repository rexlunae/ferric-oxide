//! Ferric Oxide: preset boot environment lines from QEMU's firmware configuration device
//! (the `fw_cfg_env` feature; x86_64 UEFI only).
//!
//! A harness starts QEMU with `-fw_cfg name=opt/org.ferric-oxide/boot-env,file=<path>`, and
//! the lines of that file join the boot environment before the resolution menu, as if typed
//! into the editor. An unattended boot can so pick the trace-mode kernel
//! (`FERRIC_KERNEL=trace`), skip the menu (`FERRIC_BOOT_RESOLUTION=auto`) or set any other
//! key, with or without a display.
//!
//! The fw_cfg ports are touched only when CPUID says a hypervisor runs the bootloader, and
//! read past the signature only when it is QEMU's. On real hardware nothing is probed.

use core::arch::x86_64::__cpuid;
use core::str;

use ferric_abi::boot_env::{EnvBuf, FW_CFG_FILE, MAX_PRESET_BYTES};
use ferric_fwcfg::{DATA_PORT, FwCfg, SELECTOR_PORT, Transport};

/// CPUID leaf 1's ECX bit that hypervisors set for their guests and processors leave clear.
const HYPERVISOR_BIT: u32 = 1 << 31;

/// QEMU's fw_cfg registers, reached through x86 I/O ports. Only [`load_preset`] makes one,
/// after CPUID reported a hypervisor.
struct Ports;

impl Transport for Ports {
    fn select(&mut self, key: u16) {
        // SAFETY: a `Ports` exists only under a hypervisor, where port 0x510 is QEMU's fw_cfg
        // selector or unassigned. Writing it selects an item and touches no memory.
        unsafe { x86::io::outw(SELECTOR_PORT, key) }
    }

    fn read(&mut self, buf: &mut [u8]) {
        for byte in buf {
            // SAFETY: as in `select`, port 0x511 is QEMU's fw_cfg data register or unassigned.
            // Reading it advances through the selected item and touches no memory.
            *byte = unsafe { x86::io::inb(DATA_PORT) };
        }
    }
}

/// Adds the lines of the fw_cfg file [`FW_CFG_FILE`] to `preset`, when QEMU runs the
/// bootloader and was given the file. A file over [`MAX_PRESET_BYTES`] or not UTF-8 is
/// ignored, and so is each line that is not `KEY=VALUE`, with a warning.
pub fn load_preset(preset: &mut EnvBuf<'_>) {
    if __cpuid(1).ecx & HYPERVISOR_BIT == 0 {
        return;
    }
    let Some(mut fw_cfg) = FwCfg::probe(Ports) else {
        return;
    };
    let file = match fw_cfg.find(FW_CFG_FILE) {
        Ok(Some(file)) => file,
        Ok(None) => return,
        Err(err) => {
            log::warn!("fw_cfg: {err}; no preset boot environment");
            return;
        }
    };
    let mut buf = [0; MAX_PRESET_BYTES];
    let text = match fw_cfg.read_file(&file, &mut buf) {
        Ok(text) => text,
        Err(err) => {
            log::warn!("fw_cfg {FW_CFG_FILE}: {err}; ignored");
            return;
        }
    };
    let Ok(text) = str::from_utf8(text) else {
        log::warn!("fw_cfg {FW_CFG_FILE}: not UTF-8; ignored");
        return;
    };
    let added = preset.merge(text.as_bytes(), |number, line| {
        // Lines of UTF-8 text split at newlines are UTF-8.
        let line = str::from_utf8(line).unwrap_or_default();
        log::warn!("fw_cfg {FW_CFG_FILE}:{number}: not KEY=VALUE, ignored: {line}");
    });
    match added {
        Ok(added) => log::info!("fw_cfg {FW_CFG_FILE}: {added} boot environment lines"),
        Err(err) => log::warn!("fw_cfg {FW_CFG_FILE}: {err}; the rest is ignored"),
    }
}
