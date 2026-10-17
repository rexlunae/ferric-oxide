//! Primary fault logging (section 7.3, registers 10.4.9 and 10.4.14, appendix A).
//!
//! When a unit blocks a DMA request it records the request in one of its fault recording
//! registers, sets the fault status register and, unless masked, sends the fault event
//! interrupt programmed in FEDATA/FEADDR. Software reads each record whose F bit is set,
//! writes F back to clear it, and clears the overflow bits in the status register.

use core::fmt;

use crate::Bdf;
use crate::regs::Capability;

/// Fault status register (10.4.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultStatus(pub u32);

impl FaultStatus {
    /// PFO, bit 0: a fault arrived while every recording register was full.
    pub const PFO: u32 = 1 << 0;
    /// PPF, bit 1: at least one recording register holds a fault (read-only).
    pub const PPF: u32 = 1 << 1;
    /// The write-1-to-clear bits: PFO, AFO, APF, IQE, ICE, ITE.
    pub const CLEAR: u32 = 0x7d;

    /// Whether any recording register holds a fault.
    pub const fn pending(self) -> bool {
        self.0 & Self::PPF != 0
    }
    /// Whether faults were lost because the recording registers were full.
    pub const fn overflow(self) -> bool {
        self.0 & Self::PFO != 0
    }
    /// FRI, bits 15:8: the recording register hardware wrote first, where software should
    /// start reading.
    pub const fn first_index(self) -> usize {
        ((self.0 >> 8) & 0xff) as usize
    }
    /// The value to write back to clear the sticky error bits that are set.
    pub const fn clear_value(self) -> u32 {
        self.0 & Self::CLEAR
    }
}

/// Bit 63 of a fault record's high word (bit 127 of the record): F, the record holds a fault.
/// Writing it back as 1 clears the record.
pub const RECORD_F: u64 = 1 << 63;

/// The register offsets of one fault recording register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordRegs {
    /// The low 64 bits (the faulting address).
    pub low: usize,
    /// The high 64 bits (source, reason, type, F).
    pub high: usize,
    /// The 32-bit word holding F, to write [`RECORD_F_DWORD`] to.
    pub f_dword: usize,
}

/// The value that clears a record when written to [`RecordRegs::f_dword`]: F is bit 31 of the
/// record's last 32-bit word.
pub const RECORD_F_DWORD: u32 = 1 << 31;

/// Every fault recording register of a unit, in the order to read them: starting at the
/// index the status register gives and wrapping around.
pub fn records(cap: Capability, status: FaultStatus) -> impl Iterator<Item = RecordRegs> {
    let count = cap.fault_recording_count();
    let first = status.first_index() % count;
    (0..count).map(move |i| {
        let low = cap.fault_recording_offset() + 16 * ((first + i) % count);
        RecordRegs {
            low,
            high: low + 8,
            f_dword: low + 12,
        }
    })
}

/// The fault event address (FEADDR, 10.4.12) that delivers the fault interrupt to the local
/// APIC with ID `apic_id`, in physical destination mode. IDs above 255 need interrupt
/// remapping.
pub const fn event_address(apic_id: u8) -> u32 {
    0xfee0_0000 | ((apic_id as u32) << 12)
}

/// Whether the request was a read, a write or something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// A DMA write (T2:T1 = 00).
    Write,
    /// A DMA read or atomic operation (T2:T1 = 01).
    Read,
    /// A page request or another request type (T2 = 1).
    Other,
}

/// A non-recoverable fault reason (appendix A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FaultReason {
    /// 0x01: the root entry for the bus is not present.
    RootNotPresent,
    /// 0x02: the context entry for the device is not present.
    ContextNotPresent,
    /// 0x03: the context entry is programmed incorrectly.
    ContextInvalid,
    /// 0x04: the address is beyond the domain's address width.
    AddressBeyondWidth,
    /// 0x05: a write hit a page without write permission.
    WriteDenied,
    /// 0x06: a read hit a page without read permission (or not mapped at all).
    ReadDenied,
    /// 0x07: a second-stage table could not be read.
    PageTableAccess,
    /// 0x08: the root table could not be read.
    RootTableAccess,
    /// 0x09: the context table could not be read.
    ContextTableAccess,
    /// 0x0a: a reserved bit is set in the root entry.
    RootReserved,
    /// 0x0b: a reserved bit is set in the context entry.
    ContextReserved,
    /// 0x0c: a reserved bit is set in a second-stage entry.
    PageTableReserved,
    /// 0x0d: the context entry's translation type blocks this request type.
    BlockedByTranslationType,
    /// 0x20-0x26: an interrupt remapping fault.
    InterruptRemapping(u8),
    /// Another code (scalable-mode and newer reasons).
    Other(u8),
}

impl FaultReason {
    /// Decodes a fault reason code.
    pub const fn from_code(code: u8) -> Self {
        match code {
            0x01 => Self::RootNotPresent,
            0x02 => Self::ContextNotPresent,
            0x03 => Self::ContextInvalid,
            0x04 => Self::AddressBeyondWidth,
            0x05 => Self::WriteDenied,
            0x06 => Self::ReadDenied,
            0x07 => Self::PageTableAccess,
            0x08 => Self::RootTableAccess,
            0x09 => Self::ContextTableAccess,
            0x0a => Self::RootReserved,
            0x0b => Self::ContextReserved,
            0x0c => Self::PageTableReserved,
            0x0d => Self::BlockedByTranslationType,
            c @ 0x20..=0x26 => Self::InterruptRemapping(c),
            c => Self::Other(c),
        }
    }
}

impl fmt::Display for FaultReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::RootNotPresent => "root entry not present",
            Self::ContextNotPresent => "context entry not present",
            Self::ContextInvalid => "invalid context entry",
            Self::AddressBeyondWidth => "address beyond the domain's width",
            Self::WriteDenied => "write not permitted",
            Self::ReadDenied => "read not permitted",
            Self::PageTableAccess => "cannot read a page table",
            Self::RootTableAccess => "cannot read the root table",
            Self::ContextTableAccess => "cannot read a context table",
            Self::RootReserved => "reserved bit set in root entry",
            Self::ContextReserved => "reserved bit set in context entry",
            Self::PageTableReserved => "reserved bit set in a page-table entry",
            Self::BlockedByTranslationType => "request type blocked by the context entry",
            Self::InterruptRemapping(c) => return write!(f, "interrupt remapping fault {c:#04x}"),
            Self::Other(c) => return write!(f, "fault reason {c:#04x}"),
        };
        f.write_str(s)
    }
}

/// One decoded fault recording register (10.4.14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultRecord {
    /// The device that made the request.
    pub source: Bdf,
    /// Why it was blocked.
    pub reason: FaultReason,
    /// The page of the faulting address (bits 63:12; the low 12 bits are zero).
    pub address: u64,
    /// Read, write or other.
    pub kind: RequestKind,
    /// The PASID, when the request carried one.
    pub pasid: Option<u32>,
}

impl FaultRecord {
    /// Decodes a record from its low and high 64-bit words, or `None` when its F bit is
    /// clear.
    pub const fn decode(lo: u64, hi: u64) -> Option<Self> {
        if hi & RECORD_F == 0 {
            return None;
        }
        let t1 = hi & (1 << 62) != 0;
        let t2 = hi & (1 << 28) != 0;
        let kind = match (t2, t1) {
            (false, false) => RequestKind::Write,
            (false, true) => RequestKind::Read,
            (true, _) => RequestKind::Other,
        };
        let pasid = if hi & (1 << 31) != 0 {
            Some(((hi >> 40) & 0xf_ffff) as u32)
        } else {
            None
        };
        Some(Self {
            source: Bdf::from_source_id(hi as u16),
            reason: FaultReason::from_code((hi >> 32) as u8),
            address: lo & !0xfff,
            kind,
            pasid,
        })
    }
}

impl fmt::Display for FaultRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            RequestKind::Write => "write",
            RequestKind::Read => "read",
            RequestKind::Other => "request",
        };
        write!(
            f,
            "{} {kind} at {:#x}: {}",
            self.source, self.address, self.reason
        )?;
        if let Some(pasid) = self.pasid {
            write!(f, " (PASID {pasid:#x})")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::*;

    #[test]
    fn status_bits() {
        let st = FaultStatus(0x0302);
        assert!(st.pending());
        assert!(!st.overflow());
        assert_eq!(st.first_index(), 3);
        assert_eq!(st.clear_value(), 0);
        assert_eq!(FaultStatus(0x7f).clear_value(), 0x7d);
    }

    #[test]
    fn record_offsets_wrap_from_first_index() {
        // FRO 0x22 (offset 0x220), 4 records, first index 2.
        let cap = Capability((0x22 << 24) | (3 << 40));
        let offs: std::vec::Vec<_> = records(cap, FaultStatus(2 << 8)).map(|r| r.low).collect();
        assert_eq!(offs, [0x240, 0x250, 0x220, 0x230]);
        let r = records(cap, FaultStatus(0)).next().unwrap();
        assert_eq!((r.high, r.f_dword), (0x228, 0x22c));
        // An index beyond the record count does not run off the end.
        assert_eq!(records(cap, FaultStatus(9 << 8)).next().unwrap().low, 0x230);
        assert_eq!(event_address(3), 0xfee0_3000);
        assert_eq!(u64::from(RECORD_F_DWORD) << 32, RECORD_F);
    }

    #[test]
    fn decode_records() {
        assert_eq!(FaultRecord::decode(0x1234_5000, 0), None);
        // A write from 00:01.0 to an unmapped page: F, T1 = 0, reason 5, SID 0x0008.
        let hi = RECORD_F | (0x05 << 32) | 0x0008;
        let rec = FaultRecord::decode(0x1_2345_6789, hi).unwrap();
        assert_eq!(rec.source, Bdf::new(0, 1, 0));
        assert_eq!(rec.kind, RequestKind::Write);
        assert_eq!(rec.reason, FaultReason::WriteDenied);
        assert_eq!(rec.address, 0x1_2345_6000);
        assert_eq!(
            rec.to_string(),
            "00:01.0 write at 0x123456000: write not permitted"
        );

        // A read with PASID 0x42 from 03:00.1, context entry not present.
        let hi = RECORD_F | (1 << 62) | (0x42 << 40) | (0x02 << 32) | (1 << 31) | 0x0301;
        let rec = FaultRecord::decode(0x8000, hi).unwrap();
        assert_eq!(rec.kind, RequestKind::Read);
        assert_eq!(rec.pasid, Some(0x42));
        assert_eq!(rec.reason, FaultReason::ContextNotPresent);
        assert_eq!(rec.source, Bdf::new(3, 0, 1));
        assert!(rec.to_string().ends_with("(PASID 0x42)"));

        let other = FaultRecord::decode(0, RECORD_F | (1 << 28) | (0x22 << 32)).unwrap();
        assert_eq!(other.kind, RequestKind::Other);
        assert_eq!(other.reason, FaultReason::InterruptRemapping(0x22));
        assert_eq!(FaultReason::from_code(0x30), FaultReason::Other(0x30));
    }
}
