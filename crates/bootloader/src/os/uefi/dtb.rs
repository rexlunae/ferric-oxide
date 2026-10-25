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

/// The flattened-device-tree header, which is always 40 bytes.
const FDT_HEADER_LEN: usize = 40;
/// DTB header field offsets (see the Devicetree Specification, section 5.3).
const FDT_HEADER_MAGIC: u32 = 0xd00d_feed;
const FDT_TOTAL_SIZE_OFFSET: usize = 4;
/// A generous upper bound for a firmware configuration-table DTB. Firmware hands
/// the table over without an explicit allocation size, so the only honest checks
/// are the header magic and a cap that no real DTB exceeds.
const MAX_DTB_SIZE: usize = 64 * 1024 * 1024;

fn parse_dtb(os: &impl Os, address: *const u8) -> Option<(u64, u64)> {
    if address.is_null() {
        return None;
    }
    // SAFETY: the UEFI device-tree configuration table points to a firmware-owned
    // DTB; read the fixed-size header only, which is the one bounded access we can
    // make without trusting the header's own declared length.
    let header = unsafe { slice::from_raw_parts(address, FDT_HEADER_LEN) };
    if BE::read_u32(&header[0..4]) != FDT_HEADER_MAGIC {
        return None;
    }
    let length = BE::read_u32(&header[FDT_TOTAL_SIZE_OFFSET..FDT_TOTAL_SIZE_OFFSET + 4]) as usize;
    // The header declares how large the blob is. A malformed or hostile firmware
    // header could claim a length beyond the firmware's actual allocation, so it is
    // rejected before `from_raw_parts` materialises an out-of-bounds slice.
    if length < FDT_HEADER_LEN || length > MAX_DTB_SIZE {
        println!("Rejecting DTB with impossible size {}", length);
        return None;
    }
    // SAFETY: `length` is the DTB header's declared total size, checked to lie in
    // `[header, MAX_DTB_SIZE]`, and `Tree::new` then re-checks the block extents the
    // header names against this slice.
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
