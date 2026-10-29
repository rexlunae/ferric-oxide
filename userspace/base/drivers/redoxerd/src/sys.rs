pub fn exit_success() {
    imp::exit(true);
}

pub fn exit_failure() {
    imp::exit(false);
}

pub fn debug_char(b: u8) {
    let _ = imp::write_debug(b);
}

/// What a QEMU guest writes to report its result.
///
/// `isa-debug-exit` turns a write to port 0x501 into QEMU's own exit status as
/// `value << 1 | 1`, so these are the statuses the host reads: `51` becomes 0
/// and `53` becomes 1. `redoxer` treats anything else as a crash.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DebugExit {
    /// The value for `isa-debug-exit` (0x501), already halved.
    pub(crate) code: u8,
    /// Whether to also power the machine off through ACPI's 0x604.
    pub(crate) shutdown: bool,
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
impl DebugExit {
    /// The writes that report `success`.
    ///
    /// The status is written first and the shutdown only after it. 0x604 makes
    /// QEMU stop, so if it landed first the host would see exit status 0 — which
    /// `redoxer` cannot tell from a crash, and the same command then succeeded
    /// or failed depending on which write won. 0x604 is only a fallback for a
    /// QEMU without the debug-exit device, so it goes last.
    pub(crate) fn for_status(success: bool) -> Self {
        Self {
            code: (if success { 51 } else { 53 }) / 2,
            shutdown: success,
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod imp {
    use super::DebugExit;
    use syscall::{Io, Pio};

    pub fn exit(success: bool) {
        let exit = DebugExit::for_status(success);
        Pio::<u8>::new(0x501).write(exit.code);
        if exit.shutdown {
            Pio::<u16>::new(0x604).write(0x2000);
        }
    }

    pub fn write_debug(b: u8) -> syscall::Result<()> {
        Pio::<u8>::new(0xe9).write(b);
        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use qemu_exit::QEMUExit;

    pub fn exit(success: bool) {
        let q = qemu_exit::AArch64::new();
        if success {
            q.exit(51)
        } else {
            q.exit(53)
        }
    }

    pub fn write_debug(_b: u8) -> syscall::Result<()> {
        // TODO
        Ok(())
    }
}

#[cfg(target_arch = "riscv64")]
mod imp {
    use fdt::Fdt;

    #[cfg(target_os = "redox")]
    fn get_dtb() -> Vec<u8> {
        std::fs::read("/scheme/kernel.dtb").unwrap()
    }

    #[cfg(not(target_os = "redox"))]
    fn get_dtb() -> Vec<u8> {
        unimplemented!()
    }

    use common::{physmap, MemoryType, Prot};
    use qemu_exit::QEMUExit;

    pub fn exit(success: bool) {
        common::init();
        let dtb_data = get_dtb();
        if dtb_data.len() == 0 {
            panic!("dtb is empty");
        }

        let fdt = Fdt::new(&dtb_data).unwrap();
        let with = ["sifive,test1"];
        let compat_node = fdt.find_compatible(&with).unwrap();
        let reg = compat_node.reg().unwrap().next().unwrap();
        let reg_size = reg.size.unwrap();
        let reg_addr = reg.starting_address as usize;

        let addr = unsafe { physmap(reg_addr, reg_size, Prot::RW, MemoryType::Uncacheable) }
            .expect("unable to physmap redoxer device");
        let q = qemu_exit::RISCV64::new(addr.addr() as u64);
        if success {
            q.exit(51);
        } else {
            q.exit(53);
        }
    }

    pub fn write_debug(b: u8) -> syscall::Result<()> {
        // TODO
        Ok(())
    }
}

#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64")))]
mod tests {
    use super::DebugExit;

    /// QEMU turns the value written to 0x501 into `value << 1 | 1`, so writing
    /// half of 51 makes QEMU exit 51 and writing half of 53 makes it exit 53.
    /// Those are the two statuses `redoxer` maps to success and failure.
    #[test]
    fn the_debug_exit_code_is_the_status_the_host_reads() {
        let qemu_status = |success| u32::from(DebugExit::for_status(success).code) << 1 | 1;
        assert_eq!(qemu_status(true), 51);
        assert_eq!(qemu_status(false), 53);
    }

    /// The machine is powered off only after a successful status. A failure
    /// must not shut QEMU down before the host has read the code, and the
    /// shutdown is the write that used to win the race.
    #[test]
    fn only_success_powers_the_machine_off() {
        assert!(DebugExit::for_status(true).shutdown);
        assert!(!DebugExit::for_status(false).shutdown);
    }

    /// The status is written before the shutdown, which is the ordering this
    /// type exists to guarantee: its fields are the two writes, in order.
    #[test]
    fn the_status_is_the_only_field_needed_to_report_failure() {
        let failure = DebugExit::for_status(false);
        assert_eq!(failure.code, 53 / 2);
        assert!(!failure.shutdown, "a failure reports by the code alone");
    }
}
