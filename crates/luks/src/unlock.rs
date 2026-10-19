//! Recovering the volume key from a passphrase (LUKS1 section 4.3; LUKS2 section 4.3).
//!
//! For each keyslot: derive the area key from the passphrase with the keyslot's KDF, read and
//! decrypt the key material, merge it with the AF splitter, and check the candidate against the
//! digest. The first keyslot whose candidate matches gives the volume key.
//!
//! The KDF parameters come from the header, which anyone with the disk can write, so
//! [`Limits`] caps what an unlock may cost before any work is done.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use argon2::{Algorithm, Argon2, Params, Version};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::af;
use crate::cipher::{Cipher, CipherError, KEY_MATERIAL_SECTOR};
use crate::hash::HashAlg;
use crate::luks1::{self, Luks1};
use crate::luks2::Luks2;
use crate::luks2::metadata::{Argon2Variant, DigestKind, Kdf, Keyslot, Luks2Keyslot, Priority};

/// A recovered volume key. Its bytes are wiped when it is dropped, compared in constant time,
/// and never printed.
#[derive(Clone)]
pub struct VolumeKey(Zeroizing<Vec<u8>>);

impl PartialEq for VolumeKey {
    fn eq(&self, other: &Self) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }
}

impl Eq for VolumeKey {}

impl VolumeKey {
    /// The key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl core::fmt::Debug for VolumeKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "VolumeKey({} bytes)", self.0.len())
    }
}

/// The volume key and the keyslot that gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unlocked {
    /// The keyslot number (the LUKS1 slot index, from 0).
    pub keyslot: u32,
    /// The volume key.
    pub key: VolumeKey,
}

/// Upper bounds on the work one keyslot may ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most PBKDF2 iterations, for keyslots and digests.
    pub max_pbkdf2_iterations: u32,
    /// The most Argon2 memory, in KiB.
    pub max_argon2_memory_kib: u32,
    /// The most Argon2 passes.
    pub max_argon2_time: u32,
    /// The most Argon2 lanes.
    pub max_argon2_lanes: u32,
}

impl Default for Limits {
    /// 2^24 PBKDF2 iterations, and Argon2 up to 4 GiB, 64 passes and 16 lanes: above what
    /// interactive formatting chooses on current hardware, below what would stall a boot.
    fn default() -> Self {
        Self {
            max_pbkdf2_iterations: 1 << 24,
            max_argon2_memory_kib: 4 << 20,
            max_argon2_time: 64,
            max_argon2_lanes: 16,
        }
    }
}

impl Luks2 {
    /// Recovers the volume key with `passphrase`, trying keyslots of high priority, then normal
    /// priority, each in number order. Keyslots of priority 0, and keyslots whose digest is not
    /// bound to a segment (unbound keys), are skipped; use [`Self::unlock_keyslot`] for them.
    ///
    /// # Errors
    ///
    /// - [`UnlockError::Requirement`] when the header has a mandatory requirement, which this
    ///   crate implements none of (section 3.5 forbids activating such a device);
    /// - [`UnlockError::NoKeyslots`] when no keyslot is a candidate;
    /// - [`UnlockError::NoMatch`] when `passphrase` opens none of them;
    /// - the first other error when every candidate failed with one (an unsupported KDF or
    ///   cipher, or one over `limits`);
    /// - [`UnlockError::Io`] at once when a read fails.
    pub fn unlock<F, E>(
        &self,
        read_at: &mut F,
        passphrase: &[u8],
        limits: &Limits,
    ) -> Result<Unlocked, UnlockError<E>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), E>,
    {
        if let Some(requirement) = self.metadata.config.requirements.first() {
            return Err(UnlockError::Requirement(requirement.clone()));
        }
        let mut candidates: Vec<(Priority, u32)> = self
            .metadata
            .keyslots
            .iter()
            .filter_map(|(&id, slot)| match slot {
                Keyslot::Luks2(k) if k.priority != Priority::Ignore => Some((k.priority, id)),
                _ => None,
            })
            .filter(|&(_, id)| {
                self.metadata
                    .digest_of(id)
                    .is_some_and(|d| !d.segments.is_empty())
            })
            .collect();
        candidates.sort_by_key(|&(priority, id)| (core::cmp::Reverse(priority), id));
        try_each(candidates.into_iter().map(|(_, id)| id), |id| {
            self.unlock_keyslot(read_at, id, passphrase, limits)
        })
    }

    /// Recovers the key in keyslot `id` with `passphrase`.
    ///
    /// # Errors
    ///
    /// [`UnlockError::NoSuchKeyslot`] when there is no `luks2` keyslot `id`,
    /// [`UnlockError::NoMatch`] for the wrong passphrase, and the errors of [`Self::unlock`].
    pub fn unlock_keyslot<F, E>(
        &self,
        read_at: &mut F,
        id: u32,
        passphrase: &[u8],
        limits: &Limits,
    ) -> Result<VolumeKey, UnlockError<E>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), E>,
    {
        let Some(Keyslot::Luks2(slot)) = self.metadata.keyslots.get(&id) else {
            return Err(UnlockError::NoSuchKeyslot(id));
        };
        let digest = self
            .metadata
            .digest_of(id)
            .ok_or(UnlockError::NoSuchKeyslot(id))?;
        let DigestKind::Pbkdf2 {
            hash,
            iterations,
            salt,
            digest,
        } = &digest.kind
        else {
            return Err(UnlockError::UnsupportedDigest);
        };
        check_pbkdf2(*iterations, limits)?;
        let candidate = luks2_candidate(slot, read_at, passphrase, limits)?;
        if verify(hash, &candidate, salt, *iterations, digest)? {
            Ok(VolumeKey(candidate))
        } else {
            Err(UnlockError::NoMatch)
        }
    }
}

fn luks2_candidate<F, E>(
    slot: &Luks2Keyslot,
    read_at: &mut F,
    passphrase: &[u8],
    limits: &Limits,
) -> Result<Zeroizing<Vec<u8>>, UnlockError<E>>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), E>,
{
    let cipher = Cipher::from_dm_crypt(&slot.area.encryption)?;
    let mut area_key = Zeroizing::new(vec![0u8; slot.area.key_size as usize]);
    derive(&slot.kdf, passphrase, limits, &mut area_key)?;
    recover(
        cipher,
        &area_key,
        &slot.af.hash,
        read_at,
        slot.area.offset,
        slot.key_size as usize,
        slot.af.stripes as usize,
    )
}

impl Luks1 {
    /// Recovers the volume key with `passphrase`, trying the enabled key slots in order.
    ///
    /// # Errors
    ///
    /// As [`Luks2::unlock`], without the requirement check (LUKS1 has none).
    pub fn unlock<F, E>(
        &self,
        read_at: &mut F,
        passphrase: &[u8],
        limits: &Limits,
    ) -> Result<Unlocked, UnlockError<E>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), E>,
    {
        let enabled = (0..luks1::KEY_SLOTS as u32).filter(|&i| self.keyslots[i as usize].enabled);
        try_each(enabled, |id| {
            self.unlock_keyslot(read_at, id, passphrase, limits)
        })
    }

    /// Recovers the key in slot `index` (from 0) with `passphrase`.
    ///
    /// # Errors
    ///
    /// [`UnlockError::NoSuchKeyslot`] when the slot does not exist or is disabled, and the
    /// errors of [`Self::unlock`].
    pub fn unlock_keyslot<F, E>(
        &self,
        read_at: &mut F,
        index: u32,
        passphrase: &[u8],
        limits: &Limits,
    ) -> Result<VolumeKey, UnlockError<E>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), E>,
    {
        let slot = self
            .keyslots
            .get(index as usize)
            .filter(|slot| slot.enabled)
            .ok_or(UnlockError::NoSuchKeyslot(index))?;
        let cipher = Cipher::from_luks1(&self.cipher_name, &self.cipher_mode)?;
        check_pbkdf2(slot.iterations, limits)?;
        check_pbkdf2(self.mk_digest_iterations, limits)?;
        let key_len = self.key_bytes as usize;
        let mut slot_key = Zeroizing::new(vec![0u8; key_len]);
        pbkdf2(
            &self.hash,
            passphrase,
            &slot.salt,
            slot.iterations,
            &mut slot_key,
        )?;
        let candidate = recover(
            cipher,
            &slot_key,
            &self.hash,
            read_at,
            u64::from(slot.key_material_offset) * luks1::SECTOR,
            key_len,
            slot.stripes as usize,
        )?;
        let digest_ok = verify(
            &self.hash,
            &candidate,
            &self.mk_digest_salt,
            self.mk_digest_iterations,
            &self.mk_digest,
        )?;
        if digest_ok {
            Ok(VolumeKey(candidate))
        } else {
            Err(UnlockError::NoMatch)
        }
    }
}

/// Tries `unlock` on each keyslot in turn: the first success wins, a wrong passphrase moves on,
/// an I/O error stops, and other errors are kept in case nothing succeeds.
fn try_each<E>(
    ids: impl Iterator<Item = u32>,
    mut unlock: impl FnMut(u32) -> Result<VolumeKey, UnlockError<E>>,
) -> Result<Unlocked, UnlockError<E>> {
    let mut tried = false;
    let mut first_error = None;
    for id in ids {
        match unlock(id) {
            Ok(key) => return Ok(Unlocked { keyslot: id, key }),
            Err(UnlockError::NoMatch) => tried = true,
            Err(error @ UnlockError::Io { .. }) => return Err(error),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    Err(match (tried, first_error) {
        (true, _) => UnlockError::NoMatch,
        (false, Some(error)) => error,
        (false, None) => UnlockError::NoKeyslots,
    })
}

/// Reads, decrypts and merges one keyslot's key material.
fn recover<F, E>(
    cipher: Cipher,
    area_key: &[u8],
    af_hash: &HashAlg,
    read_at: &mut F,
    offset: u64,
    key_len: usize,
    stripes: usize,
) -> Result<Zeroizing<Vec<u8>>, UnlockError<E>>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), E>,
{
    let len = (key_len * stripes).div_ceil(KEY_MATERIAL_SECTOR) * KEY_MATERIAL_SECTOR;
    let mut material = Zeroizing::new(vec![0u8; len]);
    read_at(offset, &mut material).map_err(|source| UnlockError::Io { offset, source })?;
    cipher.decrypt_key_material(area_key, &mut material)?;
    af::merge(af_hash, &material, key_len, stripes)
        .ok_or_else(|| UnlockError::UnsupportedHash(af_hash.name().into()))
}

fn derive<E>(
    kdf: &Kdf,
    passphrase: &[u8],
    limits: &Limits,
    out: &mut [u8],
) -> Result<(), UnlockError<E>> {
    match kdf {
        Kdf::Pbkdf2 {
            hash,
            iterations,
            salt,
        } => {
            check_pbkdf2(*iterations, limits)?;
            pbkdf2(hash, passphrase, salt, *iterations, out)
        }
        Kdf::Argon2 {
            variant,
            time,
            memory_kib,
            cpus,
            salt,
        } => {
            if *memory_kib > limits.max_argon2_memory_kib
                || *time > limits.max_argon2_time
                || *cpus > limits.max_argon2_lanes
            {
                return Err(UnlockError::OverLimits);
            }
            let algorithm = match variant {
                Argon2Variant::I => Algorithm::Argon2i,
                Argon2Variant::Id => Algorithm::Argon2id,
            };
            let params = Params::new(*memory_kib, *time, *cpus, Some(out.len()))
                .map_err(|_| UnlockError::Argon2Parameters)?;
            Argon2::new(algorithm, Version::V0x13, params)
                .hash_password_into(passphrase, salt, out)
                .map_err(|_| UnlockError::Argon2Parameters)
        }
        Kdf::Other(kind) => Err(UnlockError::UnsupportedKdf(kind.clone())),
    }
}

fn check_pbkdf2<E>(iterations: u32, limits: &Limits) -> Result<(), UnlockError<E>> {
    if iterations > limits.max_pbkdf2_iterations {
        Err(UnlockError::OverLimits)
    } else {
        Ok(())
    }
}

fn pbkdf2<E>(
    hash: &HashAlg,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), UnlockError<E>> {
    hash.pbkdf2(password, salt, iterations, out)
        .ok_or_else(|| UnlockError::UnsupportedHash(hash.name().into()))
}

/// Whether PBKDF2 of `candidate` matches `expected`, compared in constant time.
fn verify<E>(
    hash: &HashAlg,
    candidate: &[u8],
    salt: &[u8],
    iterations: u32,
    expected: &[u8],
) -> Result<bool, UnlockError<E>> {
    if expected.is_empty() {
        return Ok(false);
    }
    let mut computed = Zeroizing::new(vec![0u8; expected.len()]);
    pbkdf2(hash, candidate, salt, iterations, &mut computed)?;
    Ok(bool::from(computed.ct_eq(expected)))
}

/// A volume key that could not be recovered.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UnlockError<E> {
    /// Reading key material failed.
    #[error("reading LUKS key material at {offset:#x} failed")]
    Io {
        /// Where the read started.
        offset: u64,
        /// The read error.
        #[source]
        source: E,
    },
    /// The passphrase opens none of the keyslots tried.
    #[error("the passphrase does not open any LUKS keyslot")]
    NoMatch,
    /// No keyslot can be tried.
    #[error("the LUKS header has no usable keyslot")]
    NoKeyslots,
    /// The named keyslot does not exist, is disabled, or holds no key.
    #[error("LUKS keyslot {0} does not hold a key")]
    NoSuchKeyslot(u32),
    /// The header has a mandatory requirement this crate does not implement.
    #[error("the LUKS2 header requires {0:?}, which is not supported")]
    Requirement(String),
    /// The keyslot's cipher cannot be used.
    #[error("the LUKS keyslot cipher cannot be used")]
    Cipher(#[from] CipherError),
    /// The keyslot's KDF is not one this crate implements.
    #[error("the LUKS2 KDF {0:?} is not supported")]
    UnsupportedKdf(String),
    /// A hash the keyslot or digest uses is not one this crate implements.
    #[error("the hash {0:?} is not supported")]
    UnsupportedHash(String),
    /// The digest is not of type `pbkdf2`.
    #[error("the LUKS2 digest type is not supported")]
    UnsupportedDigest,
    /// The keyslot asks for more work than [`Limits`] allow.
    #[error("the LUKS keyslot asks for more KDF work than the limits allow")]
    OverLimits,
    /// The Argon2 parameters are out of the algorithm's range.
    #[error("the LUKS2 Argon2 parameters are not valid")]
    Argon2Parameters,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_each_prefers_success_then_no_match() {
        type R = Result<VolumeKey, UnlockError<()>>;
        let key = || VolumeKey(Zeroizing::new(vec![1]));
        let r = try_each([1, 2].into_iter(), |id| -> R {
            if id == 2 {
                Ok(key())
            } else {
                Err(UnlockError::NoMatch)
            }
        });
        assert_eq!(r.expect("slot 2").keyslot, 2);
        let r = try_each([1, 2].into_iter(), |id| -> R {
            if id == 1 {
                Err(UnlockError::OverLimits)
            } else {
                Err(UnlockError::NoMatch)
            }
        });
        assert!(matches!(r, Err(UnlockError::NoMatch)));
        let r = try_each([1].into_iter(), |_| -> R { Err(UnlockError::OverLimits) });
        assert!(matches!(r, Err(UnlockError::OverLimits)));
        let r = try_each(core::iter::empty(), |_| -> R { unreachable!() });
        assert!(matches!(r, Err(UnlockError::NoKeyslots)));
        let r = try_each([1, 2].into_iter(), |id| -> R {
            assert_eq!(id, 1, "an I/O error stops the search");
            Err(UnlockError::Io {
                offset: 0,
                source: (),
            })
        });
        assert!(matches!(r, Err(UnlockError::Io { .. })));
    }

    #[test]
    fn limits_are_enforced_before_work() {
        let limits = Limits {
            max_pbkdf2_iterations: 10,
            ..Limits::default()
        };
        let kdf = Kdf::Pbkdf2 {
            hash: HashAlg::Sha256,
            iterations: 11,
            salt: vec![0; 32],
        };
        let mut out = [0u8; 32];
        assert!(matches!(
            derive::<()>(&kdf, b"p", &limits, &mut out),
            Err(UnlockError::OverLimits)
        ));
        let kdf = Kdf::Argon2 {
            variant: Argon2Variant::Id,
            time: 1,
            memory_kib: u32::MAX,
            cpus: 1,
            salt: vec![0; 32],
        };
        assert!(matches!(
            derive::<()>(&kdf, b"p", &Limits::default(), &mut out),
            Err(UnlockError::OverLimits)
        ));
        let kdf = Kdf::Other("scrypt".into());
        assert!(matches!(
            derive::<()>(&kdf, b"p", &Limits::default(), &mut out),
            Err(UnlockError::UnsupportedKdf(_))
        ));
    }

    #[test]
    fn volume_keys_do_not_print_their_bytes() {
        let key = VolumeKey(Zeroizing::new(vec![0xaa; 32]));
        assert_eq!(alloc::format!("{key:?}"), "VolumeKey(32 bytes)");
    }
}
