//! Initialize port-I/O debug output before memory, then optional LPSS MMIO after it.

use crate::{
    devices::{serial::SerialKind, uart_16550::SerialPort},
    memory::map_device_memory,
    syscall::io::{MmioPtr, Pio},
};
use spin::Mutex;

/// First legacy port-I/O debug UART.
pub static COM1: Mutex<SerialKind> = Mutex::new(SerialKind::NotPresent);
/// Second legacy port-I/O debug UART.
pub static COM2: Mutex<SerialKind> = Mutex::new(SerialKind::NotPresent);

/// Optional LPSS debug UART, installed after kernel memory initialization.
pub static LPSS: Mutex<SerialKind> = Mutex::new(SerialKind::NotPresent);

/// Probe port-I/O debug outputs without allocating or changing page tables.
///
/// Runs once on the bootstrap processor before memory initialization.
pub fn init() {
    #[cfg(feature = "system76_ec_debug")]
    super::system76_ec::init();

    if cfg!(not(feature = "serial_debug")) {
        // FIXME remove serial_debug feature once ACPI SPCR is respected on UEFI boots.
        return;
    }

    let mut com1 = SerialPort::<Pio<u8>>::new(0x3F8);
    if com1.init().is_ok() {
        *COM1.lock() = SerialKind::Ns16550Pio(com1);
    }
    let mut com2 = SerialPort::<Pio<u8>>::new(0x2F8);
    if com2.init().is_ok() {
        *COM2.lock() = SerialKind::Ns16550Pio(com2);
    }
}

/// Probe the optional LPSS debug UART using the kernel's final physical mapping.
///
/// # Panics
///
/// Panics if the kernel cannot allocate the UART mapping's page tables.
///
/// # Safety
///
/// Call once on the bootstrap processor after the kernel mapper and frame
/// allocator are initialized, before starting other processors.
pub unsafe fn init_lpss_after_memory() {
    // FIXME remove explicit LPSS handling once ACPI SPCR is supported
    if cfg!(not(all(feature = "serial_debug", feature = "lpss_debug"))) {
        return;
    }

    // SAFETY: The caller has initialized the kernel mapper and allocator. Map the
    // UART only now: the bootloader's huge-page map cannot accept this 4 KiB map.
    let virt = unsafe {
        map_device_memory(
            // TODO: Make this configurable
            crate::memory::PhysicalAddress::new(0xFE032000),
            4,
        )
    };

    // SAFETY: The device mapping above covers the UART's register page, and its
    // 32-bit registers use the spacing expected by this SerialPort implementation.
    let mut lpss = unsafe { SerialPort::<MmioPtr<u32>>::new(virt.data()) };
    if lpss.init().is_ok() {
        *LPSS.lock() = SerialKind::Ns16550u32(lpss);
    }
}
