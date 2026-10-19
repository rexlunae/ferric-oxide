//! The LUKS1 partition header (LUKS1 On-Disk Format Specification 1.2.3, section 3, figures 1
//! and 2, and the constants of section 5).
//!
//! The header is 592 bytes at sector 0: the cipher, mode and hash names, the payload offset, the
//! volume ("master") key length and its PBKDF2 digest, the UUID, and eight 48-byte key slots.

use alloc::string::String;

use thiserror::Error;

use crate::bytes::{be16, be32, c_string};
use crate::hash::HashAlg;

/// The size of the LUKS1 header, in bytes.
pub const HEADER_LEN: usize = 592;
/// The magic both LUKS1 and the primary LUKS2 header start with.
pub const MAGIC: [u8; 6] = *b"LUKS\xba\xbe";
/// The number of key slots.
pub const KEY_SLOTS: usize = 8;
/// The length of the volume-key digest.
pub const DIGEST_LEN: usize = 20;
/// The length of the PBKDF2 salts.
pub const SALT_LEN: usize = 32;
/// LUKS1 counts offsets in 512-byte sectors.
pub const SECTOR: u64 = 512;
/// The `active` value of a disabled key slot.
pub const KEY_DISABLED: u32 = 0x0000_dead;
/// The `active` value of an enabled key slot.
pub const KEY_ENABLED: u32 = 0x00ac_71f3;
/// The largest volume key this crate accepts, in bytes (a 512-bit XTS key).
pub const MAX_KEY_BYTES: u32 = 64;
/// The largest stripe count this crate accepts. The specification lets the creator choose;
/// every known implementation uses 4000, and the bound keeps a hostile header from making the
/// reader allocate gigabytes of key material.
pub const MAX_STRIPES: u32 = 65_536;

const SLOT_OFFSET: usize = 208;
const SLOT_LEN: usize = 48;

/// A parsed LUKS1 header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Luks1 {
    /// The cipher name, such as `aes`.
    pub cipher_name: String,
    /// The cipher mode, such as `xts-plain64`.
    pub cipher_mode: String,
    /// The hash for PBKDF2 and the AF splitter.
    pub hash: HashAlg,
    /// The start of the encrypted payload, in 512-byte sectors.
    pub payload_offset: u32,
    /// The volume key length in bytes.
    pub key_bytes: u32,
    /// PBKDF2 of the volume key, to recognise the right key.
    pub mk_digest: [u8; DIGEST_LEN],
    /// The salt of [`Self::mk_digest`].
    pub mk_digest_salt: [u8; SALT_LEN],
    /// The PBKDF2 iterations of [`Self::mk_digest`].
    pub mk_digest_iterations: u32,
    /// The partition UUID, as text.
    pub uuid: String,
    /// The eight key slots.
    pub keyslots: [Keyslot; KEY_SLOTS],
}

/// One LUKS1 key slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keyslot {
    /// Whether the slot holds a key.
    pub enabled: bool,
    /// The PBKDF2 iterations for the passphrase.
    pub iterations: u32,
    /// The PBKDF2 salt for the passphrase.
    pub salt: [u8; SALT_LEN],
    /// The start of the slot's key material, in 512-byte sectors.
    pub key_material_offset: u32,
    /// The anti-forensic stripe count.
    pub stripes: u32,
}

impl Luks1 {
    /// Parses the header from the first [`HEADER_LEN`] bytes of the device.
    ///
    /// # Errors
    ///
    /// A [`Luks1Error`] naming the first field that is not valid.
    pub fn parse(header: &[u8; HEADER_LEN]) -> Result<Self, Luks1Error> {
        if header[..6] != MAGIC {
            return Err(Luks1Error::Magic);
        }
        let version = be16(header, 6);
        if version != 1 {
            return Err(Luks1Error::Version(version));
        }
        let string =
            |offset, len, field| c_string(header, offset, len).ok_or(Luks1Error::String { field });
        let key_bytes = be32(header, 108);
        if key_bytes == 0 || key_bytes > MAX_KEY_BYTES {
            return Err(Luks1Error::KeyBytes(key_bytes));
        }
        let mk_digest_iterations = be32(header, 164);
        if mk_digest_iterations == 0 {
            return Err(Luks1Error::DigestIterations);
        }
        let mut keyslots = [Keyslot {
            enabled: false,
            iterations: 0,
            salt: [0; SALT_LEN],
            key_material_offset: 0,
            stripes: 0,
        }; KEY_SLOTS];
        for (index, slot) in keyslots.iter_mut().enumerate() {
            *slot = parse_slot(header, index)?;
        }
        Ok(Self {
            cipher_name: string(8, 32, "cipher-name")?,
            cipher_mode: string(40, 32, "cipher-mode")?,
            hash: HashAlg::from_name(&string(72, 32, "hash-spec")?),
            payload_offset: be32(header, 104),
            key_bytes,
            mk_digest: header[112..132].try_into().expect("20-byte field"),
            mk_digest_salt: header[132..164].try_into().expect("32-byte field"),
            mk_digest_iterations,
            uuid: string(168, 40, "uuid")?,
            keyslots,
        })
    }

    /// The start of the encrypted payload, in bytes.
    pub fn payload_offset_bytes(&self) -> u64 {
        u64::from(self.payload_offset) * SECTOR
    }
}

fn parse_slot(header: &[u8], index: usize) -> Result<Keyslot, Luks1Error> {
    let base = SLOT_OFFSET + index * SLOT_LEN;
    let enabled = match be32(header, base) {
        KEY_ENABLED => true,
        KEY_DISABLED => false,
        value => return Err(Luks1Error::SlotState { slot: index, value }),
    };
    let slot = Keyslot {
        enabled,
        iterations: be32(header, base + 4),
        salt: header[base + 8..base + 40]
            .try_into()
            .expect("32-byte field"),
        key_material_offset: be32(header, base + 40),
        stripes: be32(header, base + 44),
    };
    if !enabled {
        return Ok(slot);
    }
    if slot.iterations == 0 {
        return Err(Luks1Error::SlotIterations { slot: index });
    }
    if slot.stripes == 0 || slot.stripes > MAX_STRIPES {
        return Err(Luks1Error::Stripes {
            slot: index,
            stripes: slot.stripes,
        });
    }
    // Key material starts after the header (section 4.1 puts it at sector 592 / 512 + 1 or later).
    if u64::from(slot.key_material_offset) * SECTOR < HEADER_LEN as u64 {
        return Err(Luks1Error::KeyMaterialOffset { slot: index });
    }
    Ok(slot)
}

/// A LUKS1 header field that is not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Luks1Error {
    /// The magic is not `LUKS\xba\xbe`.
    #[error("not a LUKS header: wrong magic")]
    Magic,
    /// The version is not 1.
    #[error("LUKS header version {0} is not 1")]
    Version(u16),
    /// A string field has no NUL terminator or is not ASCII.
    #[error("LUKS1 header field {field} is not a NUL-terminated ASCII string")]
    String {
        /// The field's name in the specification.
        field: &'static str,
    },
    /// The volume key length is zero or larger than [`MAX_KEY_BYTES`].
    #[error("LUKS1 volume key length {0} is not supported")]
    KeyBytes(u32),
    /// The volume-key digest has zero iterations.
    #[error("LUKS1 volume-key digest has zero iterations")]
    DigestIterations,
    /// A key slot's `active` field is neither the enabled nor the disabled magic.
    #[error("LUKS1 key slot {slot} has state {value:#010x}, neither enabled nor disabled")]
    SlotState {
        /// The slot index, from 0.
        slot: usize,
        /// The `active` value.
        value: u32,
    },
    /// An enabled key slot has zero PBKDF2 iterations.
    #[error("LUKS1 key slot {slot} has zero iterations")]
    SlotIterations {
        /// The slot index, from 0.
        slot: usize,
    },
    /// An enabled key slot's stripe count is zero or larger than [`MAX_STRIPES`].
    #[error("LUKS1 key slot {slot} has {stripes} stripes")]
    Stripes {
        /// The slot index, from 0.
        slot: usize,
        /// The stripe count.
        stripes: u32,
    },
    /// An enabled key slot's key material overlaps the header.
    #[error("LUKS1 key slot {slot}'s key material overlaps the header")]
    KeyMaterialOffset {
        /// The slot index, from 0.
        slot: usize,
    },
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid header with slot 0 enabled, for tests to mutate.
    pub(crate) fn header() -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..6].copy_from_slice(&MAGIC);
        h[6..8].copy_from_slice(&1u16.to_be_bytes());
        h[8..11].copy_from_slice(b"aes");
        h[40..51].copy_from_slice(b"xts-plain64");
        h[72..78].copy_from_slice(b"sha256");
        h[104..108].copy_from_slice(&4096u32.to_be_bytes());
        h[108..112].copy_from_slice(&32u32.to_be_bytes());
        h[164..168].copy_from_slice(&1000u32.to_be_bytes());
        h[168..172].copy_from_slice(b"uuid");
        for slot in 0..KEY_SLOTS {
            let base = SLOT_OFFSET + slot * SLOT_LEN;
            let state = if slot == 0 { KEY_ENABLED } else { KEY_DISABLED };
            h[base..base + 4].copy_from_slice(&state.to_be_bytes());
            h[base + 4..base + 8].copy_from_slice(&1000u32.to_be_bytes());
            let offset = 8 + 256 * slot as u32;
            h[base + 40..base + 44].copy_from_slice(&offset.to_be_bytes());
            h[base + 44..base + 48].copy_from_slice(&4000u32.to_be_bytes());
        }
        h
    }

    #[test]
    fn a_valid_header_parses() {
        let parsed = Luks1::parse(&header()).expect("valid");
        assert_eq!(parsed.cipher_name, "aes");
        assert_eq!(parsed.cipher_mode, "xts-plain64");
        assert_eq!(parsed.hash, HashAlg::Sha256);
        assert_eq!(parsed.payload_offset_bytes(), 4096 * 512);
        assert_eq!(parsed.key_bytes, 32);
        assert_eq!(parsed.uuid, "uuid");
        assert!(parsed.keyslots[0].enabled);
        assert!(!parsed.keyslots[1].enabled);
        assert_eq!(parsed.keyslots[0].stripes, 4000);
    }

    #[test]
    fn invalid_fields_are_named() {
        type Edit = fn(&mut [u8; HEADER_LEN]);
        let cases: [(Edit, Luks1Error); 9] = [
            (|h| h[0] = b'X', Luks1Error::Magic),
            (|h| h[7] = 2, Luks1Error::Version(2)),
            (
                |h| h[8..40].fill(b'a'),
                Luks1Error::String {
                    field: "cipher-name",
                },
            ),
            (|h| h[111] = 65, Luks1Error::KeyBytes(65)),
            (|h| h[164..168].fill(0), Luks1Error::DigestIterations),
            (
                |h| h[SLOT_OFFSET + 48..SLOT_OFFSET + 52].fill(1),
                Luks1Error::SlotState {
                    slot: 1,
                    value: 0x0101_0101,
                },
            ),
            (
                |h| h[SLOT_OFFSET + 4..SLOT_OFFSET + 8].fill(0),
                Luks1Error::SlotIterations { slot: 0 },
            ),
            (
                |h| h[SLOT_OFFSET + 44..SLOT_OFFSET + 48].fill(0),
                Luks1Error::Stripes {
                    slot: 0,
                    stripes: 0,
                },
            ),
            (
                |h| h[SLOT_OFFSET + 40..SLOT_OFFSET + 44].fill(0),
                Luks1Error::KeyMaterialOffset { slot: 0 },
            ),
        ];
        for (mutate, error) in cases {
            let mut h = header();
            mutate(&mut h);
            assert_eq!(Luks1::parse(&h), Err(error));
        }
    }

    #[test]
    fn disabled_slots_are_not_checked() {
        let mut h = header();
        // Slot 3 is disabled; zero iterations and stripes there are fine.
        let base = SLOT_OFFSET + 3 * SLOT_LEN;
        h[base + 4..base + 48].fill(0);
        assert!(Luks1::parse(&h).is_ok());
    }
}
