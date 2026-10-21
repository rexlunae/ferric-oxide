//! PCI daemon for the Realtek RTL8125B 2.5GbE controller.
//!
//! pcid matches the function by PCI ID and runs this binary. The daemon maps the
//! controller's memory BAR, allocates an interrupt vector, and wraps
//! [`device::Rtl8125`] in the shared network scheme. Packet I/O and all
//! controller initialisation live in the `device` module; this file only wires
//! the Redox event loop to the adapter.

#![deny(unsafe_op_in_unsafe_fn)]

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

use common::MemoryType;
use driver_network::NetworkScheme;
use event::{EventQueue, user_data};
use pcid_interface::PciFunctionHandle;
use pcid_interface::irq_helpers::pci_allocate_interrupt_vector;

pub mod device;

/// Map the controller's memory BAR.
///
/// The RTL8125 exposes its register file through BAR2 on the reference
/// hardware. Some variants place it in BAR1, so both are tried before giving
/// up. A function that does not expose either BAR cannot be driven by this
/// driver.
fn map_bar(pcid_handle: &mut PciFunctionHandle) -> *mut u8 {
    let config = pcid_handle.config();

    for &barnum in &[2, 1] {
        match config.func.bars[usize::from(barnum)] {
            pcid_interface::PciBar::Memory32 { .. } | pcid_interface::PciBar::Memory64 { .. } => {
                // SAFETY: pcid has verified that this is a memory BAR of this
                // function, and the returned mapping stays alive for the
                // lifetime of the daemon.
                return unsafe {
                    pcid_handle
                        .map_bar(barnum, MemoryType::Uncacheable)
                        .ptr
                        .as_ptr()
                };
            }
            other => log::warn!("BAR {} is {:?} instead of a memory BAR", barnum, other),
        }
    }
    panic!("rtl8125d: failed to find a memory BAR");
}

fn main() {
    pcid_interface::pci_daemon(daemon);
}

fn daemon(daemon: daemon::Daemon, mut pcid_handle: PciFunctionHandle) -> ! {
    let pci_config = pcid_handle.config();

    let mut name = pci_config.func.name();
    name.push_str("_rtl8125");

    common::setup_logging(
        "net",
        "pci",
        &name,
        common::output_level(),
        common::file_level(),
    );

    log::info!("RTL8125 {}", pci_config.func.display());

    let bar = map_bar(&mut pcid_handle);
    let irq_file = pci_allocate_interrupt_vector(&mut pcid_handle, "rtl8125d");

    let mut scheme = NetworkScheme::new(
        // SAFETY: `bar` is the uncacheable mapping of this function's register
        // BAR. The adapter owns it until the process exits.
        move || unsafe {
            device::Rtl8125::new(bar as usize).expect("rtl8125d: failed to initialise device")
        },
        daemon,
        format!("network.{name}"),
    );

    user_data! {
        enum Source {
            Irq,
            Scheme,
        }
    }

    let event_queue = EventQueue::<Source>::new().expect("rtl8125d: could not create event queue");
    event_queue
        .subscribe(
            irq_file.irq_handle().as_raw_fd() as usize,
            Source::Irq,
            event::EventFlags::READ,
        )
        .expect("rtl8125d: could not subscribe to interrupt vector");
    event_queue
        .subscribe(
            scheme.event_handle().raw(),
            Source::Scheme,
            event::EventFlags::READ,
        )
        .expect("rtl8125d: could not subscribe to network scheme");

    libredox::call::setrens(0, 0).expect("rtl8125d: failed to enter null namespace");

    scheme
        .tick()
        .expect("rtl8125d: failed to service network scheme");

    for event in event_queue.map(|event| event.expect("rtl8125d: failed to get next event")) {
        match event.user_data {
            Source::Irq => {
                let mut irq = [0; 8];
                irq_file
                    .irq_handle()
                    .read_exact(&mut irq)
                    .expect("rtl8125d: failed to read interrupt vector");
                if scheme.adapter_mut().irq() {
                    irq_file
                        .irq_handle()
                        .write_all(&irq)
                        .expect("rtl8125d: failed to acknowledge interrupt");
                    scheme
                        .tick()
                        .expect("rtl8125d: failed to service received packets");
                }
            }
            Source::Scheme => {
                scheme
                    .tick()
                    .expect("rtl8125d: failed to service network scheme");
            }
        }
    }
    unreachable!("rtl8125d: event queue ended");
}
