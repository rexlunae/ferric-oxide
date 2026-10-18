//! The event log (section 2.5).
//!
//! The IOMMU reports faults and errors by writing 128-bit [`Event`]s into a ring in memory and
//! moving the event log tail pointer register; software reads them from the head and moves the
//! head pointer register past them. The event code is in bits 63:60 of the first word. When the
//! ring is full the IOMMU sets EventOverflow in the status register and drops further events.
//!
//! [`Event::decode`] turns an entry into an [`EventKind`] with the fields that matter for each:
//! the device ID, the domain, the address and, for I/O page faults, the flags word
//! ([`FaultFlags`]). A device that keeps issuing a bad request (an emulated device copying a
//! buffer word by word, say) can log one event per access, so [`Coalescer`] folds runs of
//! identical events into one with a repeat count, for logging.

use core::fmt;

use crate::DeviceId;
use crate::regs::{RING_POINTER_MASK, RingSize};

/// The flags of an I/O page fault and similar events: bits 27:16 of word 0's high half
/// (word 0 bits 59:48), section 2.5.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FaultFlags(pub u16);

impl FaultFlags {
    /// GN: the request carried a PASID (guest translation).
    pub const fn guest(self) -> bool {
        self.0 & (1 << 0) != 0
    }

    /// NX: the request was an instruction fetch.
    pub const fn no_execute(self) -> bool {
        self.0 & (1 << 1) != 0
    }

    /// US: the request was a user-mode one.
    pub const fn user(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// I: the request was an interrupt request rather than a memory access.
    pub const fn interrupt(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// PR: a translation was present, so the fault is about permissions.
    pub const fn present(self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// RW: the request was a write.
    pub const fn write(self) -> bool {
        self.0 & (1 << 5) != 0
    }

    /// PE: the access was refused by a permission check.
    pub const fn permission(self) -> bool {
        self.0 & (1 << 6) != 0
    }

    /// RZ: a reserved bit was set in a table entry.
    pub const fn reserved_bit(self) -> bool {
        self.0 & (1 << 7) != 0
    }

    /// TR: the request was a translation request (ATS).
    pub const fn translation_request(self) -> bool {
        self.0 & (1 << 8) != 0
    }
}

impl fmt::Display for FaultFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "flags {:#05x}", self.0)?;
        let names = [
            (self.guest(), "GN"),
            (self.no_execute(), "NX"),
            (self.user(), "US"),
            (self.interrupt(), "I"),
            (self.present(), "PR"),
            (self.write(), "RW"),
            (self.permission(), "PE"),
            (self.reserved_bit(), "RZ"),
            (self.translation_request(), "TR"),
        ];
        for (set, name) in names {
            if set {
                write!(f, " {name}")?;
            }
        }
        Ok(())
    }
}

/// What an event reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventKind {
    /// 01h, ILLEGAL_DEV_TABLE_ENTRY: a device table entry has a reserved or invalid field.
    IllegalDeviceTableEntry {
        /// The device.
        device: DeviceId,
        /// The flags.
        flags: FaultFlags,
        /// The address of the request.
        address: u64,
    },
    /// 02h, IO_PAGE_FAULT: a request found no translation or no permission.
    IoPageFault {
        /// The device.
        device: DeviceId,
        /// The domain ID (or the PASID's high bits for a guest request).
        domain: u16,
        /// The flags.
        flags: FaultFlags,
        /// The IOVA of the request.
        address: u64,
    },
    /// 03h, DEV_TAB_HARDWARE_ERROR: reading the device table failed.
    DeviceTableHardwareError {
        /// The device.
        device: DeviceId,
        /// The address that could not be read.
        address: u64,
    },
    /// 04h, PAGE_TAB_HARDWARE_ERROR: reading a page table failed.
    PageTableHardwareError {
        /// The device.
        device: DeviceId,
        /// The domain ID.
        domain: u16,
        /// The address that could not be read.
        address: u64,
    },
    /// 05h, ILLEGAL_COMMAND_ERROR: the command at `address` is invalid; the command buffer
    /// stopped there.
    IllegalCommand {
        /// The physical address of the command.
        address: u64,
    },
    /// 06h, COMMAND_HARDWARE_ERROR: fetching a command failed.
    CommandHardwareError {
        /// The address that could not be read.
        address: u64,
    },
    /// 07h, IOTLB_INV_TIMEOUT: a device did not answer an IOTLB invalidation.
    IotlbInvalidationTimeout {
        /// The device.
        device: DeviceId,
        /// The address of the invalidation command.
        address: u64,
    },
    /// 08h, INVALID_DEVICE_REQUEST: a request the device is not allowed to make.
    InvalidDeviceRequest {
        /// The device.
        device: DeviceId,
        /// The flags.
        flags: FaultFlags,
        /// The address of the request.
        address: u64,
    },
    /// Any other event code.
    Other {
        /// The code.
        code: u8,
    },
}

/// One event log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Event(pub [u64; 2]);

impl Event {
    /// An entry the IOMMU has not written: all zero.
    pub const EMPTY: Self = Self([0; 2]);

    /// The event code, bits 63:60 of word 0.
    pub const fn code(self) -> u8 {
        (self.0[0] >> 60) as u8
    }

    /// Decodes the entry.
    pub const fn decode(self) -> EventKind {
        let device = DeviceId(self.0[0] as u16);
        let domain = (self.0[0] >> 32) as u16;
        let flags = FaultFlags(((self.0[0] >> 48) & 0xfff) as u16);
        let address = self.0[1];
        match self.code() {
            0x1 => EventKind::IllegalDeviceTableEntry {
                device,
                flags,
                address,
            },
            0x2 => EventKind::IoPageFault {
                device,
                domain,
                flags,
                address,
            },
            0x3 => EventKind::DeviceTableHardwareError { device, address },
            0x4 => EventKind::PageTableHardwareError {
                device,
                domain,
                address,
            },
            0x5 => EventKind::IllegalCommand { address },
            0x6 => EventKind::CommandHardwareError { address },
            0x7 => EventKind::IotlbInvalidationTimeout { device, address },
            0x8 => EventKind::InvalidDeviceRequest {
                device,
                flags,
                address,
            },
            code => EventKind::Other { code },
        }
    }

    /// Whether the event is about a device's DMA (as opposed to the IOMMU's own commands).
    pub const fn is_dma_fault(self) -> bool {
        matches!(
            self.decode(),
            EventKind::IoPageFault { .. }
                | EventKind::IllegalDeviceTableEntry { .. }
                | EventKind::InvalidDeviceRequest { .. }
        )
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = |flags: &FaultFlags| if flags.write() { "write" } else { "read" };
        match self {
            Self::IllegalDeviceTableEntry {
                device,
                flags,
                address,
            } => write!(
                f,
                "{device} {} at {address:#x}: illegal device table entry ({flags})",
                kind(flags)
            ),
            Self::IoPageFault {
                device,
                domain,
                flags,
                address,
            } => write!(
                f,
                "{device} {} at {address:#x}: I/O page fault (domain {domain}, {flags})",
                kind(flags)
            ),
            Self::DeviceTableHardwareError { device, address } => write!(
                f,
                "{device}: hardware error reading the device table at {address:#x}"
            ),
            Self::PageTableHardwareError {
                device,
                domain,
                address,
            } => write!(
                f,
                "{device}: hardware error reading a page table of domain {domain} at {address:#x}"
            ),
            Self::IllegalCommand { address } => {
                write!(f, "illegal command at {address:#x}")
            }
            Self::CommandHardwareError { address } => {
                write!(f, "hardware error fetching the command at {address:#x}")
            }
            Self::IotlbInvalidationTimeout { device, address } => write!(
                f,
                "{device}: device-TLB invalidation timed out (command at {address:#x})"
            ),
            Self::InvalidDeviceRequest {
                device,
                flags,
                address,
            } => write!(
                f,
                "{device} {} at {address:#x}: invalid device request ({flags})",
                kind(flags)
            ),
            Self::Other { code } => write!(f, "event code {code:#x}"),
        }
    }
}

/// The event log's size and the encoding of its pointer registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLog {
    size: RingSize,
}

impl EventLog {
    /// An event log of `size`.
    pub const fn new(size: RingSize) -> Self {
        Self { size }
    }

    /// The log's size.
    pub const fn size(&self) -> RingSize {
        self.size
    }

    /// The slot a head or tail pointer register value names.
    pub const fn slot(&self, register: u64) -> u32 {
        ((register & RING_POINTER_MASK) >> 4) as u32 & (self.size.entries() - 1)
    }

    /// The pointer register value for `slot`.
    pub const fn register(&self, slot: u32) -> u64 {
        ((slot & (self.size.entries() - 1)) as u64) << 4
    }

    /// The slots holding unread events, from the head register to the tail register, in order.
    pub fn unread(&self, head: u64, tail: u64) -> impl Iterator<Item = u32> + use<> {
        let n = self.size.entries();
        let (first, last) = (self.slot(head), self.slot(tail));
        let count = (last + n - first) % n;
        (0..count).map(move |i| (first + i) % n)
    }
}

/// A run of identical events, as [`Coalescer`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// The event.
    pub event: Event,
    /// How many times it was logged in a row.
    pub count: u32,
}

/// Folds runs of identical events into one [`Run`] each.
#[derive(Clone, Copy, Debug, Default)]
pub struct Coalescer {
    current: Option<Run>,
}

impl Coalescer {
    /// Nothing seen yet.
    pub const fn new() -> Self {
        Self { current: None }
    }

    /// Adds `event`; returns the run it ends, when it is different from the one before.
    pub fn push(&mut self, event: Event) -> Option<Run> {
        match &mut self.current {
            Some(run) if run.event == event => {
                run.count = run.count.saturating_add(1);
                None
            }
            current => current.replace(Run { event, count: 1 }),
        }
    }

    /// Ends the current run and returns it.
    pub fn finish(&mut self) -> Option<Run> {
        self.current.take()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;
    use std::vec::Vec;

    use super::*;

    #[test]
    fn decodes_events() {
        // An I/O page fault for a write by 00:10.0 in domain 5 to 0x8000_0000 with PR, RW, PE.
        let e = Event([
            (0x2u64 << 60) | (0x070u64 << 48) | (5 << 32) | 0x0080,
            0x8000_0000,
        ]);
        assert_eq!(
            e.decode(),
            EventKind::IoPageFault {
                device: DeviceId(0x80),
                domain: 5,
                flags: FaultFlags(0x070),
                address: 0x8000_0000,
            }
        );
        assert!(e.is_dma_fault());
        assert_eq!(
            e.decode().to_string(),
            "00:10.0 write at 0x80000000: I/O page fault (domain 5, flags 0x070 PR RW PE)"
        );
        // QEMU 10.2's entries: device and flags only, no address or domain.
        let qemu = Event([0x200a_0000_0000_0080, 0]);
        assert_eq!(
            qemu.decode().to_string(),
            "00:10.0 read at 0x0: I/O page fault (domain 0, flags 0x00a NX I)"
        );
        let illegal = Event([0x5000_0000_0000_0000, 0x90_0040]);
        assert_eq!(
            illegal.decode(),
            EventKind::IllegalCommand { address: 0x90_0040 }
        );
        assert!(!illegal.is_dma_fault());
        assert_eq!(
            Event([0xf << 60, 0]).decode(),
            EventKind::Other { code: 0xf }
        );
    }

    #[test]
    fn log_slots() {
        let log = EventLog::new(RingSize::ONE_PAGE);
        assert_eq!(log.unread(0x10, 0x40).collect::<Vec<_>>(), [1, 2, 3]);
        // Wrapped.
        assert_eq!(log.unread(0xfe0, 0x10).collect::<Vec<_>>(), [254, 255, 0]);
        assert_eq!(log.unread(0x40, 0x40).count(), 0);
        assert_eq!(log.register(256), 0);
        assert_eq!(log.register(3), 0x30);
    }

    #[test]
    fn coalesces_runs() {
        let a = Event([0x200a_0000_0000_0080, 0]);
        let b = Event([0x2009_0000_0000_0080, 0]);
        let mut c = Coalescer::new();
        let mut runs = Vec::new();
        for e in [a, a, a, b, a] {
            runs.extend(c.push(e));
        }
        runs.extend(c.finish());
        assert_eq!(
            runs,
            [
                Run { event: a, count: 3 },
                Run { event: b, count: 1 },
                Run { event: a, count: 1 }
            ]
        );
        assert_eq!(c.finish(), None);
    }
}
