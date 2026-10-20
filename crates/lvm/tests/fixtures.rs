//! Reads the PVs that lvm2 2.03.16 wrote (see `fixtures/README`) and checks the metadata and
//! the mappings against what `vgcfgbackup` reported for them.

use std::convert::Infallible;

use ferric_lvm::{MapError, Pv, ReadError, Uuid, VolumeGroup};

const MIB: u64 = 1 << 20;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).expect("fixture")
}

/// Reads `image`, zero-extended past its end (the fixtures stop after the metadata).
fn read(image: &[u8]) -> Result<Pv, ReadError<Infallible>> {
    Pv::read(&mut |offset: u64, buf: &mut [u8]| {
        buf.fill(0);
        let start = (offset as usize).min(image.len());
        let end = (start + buf.len()).min(image.len());
        buf[..end - start].copy_from_slice(&image[start..end]);
        Ok(())
    })
}

fn vg(name: &str) -> VolumeGroup {
    read(&fixture(name))
        .expect("valid PV")
        .vg
        .expect("metadata")
}

fn uuid(text: &str) -> Uuid {
    Uuid::parse(text).expect("UUID")
}

/// Maps `offset` of `lv` to (PV key, device offset, run length).
fn at(vg: &VolumeGroup, lv: &str, offset: u64) -> Result<(String, u64, u64), MapError> {
    let lv = vg.logical_volume(lv).expect("LV");
    vg.map(lv, offset)
        .map(|m| (vg.physical_volumes[m.pv].key.clone(), m.offset, m.len))
}

fn key(pv: &str, offset: u64, len: u64) -> Result<(String, u64, u64), MapError> {
    Ok((pv.into(), offset, len))
}

#[test]
fn two_pv_group_reads_the_same_from_either_pv() {
    let pv0 = read(&fixture("stripevg-pv0.img")).unwrap();
    let pv1 = read(&fixture("stripevg-pv1.img")).unwrap();
    assert_eq!(
        pv0.label.pv_uuid,
        uuid("l4QNdM-ZFBH-Adli-LGLH-Bqf0-fUql-GsZOc7")
    );
    assert_eq!(
        pv1.label.pv_uuid,
        uuid("fh572Z-EDI3-4mBm-uAs4-VFCU-YP5u-aO3MSG")
    );
    assert_eq!(pv0.vg, pv1.vg);
    let vg = pv0.vg.unwrap();
    assert_eq!(vg.name, "stripevg");
    assert_eq!(vg.id, uuid("FxvYx3-oQgF-LhWg-DQ4E-wmFX-sllJ-Al2Hdc"));
    assert_eq!((vg.seqno, vg.extent_bytes), (4, MIB));
    assert_eq!(vg.pv_index(&pv1.label.pv_uuid), Some(1));
    let names: Vec<_> = vg
        .logical_volumes
        .iter()
        .map(|lv| lv.name.as_str())
        .collect();
    assert_eq!(names, ["striped", "spans", "tail"]);
    for pv in &vg.physical_volumes {
        assert_eq!(
            (pv.pe_start, pv.pe_count, pv.device_bytes),
            (MIB, 63, 64 * MIB)
        );
    }
}

#[test]
fn linear_segments_span_pvs() {
    let vg = vg("stripevg-pv0.img");
    let spans = vg.logical_volume("spans").unwrap();
    assert_eq!(vg.lv_bytes(spans), 70 * MIB);
    // Segment 1: 59 extents on pv0 from PE 4; segment 2: 11 extents on pv1 from PE 4.
    assert_eq!(at(&vg, "spans", 0), key("pv0", 5 * MIB, 59 * MIB));
    assert_eq!(at(&vg, "spans", 59 * MIB - 1), key("pv0", 64 * MIB - 1, 1));
    assert_eq!(at(&vg, "spans", 59 * MIB), key("pv1", 5 * MIB, 11 * MIB));
    assert_eq!(at(&vg, "spans", 70 * MIB), Err(MapError::OutOfRange));
    assert_eq!(at(&vg, "tail", 0), key("pv1", 16 * MIB, 3 * MIB));
}

#[test]
fn striped_volume_alternates_64k_chunks() {
    let vg = vg("stripevg-pv1.img");
    let k = 64 << 10;
    assert_eq!(at(&vg, "striped", 0), key("pv0", MIB, k));
    assert_eq!(at(&vg, "striped", k), key("pv1", MIB, k));
    assert_eq!(
        at(&vg, "striped", 2 * k + 7),
        key("pv0", MIB + k + 7, k - 7)
    );
    // 8 MiB over two stripes: each stripe holds 4 MiB, the last byte on pv1.
    assert_eq!(at(&vg, "striped", 8 * MIB - 1), key("pv1", 5 * MIB - 1, 1));
}

#[test]
fn metadata_that_wraps_round_the_area_is_read() {
    let image = fixture("wrapped-mda.img");
    let pv = read(&image).unwrap();
    let vg = pv.vg.unwrap();
    assert_eq!(vg.name, "wrapvg");
    assert_eq!(vg.seqno, 19);
    assert_eq!(
        pv.label.pv_uuid,
        uuid("EN3JLp-3cVA-3sow-tA7n-r0Oe-Yxs2-vBgSHH")
    );
    assert_eq!(vg.logical_volumes.len(), 12);
    let first = vg.physical_volumes[0].pe_start;
    assert_eq!(first, 64 << 10);
    assert_eq!(
        at(&vg, "lv_with_a_long_name_number_1", 0),
        key("pv0", first, MIB)
    );
    assert_eq!(
        at(&vg, "lv_with_a_long_name_number_10", 0),
        key("pv0", first + 9 * MIB, MIB)
    );
}

#[test]
fn extended_volume_maps_its_later_extent() {
    let vg = vg("two-lvs.img");
    let e = 4 * MIB;
    assert_eq!(vg.extent_bytes, e);
    assert_eq!(at(&vg, "lv1", 0), key("pv0", MIB, 2 * e));
    assert_eq!(at(&vg, "lv1", 2 * e), key("pv0", MIB + 5 * e, e));
    assert_eq!(at(&vg, "lv2", 0), key("pv0", MIB + 2 * e, 3 * e));
}

/// Flipping a bit in any byte `Pv::read` reads either leaves the result unchanged (bytes it
/// does not depend on, such as the unused label sectors) or makes the read fail; none yields
/// different metadata.
#[test]
fn no_bit_flip_changes_the_metadata() {
    for name in ["two-lvs.img", "wrapped-mda.img", "stripevg-pv0.img"] {
        let image = fixture(name);
        let good = read(&image).unwrap();
        let mut touched = Vec::new();
        Pv::read(
            &mut |offset: u64, buf: &mut [u8]| -> Result<(), Infallible> {
                let start = offset as usize;
                touched.extend(start..start + buf.len());
                buf.copy_from_slice(&image[start..start + buf.len()]);
                Ok(())
            },
        )
        .unwrap();
        touched.sort_unstable();
        touched.dedup();
        let mut failed = 0;
        let mut corrupt = image.clone();
        // One bit per byte, a different one in each byte, keeps the debug-build run short.
        for &byte in &touched {
            let bit = 1 << (byte % 8);
            corrupt[byte] ^= bit;
            match read(&corrupt) {
                Ok(pv) => assert_eq!(pv, good, "{name}: byte {byte} changed the metadata"),
                Err(_) => failed += 1,
            }
            corrupt[byte] ^= bit;
        }
        // Everything but the three label sectors that hold no label is checksummed.
        assert!(
            failed >= touched.len() - 3 * 512,
            "{name}: {failed} of {}",
            touched.len()
        );
    }
}

#[test]
fn a_blank_device_has_no_label() {
    assert!(matches!(
        read(&[0; 4096]),
        Err(ReadError::Label(ferric_lvm::LabelError::NotFound))
    ));
}

/// Rewrites the PV label of `image` with `device_size` and `metadata_areas`, keeping its data
/// areas, and reseals its checksum.
fn relabel(image: &mut [u8], device_size: u64, metadata_areas: &[ferric_lvm::Area]) {
    let label = read(image).expect("valid PV").label;
    let sector = &mut image[usize::from(label.sector) * 512..][..512];
    let header = u32::from_le_bytes(sector[20..24].try_into().unwrap()) as usize;
    let mut at = header + 32;
    sector[at..at + 8].copy_from_slice(&device_size.to_le_bytes());
    at += 8;
    let terminator = ferric_lvm::Area { offset: 0, size: 0 };
    let areas = label.data_areas.iter().chain([&terminator]);
    for area in areas.chain(metadata_areas).chain([&terminator]) {
        sector[at..at + 8].copy_from_slice(&area.offset.to_le_bytes());
        sector[at + 8..at + 16].copy_from_slice(&area.size.to_le_bytes());
        at += 16;
    }
    let sum = ferric_lvm::crc::checksum(&sector[20..]);
    sector[16..20].copy_from_slice(&sum.to_le_bytes());
}

#[test]
fn a_metadata_area_past_the_device_is_refused() {
    let mut image = fixture("two-lvs.img");
    let label = read(&image).unwrap().label;
    let area = label.metadata_areas[0];
    relabel(&mut image, area.offset + area.size - 1, &[area]);
    assert!(matches!(
        read(&image),
        Err(ReadError::Metadata {
            area: 0,
            source: ferric_lvm::MetadataError::Mda(ferric_lvm::MdaError::PastDevice { .. }),
        })
    ));
}

/// An empty area next to a failed one is an error, not "no group": the failed one may have
/// held the only metadata.
#[test]
fn an_empty_area_does_not_hide_a_failed_one() {
    let mut image = fixture("two-lvs.img");
    let label = read(&image).unwrap().label;
    let area = label.metadata_areas[0];
    // Empty the real area: clear its raw locations and reseal its header.
    let header = &mut image[area.offset as usize..][..512];
    header[40..136].fill(0);
    let sum = ferric_lvm::crc::checksum(&header[4..]);
    header[..4].copy_from_slice(&sum.to_le_bytes());
    assert_eq!(read(&image).unwrap().vg, None);
    // A second area with no header at all (zeros, past the fixture's end) fails.
    let broken = ferric_lvm::Area {
        offset: area.offset + area.size,
        size: area.size,
    };
    relabel(&mut image, label.device_size, &[area, broken]);
    assert!(matches!(
        read(&image),
        Err(ReadError::Metadata {
            area: 1,
            source: ferric_lvm::MetadataError::Mda(ferric_lvm::MdaError::Checksum),
        })
    ));
}
