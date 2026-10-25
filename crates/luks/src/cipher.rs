//! Decryption of keyslot key material and of data sectors.
//!
//! Key material is encrypted with the keyslot's cipher in 512-byte sectors, the first at sector
//! 0 (LUKS1 appendix B: "cipher modes using IVs and tweaks must start from the all-zero
//! IV/tweak"). This module implements AES in XTS mode (IEEE 1619) with the `plain64` IV, the
//! default of both formats: the tweak is the sector number as a little-endian 128-bit value,
//! which is what `plain64` gives for every sector a keyslot can have. `plain` (the 32-bit form)
//! is the same for sector numbers below 2^32, which covers any key material.
//!
//! [`DataCipher`] holds an expanded volume key for decrypting data sectors one at a time with
//! the IV the caller gives; [`crate::data`] works out which IV each sector of a segment has.

use alloc::boxed::Box;

use aes::cipher::consts::U16;
use aes::cipher::{BlockCipher, BlockDecrypt, BlockEncrypt, BlockSizeUser, KeyInit};
use aes::{Aes128, Aes256};
use thiserror::Error;
use xts_mode::{Xts128, get_tweak_default};

/// The sector size key material is encrypted in.
pub const KEY_MATERIAL_SECTOR: usize = 512;

/// A cipher a keyslot names, resolved to one this crate implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cipher {
    /// AES-XTS with the `plain64` (or `plain`) IV.
    AesXtsPlain64,
}

impl Cipher {
    /// The cipher a LUKS2 dm-crypt specification such as `aes-xts-plain64` names.
    ///
    /// # Errors
    ///
    /// [`CipherError::Unsupported`] for any other cipher.
    pub fn from_dm_crypt(spec: &str) -> Result<Self, CipherError> {
        match spec.split_once('-') {
            Some((name, mode)) => Self::from_luks1(name, mode),
            None => Err(CipherError::Unsupported),
        }
    }

    /// The cipher a LUKS1 header's `cipher-name` and `cipher-mode` name.
    ///
    /// # Errors
    ///
    /// [`CipherError::Unsupported`] for any other cipher.
    pub fn from_luks1(name: &str, mode: &str) -> Result<Self, CipherError> {
        match (name, mode) {
            ("aes", "xts-plain64" | "xts-plain") => Ok(Self::AesXtsPlain64),
            _ => Err(CipherError::Unsupported),
        }
    }

    /// Decrypts `data`, a whole number of 512-byte sectors starting at sector 0, with `key`.
    ///
    /// # Errors
    ///
    /// [`CipherError::KeySize`] when `key` is not 32 or 64 bytes (AES-128 or AES-256 in XTS),
    /// and [`CipherError::Length`] when `data` is not a whole number of sectors.
    pub fn decrypt_key_material(self, key: &[u8], data: &mut [u8]) -> Result<(), CipherError> {
        if !data.len().is_multiple_of(KEY_MATERIAL_SECTOR) {
            return Err(CipherError::Length);
        }
        match self {
            Self::AesXtsPlain64 => match key.len() {
                32 => xts::<Aes128>(key, data),
                64 => xts::<Aes256>(key, data),
                _ => return Err(CipherError::KeySize(key.len())),
            },
        }
        Ok(())
    }
}

/// A volume key expanded for one cipher, for decrypting data sectors.
///
/// The expanded round keys are zeroed when it is dropped.
pub struct DataCipher(DataKey);

// Boxed: the expanded keys are about 2 KiB, too much to move around by value.
enum DataKey {
    Aes128(Box<Xts128<Aes128>>),
    Aes256(Box<Xts128<Aes256>>),
}

impl DataCipher {
    /// Expands `key` for `cipher`.
    ///
    /// # Errors
    ///
    /// [`CipherError::KeySize`] when `key` is not 32 or 64 bytes (AES-128 or AES-256 in XTS).
    pub fn new(cipher: Cipher, key: &[u8]) -> Result<Self, CipherError> {
        match cipher {
            Cipher::AesXtsPlain64 => match key.len() {
                32 => Ok(Self(DataKey::Aes128(Box::new(xts_key(key))))),
                64 => Ok(Self(DataKey::Aes256(Box::new(xts_key(key))))),
                _ => Err(CipherError::KeySize(key.len())),
            },
        }
    }

    /// Decrypts one sector in place with the `plain64` IV `iv`: the XTS tweak is `iv` as a
    /// little-endian 128-bit value.
    ///
    /// # Errors
    ///
    /// [`CipherError::Length`] when `sector` is shorter than one 16-byte cipher block; XTS
    /// handles any longer length.
    pub fn decrypt_sector(&self, sector: &mut [u8], iv: u64) -> Result<(), CipherError> {
        if sector.len() < 16 {
            return Err(CipherError::Length);
        }
        let tweak = u128::from(iv).to_le_bytes();
        match &self.0 {
            DataKey::Aes128(xts) => xts.decrypt_sector(sector, tweak),
            DataKey::Aes256(xts) => xts.decrypt_sector(sector, tweak),
        }
        Ok(())
    }
}

impl core::fmt::Debug for DataCipher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let bits = match self.0 {
            DataKey::Aes128(_) => 128,
            DataKey::Aes256(_) => 256,
        };
        write!(f, "DataCipher(AES-{bits}-XTS)")
    }
}

fn xts_key<C>(key: &[u8]) -> Xts128<C>
where
    C: KeyInit + BlockCipher + BlockEncrypt + BlockDecrypt + BlockSizeUser<BlockSize = U16>,
{
    let (data_key, tweak_key) = key.split_at(key.len() / 2);
    Xts128::new(
        C::new_from_slice(data_key).expect("half of a checked key length"),
        C::new_from_slice(tweak_key).expect("half of a checked key length"),
    )
}

fn xts<C>(key: &[u8], data: &mut [u8])
where
    C: KeyInit + BlockCipher + BlockEncrypt + BlockDecrypt + BlockSizeUser<BlockSize = U16>,
{
    xts_key::<C>(key).decrypt_area(data, KEY_MATERIAL_SECTOR, 0, get_tweak_default);
}

/// Key material that cannot be decrypted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CipherError {
    /// The cipher is not one this crate implements.
    #[error("the keyslot cipher is not supported")]
    Unsupported,
    /// The key size does not fit the cipher.
    #[error("a {0}-byte key does not fit the keyslot cipher")]
    KeySize(usize),
    /// Key material is not a whole number of 512-byte sectors, or a data sector is shorter
    /// than one cipher block.
    #[error("the data is not a whole number of cipher sectors")]
    Length,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IEEE 1619-2007 annex B, vector 4: AES-128-XTS, data unit sequence number 0, 512 bytes.
    #[test]
    fn decrypts_the_ieee_1619_vector() {
        let key: [u8; 32] = [
            0x27, 0x18, 0x28, 0x18, 0x28, 0x45, 0x90, 0x45, 0x23, 0x53, 0x60, 0x28, 0x74, 0x71,
            0x35, 0x26, 0x31, 0x41, 0x59, 0x26, 0x53, 0x58, 0x97, 0x93, 0x23, 0x84, 0x62, 0x64,
            0x33, 0x83, 0x27, 0x95,
        ];
        let plain: alloc::vec::Vec<u8> = (0..512).map(|i| i as u8).collect();
        let mut data = plain.clone();
        // Encrypt with the same construction, then check the published first ciphertext bytes.
        let xts = Xts128::new(
            Aes128::new_from_slice(&key[..16]).unwrap(),
            Aes128::new_from_slice(&key[16..]).unwrap(),
        );
        xts.encrypt_area(&mut data, 512, 0, get_tweak_default);
        assert_eq!(data[..8], [0x27, 0xa7, 0x47, 0x9b, 0xef, 0xa1, 0xd4, 0x76]);
        Cipher::AesXtsPlain64
            .decrypt_key_material(&key, &mut data)
            .expect("decrypts");
        assert_eq!(data, plain);
    }

    /// A data sector decrypts with the tweak its IV gives, the same as key material at that
    /// sector number.
    #[test]
    fn data_sectors_use_the_given_iv() {
        let key: alloc::vec::Vec<u8> = (0..64).collect();
        let plain: alloc::vec::Vec<u8> = (0..1024).map(|i| (i * 3) as u8).collect();
        let mut data = plain.clone();
        let xts = Xts128::new(
            Aes256::new_from_slice(&key[..32]).unwrap(),
            Aes256::new_from_slice(&key[32..]).unwrap(),
        );
        xts.encrypt_area(&mut data, 512, 7, get_tweak_default);
        let cipher = DataCipher::new(Cipher::AesXtsPlain64, &key).expect("key");
        assert_eq!(alloc::format!("{cipher:?}"), "DataCipher(AES-256-XTS)");
        let (first, second) = data.split_at_mut(512);
        cipher.decrypt_sector(first, 7).expect("decrypts");
        cipher.decrypt_sector(second, 8).expect("decrypts");
        assert_eq!(data, plain);
        assert_eq!(
            cipher.decrypt_sector(&mut [0; 15], 0),
            Err(CipherError::Length)
        );
        assert_eq!(
            DataCipher::new(Cipher::AesXtsPlain64, &key[..16]).map(|_| ()),
            Err(CipherError::KeySize(16))
        );
    }

    #[test]
    fn names_and_sizes_are_checked() {
        assert_eq!(
            Cipher::from_dm_crypt("aes-xts-plain64"),
            Ok(Cipher::AesXtsPlain64)
        );
        assert_eq!(
            Cipher::from_luks1("aes", "xts-plain"),
            Ok(Cipher::AesXtsPlain64)
        );
        assert_eq!(
            Cipher::from_dm_crypt("aes-cbc-essiv:sha256"),
            Err(CipherError::Unsupported)
        );
        assert_eq!(
            Cipher::from_dm_crypt("serpent"),
            Err(CipherError::Unsupported)
        );
        let mut data = [0u8; 512];
        assert_eq!(
            Cipher::AesXtsPlain64.decrypt_key_material(&[0; 48], &mut data),
            Err(CipherError::KeySize(48))
        );
        assert_eq!(
            Cipher::AesXtsPlain64.decrypt_key_material(&[0; 32], &mut data[..100]),
            Err(CipherError::Length)
        );
        assert!(
            Cipher::AesXtsPlain64
                .decrypt_key_material(&[0; 64], &mut data)
                .is_ok()
        );
    }
}
