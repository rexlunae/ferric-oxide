//! Ferric Oxide: the boot self-test of DMA domains, run when VT-d or AMD-Vi is enabled.
//!
//! It creates a [`Domain`], maps and unmaps a few IOVA ranges and checks the tables by
//! walking them, then attaches a device that cannot be doing DMA, checks its context or
//! device table entry, maps and unmaps while it is attached (so the unit really receives the
//! IOTLB invalidations), detaches it, and puts it back in the identity domain. It logs one
//! line with the outcome, which the boot CI checks.
//!
//! The device is one with nothing at its address, or else a host bridge, which never
//! initiates DMA. Neither may be named by reserved memory (an RMRR or an IVMD).

use core::fmt;

use ferric_vtd::{
    dmar::{ScopeKind, Structure},
    table::Access,
    Bdf,
};

use super::{
    domain::{self, Attachment, Device, Domain, DomainError},
    pci_config_read,
};

/// What went wrong.
#[derive(Debug)]
enum TestError {
    /// A domain operation failed.
    Domain(DomainError),
    /// The tables or a context entry did not read back as expected.
    Mismatch(&'static str),
}

impl From<DomainError> for TestError {
    fn from(err: DomainError) -> Self {
        Self::Domain(err)
    }
}

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Domain(err) => write!(f, "{err}"),
            Self::Mismatch(what) => write!(f, "{what} read back wrong"),
        }
    }
}

/// Fails with `what` unless `ok`.
fn expect(ok: bool, what: &'static str) -> Result<(), TestError> {
    if ok {
        Ok(())
    } else {
        Err(TestError::Mismatch(what))
    }
}

/// Runs the test and logs the result.
pub(super) fn run() {
    match test() {
        Ok(None) => {
            info!("IOMMU: domain self-test passed (attach/map/unmap/detach)");
        }
        Ok(Some(why)) => {
            info!("IOMMU: domain self-test passed (map/unmap; attach skipped: {why})");
        }
        Err(err) => {
            error!("IOMMU: domain self-test failed: {err}");
        }
    }
}

/// The test; `Ok(Some(reason))` when the attach step was skipped.
fn test() -> Result<Option<&'static str>, TestError> {
    let mut domain = Domain::new()?;
    map_unmap(&mut domain)?;
    expect(domain.table_pages() == 1, "emptied table pages")?;
    let Some(device) = idle_device() else {
        return Ok(Some("no idle device"));
    };
    attach_detach(&mut domain, device)?;
    Ok(None)
}

/// Maps a 4 KiB, a 12 KiB (16 KiB block) and a 2 MiB range, checks them, and unmaps them.
fn map_unmap(domain: &mut Domain) -> Result<(), TestError> {
    // Physical addresses nothing reads: no device in the domain issues DMA.
    let cases = [
        (0x1000, 0x1234_5000, Access::Read),
        (0x3000, 0x2000_0000, Access::Write),
        (0x20_0000, 0x4000_0000, Access::ReadWrite),
    ];
    let mut ranges = [None; 3];
    for ((len, phys, access), slot) in cases.into_iter().zip(&mut ranges) {
        let range = domain.alloc_iova(len)?;
        domain.map(range.start, phys, range.len, access)?;
        *slot = Some((range, phys, access));
    }
    for &(range, phys, access) in ranges.iter().flatten() {
        let last = range.len.saturating_sub(1);
        for offset in [0, (last >> 1) & !7, last] {
            expect(
                domain.translate(range.start.saturating_add(offset))
                    == Some((phys.saturating_add(offset), access)),
                "a mapping",
            )?;
        }
        expect(
            !(0xfee0_0000..0xfef0_0000).contains(&range.start),
            "the IOVA allocator",
        )?;
    }
    for &(range, _, _) in ranges.iter().flatten() {
        domain.unmap(range.start, range.len)?;
        expect(domain.translate(range.start).is_none(), "an unmapped range")?;
        domain.free_iova(range)?;
    }
    Ok(())
}

/// Attaches `device`, checks its entry, maps and unmaps while attached, detaches it and
/// restores its identity entry.
fn attach_detach(domain: &mut Domain, device: Device) -> Result<(), TestError> {
    let before = domain::attachment(device)?;
    domain.attach(device)?;
    expect(
        domain::attachment(device)?
            == Attachment::Domain {
                id: domain.id(),
                root: domain.root_table(),
            },
        "the attached device's entry",
    )?;
    let range = domain.alloc_iova(0x8000)?;
    domain.map(range.start, 0x3000_0000, range.len, Access::ReadWrite)?;
    domain.unmap(range.start, range.len)?;
    domain.free_iova(range)?;
    domain.detach(device)?;
    expect(
        domain::attachment(device)? == Attachment::Blocked,
        "the blocked device's entry",
    )?;
    domain::restore_identity(device)?;
    expect(
        domain::attachment(device)? == before,
        "the restored device's entry",
    )?;
    Ok(())
}

/// Whether `device` can be attached by the test: a unit serves it, no reserved memory names
/// it, and it is absent or a host bridge.
fn usable(device: Device, want_absent: bool) -> bool {
    if device.segment != 0
        || domain::has_reserved_memory(device)
        || domain::unit_index(device).is_err()
    {
        return false;
    }
    // Reading the vendor or class register has no side effects.
    let id = pci_config_read(device.bdf, 0);
    if want_absent {
        return id & 0xffff == 0xffff;
    }
    // Class 06h, subclass 00h: host bridge.
    id & 0xffff != 0xffff && pci_config_read(device.bdf, 8) >> 16 == 0x0600
}

/// A device the test can attach without disturbing anything: an empty slot on bus 0 that a
/// unit serves (VT-d units with INCLUDE_PCI_ALL and AMD-Vi IVHDs with an "all" entry serve
/// every slot), or else a host bridge a DRHD lists (QEMU lists each device), or else any
/// host bridge on bus 0 a unit serves.
fn idle_device() -> Option<Device> {
    let empty = (1..32u8)
        .rev()
        .map(|dev| Device {
            segment: 0,
            bdf: Bdf::new(0, dev, 0),
        })
        .find(|&d| usable(d, true));
    empty
        .or_else(|| {
            let dmar = super::DMAR.get()?;
            dmar.structures()
                .filter_map(|s| match s {
                    Ok(Structure::Drhd(drhd)) => Some(drhd),
                    _ => None,
                })
                .flat_map(|drhd| {
                    drhd.scopes
                        .iter()
                        .flatten()
                        .filter(|scope| scope.kind == ScopeKind::PciEndpoint)
                        .filter_map(|scope| scope.direct_bdf())
                        .map(move |bdf| Device {
                            segment: drhd.segment,
                            bdf,
                        })
                })
                .find(|&d| usable(d, false))
        })
        .or_else(|| {
            (0..32u8)
                .map(|dev| Device {
                    segment: 0,
                    bdf: Bdf::new(0, dev, 0),
                })
                .find(|&d| usable(d, false))
        })
}
