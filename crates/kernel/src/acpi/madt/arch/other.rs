use super::Madt;

pub(super) fn init(madt: Madt) {
    for madt_entry in madt.iter() {
        crate::debug!("      {:#x?}", madt_entry);
    }

    crate::warn!("MADT not yet handled on this platform");
}
