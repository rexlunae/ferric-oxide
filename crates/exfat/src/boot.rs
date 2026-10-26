//! The Main Boot Region: the boot sector's fields and the boot checksum.
//!
//! From the "Main Boot Sector" and "Main and Backup Boot Checksum Sub-regions" sections of
//! the exFAT specification. The boot region is the first 12 sectors: the boot sector, 8
//! extended boot sectors, the OEM parameters, a reserved sector and the checksum sector,
//! whose every 32-bit word repeats the checksum of the 11 sectors before it.

use thiserror::Error;

/// The `FileSystemName` field of an exFAT boot sector.
pub const SIGNATURE: &[u8; 8] = b"EXFAT   ";

/// How many bytes of the boot sector [`BootSector::parse`] reads.
pub const SECTOR_PREFIX: usize = 512;

/// The sectors the boot checksum covers: the boot sector up to the OEM and reserved ones.
pub const CHECKSUMMED_SECTORS: u64 = 11;

/// Why a boot sector was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum BootSectorError {
    /// `FileSystemName` is not `"EXFAT   "`.
    #[error("not an exFAT boot sector")]
    NotExfat,
    /// The boot signature at offset 510 is not `55 AA`.
    #[error("the boot signature is missing")]
    NoSignature,
    /// `MustBeZero` (bytes 11 to 63, where FAT keeps its BIOS parameter block) is not zero.
    #[error("the bytes where FAT keeps its BPB are not zero")]
    FatBpb,
    /// `BytesPerSectorShift` is outside 9 to 12 (512 to 4096-byte sectors).
    #[error("{shift} is not a sector size shift between 9 and 12")]
    SectorShift {
        /// The shift found.
        shift: u8,
    },
    /// `SectorsPerClusterShift` makes clusters larger than 32 MiB.
    #[error("clusters of 2^{shift} sectors are over 32 MiB")]
    ClusterShift {
        /// The shift found.
        shift: u8,
    },
    /// `NumberOfFats` is not 1 or 2.
    #[error("{count} FATs; exFAT has one or two")]
    FatCount {
        /// The count found.
        count: u8,
    },
    /// `FileSystemRevision`'s major version is not 1.
    #[error("file system revision {major}.{minor} is not 1.x")]
    Revision {
        /// The major version.
        major: u8,
        /// The minor version.
        minor: u8,
    },
    /// The FAT, the cluster heap or the root directory lies outside the volume, or the FAT
    /// is too short for the cluster count.
    #[error("the FAT, the cluster heap or the root directory is outside the volume")]
    Layout,
}

/// The fields of the boot sector the reader needs, checked for consistency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootSector {
    /// The volume's length in sectors.
    pub volume_sectors: u64,
    /// The first FAT, in sectors from the volume's start.
    pub fat_offset: u32,
    /// Each FAT's length in sectors.
    pub fat_length: u32,
    /// The cluster heap, in sectors from the volume's start.
    pub heap_offset: u32,
    /// The clusters in the heap; they are numbered from 2.
    pub cluster_count: u32,
    /// The root directory's first cluster.
    pub root_cluster: u32,
    /// The volume serial number.
    pub serial: u32,
    /// Bytes per sector, as a power of two.
    pub sector_shift: u8,
    /// Sectors per cluster, as a power of two.
    pub cluster_shift: u8,
    /// How many FATs there are (1, or 2 for TexFAT).
    pub fat_count: u8,
    /// Which FAT and allocation bitmap are in use (`VolumeFlags` bit 0).
    pub active_fat: u8,
    /// Whether the volume was not cleanly unmounted (`VolumeFlags` bit 1).
    pub dirty: bool,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from(le32(b, at)) | u64::from(le32(b, at + 4)) << 32
}

impl BootSector {
    /// Reads and checks the first [`SECTOR_PREFIX`] bytes of a boot sector.
    ///
    /// # Errors
    ///
    /// The [`BootSectorError`] for the first field that is wrong.
    pub fn parse(b: &[u8; SECTOR_PREFIX]) -> Result<Self, BootSectorError> {
        if &b[3..11] != SIGNATURE {
            return Err(BootSectorError::NotExfat);
        }
        if b[510..512] != [0x55, 0xAA] {
            return Err(BootSectorError::NoSignature);
        }
        if b[11..64].iter().any(|&x| x != 0) {
            return Err(BootSectorError::FatBpb);
        }
        let revision = le16(b, 104);
        let (major, minor) = ((revision >> 8) as u8, revision as u8);
        if major != 1 {
            return Err(BootSectorError::Revision { major, minor });
        }
        let sector_shift = b[108];
        if !(9..=12).contains(&sector_shift) {
            return Err(BootSectorError::SectorShift {
                shift: sector_shift,
            });
        }
        let cluster_shift = b[109];
        if u32::from(sector_shift) + u32::from(cluster_shift) > 25 {
            return Err(BootSectorError::ClusterShift {
                shift: cluster_shift,
            });
        }
        let fat_count = b[110];
        if !(1..=2).contains(&fat_count) {
            return Err(BootSectorError::FatCount { count: fat_count });
        }
        let flags = le16(b, 106);
        let boot = Self {
            volume_sectors: le64(b, 72),
            fat_offset: le32(b, 80),
            fat_length: le32(b, 84),
            heap_offset: le32(b, 88),
            cluster_count: le32(b, 92),
            root_cluster: le32(b, 96),
            serial: le32(b, 100),
            sector_shift,
            cluster_shift,
            fat_count,
            // The second FAT is used only when there are two.
            active_fat: if fat_count == 2 { (flags & 1) as u8 } else { 0 },
            dirty: flags & 2 != 0,
        };
        boot.check_layout()?;
        Ok(boot)
    }

    /// The layout checks of the specification's field descriptions: the FATs after the boot
    /// regions, the heap after the FATs, both inside the volume, every cluster's FAT entry
    /// inside the FAT, and the root directory a cluster of the heap.
    fn check_layout(&self) -> Result<(), BootSectorError> {
        let fats_end =
            u64::from(self.fat_offset) + u64::from(self.fat_length) * u64::from(self.fat_count);
        let heap_end =
            u64::from(self.heap_offset) + (u64::from(self.cluster_count) << self.cluster_shift);
        let fat_bytes = u64::from(self.fat_length) << self.sector_shift;
        let ok = self.fat_offset >= 24
            && u64::from(self.heap_offset) >= fats_end
            && heap_end <= self.volume_sectors
            && fat_bytes >= (u64::from(self.cluster_count) + 2) * 4
            && self.cluster_count <= 0xFFFF_FFF5
            && (2..u64::from(self.cluster_count) + 2).contains(&u64::from(self.root_cluster));
        if ok {
            Ok(())
        } else {
            Err(BootSectorError::Layout)
        }
    }

    /// Bytes per sector.
    pub const fn sector_size(&self) -> u64 {
        1 << self.sector_shift
    }

    /// Bytes per cluster.
    pub const fn cluster_size(&self) -> u64 {
        1 << (self.sector_shift + self.cluster_shift)
    }

    /// The byte offset of the active FAT.
    pub fn fat_start(&self) -> u64 {
        (u64::from(self.fat_offset) + u64::from(self.active_fat) * u64::from(self.fat_length))
            << self.sector_shift
    }

    /// The byte offset of `cluster`, which must be 2 or more.
    pub fn cluster_start(&self, cluster: u32) -> u64 {
        (u64::from(self.heap_offset) << self.sector_shift)
            + (u64::from(cluster - 2) << (self.sector_shift + self.cluster_shift))
    }

    /// Whether `cluster` names a cluster of the heap.
    pub fn is_cluster(&self, cluster: u32) -> bool {
        cluster >= 2 && u64::from(cluster) < u64::from(self.cluster_count) + 2
    }
}

/// Adds `sector`, the `index`th sector of the boot region, to the running boot checksum,
/// skipping `VolumeFlags` and `PercentInUse` in the boot sector, which change without the
/// checksum being rewritten.
pub fn checksum(mut sum: u32, index: u64, sector: &[u8]) -> u32 {
    for (i, &byte) in sector.iter().enumerate() {
        if index == 0 && matches!(i, 106 | 107 | 112) {
            continue;
        }
        sum = sum.rotate_right(1).wrapping_add(u32::from(byte));
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid boot sector: 512-byte sectors, 4 KiB clusters.
    pub(crate) fn sector() -> [u8; SECTOR_PREFIX] {
        let mut b = [0; SECTOR_PREFIX];
        b[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        b[3..11].copy_from_slice(SIGNATURE);
        b[72..80].copy_from_slice(&65_536u64.to_le_bytes());
        b[80..84].copy_from_slice(&128u32.to_le_bytes());
        b[84..88].copy_from_slice(&64u32.to_le_bytes());
        b[88..92].copy_from_slice(&256u32.to_le_bytes());
        b[92..96].copy_from_slice(&8000u32.to_le_bytes());
        b[96..100].copy_from_slice(&5u32.to_le_bytes());
        b[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        b[108] = 9;
        b[109] = 3;
        b[110] = 1;
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn a_boot_sector_parses() {
        let boot = BootSector::parse(&sector()).unwrap();
        assert_eq!((boot.sector_size(), boot.cluster_size()), (512, 4096));
        assert_eq!(boot.fat_start(), 128 * 512);
        assert_eq!(boot.cluster_start(2), 256 * 512);
        assert_eq!(boot.cluster_start(3), 256 * 512 + 4096);
        assert!(boot.is_cluster(8001) && !boot.is_cluster(8002) && !boot.is_cluster(1));
    }

    #[test]
    fn bad_fields_are_refused() {
        let with = |at: usize, v: u8| {
            let mut b = sector();
            b[at] = v;
            BootSector::parse(&b).unwrap_err()
        };
        assert_eq!(with(3, b'F'), BootSectorError::NotExfat);
        assert_eq!(with(511, 0), BootSectorError::NoSignature);
        assert_eq!(with(20, 1), BootSectorError::FatBpb);
        assert_eq!(with(108, 13), BootSectorError::SectorShift { shift: 13 });
        assert_eq!(with(109, 17), BootSectorError::ClusterShift { shift: 17 });
        assert_eq!(with(110, 3), BootSectorError::FatCount { count: 3 });
        assert!(matches!(
            with(105, 2),
            BootSectorError::Revision { major: 2, .. }
        ));
        // The root directory past the last cluster.
        assert_eq!(with(99, 0xFF), BootSectorError::Layout);
    }

    #[test]
    fn the_checksum_skips_the_changing_fields() {
        let a = sector();
        let mut b = a;
        b[106] = 2;
        b[112] = 50;
        assert_eq!(checksum(0, 0, &a), checksum(0, 0, &b));
        assert_ne!(checksum(0, 1, &a), checksum(0, 1, &b));
    }
}
