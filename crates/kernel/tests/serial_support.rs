//! Mock UARTs and memory readiness for the actual x86 serial initialization module.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

#[derive(Debug, PartialEq)]
enum Probe {
    #[cfg(feature = "system76_ec_debug")]
    EmbeddedController,
    Port(u16),
    Map(usize, usize),
    Mmio(usize),
}

static PROBES: Mutex<Vec<Probe>> = Mutex::new(Vec::new());
static MEMORY_READY: AtomicBool = AtomicBool::new(false);
const UART_VIRTUAL: usize = 0x1000_0000;

pub(crate) mod memory {
    pub(crate) struct PhysicalAddress(usize);

    impl PhysicalAddress {
        pub(crate) fn new(address: usize) -> Self {
            Self(address)
        }
    }

    pub(crate) struct VirtualAddress(usize);

    impl VirtualAddress {
        pub(crate) fn data(&self) -> usize {
            self.0
        }
    }

    pub(crate) unsafe fn map_device_memory(address: PhysicalAddress, len: usize) -> VirtualAddress {
        assert!(
            super::MEMORY_READY.load(super::Ordering::Relaxed),
            "device mapping attempted before kernel memory initialization"
        );
        super::PROBES
            .lock()
            .unwrap()
            .push(super::Probe::Map(address.0, len));
        VirtualAddress(super::UART_VIRTUAL)
    }
}

pub(crate) mod syscall {
    pub(crate) mod io {
        pub(crate) struct Pio<T>(core::marker::PhantomData<T>);
        pub(crate) struct MmioPtr<T>(core::marker::PhantomData<T>);
    }
}

#[cfg(feature = "system76_ec_debug")]
pub(crate) mod system76_ec {
    pub(crate) fn init() {
        super::PROBES
            .lock()
            .unwrap()
            .push(super::Probe::EmbeddedController);
    }
}

pub(crate) mod devices {
    pub(crate) mod uart_16550 {
        use crate::syscall::io::{MmioPtr, Pio};
        use core::marker::PhantomData;

        pub(crate) struct SerialPort<T> {
            pub(crate) address: usize,
            kind: PhantomData<T>,
        }

        impl SerialPort<Pio<u8>> {
            pub(crate) fn new(address: u16) -> Self {
                super::super::PROBES
                    .lock()
                    .unwrap()
                    .push(super::super::Probe::Port(address));
                Self {
                    address: usize::from(address),
                    kind: PhantomData,
                }
            }
        }

        impl SerialPort<MmioPtr<u32>> {
            pub(crate) unsafe fn new(address: usize) -> Self {
                super::super::PROBES
                    .lock()
                    .unwrap()
                    .push(super::super::Probe::Mmio(address));
                Self {
                    address,
                    kind: PhantomData,
                }
            }
        }

        impl<T> SerialPort<T> {
            pub(crate) fn init(&mut self) -> Result<(), core::convert::Infallible> {
                Ok(())
            }
        }
    }

    pub(crate) mod serial {
        use super::uart_16550::SerialPort;
        use crate::syscall::io::{MmioPtr, Pio};

        pub(crate) enum SerialKind {
            NotPresent,
            Ns16550Pio(SerialPort<Pio<u8>>),
            Ns16550u32(SerialPort<MmioPtr<u32>>),
        }
    }
}

#[test]
fn early_serial_uses_only_ports_and_lpss_waits_for_kernel_memory() {
    use crate::x86_serial;
    use devices::serial::SerialKind;

    x86_serial::init();
    let mut expected = Vec::new();
    #[cfg(feature = "system76_ec_debug")]
    expected.push(Probe::EmbeddedController);
    expected.extend([Probe::Port(0x3f8), Probe::Port(0x2f8)]);
    assert_eq!(*PROBES.lock().unwrap(), expected);
    assert!(matches!(*x86_serial::LPSS.lock(), SerialKind::NotPresent));
    for (port, expected_address) in [(&x86_serial::COM1, 0x3f8), (&x86_serial::COM2, 0x2f8)] {
        let guard = port.lock();
        let SerialKind::Ns16550Pio(uart) = &*guard else {
            panic!("early port was not installed");
        };
        assert_eq!(uart.address, expected_address);
    }

    MEMORY_READY.store(true, Ordering::Relaxed);
    // SAFETY: The mock mapper and allocator are now ready; this is the sole probe.
    unsafe { x86_serial::init_lpss_after_memory() };
    expected.extend([Probe::Map(0xfe03_2000, 4), Probe::Mmio(UART_VIRTUAL)]);
    assert_eq!(*PROBES.lock().unwrap(), expected);
    let guard = x86_serial::LPSS.lock();
    let SerialKind::Ns16550u32(uart) = &*guard else {
        panic!("LPSS port was not installed after memory initialization");
    };
    assert_eq!(uart.address, UART_VIRTUAL);
}
