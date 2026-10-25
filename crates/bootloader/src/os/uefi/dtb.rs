//! UEFI device-tree handoff through the shared crates.io-backed parser.
//!
//! Firmware owns the input until ExitBootServices. The validated blob is copied
//! into page-aligned bootloader memory for the kernel's immutable handoff.

use crate::Os;
use alloc::vec::Vec;
use byteorder::BE;
use byteorder::ByteOrder;
use core::slice;
use uefi::guid::DEVICE_TREE_GUID;

fn parse_dtb(os: &impl Os, address: *const u8) -> Option<(u64, u64)> {
    if address.is_null() {
        return None;
    }
    // SAFETY: the UEFI device-tree configuration table points to a firmware-owned
    // DTB header and its declared blob, readable until ExitBootServices.
    let header = unsafe { slice::from_raw_parts(address, 40) };
    let length = BE::read_u32(&header[4..8]) as usize;
    // SAFETY: the configuration table's contract covers the declared blob.
    let data = unsafe { slice::from_raw_parts(address, length) };
    let fdt = match ferric_devicetree::Tree::new(data) {
        Ok(tree) => tree,
        Err(error) => {
            println!("Failed to parse DTB: {}", error);
            return None;
        }
    };
    let mut rsdps_area = Vec::from(fdt.raw_data());
    let align = 8;
    rsdps_area.resize(rsdps_area.len().div_ceil(align) * align, 0u8);
    let size = rsdps_area.len();
    let base = os.alloc_zeroed_page_aligned(size);
    // SAFETY: the allocator returns a writable allocation of at least `size`
    // bytes, disjoint from the firmware blob and this temporary buffer.
    unsafe { slice::from_raw_parts_mut(base, size) }.copy_from_slice(&rsdps_area);
    Some((base as u64, size as u64))
}

pub(crate) fn find_dtb(os: &impl Os) -> Option<(u64, u64)> {
    let cfg_tables = std::system_table().config_tables();
    for cfg_table in cfg_tables.iter() {
        if cfg_table.VendorGuid == DEVICE_TREE_GUID {
            let addr = cfg_table.VendorTable;
            return parse_dtb(os, addr as *const u8);
        }
    }

    None
}
