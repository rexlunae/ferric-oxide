//! Read-only LVM2: physical-volume labels, volume-group metadata, and the mapping from
//! logical-volume offsets to physical-volume offsets.
//!
//! This crate is written from published documentation, not from LVM2 or Linux source: the
//! libvslvm "Logical Volume Manager (LVM) format" description for the on-disk structures and
//! the metadata text, and the lvm2 manual pages pvck(8), lvcreate(8) and lvmraid(7) for the
//! meaning of metadata areas and striped segments.
//!
//! The pieces fit together as follows. [`Pv::read`] reads the start of a device, finds the
//! [`PvLabel`] in one of its first four sectors, reads each metadata area the label lists
//! ([`MdaHeader`]), and checks and parses the newest metadata text ([`text`]) into a typed
//! [`VolumeGroup`]. Every structure is checksummed, and every checksum is verified. With the
//! group of each device in hand, [`VolumeGroup::map`] turns an offset in a [`LogicalVolume`] into
//! a PV index and an offset on that PV's device. Linear and striped segments are mapped; other
//! segment types (mirrors, RAID, thin pools, caches, snapshots) are listed but refused.
//!
//! Devices are read through a caller-supplied `read_at(offset, buffer)` function, so the crate
//! works on any block device, image file or in-memory buffer without `std`.
//!
//! # Examples
//!
//! ```
//! # fn main() -> Result<(), Box<dyn core::error::Error>> {
//! let image = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/two-lvs.img"))?;
//! let mut read_at = |offset: u64, buf: &mut [u8]| -> Result<(), core::convert::Infallible> {
//!     let start = offset as usize;
//!     buf.copy_from_slice(&image[start..start + buf.len()]);
//!     Ok(())
//! };
//! let pv = ferric_lvm::Pv::read(&mut read_at)?;
//! let vg = pv.vg.expect("this PV holds metadata");
//! let lv = vg.logical_volume("lv1").expect("the image has lv1");
//! // lv1's third extent was added later, after lv2, at physical extent 5.
//! let mapping = vg.map(lv, 2 * vg.extent_bytes)?;
//! assert_eq!(mapping.offset, (1 << 20) + 5 * vg.extent_bytes);
//! # Ok(())
//! # }
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod crc;
pub mod label;
pub mod map;
pub mod mda;
pub mod text;
pub mod vg;

use alloc::vec::Vec;

use thiserror::Error;

pub use label::{Area, LabelError, PvLabel, Uuid};
pub use map::{MapError, Mapping};
pub use mda::{MdaError, MdaHeader, RawLocation};
pub use text::TextError;
pub use vg::{LogicalVolume, PhysicalVolume, Segment, SegmentKind, Stripe, VgError, VolumeGroup};

use label::{LABEL_SECTORS, SECTOR};

/// One physical volume: its label and, when it holds metadata, its volume group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pv {
    /// The label.
    pub label: PvLabel,
    /// The newest volume-group metadata in this PV's metadata areas, or `None` when it has no
    /// metadata areas or they are empty (a PV whose group keeps metadata on other PVs, or one
    /// in no group).
    pub vg: Option<VolumeGroup>,
}

impl Pv {
    /// Reads a physical volume's label and metadata through `read_at`.
    ///
    /// Metadata areas that fail to read or check are skipped as long as another one is good,
    /// since LVM keeps a second copy for that reason; the newest good copy (highest `seqno`)
    /// is used.
    ///
    /// # Errors
    ///
    /// - [`ReadError::Io`] when the label sectors cannot be read.
    /// - [`ReadError::Label`] when there is no valid label.
    /// - [`ReadError::Metadata`] with the first area's error when no metadata area is good.
    pub fn read<F, E>(read_at: &mut F) -> Result<Self, ReadError<E>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), E>,
    {
        let mut start = [0u8; SECTOR * LABEL_SECTORS];
        read_at(0, &mut start).map_err(|source| ReadError::Io { offset: 0, source })?;
        let label = PvLabel::find(&start)?;
        let mut newest: Option<VolumeGroup> = None;
        let mut first_error = None;
        let mut empty = 0;
        for (index, area) in label.metadata_areas.iter().enumerate() {
            match read_area(read_at, area, &label) {
                Ok(Some(vg)) => {
                    if newest.as_ref().is_none_or(|n| vg.seqno > n.seqno) {
                        newest = Some(vg);
                    }
                }
                Ok(None) => empty += 1,
                Err(source) => {
                    first_error.get_or_insert(ReadError::Metadata {
                        area: index,
                        source,
                    });
                }
            }
        }
        match (newest, first_error) {
            (None, Some(error)) if empty == 0 => Err(error),
            (vg, _) => Ok(Self { label, vg }),
        }
    }
}

/// Reads one metadata area: `None` when it holds no metadata.
fn read_area<F, E>(
    read_at: &mut F,
    area: &Area,
    label: &PvLabel,
) -> Result<Option<VolumeGroup>, MetadataError<E>>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), E>,
{
    let mut sector = [0u8; SECTOR];
    read_at(area.offset, &mut sector).map_err(|source| MetadataError::Io {
        offset: area.offset,
        source,
    })?;
    let header = MdaHeader::parse(&sector, area.offset)?;
    if header.size != area.size {
        return Err(MetadataError::Mda(MdaError::Size(header.size)));
    }
    let Some(location) = header.current() else {
        return Ok(None);
    };
    let text: Vec<u8> = header.read_text(read_at, location).map_err(|e| match e {
        mda::TextReadError::Mda(e) => MetadataError::Mda(e),
        mda::TextReadError::Io { offset, source } => MetadataError::Io { offset, source },
    })?;
    let vg = VolumeGroup::from_text(&text::parse(&text)?)?;
    if vg.pv_index(&label.pv_uuid).is_none() {
        return Err(MetadataError::NotInGroup);
    }
    Ok(Some(vg))
}

/// A physical volume that could not be read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReadError<E> {
    /// Reading the label sectors failed.
    #[error("reading the LVM label at {offset:#x} failed")]
    Io {
        /// Where the read started.
        offset: u64,
        /// The read error.
        #[source]
        source: E,
    },
    /// There is no valid label.
    #[error(transparent)]
    Label(#[from] LabelError),
    /// No metadata area is good; this is the first area's error.
    #[error("LVM metadata area {area} is not usable")]
    Metadata {
        /// The index of the area in [`PvLabel::metadata_areas`].
        area: usize,
        /// What is wrong with it.
        #[source]
        source: MetadataError<E>,
    },
}

/// A metadata area that is not usable.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MetadataError<E> {
    /// A read failed.
    #[error("reading LVM metadata at {offset:#x} failed")]
    Io {
        /// Where the read started.
        offset: u64,
        /// The read error.
        #[source]
        source: E,
    },
    /// The header or text location is not valid, or the text checksum does not match.
    #[error(transparent)]
    Mda(#[from] MdaError),
    /// The text does not parse.
    #[error(transparent)]
    Text(#[from] TextError),
    /// The parsed metadata is not a consistent volume group.
    #[error(transparent)]
    Vg(#[from] VgError),
    /// The group's metadata does not list this PV.
    #[error("the LVM metadata does not list this PV")]
    NotInGroup,
}
