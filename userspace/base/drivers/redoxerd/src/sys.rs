pub fn exit_success() {
    imp::exit(true);
}

pub fn exit_failure() {
    imp::exit(false);
}

pub fn debug_char(b: u8) {
    let _ = imp::write_debug(b);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod imp {
    use syscall::{Io, Pio};

    pub fn exit(success: bool) {
        // `isa-debug-exit` on 0x501 is what reports the status to the caller:
        // QEMU exits with `code << 1 | 1`, so 51 means success. Write it first.
        //
        // 0x604 is ACPI's sleep-and-shutdown port. Writing it makes the machine
        // power off, and if it lands before the debug-exit write QEMU halts
        // with status 0, which redoxer cannot tell from a crash — the same
        // command then succeeded or failed depending on the race. It is only a
        // fallback for a QEMU whose debug-exit device is absent, so write the
        // status first and let the shutdown follow.
        let status = if success { 51 } else { 53 };
        Pio::<u8>::new(0x501).write(status / 2);
        if success {
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
