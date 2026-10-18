//! Client-side verification of a registry checkout.
//!
//! [`update`] follows the TUF client workflow (specification 1.0,
//! section 5) against a local checkout, starting from a [`ClientState`] the
//! client kept from last time:
//!
//! 1. **Root.** Each newer `metadata/root/<N>.json`, in order, must be signed
//!    by a threshold of both the trusted root's and its own root keys, and
//!    carry version `N`. The newest root must not have expired.
//! 2. **Timestamp.** Signed by the root's timestamp role, not older than the
//!    trusted version, not expired, and byte-for-byte identical when it is
//!    the version the client already trusts.
//! 3. **Snapshot.** Its length and BLAKE3 match the timestamp; signed by the
//!    snapshot role; version as the timestamp says, not older than trusted;
//!    no file it lists is older than in the trusted snapshot; not expired.
//! 4. **Targets.** Matches the snapshot, signed by the targets role, not
//!    expired.
//! 5. **Delegations.** Each delegated role's file matches the snapshot, is
//!    signed by a threshold of the role's keys, has not expired, and lists
//!    only paths the delegation gives it.
//!
//! The result, [`VerifiedRegistry`], reads registry files only after
//! checking their length and BLAKE3 against the targets metadata (top-level
//! first, then the delegation that covers the path). Files that no metadata
//! lists cannot be read. A package may disappear from a newer snapshot; its
//! file then simply stops being readable. A package's owners must also be
//! the keys its delegation grants, so tag authority cannot diverge from
//! update authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use fox_core::{KeyId, PackageName};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::layout::{
    ADVISORIES_DIR, ArtifactList, LayoutError, PACKAGES_DIR, PackageEntry, REGISTRY_FILE,
    RegistryConfig, artifacts_path, package_path,
};
use crate::metadata::{
    FileMeta, MetadataError, RoleKeys, RoleName, Root, Signed, Snapshot, Targets, Timestamp,
    VersionMeta, Versioned,
};

/// The metadata directory.
pub const METADATA_DIR: &str = "metadata";
/// Root versions, `metadata/root/<N>.json`.
pub const ROOT_DIR: &str = "metadata/root";
/// The targets file name under `metadata/`.
pub const TARGETS_FILE: &str = "targets.json";
/// The snapshot file.
pub const SNAPSHOT_FILE: &str = "metadata/snapshot.json";
/// The timestamp file.
pub const TIMESTAMP_FILE: &str = "metadata/timestamp.json";
/// Delegated metadata, `metadata/delegations/<name>.json`.
pub const DELEGATIONS_DIR: &str = "metadata/delegations";

/// Largest metadata or signed target file read.
///
/// The limit covers files whose length comes from signed metadata, so a
/// signer cannot make a client allocate an arbitrary amount of memory while
/// verifying a checkout.
const MAX_METADATA: u64 = 16 * 1024 * 1024;

/// Why a registry checkout did not verify.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// A file could not be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A metadata file is larger than 16 MiB.
    #[error("{} is too large", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// A metadata file is invalid or not signed by enough of its role's
    /// keys.
    #[error("{file} does not verify")]
    Metadata {
        /// The file, relative to the registry.
        file: String,
        /// What failed.
        #[source]
        source: MetadataError,
    },
    /// A metadata file has expired.
    #[error("{file} expired at {expires}")]
    Expired {
        /// The file.
        file: String,
        /// When it expired.
        expires: OffsetDateTime,
    },
    /// A metadata file is older than one already trusted.
    #[error("{file} is version {offered}, older than trusted version {trusted}")]
    Rollback {
        /// The file.
        file: String,
        /// The trusted version.
        trusted: u64,
        /// The version offered.
        offered: u64,
    },
    /// A metadata file is not the version that the metadata above it names.
    #[error("{file} is version {found}, but {expected} was expected")]
    VersionMismatch {
        /// The file.
        file: String,
        /// The version named for it.
        expected: u64,
        /// Its version.
        found: u64,
    },
    /// A file's length or BLAKE3 does not match its metadata.
    #[error("{file} does not match its signed length and BLAKE3")]
    HashMismatch {
        /// The file.
        file: String,
    },
    /// The snapshot does not describe a metadata file it must.
    #[error("the snapshot does not list {file}")]
    MissingMeta {
        /// The file.
        file: String,
    },
    /// A delegated role lists a path outside its delegation.
    #[error("delegated role {role} lists {path}, which is not delegated to it")]
    DelegationPath {
        /// The role.
        role: String,
        /// The path.
        path: String,
    },
    /// A registry path is not signed by any metadata.
    #[error("{path} is not listed in the signed targets")]
    UnknownTarget {
        /// The path.
        path: String,
    },
    /// A registry path is absolute or leaves the registry.
    #[error("{path:?} is not a safe registry path")]
    UnsafePath {
        /// The path.
        path: String,
    },
    /// A verified registry file is invalid.
    #[error("invalid {path}")]
    Layout {
        /// The file.
        path: String,
        /// What is wrong.
        #[source]
        source: Box<LayoutError>,
    },
    /// The root names a different registry than the trusted one.
    #[error("the registry is now called {found}, not {trusted}")]
    RegistryName {
        /// The trusted name.
        trusted: String,
        /// The new name.
        found: String,
    },
    /// The client state file is invalid.
    #[error("invalid client state")]
    State(#[source] serde_json::Error),
    /// Signed metadata changed without a version bump.
    #[error("{file} changed at the same version; refusing an equivocation")]
    Equivocation {
        /// The file.
        file: String,
    },
    /// A package's owners do not match the keys of its delegation.
    #[error("package {package} has owners that do not match its delegation keys")]
    OwnerMismatch {
        /// The package.
        package: String,
    },
}

/// What a client remembers about a registry between updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientState {
    /// The newest trusted root.
    pub root: Signed<Root>,
    /// The newest trusted timestamp version.
    pub timestamp: u64,
    /// The newest trusted snapshot version.
    pub snapshot: u64,
    /// Versions of the files the trusted snapshot listed.
    pub meta: BTreeMap<String, u64>,
    /// The length and BLAKE3 of the trusted timestamp, so a same-version
    /// replacement is refused instead of accepted as fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_file: Option<FileMeta>,
}

impl ClientState {
    /// Starts trusting `root`, which must be signed by a threshold of its own
    /// root keys. Whoever supplies it is trusted completely, so it should
    /// come from the image, a pinned file or a fingerprint the user checked.
    ///
    /// # Errors
    ///
    /// [`VerifyError::Metadata`] if it is not self-signed.
    pub fn trust_root(root: Signed<Root>) -> Result<Self, VerifyError> {
        verify_role(&root, &root.signed, RoleName::Root, "root")?;
        Ok(Self {
            root,
            timestamp: 0,
            snapshot: 0,
            meta: BTreeMap::new(),
            timestamp_file: None,
        })
    }

    /// Parses saved state.
    ///
    /// # Errors
    ///
    /// [`VerifyError::State`] if it is invalid.
    pub fn from_json(bytes: &[u8]) -> Result<Self, VerifyError> {
        serde_json::from_slice(bytes).map_err(VerifyError::State)
    }

    /// Serializes the state.
    ///
    /// # Errors
    ///
    /// [`VerifyError::State`] if serialization fails.
    pub fn to_json(&self) -> Result<Vec<u8>, VerifyError> {
        serde_json::to_vec_pretty(self).map_err(VerifyError::State)
    }
}

/// A verified registry checkout.
#[derive(Debug, Clone)]
pub struct VerifiedRegistry {
    dir: PathBuf,
    root: Root,
    files: BTreeMap<String, FileMeta>,
    delegations: BTreeMap<String, RoleKeys>,
}

/// Verifies the checkout at `dir` against `state` at time `now`, and
/// advances `state` to the verified metadata. `state` is changed only when
/// verification succeeds.
///
/// # Errors
///
/// [`VerifyError`] for the first check that fails; see the module docs.
pub fn update(
    dir: &Path,
    state: &mut ClientState,
    now: OffsetDateTime,
) -> Result<VerifiedRegistry, VerifyError> {
    let root = update_root(dir, &state.root)?;
    check_expiry(&root.signed, "root", now)?;
    let roles = &root.signed;

    let (timestamp_bytes, timestamp): (Vec<u8>, Signed<Timestamp>) = load(dir, TIMESTAMP_FILE)?;
    verify_role(&timestamp, roles, RoleName::Timestamp, TIMESTAMP_FILE)?;
    not_older(TIMESTAMP_FILE, state.timestamp, timestamp.signed.version)?;
    // The timestamp pins the whole chain below it by hash, so accepting a
    // same-version timestamp with different bytes would let a signer feed a
    // client a different snapshot, targets and packages without a bump.
    if timestamp.signed.version == state.timestamp
        && let Some(trusted) = state.timestamp_file
        && !trusted.matches(&timestamp_bytes)
    {
        return Err(VerifyError::Equivocation {
            file: TIMESTAMP_FILE.to_owned(),
        });
    }
    check_expiry(&timestamp.signed, TIMESTAMP_FILE, now)?;

    let snapshot: Signed<Snapshot> =
        load_described(dir, SNAPSHOT_FILE, &timestamp.signed.snapshot)?;
    verify_role(&snapshot, roles, RoleName::Snapshot, SNAPSHOT_FILE)?;
    not_older(SNAPSHOT_FILE, state.snapshot, snapshot.signed.version)?;
    for (file, trusted) in &state.meta {
        if let Some(meta) = snapshot.signed.meta.get(file) {
            not_older(file, *trusted, meta.version)?;
        }
    }
    check_expiry(&snapshot.signed, SNAPSHOT_FILE, now)?;

    let (files, delegations) = verify_targets(dir, roles, &snapshot.signed, now)?;
    let registry = VerifiedRegistry {
        dir: dir.to_path_buf(),
        root: root.signed.clone(),
        files,
        delegations,
    };
    *state = ClientState {
        root,
        timestamp: timestamp.signed.version,
        snapshot: snapshot.signed.version,
        meta: snapshot
            .signed
            .meta
            .iter()
            .map(|(file, meta)| (file.clone(), meta.version))
            .collect(),
        timestamp_file: Some(FileMeta::of(&timestamp_bytes)),
    };
    Ok(registry)
}

/// Walks `metadata/root/<N>.json` up from the trusted version.
fn update_root(dir: &Path, trusted: &Signed<Root>) -> Result<Signed<Root>, VerifyError> {
    let mut current = trusted.clone();
    loop {
        let next = current.signed.version + 1;
        let file = format!("{ROOT_DIR}/{next}.json");
        if !dir.join(&file).is_file() {
            return Ok(current);
        }
        let candidate: Signed<Root> = load(dir, &file)?.1;
        verify_role(&candidate, &current.signed, RoleName::Root, &file)?;
        verify_role(&candidate, &candidate.signed, RoleName::Root, &file)?;
        if candidate.signed.version != next {
            return Err(VerifyError::VersionMismatch {
                file,
                expected: next,
                found: candidate.signed.version,
            });
        }
        if candidate.signed.registry != current.signed.registry {
            return Err(VerifyError::RegistryName {
                trusted: current.signed.registry,
                found: candidate.signed.registry,
            });
        }
        current = candidate;
    }
}

/// The signed files and the delegation keys of a verified targets tree.
type VerifiedTargets = (BTreeMap<String, FileMeta>, BTreeMap<String, RoleKeys>);

/// Verifies targets and delegations, and returns every signed file.
fn verify_targets(
    dir: &Path,
    roles: &Root,
    snapshot: &Snapshot,
    now: OffsetDateTime,
) -> Result<VerifiedTargets, VerifyError> {
    let targets_file = format!("{METADATA_DIR}/{TARGETS_FILE}");
    let targets: Signed<Targets> =
        load_described(dir, &targets_file, meta(snapshot, TARGETS_FILE)?)?;
    verify_role(&targets, roles, RoleName::Targets, &targets_file)?;
    check_expiry(&targets.signed, &targets_file, now)?;
    let mut files = BTreeMap::new();
    for (path, file) in &targets.signed.targets {
        check_path(path)?;
        files.insert(path.clone(), *file);
    }
    let mut delegations = BTreeMap::new();
    for delegation in &targets.signed.delegations {
        let name = format!("delegations/{}.json", delegation.name);
        // The name becomes a filesystem path below; a delegation whose name
        // contains `..` or an absolute component would otherwise read
        // metadata outside the checkout.
        check_path(&name)?;
        let file = format!("{METADATA_DIR}/{name}");
        let delegated: Signed<Targets> = load_described(dir, &file, meta(snapshot, &name)?)?;
        verify_keys(&delegated, &delegation.keys, &file)?;
        check_expiry(&delegated.signed, &file, now)?;
        for (path, meta) in &delegated.signed.targets {
            if !delegation.paths.contains(path) {
                return Err(VerifyError::DelegationPath {
                    role: delegation.name.clone(),
                    path: path.clone(),
                });
            }
            check_path(path)?;
            // Top-level targets take precedence, as in TUF's search order.
            files.entry(path.clone()).or_insert(*meta);
        }
        delegations.insert(delegation.name.clone(), delegation.keys.clone());
    }
    Ok((files, delegations))
}

fn meta<'a>(snapshot: &'a Snapshot, name: &str) -> Result<&'a VersionMeta, VerifyError> {
    snapshot
        .meta
        .get(name)
        .ok_or_else(|| VerifyError::MissingMeta {
            file: format!("{METADATA_DIR}/{name}"),
        })
}

fn verify_role<T: Serialize>(
    metadata: &Signed<T>,
    root: &Root,
    role: RoleName,
    file: &str,
) -> Result<(), VerifyError> {
    let keys = root.roles.get(&role).ok_or_else(|| VerifyError::Metadata {
        file: file.to_owned(),
        source: MetadataError::ZeroThreshold,
    })?;
    verify_keys(metadata, keys, file)
}

fn verify_keys<T: Serialize>(
    metadata: &Signed<T>,
    keys: &RoleKeys,
    file: &str,
) -> Result<(), VerifyError> {
    metadata
        .verify(keys)
        .map_err(|source| VerifyError::Metadata {
            file: file.to_owned(),
            source,
        })
}

fn check_expiry(
    metadata: &impl Versioned,
    file: &str,
    now: OffsetDateTime,
) -> Result<(), VerifyError> {
    let expires = metadata.expires();
    if expires.is_expired(now) {
        return Err(VerifyError::Expired {
            file: file.to_owned(),
            expires: expires.time(),
        });
    }
    Ok(())
}

fn not_older(file: &str, trusted: u64, offered: u64) -> Result<(), VerifyError> {
    if offered < trusted {
        return Err(VerifyError::Rollback {
            file: file.to_owned(),
            trusted,
            offered,
        });
    }
    Ok(())
}

/// Reads and parses a metadata file.
fn load<T: for<'de> Deserialize<'de>>(
    dir: &Path,
    file: &str,
) -> Result<(Vec<u8>, Signed<T>), VerifyError> {
    let bytes = read_limited(dir, file, MAX_METADATA)?;
    let parsed = Signed::from_json(&bytes).map_err(|source| VerifyError::Metadata {
        file: file.to_owned(),
        source,
    })?;
    Ok((bytes, parsed))
}

/// Reads a metadata file that `described` gives the length, hash and
/// version of.
fn load_described<T: for<'de> Deserialize<'de> + Versioned>(
    dir: &Path,
    file: &str,
    described: &VersionMeta,
) -> Result<Signed<T>, VerifyError> {
    let bytes = read_limited(dir, file, described.file.length)?;
    if !described.file.matches(&bytes) {
        return Err(VerifyError::HashMismatch {
            file: file.to_owned(),
        });
    }
    let parsed: Signed<T> = Signed::from_json(&bytes).map_err(|source| VerifyError::Metadata {
        file: file.to_owned(),
        source,
    })?;
    if parsed.signed.version() != described.version {
        return Err(VerifyError::VersionMismatch {
            file: file.to_owned(),
            expected: described.version,
            found: parsed.signed.version(),
        });
    }
    Ok(parsed)
}

/// Reads the file `file` under `base`, at most `limit` bytes, failing if it
/// is longer.
///
/// The resolved path must stay inside `base`, so a symlink in the checkout
/// cannot make a read follow it outside the registry, and `limit` is capped
/// at [`MAX_METADATA`] so metadata cannot name an unbounded length.
fn read_limited(base: &Path, file: &str, limit: u64) -> Result<Vec<u8>, VerifyError> {
    let path = base.join(file);
    let io_error = |source| VerifyError::Io {
        path: path.clone(),
        source,
    };
    let base = fs::canonicalize(base).map_err(io_error)?;
    let resolved = fs::canonicalize(&path).map_err(io_error)?;
    if !resolved.starts_with(&base) {
        return Err(VerifyError::UnsafePath {
            path: file.to_owned(),
        });
    }
    let limit = limit.min(MAX_METADATA);
    let file = fs::File::open(&resolved).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Err(VerifyError::TooLarge { path });
    }
    Ok(bytes)
}

/// Accepts relative paths of normal components only.
pub(crate) fn check_path(path: &str) -> Result<(), VerifyError> {
    let unsafe_path = || VerifyError::UnsafePath {
        path: path.to_owned(),
    };
    if path.is_empty() || path.contains('\\') {
        return Err(unsafe_path());
    }
    if !Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(unsafe_path());
    }
    Ok(())
}

impl VerifiedRegistry {
    /// The verified root metadata.
    pub fn root(&self) -> &Root {
        &self.root
    }

    /// Every signed registry path and its length and hash.
    pub fn files(&self) -> &BTreeMap<String, FileMeta> {
        &self.files
    }

    /// Reads registry file `path` after checking it against its signed
    /// length and BLAKE3.
    ///
    /// # Errors
    ///
    /// [`VerifyError::UnknownTarget`] if no metadata lists it,
    /// [`VerifyError::UnsafePath`] if `path` or a symlink under it leaves
    /// the registry, [`VerifyError::Io`] or [`VerifyError::TooLarge`] if it
    /// cannot be read, and [`VerifyError::HashMismatch`] if it does not
    /// match.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, VerifyError> {
        check_path(path)?;
        let meta = self
            .files
            .get(path)
            .ok_or_else(|| VerifyError::UnknownTarget {
                path: path.to_owned(),
            })?;
        let bytes = read_limited(&self.dir, path, meta.length)?;
        if !meta.matches(&bytes) {
            return Err(VerifyError::HashMismatch {
                file: path.to_owned(),
            });
        }
        Ok(bytes)
    }

    fn read_text(&self, path: &str) -> Result<String, VerifyError> {
        String::from_utf8(self.read(path)?).map_err(|error| VerifyError::Io {
            path: self.dir.join(path),
            source: io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()),
        })
    }

    /// `registry.toml`.
    ///
    /// # Errors
    ///
    /// As [`VerifiedRegistry::read`], and [`VerifyError::Layout`] if it is
    /// invalid.
    pub fn config(&self) -> Result<RegistryConfig, VerifyError> {
        let text = self.read_text(REGISTRY_FILE)?;
        let config = RegistryConfig::from_toml(&text).map_err(|source| VerifyError::Layout {
            path: REGISTRY_FILE.to_owned(),
            source: Box::new(source),
        })?;
        // The signed root fixes the registry's name; a `registry.toml` that
        // claims a different one must not let a caller qualify packages
        // under an identity the root does not authorize.
        if config.name != self.root.registry {
            return Err(VerifyError::RegistryName {
                trusted: self.root.registry.clone(),
                found: config.name,
            });
        }
        Ok(config)
    }

    /// The names of every signed package file.
    pub fn package_names(&self) -> Vec<PackageName> {
        self.files
            .keys()
            .filter_map(|path| {
                let rest = path.strip_prefix(PACKAGES_DIR)?.strip_prefix('/')?;
                let (_, file) = rest.split_once('/')?;
                PackageName::new(file.strip_suffix(".toml")?).ok()
            })
            .filter(|name| self.files.contains_key(&package_path(name)))
            .collect()
    }

    /// The package file of `name`, if the registry has one.
    ///
    /// # Errors
    ///
    /// As [`VerifiedRegistry::read`], [`VerifyError::Layout`] if it is
    /// invalid, and [`VerifyError::OwnerMismatch`] if its owners disagree
    /// with the keys its delegation grants.
    pub fn package(&self, name: &PackageName) -> Result<Option<PackageEntry>, VerifyError> {
        let path = package_path(name);
        if !self.files.contains_key(&path) {
            return Ok(None);
        }
        let text = self.read_text(&path)?;
        let entry =
            PackageEntry::from_toml(&text, &path).map_err(|source| VerifyError::Layout {
                path,
                source: Box::new(source),
            })?;
        // The owners an installer checks release tags against must be the
        // same keys the targets metadata delegates update authority to, or a
        // package edit could claim owners whose tags the delegation would
        // never authorize.
        if let Some(keys) = self.delegations.get(name.as_str()) {
            let owners: BTreeSet<KeyId> = entry
                .owners
                .iter()
                .map(|owner| owner.delegation_key())
                .collect();
            if owners != keys.keyids {
                return Err(VerifyError::OwnerMismatch {
                    package: name.as_str().to_owned(),
                });
            }
        }
        Ok(Some(entry))
    }

    /// The artifact list of `name` `version`, if there is one.
    ///
    /// # Errors
    ///
    /// As [`VerifiedRegistry::package`].
    pub fn artifacts(
        &self,
        name: &PackageName,
        version: &semver::Version,
    ) -> Result<Option<ArtifactList>, VerifyError> {
        let path = artifacts_path(name, version);
        if !self.files.contains_key(&path) {
            return Ok(None);
        }
        let text = self.read_text(&path)?;
        ArtifactList::from_toml(&text, &path)
            .map(Some)
            .map_err(|source| VerifyError::Layout {
                path,
                source: Box::new(source),
            })
    }

    /// The signed paths under `advisories/`.
    pub fn advisory_paths(&self) -> Vec<&str> {
        self.files
            .keys()
            .filter(|path| {
                path.strip_prefix(ADVISORIES_DIR)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .map(String::as_str)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_paths_must_stay_inside() {
        for good in ["registry.toml", "packages/ri/ripgrep.toml"] {
            check_path(good).unwrap();
        }
        for bad in ["", "/etc/passwd", "../x", "a/../b", "./a", "a\\b"] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn described_lengths_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big"),
            vec![0u8; (MAX_METADATA as usize) + 1],
        )
        .unwrap();
        // A signed length larger than the cap cannot make the reader
        // allocate an unbounded buffer; the file still trips the limit.
        assert!(matches!(
            read_limited(dir.path(), "big", u64::MAX),
            Err(VerifyError::TooLarge { .. })
        ));
    }
}
