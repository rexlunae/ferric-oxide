//! The signed trust metadata: TUF roles in the `fox-tuf/1` format.
//!
//! The model is The Update Framework's (TUF specification 1.0, sections 2
//! and 4) cut down to what a git-hosted registry needs:
//!
//! | File | Role | Says |
//! | --- | --- | --- |
//! | `metadata/root/<N>.json` | root | the keys and threshold of every top-level role; version `N` |
//! | `metadata/targets.json` | targets | the length and BLAKE3 of every registry file outside a delegation, and the delegations |
//! | `metadata/delegations/<name>.json` | one package's owners | the length and BLAKE3 of that package's file |
//! | `metadata/snapshot.json` | snapshot | the version, length and BLAKE3 of `targets.json` and every delegation |
//! | `metadata/timestamp.json` | timestamp | the version, length and BLAKE3 of `snapshot.json`; short expiry |
//!
//! Every file is a JSON envelope `{"signed": …, "signatures": […]}`. A
//! signature is Ed25519 over `fox-tuf-metadata/1\0` followed by the
//! canonical JSON ([`crate::canonical`]) of `signed`, by a key named by its
//! hex public key. Differences from TUF: key ids are the public keys
//! themselves rather than hashes of their encoding, hashes are BLAKE3,
//! delegations name exact paths rather than patterns, and there is no
//! consistent-snapshot file naming (git provides consistent checkouts). The
//! files are not meant to be read by other TUF clients.

use std::collections::{BTreeMap, BTreeSet};

use fox_core::signing::{sign_detached, verify_detached};
use fox_core::{Blake3Digest, Ed25519Signature, KeyId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::canonical;

/// The context every metadata signature covers before the canonical JSON.
pub const METADATA_CONTEXT: &[u8] = b"fox-tuf-metadata/1\0";

/// The metadata format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SpecVersion {
    /// The first format, written `"fox-tuf/1"`.
    #[default]
    #[serde(rename = "fox-tuf/1")]
    V1,
}

/// When metadata stops being valid, written as RFC 3339 in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Expiry(OffsetDateTime);

impl Expiry {
    /// The expiry `days` days after `now`, truncated to whole seconds.
    pub fn days_after(now: OffsetDateTime, days: u32) -> Self {
        let at = now + time::Duration::days(i64::from(days));
        Self(at.replace_nanosecond(0).unwrap_or(at))
    }

    /// Wraps a time.
    pub fn at(time: OffsetDateTime) -> Self {
        Self(time)
    }

    /// Whether the metadata has expired at `now`.
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        now >= self.0
    }

    /// The time.
    pub fn time(&self) -> OffsetDateTime {
        self.0
    }
}

impl Serialize for Expiry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = self
            .0
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for Expiry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        OffsetDateTime::parse(&text, &Rfc3339)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// The keys of a role and how many of them must sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleKeys {
    /// The role's Ed25519 public keys.
    pub keyids: BTreeSet<KeyId>,
    /// How many distinct keys must sign; at least 1.
    pub threshold: u32,
}

impl RoleKeys {
    /// A role of `keys` with threshold `threshold`.
    pub fn new(keys: impl IntoIterator<Item = KeyId>, threshold: u32) -> Self {
        Self {
            keyids: keys.into_iter().collect(),
            threshold,
        }
    }
}

/// A top-level role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RoleName {
    /// Signs the root metadata.
    Root,
    /// Signs `targets.json`.
    Targets,
    /// Signs `snapshot.json`.
    Snapshot,
    /// Signs `timestamp.json`.
    Timestamp,
}

impl RoleName {
    /// Every top-level role.
    pub const ALL: [Self; 4] = [Self::Root, Self::Targets, Self::Snapshot, Self::Timestamp];

    /// The role's name as written in files.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Targets => "targets",
            Self::Snapshot => "snapshot",
            Self::Timestamp => "timestamp",
        }
    }
}

/// The root role's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    /// Always `"root"`.
    #[serde(rename = "_type")]
    pub kind: RootType,
    /// Format version.
    pub spec_version: SpecVersion,
    /// Version, starting at 1 and increasing by one per change.
    pub version: u64,
    /// Expiry.
    pub expires: Expiry,
    /// The registry's name.
    pub registry: String,
    /// The keys of every top-level role.
    pub roles: BTreeMap<RoleName, RoleKeys>,
}

/// One length-and-hash description of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMeta {
    /// Length in bytes.
    pub length: u64,
    /// BLAKE3 of the contents.
    pub blake3: Blake3Digest,
}

impl FileMeta {
    /// The description of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            length: bytes.len() as u64,
            blake3: Blake3Digest::of(bytes),
        }
    }

    /// Whether `bytes` match.
    pub fn matches(&self, bytes: &[u8]) -> bool {
        *self == Self::of(bytes)
    }
}

/// A delegation of exact paths to a package's owners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delegation {
    /// The role name: the package name.
    pub name: String,
    /// The owners' keys and threshold.
    #[serde(flatten)]
    pub keys: RoleKeys,
    /// The registry paths the role may describe.
    pub paths: BTreeSet<String>,
}

/// Targets metadata: top-level or a delegation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Targets {
    /// Always `"targets"`.
    #[serde(rename = "_type")]
    pub kind: TargetsType,
    /// Format version.
    pub spec_version: SpecVersion,
    /// Version.
    pub version: u64,
    /// Expiry.
    pub expires: Expiry,
    /// Registry paths and their contents.
    pub targets: BTreeMap<String, FileMeta>,
    /// Delegated roles; empty in delegated metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegations: Vec<Delegation>,
}

/// A versioned metadata file, as snapshot and timestamp describe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionMeta {
    /// The file's `version`.
    pub version: u64,
    /// Its length and hash.
    #[serde(flatten)]
    pub file: FileMeta,
}

/// Snapshot metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Always `"snapshot"`.
    #[serde(rename = "_type")]
    pub kind: SnapshotType,
    /// Format version.
    pub spec_version: SpecVersion,
    /// Version.
    pub version: u64,
    /// Expiry.
    pub expires: Expiry,
    /// `targets.json` and `delegations/<name>.json`, by path under
    /// `metadata/`.
    pub meta: BTreeMap<String, VersionMeta>,
}

/// Timestamp metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timestamp {
    /// Always `"timestamp"`.
    #[serde(rename = "_type")]
    pub kind: TimestampType,
    /// Format version.
    pub spec_version: SpecVersion,
    /// Version.
    pub version: u64,
    /// Expiry.
    pub expires: Expiry,
    /// `snapshot.json`.
    pub snapshot: VersionMeta,
}

macro_rules! type_tag {
    ($(#[$doc:meta])* $name:ident, $text:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
        pub enum $name {
            #[doc = concat!("`\"", $text, "\"`.")]
            #[default]
            #[serde(rename = $text)]
            Tag,
        }
    };
}

type_tag!(
    /// The `_type` of root metadata.
    RootType,
    "root"
);
type_tag!(
    /// The `_type` of targets metadata.
    TargetsType,
    "targets"
);
type_tag!(
    /// The `_type` of snapshot metadata.
    SnapshotType,
    "snapshot"
);
type_tag!(
    /// The `_type` of timestamp metadata.
    TimestampType,
    "timestamp"
);

/// Metadata that carries a version and an expiry.
pub trait Versioned {
    /// The version.
    fn version(&self) -> u64;
    /// The expiry.
    fn expires(&self) -> Expiry;
}

macro_rules! versioned {
    ($($t:ty),*) => {$(
        impl Versioned for $t {
            fn version(&self) -> u64 { self.version }
            fn expires(&self) -> Expiry { self.expires }
        }
    )*};
}
versioned!(Root, Targets, Snapshot, Timestamp);

/// One signature in an envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataSignature {
    /// The signing key.
    pub keyid: KeyId,
    /// The signature.
    pub sig: Ed25519Signature,
}

/// A signed metadata file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signed<T> {
    /// The metadata.
    pub signed: T,
    /// Signatures over its canonical JSON.
    pub signatures: Vec<MetadataSignature>,
}

/// Why metadata could not be signed, parsed or verified.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MetadataError {
    /// The metadata has no canonical encoding.
    #[error("metadata has no canonical encoding")]
    Canonical(#[from] canonical::CanonicalError),
    /// Signing failed.
    #[error("cannot sign metadata")]
    Sign(#[source] pkgar_core::Error),
    /// The file is not valid metadata of the expected kind.
    #[error("invalid metadata")]
    Parse(#[source] serde_json::Error),
    /// Fewer distinct role keys signed than the threshold requires.
    #[error("{valid} valid signatures from the role's keys, {threshold} needed")]
    Threshold {
        /// Valid signatures by distinct role keys.
        valid: u32,
        /// The threshold.
        threshold: u32,
    },
    /// The role's threshold is zero.
    #[error("role threshold is zero")]
    ZeroThreshold,
}

impl<T: Serialize> Signed<T> {
    /// Unsigned metadata.
    pub fn new(signed: T) -> Self {
        Self {
            signed,
            signatures: Vec::new(),
        }
    }

    /// The bytes signatures cover.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Canonical`] if the metadata has no canonical
    /// encoding.
    pub fn signed_bytes(&self) -> Result<Vec<u8>, MetadataError> {
        Ok(canonical::to_vec(&self.signed)?)
    }

    /// Adds a signature by `secret_key` (NaCl layout), replacing an earlier
    /// signature by the same key.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Canonical`] or [`MetadataError::Sign`].
    pub fn sign(&mut self, secret_key: &[u8; 64]) -> Result<(), MetadataError> {
        let bytes = self.signed_bytes()?;
        let sig =
            sign_detached(METADATA_CONTEXT, &bytes, secret_key).map_err(MetadataError::Sign)?;
        let keyid = fox_core::signing::public_key_of(secret_key);
        self.signatures.retain(|signature| signature.keyid != keyid);
        self.signatures.push(MetadataSignature { keyid, sig });
        self.signatures.sort_by_key(|signature| signature.keyid);
        Ok(())
    }

    /// Checks that at least `keys.threshold` distinct keys of the role made
    /// valid signatures. Signatures by other keys are ignored.
    ///
    /// # Errors
    ///
    /// [`MetadataError::ZeroThreshold`], [`MetadataError::Threshold`] or
    /// [`MetadataError::Canonical`].
    pub fn verify(&self, keys: &RoleKeys) -> Result<(), MetadataError> {
        if keys.threshold == 0 {
            return Err(MetadataError::ZeroThreshold);
        }
        let bytes = self.signed_bytes()?;
        let mut valid = BTreeSet::new();
        for signature in &self.signatures {
            if keys.keyids.contains(&signature.keyid)
                && verify_detached(METADATA_CONTEXT, &bytes, &signature.sig, &signature.keyid)
                    .is_ok()
            {
                valid.insert(signature.keyid);
            }
        }
        let valid = u32::try_from(valid.len()).unwrap_or(u32::MAX);
        if valid < keys.threshold {
            return Err(MetadataError::Threshold {
                valid,
                threshold: keys.threshold,
            });
        }
        Ok(())
    }

    /// The file contents: pretty-printed JSON with a trailing newline.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Parse`] if serialization fails.
    pub fn to_json(&self) -> Result<Vec<u8>, MetadataError> {
        let mut bytes = serde_json::to_vec_pretty(self).map_err(MetadataError::Parse)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

impl<T: for<'de> Deserialize<'de>> Signed<T> {
    /// Parses a metadata file.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Parse`] if it is not valid metadata of this kind.
    pub fn from_json(bytes: &[u8]) -> Result<Self, MetadataError> {
        serde_json::from_slice(bytes).map_err(MetadataError::Parse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pkgar_core::dryoc::classic::crypto_sign::crypto_sign_seed_keypair;

    pub(crate) fn secret(seed: u8) -> [u8; 64] {
        crypto_sign_seed_keypair(&[seed; 32]).1
    }

    pub(crate) fn key(seed: u8) -> KeyId {
        fox_core::signing::public_key_of(&secret(seed))
    }

    fn timestamp() -> Signed<Timestamp> {
        Signed::new(Timestamp {
            kind: TimestampType::Tag,
            spec_version: SpecVersion::V1,
            version: 3,
            expires: Expiry::at(OffsetDateTime::UNIX_EPOCH),
            snapshot: VersionMeta {
                version: 2,
                file: FileMeta::of(b"snapshot"),
            },
        })
    }

    #[test]
    fn threshold_counts_distinct_role_keys() {
        let mut meta = timestamp();
        let role = RoleKeys::new([key(1), key(2)], 2);
        meta.sign(&secret(1)).unwrap();
        meta.sign(&secret(1)).unwrap();
        assert_eq!(meta.signatures.len(), 1);
        assert!(matches!(
            meta.verify(&role),
            Err(MetadataError::Threshold {
                valid: 1,
                threshold: 2
            })
        ));
        // A key outside the role does not count.
        meta.sign(&secret(3)).unwrap();
        assert!(meta.verify(&role).is_err());
        meta.sign(&secret(2)).unwrap();
        meta.verify(&role).unwrap();
        assert!(matches!(
            meta.verify(&RoleKeys::new([key(1)], 0)),
            Err(MetadataError::ZeroThreshold)
        ));
    }

    #[test]
    fn changed_metadata_breaks_signatures_and_round_trips() {
        let mut meta = timestamp();
        meta.sign(&secret(1)).unwrap();
        let json = meta.to_json().unwrap();
        let parsed = Signed::<Timestamp>::from_json(&json).unwrap();
        assert_eq!(parsed, meta);
        parsed.verify(&RoleKeys::new([key(1)], 1)).unwrap();

        let mut changed = parsed;
        changed.signed.version = 4;
        assert!(changed.verify(&RoleKeys::new([key(1)], 1)).is_err());
        assert!(
            String::from_utf8(json)
                .unwrap()
                .contains("\"expires\": \"1970-01-01T00:00:00Z\"")
        );
        // The wrong kind of metadata does not parse.
        assert!(Signed::<Snapshot>::from_json(&meta.to_json().unwrap()).is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let json = serde_json::to_value(timestamp()).unwrap();
        let mut extra = json.clone();
        extra["signed"]["snapshot"]["extra"] = 1.into();
        assert!(serde_json::from_value::<Signed<Timestamp>>(extra).is_err());
        let mut extra = json;
        extra["signed"]["extra"] = 1.into();
        assert!(serde_json::from_value::<Signed<Timestamp>>(extra).is_err());
    }
}
