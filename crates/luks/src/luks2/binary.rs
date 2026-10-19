//! The LUKS2 binary header (LUKS2 On-Disk Format Specification 1.1.4, section 2.1, figure 2 and
//! table 1).
//!
//! Each of the two header copies starts with one 4096-byte binary header, of which 512 bytes are
//! used, followed by the JSON area. The checksum covers the binary header and the JSON area
//! (`hdr_size` bytes in all) with the checksum field zeroed.

use alloc::string::String;

use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::bytes::{be16, be64, c_string};
use crate::hash::{HashAlg, MAX_DIGEST_LEN};

/// The size of the binary header, in bytes.
pub const BINARY_HEADER_LEN: usize = 4096;
/// The magic of the primary header (the same bytes as LUKS1's).
pub const MAGIC_PRIMARY: [u8; 6] = crate::luks1::MAGIC;
/// The magic of the secondary header.
pub const MAGIC_SECONDARY: [u8; 6] = *b"SKUL\xba\xbe";
/// The offsets the secondary header may start at, which are also the allowed `hdr_size` values
/// (table 1: 16 KiB to 4 MiB, each with a JSON area of `hdr_size - 4096` bytes).
pub const SECONDARY_OFFSETS: [u64; 9] = [
    0x4000, 0x8000, 0x1_0000, 0x2_0000, 0x4_0000, 0x8_0000, 0x10_0000, 0x20_0000, 0x40_0000,
];

const CHECKSUM_OFFSET: usize = 0x1c0;
const CHECKSUM_LEN: usize = 64;

/// Which of the two header copies a binary header is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HeaderCopy {
    /// The copy at offset 0, with magic `LUKS\xba\xbe`.
    Primary,
    /// The copy after the primary JSON area, with magic `SKUL\xba\xbe`.
    Secondary,
}

/// A parsed LUKS2 binary header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryHeader {
    /// Which copy this is, from its magic.
    pub copy: HeaderCopy,
    /// The size of the header with its JSON area, in bytes.
    pub hdr_size: u64,
    /// The update counter; the copy with the higher value is the more recent.
    pub seqid: u64,
    /// The label, possibly empty.
    pub label: String,
    /// The checksum algorithm.
    pub checksum_alg: HashAlg,
    /// The device UUID, as text.
    pub uuid: String,
    /// The secondary label, possibly empty.
    pub subsystem: String,
    /// Where this copy says it is on the device, in bytes.
    pub hdr_offset: u64,
    checksum: [u8; CHECKSUM_LEN],
}

impl BinaryHeader {
    /// Parses a binary header.
    ///
    /// # Errors
    ///
    /// A [`BinaryHeaderError`] naming the first field that is not valid.
    pub fn parse(sector: &[u8; BINARY_HEADER_LEN]) -> Result<Self, BinaryHeaderError> {
        let copy = if sector[..6] == MAGIC_PRIMARY {
            HeaderCopy::Primary
        } else if sector[..6] == MAGIC_SECONDARY {
            HeaderCopy::Secondary
        } else {
            return Err(BinaryHeaderError::Magic);
        };
        let version = be16(sector, 6);
        if version != 2 {
            return Err(BinaryHeaderError::Version(version));
        }
        let hdr_size = be64(sector, 8);
        if !SECONDARY_OFFSETS.contains(&hdr_size) {
            return Err(BinaryHeaderError::HeaderSize(hdr_size));
        }
        let string = |offset, len, field| {
            c_string(sector, offset, len).ok_or(BinaryHeaderError::String { field })
        };
        let checksum_alg = HashAlg::from_name(&string(72, 32, "csum_alg")?);
        if checksum_alg.output_len().is_none() {
            return Err(BinaryHeaderError::ChecksumAlg(checksum_alg));
        }
        Ok(Self {
            copy,
            hdr_size,
            seqid: be64(sector, 16),
            label: string(24, 48, "label")?,
            checksum_alg,
            uuid: string(168, 40, "uuid")?,
            subsystem: string(208, 48, "subsystem")?,
            hdr_offset: be64(sector, 256),
            checksum: sector[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN]
                .try_into()
                .expect("64-byte field"),
        })
    }

    /// The JSON area's size, in bytes.
    pub fn json_area_len(&self) -> u64 {
        self.hdr_size - BINARY_HEADER_LEN as u64
    }

    /// Checks the header checksum over `area`, the whole `hdr_size` bytes of this copy starting
    /// with the binary header.
    ///
    /// # Errors
    ///
    /// [`BinaryHeaderError::Checksum`] when it does not match, and
    /// [`BinaryHeaderError::AreaLength`] when `area` is not `hdr_size` bytes long.
    pub fn verify_checksum(&self, area: &[u8]) -> Result<(), BinaryHeaderError> {
        if area.len() as u64 != self.hdr_size {
            return Err(BinaryHeaderError::AreaLength);
        }
        let zeroed = [0u8; CHECKSUM_LEN];
        let parts = [
            &area[..CHECKSUM_OFFSET],
            &zeroed[..],
            &area[CHECKSUM_OFFSET + CHECKSUM_LEN..],
        ];
        let mut expected = [0u8; MAX_DIGEST_LEN];
        let len = self
            .checksum_alg
            .digest(&parts, &mut expected)
            .ok_or_else(|| BinaryHeaderError::ChecksumAlg(self.checksum_alg.clone()))?;
        // The rest of the 64-byte field must be zero (section 2.1), so compare all of it.
        expected[len..].fill(0);
        if bool::from(expected.ct_eq(&self.checksum)) {
            Ok(())
        } else {
            Err(BinaryHeaderError::Checksum)
        }
    }
}

/// A LUKS2 binary header that is not valid.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum BinaryHeaderError {
    /// Neither the primary nor the secondary magic.
    #[error("not a LUKS2 header: wrong magic")]
    Magic,
    /// The version is not 2.
    #[error("LUKS2 header version {0} is not 2")]
    Version(u16),
    /// `hdr_size` is not one of the sizes table 1 allows.
    #[error("LUKS2 header size {0:#x} is not one the specification allows")]
    HeaderSize(u64),
    /// A string field has no NUL terminator or is not ASCII.
    #[error("LUKS2 header field {field} is not a NUL-terminated ASCII string")]
    String {
        /// The field's name in the specification.
        field: &'static str,
    },
    /// The checksum algorithm is not one this crate implements.
    #[error("LUKS2 header checksum algorithm {} is not supported", .0.name())]
    ChecksumAlg(HashAlg),
    /// The bytes given to [`BinaryHeader::verify_checksum`] are not `hdr_size` long.
    #[error("the LUKS2 header area is not hdr_size bytes long")]
    AreaLength,
    /// The checksum does not match.
    #[error("LUKS2 header checksum does not match")]
    Checksum,
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;

    /// Builds one header copy of `hdr_size` bytes holding `json`, with a correct checksum.
    pub(crate) fn copy(copy: HeaderCopy, hdr_size: u64, seqid: u64, json: &str) -> Vec<u8> {
        let mut area = vec![0u8; hdr_size as usize];
        let (magic, offset) = match copy {
            HeaderCopy::Primary => (MAGIC_PRIMARY, 0),
            HeaderCopy::Secondary => (MAGIC_SECONDARY, hdr_size),
        };
        area[..6].copy_from_slice(&magic);
        area[6..8].copy_from_slice(&2u16.to_be_bytes());
        area[8..16].copy_from_slice(&hdr_size.to_be_bytes());
        area[16..24].copy_from_slice(&seqid.to_be_bytes());
        area[24..28].copy_from_slice(b"test");
        area[72..78].copy_from_slice(b"sha256");
        area[168..172].copy_from_slice(b"uuid");
        area[256..264].copy_from_slice(&offset.to_be_bytes());
        area[BINARY_HEADER_LEN..BINARY_HEADER_LEN + json.len()].copy_from_slice(json.as_bytes());
        let mut sum = [0u8; MAX_DIGEST_LEN];
        HashAlg::Sha256.digest(&[&area], &mut sum).expect("sha256");
        area[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 32].copy_from_slice(&sum[..32]);
        area
    }

    fn sector(area: &[u8]) -> &[u8; BINARY_HEADER_LEN] {
        area[..BINARY_HEADER_LEN].try_into().expect("4 KiB")
    }

    #[test]
    fn a_valid_copy_parses_and_verifies() {
        let area = copy(HeaderCopy::Secondary, 0x4000, 7, "{}");
        let header = BinaryHeader::parse(sector(&area)).expect("valid");
        assert_eq!(header.copy, HeaderCopy::Secondary);
        assert_eq!(header.hdr_size, 0x4000);
        assert_eq!(header.json_area_len(), 0x3000);
        assert_eq!(header.seqid, 7);
        assert_eq!(header.label, "test");
        assert_eq!(header.uuid, "uuid");
        assert_eq!(header.subsystem, "");
        assert_eq!(header.hdr_offset, 0x4000);
        assert_eq!(header.verify_checksum(&area), Ok(()));
        assert_eq!(
            header.verify_checksum(&area[..100]),
            Err(BinaryHeaderError::AreaLength)
        );
    }

    #[test]
    fn any_changed_byte_breaks_the_checksum() {
        let area = copy(HeaderCopy::Primary, 0x4000, 1, "{}");
        // The salt, the padding, the stored checksum (including its zero tail) and the JSON
        // area are all covered.
        for offset in [
            110,
            300,
            CHECKSUM_OFFSET,
            CHECKSUM_OFFSET + 40,
            BINARY_HEADER_LEN + 1,
            0x3fff,
        ] {
            let mut changed = area.clone();
            changed[offset] ^= 1;
            let header = BinaryHeader::parse(sector(&changed)).expect("still parses");
            assert_eq!(
                header.verify_checksum(&changed),
                Err(BinaryHeaderError::Checksum),
                "offset {offset:#x}"
            );
        }
    }

    #[test]
    fn invalid_fields_are_named() {
        let area = copy(HeaderCopy::Primary, 0x4000, 1, "{}");
        type Edit = fn(&mut [u8]);
        let cases: [(Edit, BinaryHeaderError); 5] = [
            (|a| a[1] = 0, BinaryHeaderError::Magic),
            (|a| a[7] = 1, BinaryHeaderError::Version(1)),
            (|a| a[14] = 0x50, BinaryHeaderError::HeaderSize(0x5000)),
            (
                |a| a[24..72].fill(b'x'),
                BinaryHeaderError::String { field: "label" },
            ),
            (
                |a| a[72..78].copy_from_slice(b"md5\0\0\0"),
                BinaryHeaderError::ChecksumAlg(HashAlg::Other("md5".into())),
            ),
        ];
        for (mutate, error) in cases {
            let mut changed = area.clone();
            mutate(&mut changed);
            assert_eq!(BinaryHeader::parse(sector(&changed)), Err(error));
        }
    }
}
