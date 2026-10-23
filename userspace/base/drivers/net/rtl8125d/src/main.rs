//! PCI daemon for the Realtek RTL8125B 2.5GbE controller.
//!
//! pcid matches the function by PCI ID and runs this binary. The daemon maps the
//! controller's memory BAR, takes an interrupt line, and wraps
//! [`device::Rtl8125`] in the shared network scheme. Packet I/O and all
//! controller initialisation live in the `device` module; this file only wires
//! the Redox event loop to the adapter.

#![deny(unsafe_op_in_unsafe_fn)]

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

use common::MemoryType;
use driver_network::NetworkScheme;
use event::{EventQueue, user_data};
use pcid_interface::{LegacyInterruptLine, PciFunctionHandle};

pub mod device;

/// Failures at the daemon's setup and event-loop boundaries.
#[derive(Debug, thiserror::Error)]
enum DriverError {
    #[error("no memory BAR is available")]
    NoMemoryBar,
    #[error("no supported legacy interrupt route is available")]
    InterruptRoute(#[source] syscall::Error),
    #[error("controller initialisation failed")]
    Controller(#[source] syscall::Error),
    #[error("cannot create the event queue")]
    EventQueue(#[source] libredox::error::Error),
    #[error("cannot subscribe to interrupts")]
    SubscribeInterrupt(#[source] libredox::error::Error),
    #[error("cannot subscribe to the network scheme")]
    SubscribeScheme(#[source] libredox::error::Error),
    #[error("cannot enter the null namespace")]
    Namespace(#[source] libredox::error::Error),
    #[error("cannot receive the next event")]
    NextEvent(#[source] libredox::error::Error),
    #[error("cannot read the interrupt vector")]
    ReadInterrupt(#[source] std::io::Error),
    #[error("cannot acknowledge the interrupt")]
    AcknowledgeInterrupt(#[source] std::io::Error),
    #[error("cannot service the network scheme")]
    Scheme(#[source] std::io::Error),
    #[error("the event queue closed")]
    EventQueueClosed,
}

/// Print the full error chain only at the daemon's outer boundary.
fn exit_with_error(error: DriverError) -> ! {
    log::error!("rtl8125d: {error}");
    let mut cause = std::error::Error::source(&error);
    while let Some(source) = cause {
        log::error!("rtl8125d: caused by: {source}");
        cause = source.source();
    }
    std::process::exit(1)
}

/// Map the controller's memory BAR.
///
/// The RTL8125 exposes its register file through BAR2 on the reference
/// hardware. Some variants place it in BAR1, so both are tried before giving
/// up. A function that does not expose either BAR cannot be driven by this
/// driver.
fn map_bar(pcid_handle: &mut PciFunctionHandle) -> Result<*mut u8, DriverError> {
    let config = pcid_handle.config();

    for &barnum in &[2, 1] {
        match config.func.bars[usize::from(barnum)] {
            pcid_interface::PciBar::Memory32 { .. } | pcid_interface::PciBar::Memory64 { .. } => {
                // SAFETY: pcid has verified that this is a memory BAR of this
                // function, and the returned mapping stays alive for the
                // lifetime of the daemon.
                return Ok(unsafe {
                    pcid_handle
                        .map_bar(barnum, MemoryType::Uncacheable)
                        .ptr
                        .as_ptr()
                });
            }
            other => log::warn!("BAR {} is {:?} instead of a memory BAR", barnum, other),
        }
    }
    Err(DriverError::NoMemoryBar)
}

fn main() {
    pcid_interface::pci_daemon(daemon);
}

fn daemon(daemon: daemon::Daemon, pcid_handle: PciFunctionHandle) -> ! {
    match serve(daemon, pcid_handle) {
        Ok(()) => std::process::exit(0),
        Err(error) => exit_with_error(error),
    }
}

fn serve(daemon: daemon::Daemon, mut pcid_handle: PciFunctionHandle) -> Result<(), DriverError> {
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

    let bar = map_bar(&mut pcid_handle)?;

    // The controller drives its legacy interrupt pin when the shared
    // interrupt mask register is set. Its MSI-X vectors belong to the
    // individual queues and pcid hands out only one of them, so the driver
    // asks for the pin.
    let line = require_legacy_interrupt(pcid_handle.config().func.legacy_interrupt_line)
        .map_err(DriverError::InterruptRoute)?;
    let mut irq_file = line.irq_handle("rtl8125d");

    let mut scheme = NetworkScheme::new(
        // SAFETY: `bar` is the uncacheable mapping of this function's register
        // BAR. The adapter owns it until the process exits.
        move || unsafe {
            match device::Rtl8125::new(bar as usize) {
                Ok(adapter) => adapter,
                Err(error) => exit_with_error(DriverError::Controller(error)),
            }
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

    let event_queue = EventQueue::<Source>::new().map_err(DriverError::EventQueue)?;
    event_queue
        .subscribe(
            irq_file.as_raw_fd() as usize,
            Source::Irq,
            event::EventFlags::READ,
        )
        .map_err(DriverError::SubscribeInterrupt)?;
    event_queue
        .subscribe(
            scheme.event_handle().raw(),
            Source::Scheme,
            event::EventFlags::READ,
        )
        .map_err(DriverError::SubscribeScheme)?;

    libredox::call::setrens(0, 0).map_err(DriverError::Namespace)?;

    scheme.tick().map_err(DriverError::Scheme)?;

    for event in event_queue {
        let event = event.map_err(DriverError::NextEvent)?;
        match event.user_data {
            Source::Irq => {
                let mut irq = [0; 8];
                irq_file
                    .read_exact(&mut irq)
                    .map_err(DriverError::ReadInterrupt)?;
                // Acknowledge the interrupt either way: the write is what
                // unmasks the controller's interrupt line, and the device
                // raises interrupts for events the driver does not handle.
                let work = scheme.adapter_mut().irq();
                irq_file
                    .write_all(&irq)
                    .map_err(DriverError::AcknowledgeInterrupt)?;
                if work {
                    scheme.tick().map_err(DriverError::Scheme)?;
                }
            }
            Source::Scheme => {
                scheme.tick().map_err(DriverError::Scheme)?;
            }
        }
    }
    Err(DriverError::EventQueueClosed)
}

/// Refuse unsupported interrupt routing before allocating or starting DMA rings.
fn require_legacy_interrupt(
    line: Option<LegacyInterruptLine>,
) -> syscall::Result<LegacyInterruptLine> {
    line.ok_or_else(|| syscall::Error::new(syscall::ENODEV))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_only_devices_are_refused_before_initialisation() {
        assert!(matches!(require_legacy_interrupt(None), Err(e) if e.errno == syscall::ENODEV));
        let line = LegacyInterruptLine {
            irq: 11,
            phandled: None,
        };
        assert_eq!(require_legacy_interrupt(Some(line)).unwrap().irq, 11);
    }
}
