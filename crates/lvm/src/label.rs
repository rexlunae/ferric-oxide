//! The physical-volume label: the label header and PV header in one 512-byte sector among the
//! first four of the device (libvslvm "Logical Volume Manager (LVM) format", section 2; pvck(8)).
//!
//! The label header is `LABELONE`, the sector's own number, a checksum of bytes 20..512, the
//! offset of the PV header, and the type `LVM2 001`. The PV header holds the PV UUID, the device
//! size, and two zero-terminated lists of (offset, size) areas: data areas, then metadata areas.
//! All integers are little-endian.

use alloc::vec::Vec;
use core::fmt;

use thiserror::Error;

use crate::crc;

/// The sector size LVM addresses in.
pub const SECTOR: usize = 512;
/// How many sectors from the start of the device may hold the label.
pub const LABEL_SECTORS: usize = 4;
/// The label header's identifier.
pub const LABEL_ID: [u8; 8] = *b"LABELONE";
/// The label type of LVM2 physical volumes.
pub const LABEL_TYPE: [u8; 8] = *b"LVM2 001";
/// The length of a PV or VG UUID without dashes.
pub const UUID_LEN: usize = 32;

/// A PV, VG or LV UUID: 32 characters, written in metadata with dashes as 6-4-4-4-4-4-6.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid([u8; UUID_LEN]);

/// Where the dashes go in the written form.
const GROUPS: [usize; 7] = [6, 4, 4, 4, 4, 4, 6];

impl Uuid {
    /// A UUID from its 32 undashed characters, which must be ASCII letters and digits.
    pub fn from_raw(raw: [u8; UUID_LEN]) -> Option<Self> {
        raw.iter()
            .all(u8::is_ascii_alphanumeric)
            .then_some(Self(raw))
    }

    /// A UUID from its dashed form, as metadata writes it.
    pub fn parse(text: &str) -> Option<Self> {
        let mut raw = [0u8; UUID_LEN];
        let mut groups = text.split('-');
        let mut at = 0;
        for len in GROUPS {
            let group = groups.next()?;
            if group.len() != len {
                return None;
            }
            raw[at..at + len].copy_from_slice(group.as_bytes());
            at += len;
        }
        if groups.next().is_some() {
            return None;
        }
        Self::from_raw(raw)
    }

    /// The 32 undashed characters.
    pub fn as_bytes(&self) -> &[u8; UUID_LEN] {
        &self.0
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut at = 0;
        for (i, len) in GROUPS.into_iter().enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            for &b in &self.0[at..at + len] {
                write!(f, "{}", char::from(b))?;
            }
            at += len;
        }
        Ok(())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

/// A byte range of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Area {
    /// The offset from the start of the device, in bytes.
    pub offset: u64,
    /// The size in bytes; 0 for a data area means "to the end of the device".
    pub size: u64,
}

/// A parsed PV label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PvLabel {
    /// Which of the first four sectors held it.
    pub sector: u8,
    /// The PV UUID.
    pub pv_uuid: Uuid,
    /// The device size recorded at `pvcreate`, in bytes.
    pub device_size: u64,
    /// The data areas (normally one, starting at the first extent).
    pub data_areas: Vec<Area>,
    /// The metadata areas (zero, one at the start, or two with one at the end).
    pub metadata_areas: Vec<Area>,
}

impl PvLabel {
    /// Finds and parses the label in the first [`LABEL_SECTORS`] sectors of the device.
    ///
    /// # Errors
    ///
    /// [`LabelError::NotFound`] when no sector starts with `LABELONE`, or the error of the
    /// first sector that does but is not a valid label.
    pub fn find(start: &[u8; SECTOR * LABEL_SECTORS]) -> Result<Self, LabelError> {
        let mut first_error = None;
        for (index, sector) in start.chunks_exact(SECTOR).enumerate() {
            if sector[..8] != LABEL_ID {
                continue;
            }
            match Self::parse(sector.try_into().expect("512-byte chunk"), index as u8) {
                Ok(label) => return Ok(label),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        Err(first_error.unwrap_or(LabelError::NotFound))
    }

    /// Parses the label in `sector`, which is sector `index` of the device.
    ///
    /// # Errors
    ///
    /// A [`LabelError`] naming the first field that is not valid.
    pub fn parse(sector: &[u8; SECTOR], index: u8) -> Result<Self, LabelError> {
        if sector[..8] != LABEL_ID {
            return Err(LabelError::NotFound);
        }
        let recorded = le64(sector, 8);
        if recorded != u64::from(index) {
            return Err(LabelError::SectorNumber { recorded });
        }
        if le32(sector, 16) != crc::checksum(&sector[20..]) {
            return Err(LabelError::Checksum);
        }
        if sector[24..32] != LABEL_TYPE {
            return Err(LabelError::Type);
        }
        let header = le32(sector, 20) as usize;
        // The PV header needs its UUID, size and two list terminators at least.
        if header < 32 || header + UUID_LEN + 8 + 32 > SECTOR {
            return Err(LabelError::HeaderOffset(header as u32));
        }
        let pv_uuid = Uuid::from_raw(sector[header..header + UUID_LEN].try_into().expect("32"))
            .ok_or(LabelError::Uuid)?;
        let device_size = le64(sector, header + UUID_LEN);
        let mut at = header + UUID_LEN + 8;
        let data_areas = areas(sector, &mut at)?;
        let metadata_areas = areas(sector, &mut at)?;
        Ok(Self {
            sector: index,
            pv_uuid,
            device_size,
            data_areas,
            metadata_areas,
        })
    }
}

/// Reads a zero-terminated list of areas starting at `*at`, leaving `*at` after the terminator.
fn areas(sector: &[u8], at: &mut usize) -> Result<Vec<Area>, LabelError> {
    let mut out = Vec::new();
    loop {
        if *at + 16 > sector.len() {
            return Err(LabelError::Unterminated);
        }
        let area = Area {
            offset: le64(sector, *at),
            size: le64(sector, *at + 8),
        };
        *at += 16;
        if area.offset == 0 && area.size == 0 {
            return Ok(out);
        }
        out.push(area);
    }
}

pub(crate) fn le32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
}

pub(crate) fn le64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("8 bytes"))
}

/// A PV label that is missing or not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LabelError {
    /// No sector among the first four starts with `LABELONE`.
    #[error("no LVM label in the first four sectors")]
    NotFound,
    /// The label's recorded sector number is not the sector it is in.
    #[error("the LVM label records sector {recorded}, not the one it is in")]
    SectorNumber {
        /// The recorded number.
        recorded: u64,
    },
    /// The label checksum does not match.
    #[error("the LVM label checksum does not match")]
    Checksum,
    /// The label type is not `LVM2 001`.
    #[error("the LVM label type is not LVM2 001")]
    Type,
    /// The PV header offset leaves no room for the PV header.
    #[error("the LVM PV header offset {0} is not valid")]
    HeaderOffset(u32),
    /// The PV UUID is not 32 ASCII letters and digits.
    #[error("the LVM PV UUID is not valid")]
    Uuid,
    /// An area list runs off the end of the sector.
    #[error("an LVM PV header area list is not terminated")]
    Unterminated,
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::string::ToString;

    use super::*;

    /// A valid label in sector 1 with one data area and one metadata area.
    pub(crate) fn sector() -> [u8; SECTOR] {
        let mut s = [0u8; SECTOR];
        s[..8].copy_from_slice(&LABEL_ID);
        s[8..16].copy_from_slice(&1u64.to_le_bytes());
        s[20..24].copy_from_slice(&32u32.to_le_bytes());
        s[24..32].copy_from_slice(&LABEL_TYPE);
        s[32..64].copy_from_slice(b"abcdefghijklmnopqrstuvwxyz012345");
        s[64..72].copy_from_slice(&(64u64 << 20).to_le_bytes());
        s[72..80].copy_from_slice(&(1u64 << 20).to_le_bytes());
        // Data area (1 MiB, to the end), terminator, metadata area (4096, ~1 MiB), terminator.
        s[104..112].copy_from_slice(&4096u64.to_le_bytes());
        s[112..120].copy_from_slice(&((1u64 << 20) - 4096).to_le_bytes());
        seal(&mut s);
        s
    }

    pub(crate) fn seal(s: &mut [u8; SECTOR]) {
        let sum = crc::checksum(&s[20..]);
        s[16..20].copy_from_slice(&sum.to_le_bytes());
    }

    #[test]
    fn uuids_round_trip() {
        let text = "l4QNdM-ZFBH-Adli-LGLH-Bqf0-fUql-GsZOc7";
        let uuid = Uuid::parse(text).expect("valid");
        assert_eq!(uuid.to_string(), text);
        assert_eq!(&uuid.as_bytes()[..6], b"l4QNdM");
        assert_eq!(Uuid::parse("l4QNdMZFBH-Adli-LGLH-Bqf0-fUql-GsZOc7"), None);
        assert_eq!(
            Uuid::parse("l4QNdM-ZFBH-Adli-LGLH-Bqf0-fUql-GsZOc7-x"),
            None
        );
        assert_eq!(Uuid::parse("l4QNd!-ZFBH-Adli-LGLH-Bqf0-fUql-GsZOc7"), None);
    }

    #[test]
    fn a_label_in_any_of_the_first_sectors_is_found() {
        let mut start = [0u8; SECTOR * LABEL_SECTORS];
        start[SECTOR..2 * SECTOR].copy_from_slice(&sector());
        let label = PvLabel::find(&start).expect("valid");
        assert_eq!(label.sector, 1);
        assert_eq!(label.device_size, 64 << 20);
        assert_eq!(
            label.data_areas,
            [Area {
                offset: 1 << 20,
                size: 0
            }]
        );
        assert_eq!(
            label.metadata_areas,
            [Area {
                offset: 4096,
                size: (1 << 20) - 4096
            }]
        );
        // The same label copied to sector 2 records the wrong sector.
        let mut moved = [0u8; SECTOR * LABEL_SECTORS];
        moved[2 * SECTOR..3 * SECTOR].copy_from_slice(&sector());
        assert_eq!(
            PvLabel::find(&moved),
            Err(LabelError::SectorNumber { recorded: 1 })
        );
        assert_eq!(
            PvLabel::find(&[0; SECTOR * LABEL_SECTORS]),
            Err(LabelError::NotFound)
        );
    }

    #[test]
    fn invalid_fields_are_named() {
        type Edit = fn(&mut [u8; SECTOR]);
        let cases: [(Edit, LabelError); 5] = [
            (|s| s[100] ^= 1, LabelError::Checksum),
            (|s| s[31] = b'2', LabelError::Type),
            (|s| s[20] = 8, LabelError::HeaderOffset(8)),
            (|s| s[40] = b'-', LabelError::Uuid),
            (|s| s[72..SECTOR].fill(1), LabelError::Unterminated),
        ];
        for (edit, error) in cases {
            let mut s = sector();
            edit(&mut s);
            if error != LabelError::Checksum {
                seal(&mut s);
            }
            assert_eq!(PvLabel::parse(&s, 1), Err(error));
        }
    }
}
