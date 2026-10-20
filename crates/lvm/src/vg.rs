//! Typed volume-group metadata, built from the parsed text and checked for consistency
//! (libvslvm "Logical Volume Manager (LVM) format", section 4; lvcreate(8) and lvmraid(7) for
//! the meaning of the striped segment fields).
//!
//! The text holds one volume-group section, named after the group, beside `contents`,
//! `version` and descriptive items. The group lists its physical volumes, each with an extent
//! range starting at `pe_start`, and its logical volumes, each a run of segments. A segment of
//! type `striped` with one stripe is a linear mapping; with more, extents are interleaved across
//! the stripes in chunks of `stripe_size`. Other segment types (mirrors, RAID, thin and cache
//! volumes, snapshots) are kept by name so callers can list them, but they cannot be mapped.
//!
//! All sizes in the text are in 512-byte sectors.

use alloc::string::String;
use alloc::vec::Vec;

use thiserror::Error;

use crate::label::Uuid;
use crate::text::{Section, Value};

/// The `contents` value of volume-group metadata.
pub const CONTENTS: &str = "Text Format Volume Group";
/// The sector size the metadata counts in.
pub const SECTOR_BYTES: u64 = 512;

/// A volume group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeGroup {
    /// The group's name.
    pub name: String,
    /// The group's UUID.
    pub id: Uuid,
    /// The metadata sequence number; higher is newer.
    pub seqno: u64,
    /// The extent size in bytes.
    pub extent_bytes: u64,
    /// The physical volumes, in metadata order.
    pub physical_volumes: Vec<PhysicalVolume>,
    /// The logical volumes, in metadata order.
    pub logical_volumes: Vec<LogicalVolume>,
}

/// A physical volume as the group's metadata records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalVolume {
    /// The name segments refer to it by, such as `pv0`.
    pub key: String,
    /// The PV UUID, which matches the label on the device.
    pub id: Uuid,
    /// The device size in bytes.
    pub device_bytes: u64,
    /// Where the first extent starts on the device, in bytes.
    pub pe_start: u64,
    /// How many extents the PV holds.
    pub pe_count: u64,
}

/// A logical volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalVolume {
    /// The volume's name.
    pub name: String,
    /// The volume's UUID.
    pub id: Uuid,
    /// The status flags, such as `READ`, `WRITE` and `VISIBLE`.
    pub status: Vec<String>,
    /// The segments, contiguous from extent 0.
    pub segments: Vec<Segment>,
}

/// A run of logical extents with one mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The first logical extent.
    pub start_extent: u64,
    /// How many logical extents the segment covers.
    pub extent_count: u64,
    /// How the extents are mapped.
    pub kind: SegmentKind,
}

/// How a segment's extents are mapped.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentKind {
    /// Linear (one stripe) or striped across several PVs.
    Striped {
        /// The chunk written to one stripe before moving to the next, in bytes; `None` for a
        /// single stripe.
        stripe_bytes: Option<u64>,
        /// The stripes, in order.
        stripes: Vec<Stripe>,
    },
    /// A segment type this crate does not map, by its metadata name.
    Other(String),
}

/// One stripe of a striped segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stripe {
    /// The index of the PV in [`VolumeGroup::physical_volumes`].
    pub pv: usize,
    /// The first physical extent on that PV.
    pub start_pe: u64,
}

impl VolumeGroup {
    /// Builds a volume group from parsed metadata text.
    ///
    /// # Errors
    ///
    /// A [`VgError`] naming the first item that is missing, mistyped or inconsistent.
    pub fn from_text(root: &Section) -> Result<Self, VgError> {
        if root.get("contents") != Some(&Value::Str(CONTENTS.into())) {
            return Err(VgError::Contents);
        }
        match root.get("version") {
            Some(Value::Int(1)) => {}
            _ => return Err(VgError::Version),
        }
        let mut groups = root.sections();
        let (name, vg) = groups.next().ok_or(VgError::GroupCount)?;
        if groups.next().is_some() {
            return Err(VgError::GroupCount);
        }
        let at = Path::new(name);
        let extent_sectors = uint(vg, &at, "extent_size")?;
        if extent_sectors == 0 {
            return Err(at.error("extent_size", Problem::Range));
        }
        let extent_bytes = bytes(extent_sectors, &at, "extent_size")?;
        let physical_volumes = physical_volumes(vg, &at, extent_bytes)?;
        let logical_volumes = match vg.get("logical_volumes") {
            None => Vec::new(),
            Some(Value::Section(lvs)) => lvs
                .sections()
                .map(|(lv_name, lv)| {
                    logical_volume(lv_name, lv, &physical_volumes, extent_bytes, &at)
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(at.error("logical_volumes", Problem::Type)),
        };
        Ok(Self {
            name: name.into(),
            id: uuid(vg, &at)?,
            seqno: uint(vg, &at, "seqno")?,
            extent_bytes,
            physical_volumes,
            logical_volumes,
        })
    }

    /// The index of the PV with UUID `id`.
    pub fn pv_index(&self, id: &Uuid) -> Option<usize> {
        self.physical_volumes.iter().position(|pv| pv.id == *id)
    }

    /// The logical volume named `name`.
    pub fn logical_volume(&self, name: &str) -> Option<&LogicalVolume> {
        self.logical_volumes.iter().find(|lv| lv.name == name)
    }
}

impl LogicalVolume {
    /// How many logical extents the volume has.
    pub fn extents(&self) -> u64 {
        self.segments
            .last()
            .map_or(0, |s| s.start_extent + s.extent_count)
    }
}

fn physical_volumes(
    vg: &Section,
    at: &Path<'_>,
    extent_bytes: u64,
) -> Result<Vec<PhysicalVolume>, VgError> {
    let pvs = match vg.get("physical_volumes") {
        Some(Value::Section(pvs)) => pvs,
        Some(_) => return Err(at.error("physical_volumes", Problem::Type)),
        None => return Err(at.error("physical_volumes", Problem::Missing)),
    };
    let mut out: Vec<PhysicalVolume> = Vec::new();
    for (key, pv) in pvs.sections() {
        let at = at.join(key);
        let pv = PhysicalVolume {
            key: key.into(),
            id: uuid(pv, &at)?,
            device_bytes: bytes(uint(pv, &at, "dev_size")?, &at, "dev_size")?,
            pe_start: bytes(uint(pv, &at, "pe_start")?, &at, "pe_start")?,
            pe_count: uint(pv, &at, "pe_count")?,
        };
        let end = pv
            .pe_count
            .checked_mul(extent_bytes)
            .and_then(|len| len.checked_add(pv.pe_start));
        if end.is_none_or(|end| end > pv.device_bytes) {
            return Err(at.error("pe_count", Problem::Range));
        }
        if out.iter().any(|other| other.id == pv.id) {
            return Err(at.error("id", Problem::Duplicate));
        }
        out.push(pv);
    }
    Ok(out)
}

fn logical_volume(
    name: &str,
    lv: &Section,
    pvs: &[PhysicalVolume],
    extent_bytes: u64,
    vg: &Path<'_>,
) -> Result<LogicalVolume, VgError> {
    let at = vg.join(name);
    let status = match lv.get("status") {
        None => Vec::new(),
        Some(Value::List(items)) => items
            .iter()
            .map(|item| match item {
                Value::Str(s) => Ok(s.clone()),
                _ => Err(at.error("status", Problem::Type)),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(at.error("status", Problem::Type)),
    };
    let mut segments = Vec::new();
    let mut next = 0u64;
    for (segment_name, segment) in lv.sections() {
        let at = at.join(segment_name);
        let segment = parse_segment(segment, &at, pvs, extent_bytes)?;
        if segment.start_extent != next {
            return Err(at.error("start_extent", Problem::Range));
        }
        next = segment
            .start_extent
            .checked_add(segment.extent_count)
            .ok_or(at.error("extent_count", Problem::Range))?;
        segments.push(segment);
    }
    if uint(lv, &at, "segment_count")? != segments.len() as u64 {
        return Err(at.error("segment_count", Problem::Range));
    }
    Ok(LogicalVolume {
        name: name.into(),
        id: uuid(lv, &at)?,
        status,
        segments,
    })
}

fn parse_segment(
    segment: &Section,
    at: &Path<'_>,
    pvs: &[PhysicalVolume],
    extent_bytes: u64,
) -> Result<Segment, VgError> {
    let start_extent = uint(segment, at, "start_extent")?;
    let extent_count = uint(segment, at, "extent_count")?;
    if extent_count == 0 {
        return Err(at.error("extent_count", Problem::Range));
    }
    let kind = match string(segment, at, "type")? {
        "striped" => striped(segment, at, pvs, extent_count, extent_bytes)?,
        other => SegmentKind::Other(other.into()),
    };
    Ok(Segment {
        start_extent,
        extent_count,
        kind,
    })
}

fn striped(
    segment: &Section,
    at: &Path<'_>,
    pvs: &[PhysicalVolume],
    extent_count: u64,
    extent_bytes: u64,
) -> Result<SegmentKind, VgError> {
    let stripe_count = uint(segment, at, "stripe_count")?;
    let list = match segment.get("stripes") {
        Some(Value::List(list)) => list,
        Some(_) => return Err(at.error("stripes", Problem::Type)),
        None => return Err(at.error("stripes", Problem::Missing)),
    };
    if stripe_count == 0 || list.len() as u64 != stripe_count * 2 {
        return Err(at.error("stripe_count", Problem::Range));
    }
    if !extent_count.is_multiple_of(stripe_count) {
        return Err(at.error("extent_count", Problem::Range));
    }
    let per_stripe = extent_count / stripe_count;
    let mut stripes = Vec::new();
    for pair in list.chunks_exact(2) {
        let (Value::Str(key), Value::Int(start_pe)) = (&pair[0], &pair[1]) else {
            return Err(at.error("stripes", Problem::Type));
        };
        let pv = pvs
            .iter()
            .position(|pv| pv.key == *key)
            .ok_or(at.error("stripes", Problem::UnknownPv))?;
        let start_pe = u64::try_from(*start_pe).map_err(|_| at.error("stripes", Problem::Range))?;
        if start_pe
            .checked_add(per_stripe)
            .is_none_or(|end| end > pvs[pv].pe_count)
        {
            return Err(at.error("stripes", Problem::Range));
        }
        stripes.push(Stripe { pv, start_pe });
    }
    let stripe_bytes = if stripe_count > 1 {
        let size = bytes(uint(segment, at, "stripe_size")?, at, "stripe_size")?;
        // dm-stripe needs whole chunks in each extent; lvcreate(8) only makes power-of-two
        // stripe sizes no larger than the extent.
        if size == 0 || !size.is_power_of_two() || !extent_bytes.is_multiple_of(size) {
            return Err(at.error("stripe_size", Problem::Range));
        }
        Some(size)
    } else {
        None
    };
    Ok(SegmentKind::Striped {
        stripe_bytes,
        stripes,
    })
}

/// Where in the metadata an item is: the section names leading to it.
struct Path<'a> {
    parent: Option<&'a Path<'a>>,
    name: &'a str,
}

impl<'a> Path<'a> {
    fn new(name: &'a str) -> Self {
        Self { parent: None, name }
    }

    fn join(&'a self, name: &'a str) -> Path<'a> {
        Path {
            parent: Some(self),
            name,
        }
    }

    fn sections(&self) -> Vec<String> {
        let mut out = self.parent.map_or_else(Vec::new, Path::sections);
        out.push(self.name.into());
        out
    }

    fn error(&self, item: &'static str, problem: Problem) -> VgError {
        VgError::Item {
            sections: self.sections(),
            item,
            problem,
        }
    }
}

fn uint(section: &Section, at: &Path<'_>, item: &'static str) -> Result<u64, VgError> {
    match section.get(item) {
        Some(Value::Int(n)) => u64::try_from(*n).map_err(|_| at.error(item, Problem::Range)),
        Some(_) => Err(at.error(item, Problem::Type)),
        None => Err(at.error(item, Problem::Missing)),
    }
}

fn string<'s>(section: &'s Section, at: &Path<'_>, item: &'static str) -> Result<&'s str, VgError> {
    match section.get(item) {
        Some(Value::Str(s)) => Ok(s),
        Some(_) => Err(at.error(item, Problem::Type)),
        None => Err(at.error(item, Problem::Missing)),
    }
}

fn uuid(section: &Section, at: &Path<'_>) -> Result<Uuid, VgError> {
    Uuid::parse(string(section, at, "id")?).ok_or(at.error("id", Problem::Uuid))
}

fn bytes(sectors: u64, at: &Path<'_>, item: &'static str) -> Result<u64, VgError> {
    sectors
        .checked_mul(SECTOR_BYTES)
        .ok_or(at.error(item, Problem::Range))
}

/// Volume-group metadata that is not usable.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum VgError {
    /// `contents` is not [`CONTENTS`].
    #[error("the LVM metadata is not volume-group text")]
    Contents,
    /// `version` is not 1.
    #[error("the LVM metadata text version is not 1")]
    Version,
    /// The text does not hold exactly one volume-group section.
    #[error("the LVM metadata does not hold exactly one volume group")]
    GroupCount,
    /// An item is missing, mistyped or inconsistent.
    #[error("LVM metadata item {item} in {} is not valid: {problem}", sections.join("/"))]
    Item {
        /// The sections leading to the item, from the volume group down.
        sections: Vec<String>,
        /// The item's name.
        item: &'static str,
        /// What is wrong with it.
        problem: Problem,
    },
}

/// What is wrong with a metadata item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Problem {
    /// The item is required but absent.
    #[error("missing")]
    Missing,
    /// The item has the wrong kind of value.
    #[error("wrong type")]
    Type,
    /// The value is out of range or does not agree with the rest of the metadata.
    #[error("out of range or inconsistent")]
    Range,
    /// The value is not a valid UUID.
    #[error("not a UUID")]
    Uuid,
    /// The value repeats one that must be unique.
    #[error("duplicated")]
    Duplicate,
    /// A stripe names a PV the group does not list.
    #[error("names an unknown PV")]
    UnknownPv,
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;

    use super::*;
    use crate::text;

    /// A two-PV group with a linear, a striped and a mirrored volume.
    pub(crate) const TEXT: &str = r#"
demo {
    id = "FxvYx3-oQgF-LhWg-DQ4E-wmFX-sllJ-Al2Hdc"
    seqno = 7
    extent_size = 8192
    physical_volumes {
        pv0 { id = "l4QNdM-ZFBH-Adli-LGLH-Bqf0-fUql-GsZOc7" dev_size = 131072 pe_start = 2048 pe_count = 15 }
        pv1 { id = "fh572Z-EDI3-4mBm-uAs4-VFCU-YP5u-aO3MSG" dev_size = 131072 pe_start = 2048 pe_count = 15 }
    }
    logical_volumes {
        linear {
            id = "3i3wAn-PZWM-e92G-374v-YxN1-S554-huv2vK"
            status = ["READ", "VISIBLE"]
            segment_count = 2
            segment1 { start_extent = 0 extent_count = 2 type = "striped" stripe_count = 1 stripes = ["pv0", 0] }
            segment2 { start_extent = 2 extent_count = 3 type = "striped" stripe_count = 1 stripes = ["pv1", 10] }
        }
        wide {
            id = "mt40An-sliz-05Zs-YrVA-VYUr-wIpq-Ixn73e"
            segment_count = 1
            segment1 { start_extent = 0 extent_count = 4 type = "striped" stripe_count = 2 stripe_size = 128 stripes = ["pv0", 2, "pv1", 0] }
        }
        copy {
            id = "UW2rcG-t7O3-bDyl-T5Gp-1wrV-VHdl-ipAoZl"
            segment_count = 1
            segment1 { start_extent = 0 extent_count = 1 type = "raid1" }
        }
    }
}
contents = "Text Format Volume Group"
version = 1
"#;

    pub(crate) fn demo() -> VolumeGroup {
        VolumeGroup::from_text(&text::parse(TEXT.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn a_group_is_typed() {
        let vg = demo();
        assert_eq!(
            (vg.name.as_str(), vg.seqno, vg.extent_bytes),
            ("demo", 7, 4 << 20)
        );
        assert_eq!(vg.physical_volumes[1].key, "pv1");
        assert_eq!(vg.physical_volumes[1].pe_start, 1 << 20);
        assert_eq!(vg.pv_index(&vg.physical_volumes[1].id), Some(1));
        let linear = vg.logical_volume("linear").unwrap();
        assert_eq!(linear.status, ["READ", "VISIBLE"]);
        assert_eq!(linear.extents(), 5);
        assert_eq!(
            linear.segments[1].kind,
            SegmentKind::Striped {
                stripe_bytes: None,
                stripes: vec![Stripe {
                    pv: 1,
                    start_pe: 10
                }],
            }
        );
        assert_eq!(
            vg.logical_volume("wide").unwrap().segments[0].kind,
            SegmentKind::Striped {
                stripe_bytes: Some(64 << 10),
                stripes: vec![Stripe { pv: 0, start_pe: 2 }, Stripe { pv: 1, start_pe: 0 }],
            }
        );
        assert_eq!(
            vg.logical_volume("copy").unwrap().segments[0].kind,
            SegmentKind::Other("raid1".into())
        );
    }

    fn error_for(from: &str, to: &str) -> VgError {
        assert!(TEXT.contains(from), "{from}");
        let text = TEXT.replacen(from, to, 1);
        VolumeGroup::from_text(&text::parse(text.as_bytes()).unwrap()).unwrap_err()
    }

    fn item(sections: &[&str], item: &'static str, problem: Problem) -> VgError {
        VgError::Item {
            sections: sections.iter().map(|s| s.to_string()).collect(),
            item,
            problem,
        }
    }

    #[test]
    fn inconsistent_metadata_is_named() {
        let wide1 = ["demo", "wide", "segment1"];
        let cases = [
            ("Text Format", "Other Format", VgError::Contents),
            ("version = 1", "version = 2", VgError::Version),
            (
                "extent_size = 8192",
                "extent_size = 0",
                item(&["demo"], "extent_size", Problem::Range),
            ),
            (
                "seqno = 7",
                "seqno = -7",
                item(&["demo"], "seqno", Problem::Range),
            ),
            (
                "pe_count = 15",
                "pe_count = 16",
                item(&["demo", "pv0"], "pe_count", Problem::Range),
            ),
            (
                "fh572Z-EDI3",
                "fh572Z-EDI",
                item(&["demo", "pv1"], "id", Problem::Uuid),
            ),
            (
                "pe_start = 2048 pe_count = 15 }\n",
                "pe_count = 15 }\n",
                item(&["demo", "pv0"], "pe_start", Problem::Missing),
            ),
            (
                "start_extent = 2",
                "start_extent = 3",
                item(
                    &["demo", "linear", "segment2"],
                    "start_extent",
                    Problem::Range,
                ),
            ),
            (
                "segment_count = 2",
                "segment_count = 1",
                item(&["demo", "linear"], "segment_count", Problem::Range),
            ),
            (
                "\"pv1\", 10",
                "\"pv9\", 10",
                item(
                    &["demo", "linear", "segment2"],
                    "stripes",
                    Problem::UnknownPv,
                ),
            ),
            (
                "\"pv1\", 10",
                "\"pv1\", 13",
                item(&["demo", "linear", "segment2"], "stripes", Problem::Range),
            ),
            (
                "\"pv1\", 10",
                "10, \"pv1\"",
                item(&["demo", "linear", "segment2"], "stripes", Problem::Type),
            ),
            (
                "extent_count = 4",
                "extent_count = 3",
                item(&wide1, "extent_count", Problem::Range),
            ),
            (
                "stripe_count = 2",
                "stripe_count = 3",
                item(&wide1, "stripe_count", Problem::Range),
            ),
            (
                "stripe_size = 128",
                "stripe_size = 96",
                item(&wide1, "stripe_size", Problem::Range),
            ),
            (
                "stripe_size = 128",
                "",
                item(&wide1, "stripe_size", Problem::Missing),
            ),
            (
                "type = \"raid1\"",
                "type = 1",
                item(&["demo", "copy", "segment1"], "type", Problem::Type),
            ),
        ];
        for (from, to, expected) in cases {
            assert_eq!(error_for(from, to), expected, "{from} -> {to}");
        }
        assert_eq!(
            error_for(
                "fh572Z-EDI3-4mBm-uAs4-VFCU-YP5u-aO3MSG",
                "l4QNdM-ZFBH-Adli-LGLH-Bqf0-fUql-GsZOc7"
            ),
            item(&["demo", "pv1"], "id", Problem::Duplicate)
        );
        let two = format!("{TEXT}\nother {{ }}");
        assert_eq!(
            VolumeGroup::from_text(&text::parse(two.as_bytes()).unwrap()),
            Err(VgError::GroupCount)
        );
        assert_eq!(
            item(&wide1, "stripe_size", Problem::Range).to_string(),
            "LVM metadata item stripe_size in demo/wide/segment1 is not valid: out of range or inconsistent"
        );
    }
}
