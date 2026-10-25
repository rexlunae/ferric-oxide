extern crate std;

use super::*;
use std::{vec, vec::Vec};

const FIRMWARE: &[u8] = include_bytes!("../testdata/firmware.dtb");
const MMIO: &[u8] = include_bytes!("../../kernel/src/dtb/testdata/mmio.dtb");

fn change_property(data: &mut [u8], path: &str, name: &str, value: &[u8]) {
    let tree = Tree::new(data).unwrap();
    let original = tree.find_node(path).unwrap().property(name).unwrap().value;
    assert_eq!(original.len(), value.len());
    let start = original.as_ptr() as usize - data.as_ptr() as usize;
    data[start..start + value.len()].copy_from_slice(value);
}

#[test]
fn resolves_console_alias_and_parameters() {
    let tree = Tree::new(FIRMWARE).unwrap();
    let console = tree.chosen().stdout().unwrap();
    assert_eq!(console.node().name, "serial@200");
    assert_eq!(console.params(), Some("115200n8"));
    assert_eq!(tree.chosen().bootargs(), Some("TEST=1"));
    assert_eq!(tree.root().model(), "Ferric DT regression fixture");
    let direct = Tree::new(MMIO).unwrap();
    assert_eq!(direct.chosen().stdout().unwrap().params(), Some("115200n8"));
}

#[test]
fn resolves_compatible_lists_and_interrupt_phandles() {
    let tree = Tree::new(FIRMWARE).unwrap();
    let uart = tree.find_compatible(&["arm,primecell"]).unwrap();
    assert_eq!(
        uart.property("clock-names")
            .unwrap()
            .iter_str()
            .collect::<Vec<_>>(),
        vec!["apb_pclk", "uartclk"]
    );
    let controller = uart.interrupt_parent().unwrap();
    assert!(controller.same_node(tree.find_phandle(1).unwrap()));
    assert_eq!(controller.interrupt_cells(), Some(3));
    assert!(tree.find_phandle(2).is_none());
}

#[test]
fn retains_cpu_unit_addresses_and_zero_size_cells() {
    let tree = Tree::new(FIRMWARE).unwrap();
    let cpus = tree.cpus().collect::<Vec<_>>();
    assert_eq!(cpus.len(), 2);
    assert_eq!(cpus[1].name, "cpu@100");
    let region = cpus[1].reg().unwrap().next().unwrap();
    assert_eq!(region.starting_address as usize, 0x100);
    assert_eq!(region.size, None);
    assert!(!cpus[0].same_node(cpus[1]));
    let copy = FIRMWARE.to_vec();
    assert!(!cpus[0].same_node(Tree::new(&copy).unwrap().cpus().next().unwrap()));
}

#[test]
fn defaults_each_cell_width_independently() {
    let tree = Tree::new(FIRMWARE).unwrap();
    let node = tree.find_node("/partial-cells/device@200").unwrap();
    let reg = node.reg().unwrap().next().unwrap();
    assert_eq!(reg.starting_address as usize, 0x200);
    assert_eq!(reg.size, Some(0x20));
}

#[test]
fn retains_three_cell_pci_range_addresses() {
    let tree = Tree::new(FIRMWARE).unwrap();
    let range = tree
        .find_node("/pci")
        .unwrap()
        .ranges()
        .unwrap()
        .next()
        .unwrap();
    assert_eq!(range.child_bus_address_hi, 0x02000000);
    assert_eq!(range.child_bus_address, 0);
    assert_eq!(range.parent_bus_address, 0x40000000);
    assert_eq!(range.size, 0x1000);
}

#[test]
fn translates_nested_and_identity_buses_with_exact_bounds() {
    let tree = Tree::new(MMIO).unwrap();
    for (path, expected) in [
        ("/soc/bus@1000/serial@200", Some(0x1200)),
        ("/soc/bus@1000/device@ff0", Some(0x1ff0)),
        ("/soc/bus@1000/device@ff1", None),
        ("/identity-bus/device@3000", Some(0x3000)),
    ] {
        let node = tree.find_node(path).unwrap();
        let region = node.reg().unwrap().next().unwrap();
        assert_eq!(tree.translate_mmio_address(node, &region), expected);
    }
}

#[test]
fn rejects_foreign_nodes_missing_mappings_and_address_overflow() {
    let tree = Tree::new(FIRMWARE).unwrap();
    let node = tree.find_node("/partial-cells/device@200").unwrap();
    let reg = node.reg().unwrap().next().unwrap();
    assert_eq!(tree.translate_mmio_address(node, &reg), None);
    let foreign = Tree::new(MMIO).unwrap();
    assert_eq!(foreign.translate_mmio_address(node, &reg), None);
    let direct = tree.find_node("/intc").unwrap();
    assert_eq!(
        tree.translate_mmio_address(
            direct,
            &MemoryRegion {
                starting_address: usize::MAX as *const u8,
                size: Some(2)
            }
        ),
        None
    );
}

#[test]
fn rejects_zero_overflowing_and_truncated_cell_encodings() {
    for count in [0u32, 4, u32::MAX] {
        let mut data = FIRMWARE.to_vec();
        change_property(
            &mut data,
            "/soc/bus@1000",
            "#address-cells",
            &count.to_be_bytes(),
        );
        change_property(
            &mut data,
            "/soc/bus@1000",
            "#size-cells",
            &0u32.to_be_bytes(),
        );
        let tree = Tree::new(&data).unwrap();
        assert!(
            tree.find_node("/soc/bus@1000/serial@200")
                .unwrap()
                .reg()
                .is_none()
        );
        assert!(tree.find_node("/soc/bus@1000").unwrap().ranges().is_none());
    }
    let mut data = FIRMWARE.to_vec();
    change_property(
        &mut data,
        "/soc/bus@1000",
        "#size-cells",
        &2u32.to_be_bytes(),
    );
    let tree = Tree::new(&data).unwrap();
    assert!(
        tree.find_node("/soc/bus@1000/serial@200")
            .unwrap()
            .reg()
            .is_none()
    );
}

#[test]
fn rejects_truncated_headers_and_invalid_block_extents() {
    for end in 0..40 {
        assert!(Tree::new(&FIRMWARE[..end]).is_err());
    }
    for (field, value) in [
        (4, 39u32),
        (4, u32::MAX),
        (8, u32::MAX),
        (12, u32::MAX),
        (32, u32::MAX),
        (36, u32::MAX),
        (8, 41),
        (12, 40),
    ] {
        let mut data = FIRMWARE.to_vec();
        data[field..field + 4].copy_from_slice(&value.to_be_bytes());
        assert!(Tree::new(&data).is_err(), "field {field}, value {value}");
    }
}

#[test]
fn rejects_malformed_structure_and_property_names() {
    let mut data = FIRMWARE.to_vec();
    let structure = word(&data, 8).unwrap() as usize;
    data[structure..structure + 4].copy_from_slice(&0xffffu32.to_be_bytes());
    assert!(Tree::new(&data).is_err());
    let mut data = FIRMWARE.to_vec();
    let strings = word(&data, 12).unwrap() as usize;
    data[strings] = 0xff;
    assert!(Tree::new(&data).is_err());
}

#[test]
fn validates_strings_and_borrows_only_the_declared_blob() {
    assert!(
        Property {
            name: "x",
            value: b"missing terminator"
        }
        .as_str()
        .is_none()
    );
    assert!(
        Property {
            name: "x",
            value: b"\xff\0"
        }
        .as_str()
        .is_none()
    );
    assert!(
        Property {
            name: "x",
            value: &[0; 3]
        }
        .as_usize()
        .is_none()
    );
    let mut data = FIRMWARE.to_vec();
    data.extend_from_slice(&[0xde, 0xad]);
    let tree = Tree::new(&data).unwrap();
    assert_eq!(tree.total_size(), FIRMWARE.len());
    assert_eq!(tree.raw_data(), FIRMWARE);
}
