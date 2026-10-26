//! Readable descriptions of records whose arguments are codes: what the text view prints after
//! the arguments and the Perfetto export adds to its events.
//!
//! - `mm.page_fault`: the access ([`PageFaultFlags`]) and the outcome ([`PageFaultOutcome`]),
//!   such as `write user, mapped`.
//! - `irq.deliver`: how the interrupt arrived ([`IrqSource`]) and how many handles woke.
//! - `iommu.fault`: the hardware's report decoded by its own crate, `ferric-vtd` for a VT-d
//!   fault record and `ferric-amdvi` for an AMD-Vi event log entry, the same decoders the
//!   kernel's log line uses.
//! - `mm.cr3_switch`: what the switch loaded ([`Cr3SwitchKind`]).
//!
//! Other events need no description: their arguments are plain numbers.

use ferric_amdvi::event::Event as AmdViEvent;
use ferric_trace::kernel::{
    Cr3SwitchKind, IOMMU_FAULT, IRQ_DELIVER, IommuFormat, IrqSource, MM_CR3_SWITCH, MM_PAGE_FAULT,
    PageFaultFlags, PageFaultOutcome,
};
use ferric_vtd::fault::FaultRecord;

use crate::capture::Entry;

/// The description of `entry`, or `None` for an event whose arguments say it all.
pub fn describe(entry: &Entry) -> Option<String> {
    let id = entry.event.id();
    if id == MM_PAGE_FAULT.id() {
        Some(page_fault(entry))
    } else if id == IRQ_DELIVER.id() {
        Some(irq_deliver(entry))
    } else if id == IOMMU_FAULT.id() {
        Some(iommu_fault(entry))
    } else if id == MM_CR3_SWITCH.id() {
        Some(cr3_switch_kind(entry.arg(0)))
    } else {
        None
    }
}

/// `mm.page_fault`: the access and the outcome, such as `write user, mapped`.
pub fn page_fault(entry: &Entry) -> String {
    format!(
        "{}, {}",
        PageFaultFlags::from_raw(entry.arg(2)),
        outcome_name(entry.arg(3))
    )
}

/// The name of a page fault's raw outcome, or the number when it is not one.
pub fn outcome_name(raw: u64) -> String {
    PageFaultOutcome::from_raw(raw).map_or_else(|| format!("outcome {raw}"), |o| o.to_string())
}

/// `irq.deliver`: the source and the handles woken, such as `vector, 1 handle`.
fn irq_deliver(entry: &Entry) -> String {
    let handles = entry.arg(4);
    let plural = if handles == 1 { "" } else { "s" };
    format!("{}, {handles} handle{plural}", source_name(entry.arg(2)))
}

/// The name of an interrupt's raw source, or the number when it is not one.
pub fn source_name(raw: u64) -> String {
    IrqSource::from_raw(raw).map_or_else(|| format!("source {raw}"), |s| s.to_string())
}

/// `mm.cr3_switch`: the name of what the switch loaded, or the number when it is not a kind.
pub fn cr3_switch_kind(raw: u64) -> String {
    Cr3SwitchKind::from_raw(raw).map_or_else(|| format!("kind {raw}"), |k| k.to_string())
}

/// `iommu.fault`: the decoded report, and how many times it was reported in a row.
pub fn iommu_fault(entry: &Entry) -> String {
    let (lo, hi, count) = (entry.arg(2), entry.arg(3), entry.arg(4));
    let report = match IommuFormat::from_raw(entry.arg(1)) {
        Some(IommuFormat::VtdFaultRecord) => FaultRecord::decode(lo, hi).map_or_else(
            || "VT-d fault record without its F bit".to_owned(),
            |record| record.to_string(),
        ),
        Some(IommuFormat::AmdViEvent) => AmdViEvent([lo, hi]).decode().to_string(),
        // `IommuFormat` is non-exhaustive: a newer kernel's format is shown raw.
        Some(format) => format!("{format} {lo:#x} {hi:#x}"),
        None => format!("format {} {lo:#x} {hi:#x}", entry.arg(1)),
    };
    if count > 1 {
        format!("{report} ({count} times)")
    } else {
        report
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ferric_trace::Event;
    use ferric_trace::kernel::{IRQ_ACK, SYSCALL_EXIT};

    fn entry(event: &'static Event, args: &[u64]) -> Entry {
        Entry {
            cpu: 0,
            timestamp: 0,
            event,
            args: args.to_vec(),
        }
    }

    /// A VT-d fault record's words: a read by 00:10.0 at `address`, blocked because nothing
    /// maps it (reason 06h).
    pub(crate) fn vtd_read_fault(address: u64) -> (u64, u64) {
        let hi = ferric_vtd::fault::RECORD_F | (1 << 62) | (0x06 << 32) | (0x10 << 3);
        (address, hi)
    }

    #[test]
    fn page_faults_name_the_access_and_outcome() {
        let flags = PageFaultFlags::WRITE.with(PageFaultFlags::USER, true);
        let fault = entry(&MM_PAGE_FAULT, &[0x1000, 0x2000, flags.raw(), 0]);
        assert_eq!(describe(&fault).as_deref(), Some("write user, mapped"));
        let copy = entry(&MM_PAGE_FAULT, &[0x1000, 0x2000, 0, 1]);
        assert_eq!(describe(&copy).as_deref(), Some("read, copy failed"));
        let odd = entry(&MM_PAGE_FAULT, &[0, 0, 0, 9]);
        assert_eq!(describe(&odd).as_deref(), Some("read, outcome 9"));
    }

    #[test]
    fn deliveries_name_the_source() {
        let msi = entry(&IRQ_DELIVER, &[11, 2, 1, 5, 1]);
        assert_eq!(describe(&msi).as_deref(), Some("vector, 1 handle"));
        let line = entry(&IRQ_DELIVER, &[4, 0, 0, 5, 0]);
        assert_eq!(describe(&line).as_deref(), Some("line, 0 handles"));
        assert_eq!(describe(&entry(&IRQ_ACK, &[11, 2, 5])), None);
        assert_eq!(describe(&entry(&SYSCALL_EXIT, &[1, 2])), None);
    }

    #[test]
    fn cr3_switches_name_what_they_loaded() {
        let other = entry(&MM_CR3_SWITCH, &[0, 0x1234_0000]);
        assert_eq!(describe(&other).as_deref(), Some("address space"));
        let empty = entry(&MM_CR3_SWITCH, &[1, 0]);
        assert_eq!(describe(&empty).as_deref(), Some("empty"));
        let odd = entry(&MM_CR3_SWITCH, &[5, 0]);
        assert_eq!(describe(&odd).as_deref(), Some("kind 5"));
    }

    #[test]
    fn iommu_faults_are_decoded_by_their_format() {
        let (lo, hi) = vtd_read_fault(0x3000);
        let vtd = entry(&IOMMU_FAULT, &[0xfed9_0000, 1, lo, hi, 1]);
        let text = describe(&vtd).expect("described");
        assert!(text.starts_with("00:10.0 read at 0x3000: "), "{text}");
        let cleared = entry(
            &IOMMU_FAULT,
            &[0, 1, lo, hi & !ferric_vtd::fault::RECORD_F, 1],
        );
        assert_eq!(
            describe(&cleared).as_deref(),
            Some("VT-d fault record without its F bit")
        );
        // An AMD-Vi IO_PAGE_FAULT (code 2) from device 00:10.0 at 0x5000, logged three times.
        let w0 = (0x2 << 60) | (1 << 53) | 0x80;
        let amd = entry(&IOMMU_FAULT, &[0xfd00_0000, 2, w0, 0x5000, 3]);
        let text = describe(&amd).expect("described");
        let decoded = AmdViEvent([w0, 0x5000]).decode().to_string();
        assert_eq!(text, format!("{decoded} (3 times)"));
        assert!(text.contains("0x5000"), "{text}");
        let unknown = entry(&IOMMU_FAULT, &[0, 7, 1, 2, 1]);
        assert_eq!(describe(&unknown).as_deref(), Some("format 7 0x1 0x2"));
    }
}
