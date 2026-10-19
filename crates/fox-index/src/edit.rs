//! Maintaining a registry checkout: creating it, admitting packages,
//! recording releases and re-signing the metadata.
//!
//! [`Registry`] edits the files of a working tree. Edits to package files
//! and `registry.toml` only change files; [`Registry::sign`] then brings the
//! metadata up to date, in dependency order:
//!
//! 1. **Root.** Adds signatures from the [`Keyring`] for the root keys of
//!    this root and the one before it, as a root rotation needs both.
//! 2. **Delegations.** Each package's role lists the hash of its package
//!    file.
//! 3. **Targets.** Lists every other signed file (`registry.toml`,
//!    `artifacts/**`, `advisories/**`, `vex/**` and package files no delegation
//!    covers) and the delegations.
//! 4. **Snapshot**, then **timestamp**, over the files just written.
//!
//! A role whose content changed gets a new version, a fresh expiry and no
//! signatures, then whatever signatures the keyring can add. A version no
//! key has signed yet was never published, so it is updated in place
//! instead (edits like [`Registry::admit`] leave such a pending version). A role whose
//! content did not change keeps its version and gains signatures; it is
//! also renewed when it has used up half its lifetime and the keyring holds
//! enough of its keys to meet its threshold, so a scheduled job only has to
//! run `sign` and cannot take a threshold-two role offline on its own. A role
//! may be left short of its threshold (an owner signs a delegation without the
//! snapshot key, which CI adds after the merge); [`SignReport`] says which.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fox_core::{KeyId, PackageName};
use semver::Version;
use serde::Serialize;
use time::OffsetDateTime;

use crate::advisory::{Advisory, AdvisoryError, advisory_path};
use crate::layout::{
    ADVISORIES_DIR, ARTIFACTS_DIR, ArtifactList, LayoutError, PACKAGES_DIR, PackageEntry,
    REGISTRY_FILE, RegistryConfig, Release, VEX_DIR, artifacts_path, package_path,
};
use crate::metadata::{
    Delegation, Expiry, FileMeta, MetadataError, RoleKeys, RoleName, Root, Signed, Snapshot,
    SpecVersion, Targets, Timestamp, VersionMeta, Versioned,
};
use crate::verify::{
    DELEGATIONS_DIR, METADATA_DIR, ROOT_DIR, SNAPSHOT_FILE, TARGETS_FILE, TIMESTAMP_FILE,
    check_path,
};
use crate::vex::{VexFileError, parse_vex, vex_path};

/// How long each kind of metadata stays valid, in days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lifetimes {
    /// Root: 365 days. Root keys are offline, so it is renewed by hand.
    pub root: u32,
    /// Top-level targets: 90 days.
    pub targets: u32,
    /// Package delegations: 365 days, as owners sign rarely.
    pub delegation: u32,
    /// Snapshot: 7 days.
    pub snapshot: u32,
    /// Timestamp: 1 day, re-signed by a scheduled job.
    pub timestamp: u32,
}

impl Default for Lifetimes {
    fn default() -> Self {
        Self {
            root: 365,
            targets: 90,
            delegation: 365,
            snapshot: 7,
            timestamp: 1,
        }
    }
}

/// Generates a new Ed25519 secret key (NaCl layout: seed then public key).
pub fn generate_secret() -> [u8; 64] {
    let (_, secret) = pkgar_core::dryoc::classic::crypto_sign::crypto_sign_keypair();
    secret
}

/// Role keys for a registry where `key` holds every role, threshold 1:
/// enough for a test or a personal registry.
pub fn single_key_roles(key: KeyId) -> BTreeMap<RoleName, RoleKeys> {
    RoleName::ALL
        .into_iter()
        .map(|role| (role, RoleKeys::new([key], 1)))
        .collect()
}

/// The secret keys available to [`Registry::sign`]. Each key signs every
/// role that lists it.
#[derive(Clone, Default)]
pub struct Keyring {
    secrets: BTreeMap<KeyId, [u8; 64]>,
}

impl fmt::Debug for Keyring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the secrets themselves.
        f.debug_struct("Keyring")
            .field("keys", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Keyring {
    /// An empty keyring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a secret key (NaCl layout) and returns its public key.
    pub fn add(&mut self, secret: [u8; 64]) -> KeyId {
        let key = fox_core::signing::public_key_of(&secret);
        self.secrets.insert(key, secret);
        key
    }

    /// The public keys held.
    pub fn keys(&self) -> impl Iterator<Item = &KeyId> {
        self.secrets.keys()
    }

    /// Whether the keyring holds any of `role`'s keys.
    pub fn holds_any(&self, role: &RoleKeys) -> bool {
        role.keyids.iter().any(|key| self.secrets.contains_key(key))
    }

    /// Whether the keyring holds `key`.
    pub fn holds(&self, key: &KeyId) -> bool {
        self.secrets.contains_key(key)
    }

    fn sign<T: Serialize>(
        &self,
        metadata: &mut Signed<T>,
        keys: &BTreeSet<KeyId>,
    ) -> Result<(), MetadataError> {
        for key in keys {
            if let Some(secret) = self.secrets.get(key) {
                metadata.sign(secret)?;
            }
        }
        Ok(())
    }
}

/// Options for [`Registry::sign`].
#[derive(Debug, Clone, Default)]
pub struct SignOptions {
    /// Expiry lengths for new versions.
    pub lifetimes: Lifetimes,
    /// Roles to give a new version even if nothing changed. Bumping root
    /// writes a new `metadata/root/<N>.json`.
    pub bump: BTreeSet<RoleName>,
}

/// What [`Registry::sign`] did to one metadata file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedFile {
    /// The file, relative to the registry.
    pub file: String,
    /// Its version now.
    pub version: u64,
    /// Whether its content or expiry changed: a new version, or the
    /// unsigned pending version updated.
    pub bumped: bool,
    /// Valid signatures by distinct role keys.
    pub valid: u32,
    /// The role's threshold.
    pub threshold: u32,
    /// For a root after the first: its signatures by the previous root's
    /// root keys, which clients also require. Each key set is checked
    /// against its own threshold.
    pub previous_root: Option<PreviousRootSignatures>,
}

/// A new root's signatures by the root keys of the root before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviousRootSignatures {
    /// Valid signatures by distinct previous root keys.
    pub valid: u32,
    /// The previous root role's threshold.
    pub threshold: u32,
}

impl SignedFile {
    /// Whether the file is signed by enough keys: for a root, a threshold
    /// of its own root keys and, separately, of the previous root's.
    pub fn complete(&self) -> bool {
        let previous = self
            .previous_root
            .is_none_or(|old| old.threshold > 0 && old.valid >= old.threshold);
        self.threshold > 0 && self.valid >= self.threshold && previous
    }
}

/// The outcome of [`Registry::sign`], in signing order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignReport {
    /// One entry per metadata file.
    pub files: Vec<SignedFile>,
}

impl SignReport {
    /// Whether every file is signed by enough keys.
    pub fn complete(&self) -> bool {
        self.files.iter().all(SignedFile::complete)
    }
}

/// Why a registry could not be edited.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditError {
    /// A file or directory could not be read or written.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// A metadata file is invalid or could not be signed.
    #[error("metadata file {file}")]
    Metadata {
        /// The file, relative to the registry.
        file: String,
        /// The cause.
        #[source]
        source: MetadataError,
    },
    /// A registry file is invalid.
    #[error("registry file {path}")]
    Layout {
        /// The file, relative to the registry.
        path: String,
        /// The cause.
        #[source]
        source: Box<LayoutError>,
    },
    /// An advisory is invalid or cannot be written.
    #[error("invalid advisory")]
    Advisory(#[source] Box<AdvisoryError>),
    /// A VEX document is invalid or its id unusable.
    #[error("invalid VEX document")]
    Vex(#[source] Box<VexFileError>),
    /// [`Registry::init`] found an existing registry.
    #[error("{} already holds a registry", path.display())]
    Exists {
        /// The directory.
        path: PathBuf,
    },
    /// The directory has no root metadata.
    #[error("{} is not a registry: no metadata/root/1.json", path.display())]
    NotARegistry {
        /// The directory.
        path: PathBuf,
    },
    /// The package is already in the registry.
    #[error("package {name} is already admitted")]
    AlreadyAdmitted {
        /// The package.
        name: PackageName,
    },
    /// The package is not in the registry.
    #[error("no package {name} in the registry")]
    UnknownPackage {
        /// The package.
        name: PackageName,
    },
    /// The package has no such release.
    #[error("package {name} has no release {version}")]
    UnknownRelease {
        /// The package.
        name: PackageName,
        /// The version.
        version: Version,
    },
    /// The package already has a release with this version.
    #[error("package {name} already has release {version}")]
    DuplicateRelease {
        /// The package.
        name: PackageName,
        /// The version.
        version: Version,
    },
    /// A file in a signed directory has a name that cannot be signed.
    #[error("cannot sign {}: not a plain relative path", path.display())]
    UnsafePath {
        /// The file.
        path: PathBuf,
    },
    /// A role was given no keys or a threshold above its key count.
    #[error("role {role} needs between 1 and {keys} signatures, not {threshold}")]
    BadThreshold {
        /// The role.
        role: &'static str,
        /// The keys given.
        keys: usize,
        /// The threshold given.
        threshold: u32,
    },
    /// A metadata version would overflow.
    #[error("metadata file {file} is at the highest version")]
    VersionOverflow {
        /// The file.
        file: String,
    },
    /// The root metadata does not list a top-level role.
    #[error("root metadata has no {role} role")]
    MissingRole {
        /// The missing role.
        role: &'static str,
    },
}

/// A registry working tree opened for editing.
#[derive(Debug, Clone)]
pub struct Registry {
    dir: PathBuf,
    root: Signed<Root>,
    previous_root: Option<Signed<Root>>,
    targets: Signed<Targets>,
    delegations: BTreeMap<String, Signed<Targets>>,
    snapshot: Signed<Snapshot>,
    timestamp: Signed<Timestamp>,
}

impl Registry {
    /// Creates a registry named `name` in `dir` with the given role keys:
    /// `registry.toml`, the package, artifact, advisory and VEX directories, and
    /// version 1 of every role, unsigned. Call [`Registry::sign`] next.
    ///
    /// # Errors
    ///
    /// [`EditError::Exists`] if `dir` already has `registry.toml` or
    /// `metadata/`, [`EditError::BadThreshold`] if a role's keys are
    /// unusable, and [`EditError::Io`] or [`EditError::Layout`] if a file
    /// cannot be written.
    pub fn init(
        dir: &Path,
        name: &str,
        roles: &BTreeMap<RoleName, RoleKeys>,
        now: OffsetDateTime,
    ) -> Result<Self, EditError> {
        for existing in [REGISTRY_FILE, METADATA_DIR] {
            if dir.join(existing).exists() {
                return Err(EditError::Exists {
                    path: dir.to_path_buf(),
                });
            }
        }
        for role in RoleName::ALL {
            check_role(role.as_str(), roles.get(&role))?;
        }
        let lifetimes = Lifetimes::default();
        let config = RegistryConfig::new(name);
        let text = config.to_toml().map_err(|source| EditError::Layout {
            path: REGISTRY_FILE.to_owned(),
            source: Box::new(source),
        })?;
        write_file(&dir.join(REGISTRY_FILE), text.as_bytes())?;
        for sub in [
            PACKAGES_DIR,
            ARTIFACTS_DIR,
            ADVISORIES_DIR,
            VEX_DIR,
            ROOT_DIR,
            DELEGATIONS_DIR,
        ] {
            create_dir(&dir.join(sub))?;
        }
        let root = Signed::new(Root {
            kind: Default::default(),
            spec_version: SpecVersion::V1,
            version: 1,
            expires: Expiry::days_after(now, lifetimes.root),
            registry: name.to_owned(),
            roles: roles.clone(),
        });
        let mut registry = Self {
            dir: dir.to_path_buf(),
            root,
            previous_root: None,
            targets: Signed::new(empty_targets(now, lifetimes.targets)),
            delegations: BTreeMap::new(),
            snapshot: Signed::new(Snapshot {
                kind: Default::default(),
                spec_version: SpecVersion::V1,
                version: 1,
                expires: Expiry::days_after(now, lifetimes.snapshot),
                meta: BTreeMap::new(),
            }),
            timestamp: Signed::new(Timestamp {
                kind: Default::default(),
                spec_version: SpecVersion::V1,
                version: 1,
                expires: Expiry::days_after(now, lifetimes.timestamp),
                snapshot: VersionMeta {
                    version: 1,
                    file: FileMeta::of(&[]),
                },
            }),
        };
        registry.write_root()?;
        // Write version 1 of the other roles, still unsigned.
        registry.sign(&Keyring::new(), now, &SignOptions::default())?;
        Ok(registry)
    }

    /// Opens the registry in `dir`.
    ///
    /// # Errors
    ///
    /// [`EditError::NotARegistry`] if it has no root metadata, and
    /// [`EditError::Io`] or [`EditError::Metadata`] if a metadata file
    /// cannot be read.
    pub fn open(dir: &Path) -> Result<Self, EditError> {
        let mut version = 1;
        if !dir.join(root_file(1)).is_file() {
            return Err(EditError::NotARegistry {
                path: dir.to_path_buf(),
            });
        }
        while dir.join(root_file(version + 1)).is_file() {
            version += 1;
        }
        let root: Signed<Root> = load(dir, &root_file(version))?;
        // `role_keys` trusts that every role is present, so refuse a root
        // that is not one this crate wrote instead of panicking later.
        for role in RoleName::ALL {
            if !root.signed.roles.contains_key(&role) {
                return Err(EditError::MissingRole {
                    role: role.as_str(),
                });
            }
            check_role(role.as_str(), root.signed.roles.get(&role))?;
        }
        let previous_root = if version > 1 {
            Some(load(dir, &root_file(version - 1))?)
        } else {
            None
        };
        let targets: Signed<Targets> = load(dir, &metadata_file(TARGETS_FILE))?;
        let mut delegations = BTreeMap::new();
        for delegation in &targets.signed.delegations {
            let file = metadata_file(&delegation_name(&delegation.name));
            let metadata = if dir.join(&file).is_file() {
                load(dir, &file)?
            } else {
                Signed::new(empty_targets(OffsetDateTime::UNIX_EPOCH, 0))
            };
            delegations.insert(delegation.name.clone(), metadata);
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            root,
            previous_root,
            targets,
            delegations,
            snapshot: load(dir, SNAPSHOT_FILE)?,
            timestamp: load(dir, TIMESTAMP_FILE)?,
        })
    }

    /// The registry directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The newest root, as clients should first trust it.
    pub fn root(&self) -> &Signed<Root> {
        &self.root
    }

    /// The top-level targets metadata.
    pub fn targets(&self) -> &Signed<Targets> {
        &self.targets
    }

    /// `registry.toml`.
    ///
    /// # Errors
    ///
    /// [`EditError::Io`] or [`EditError::Layout`] if it cannot be read.
    pub fn config(&self) -> Result<RegistryConfig, EditError> {
        let text = read_text(&self.dir.join(REGISTRY_FILE))?;
        RegistryConfig::from_toml(&text).map_err(|source| EditError::Layout {
            path: REGISTRY_FILE.to_owned(),
            source: Box::new(source),
        })
    }

    /// Replaces `registry.toml`.
    ///
    /// # Errors
    ///
    /// [`EditError::Layout`] or [`EditError::Io`] if it cannot be written.
    pub fn set_config(&self, config: &RegistryConfig) -> Result<(), EditError> {
        let text = config.to_toml().map_err(|source| EditError::Layout {
            path: REGISTRY_FILE.to_owned(),
            source: Box::new(source),
        })?;
        write_file(&self.dir.join(REGISTRY_FILE), text.as_bytes())
    }

    /// The package file of `name`, if there is one.
    ///
    /// # Errors
    ///
    /// [`EditError::Io`] or [`EditError::Layout`] if it cannot be read.
    pub fn package(&self, name: &PackageName) -> Result<Option<PackageEntry>, EditError> {
        let path = package_path(name);
        let file = self.dir.join(&path);
        if !file.is_file() {
            return Ok(None);
        }
        let text = read_text(&file)?;
        PackageEntry::from_toml(&text, &path)
            .map(Some)
            .map_err(|source| EditError::Layout {
                path,
                source: Box::new(source),
            })
    }

    /// Writes a package file, replacing any earlier one.
    ///
    /// # Errors
    ///
    /// [`EditError::Layout`] if the entry is invalid, and [`EditError::Io`]
    /// if it cannot be written.
    pub fn write_package(&self, entry: &PackageEntry) -> Result<(), EditError> {
        let (path, text) = package_text(entry)?;
        write_file(&self.dir.join(path), text.as_bytes())
    }

    /// Adds a package: writes its file and delegates that file to its
    /// owners, any one of whom may sign it. The top-level targets are
    /// written as an unsigned pending version that lists the delegation.
    ///
    /// # Errors
    ///
    /// [`EditError::AlreadyAdmitted`] if the package file or delegation
    /// exists, and as [`Registry::write_package`].
    pub fn admit(&mut self, entry: &PackageEntry) -> Result<(), EditError> {
        self.admit_all(core::slice::from_ref(entry))
    }

    /// Admits several packages, as [`Registry::admit`] does for each, but
    /// writes `targets.json` once. Nothing is written unless every package
    /// is new and valid, and if a write fails the package files already
    /// written are removed again, so a failed batch can be retried as is.
    ///
    /// # Errors
    ///
    /// [`EditError::AlreadyAdmitted`] if any package file or delegation
    /// exists or a name repeats, and as [`Registry::write_package`].
    pub fn admit_all(&mut self, entries: &[PackageEntry]) -> Result<(), EditError> {
        let mut names = BTreeSet::new();
        for entry in entries {
            let path = package_path(&entry.name);
            if self.dir.join(&path).exists()
                || self.delegations.contains_key(entry.name.as_str())
                || !names.insert(entry.name.as_str())
            {
                return Err(EditError::AlreadyAdmitted {
                    name: entry.name.clone(),
                });
            }
        }
        if entries.is_empty() {
            return Ok(());
        }
        // Everything that can fail without touching the disk is done
        // first: each package file is serialized and checked, and the new
        // targets metadata is built and serialized aside.
        let files = entries
            .iter()
            .map(package_text)
            .collect::<Result<Vec<_>, _>>()?;
        let mut targets = self.targets.clone();
        for entry in entries {
            targets.signed.delegations.push(Delegation {
                name: entry.name.as_str().to_owned(),
                keys: RoleKeys::new(entry.owners.iter().map(|owner| owner.delegation_key()), 1),
                paths: BTreeSet::from([package_path(&entry.name)]),
            });
        }
        targets
            .signed
            .delegations
            .sort_by(|a, b| a.name.cmp(&b.name));
        // The delegations must survive until the next `sign`, which may run
        // in another process.
        if !targets.signatures.is_empty() {
            targets.signed.version = targets.signed.version.checked_add(1).ok_or_else(|| {
                EditError::VersionOverflow {
                    file: metadata_file(TARGETS_FILE),
                }
            })?;
            targets.signatures.clear();
        }
        let file = metadata_file(TARGETS_FILE);
        let bytes = targets.to_json().map_err(|source| EditError::Metadata {
            file: file.clone(),
            source,
        })?;

        // Then the writes, undone if one fails, so that a failed batch can
        // simply be retried.
        let mut written = Vec::new();
        let result = files
            .iter()
            .try_for_each(|(path, text)| {
                let full = self.dir.join(path);
                write_file(&full, text.as_bytes())?;
                written.push(full);
                Ok(())
            })
            .and_then(|()| write_file(&self.dir.join(&file), &bytes));
        if let Err(error) = result {
            for path in &written {
                remove_new_file(path);
            }
            return Err(error);
        }
        self.targets = targets;
        for entry in entries {
            self.delegations.insert(
                entry.name.as_str().to_owned(),
                Signed::new(empty_targets(OffsetDateTime::UNIX_EPOCH, 0)),
            );
        }
        Ok(())
    }

    /// Records a release in a package file.
    ///
    /// # Errors
    ///
    /// [`EditError::UnknownPackage`], [`EditError::DuplicateRelease`], and
    /// as [`Registry::package`] and [`Registry::write_package`].
    pub fn add_release(&self, name: &PackageName, release: Release) -> Result<(), EditError> {
        let mut entry = self.existing_package(name)?;
        if entry.release(&release.version).is_some() {
            return Err(EditError::DuplicateRelease {
                name: name.clone(),
                version: release.version,
            });
        }
        entry.releases.push(release);
        entry.releases.sort_by(|a, b| a.version.cmp(&b.version));
        self.write_package(&entry)
    }

    /// Marks a release yanked, or un-yanks it.
    ///
    /// # Errors
    ///
    /// [`EditError::UnknownPackage`], [`EditError::UnknownRelease`], and as
    /// [`Registry::package`] and [`Registry::write_package`].
    pub fn set_yanked(
        &self,
        name: &PackageName,
        version: &Version,
        yanked: bool,
    ) -> Result<(), EditError> {
        let mut entry = self.existing_package(name)?;
        let release = entry
            .releases
            .iter_mut()
            .find(|release| &release.version == version)
            .ok_or_else(|| EditError::UnknownRelease {
                name: name.clone(),
                version: version.clone(),
            })?;
        release.yanked = yanked;
        self.write_package(&entry)
    }

    /// The artifact list of `name` `version`, if there is one.
    ///
    /// # Errors
    ///
    /// [`EditError::Io`] or [`EditError::Layout`] if it cannot be read.
    pub fn artifacts(
        &self,
        name: &PackageName,
        version: &Version,
    ) -> Result<Option<ArtifactList>, EditError> {
        let path = artifacts_path(name, version);
        let file = self.dir.join(&path);
        if !file.is_file() {
            return Ok(None);
        }
        let text = read_text(&file)?;
        ArtifactList::from_toml(&text, &path)
            .map(Some)
            .map_err(|source| EditError::Layout {
                path,
                source: Box::new(source),
            })
    }

    /// Writes an artifact list.
    ///
    /// # Errors
    ///
    /// [`EditError::Layout`] or [`EditError::Io`] if it cannot be written.
    pub fn write_artifacts(&self, list: &ArtifactList) -> Result<(), EditError> {
        let path = artifacts_path(&list.name, &list.version);
        let text = list.to_toml().map_err(|source| EditError::Layout {
            path: path.clone(),
            source: Box::new(source),
        })?;
        write_file(&self.dir.join(&path), text.as_bytes())
    }

    /// Writes an advisory to `advisories/<id>.toml`, replacing one with
    /// the same id (to update or withdraw it).
    ///
    /// # Errors
    ///
    /// [`EditError::Advisory`] if its id is unusable or it cannot be
    /// serialized, and [`EditError::Io`] if it cannot be written.
    pub fn write_advisory(&self, advisory: &Advisory) -> Result<(), EditError> {
        let advisory_error = |source| EditError::Advisory(Box::new(source));
        let path = advisory_path(&advisory.advisory.id).map_err(advisory_error)?;
        let text = advisory.to_toml().map_err(advisory_error)?;
        write_file(&self.dir.join(path), text.as_bytes())
    }

    /// Writes VEX document `id` to `vex/<id>.cdx.json`, replacing one with
    /// the same id; returns its path. The bytes are stored as given, after
    /// checking that they are VEX the client can use.
    ///
    /// # Errors
    ///
    /// [`EditError::Vex`] if the id is unusable or the document is not
    /// usable VEX, and [`EditError::Io`] if it cannot be written.
    pub fn write_vex(&self, id: &str, json: &[u8]) -> Result<String, EditError> {
        let vex_error = |source| EditError::Vex(Box::new(source));
        let path = vex_path(id).map_err(vex_error)?;
        parse_vex(json).map_err(vex_error)?;
        write_file(&self.dir.join(&path), json)?;
        Ok(path)
    }

    /// Removes VEX document `id`, returning its path, or `None` if there
    /// was none.
    ///
    /// # Errors
    ///
    /// [`EditError::Vex`] if the id is unusable, and [`EditError::Io`] if
    /// the file cannot be removed.
    pub fn remove_vex(&self, id: &str) -> Result<Option<String>, EditError> {
        let path = vex_path(id).map_err(|source| EditError::Vex(Box::new(source)))?;
        match fs::remove_file(self.dir.join(&path)) {
            Ok(()) => Ok(Some(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(EditError::Io {
                path: self.dir.join(&path),
                source,
            }),
        }
    }

    /// Starts a new root version that gives `role` these keys, or changes
    /// the newest root in place while clients would not yet accept it.
    /// Either way the root has no signatures until [`Registry::sign`] adds
    /// them: an in-place edit drops every signature already on it, by the
    /// new and the previous root keys alike, since they approved other
    /// bytes. Clients accept it only when a threshold of both the old and
    /// the new root keys have signed.
    ///
    /// # Errors
    ///
    /// [`EditError::BadThreshold`] if the keys are unusable, and
    /// [`EditError::Io`] or [`EditError::Metadata`] if the root cannot be
    /// written.
    pub fn set_role(
        &mut self,
        role: RoleName,
        keys: RoleKeys,
        now: OffsetDateTime,
        lifetimes: &Lifetimes,
    ) -> Result<(), EditError> {
        check_role(role.as_str(), Some(&keys))?;
        if !self.root_published() {
            // No client can trust a root that is unsigned, or that is only
            // partly signed for a rotation in progress, so edit it in place
            // rather than chaining a version nothing can follow.
            self.root.signed.roles.insert(role, keys);
            self.root.signed.expires = Expiry::days_after(now, lifetimes.root);
            // Changing the roles changes the bytes a signature covers, so
            // no signature collected for the pending root, from either key
            // set, may count towards the edited one.
            self.root.signatures.clear();
        } else {
            let mut next = self.next_root(now, lifetimes)?;
            next.roles.insert(role, keys);
            self.previous_root = Some(std::mem::replace(&mut self.root, Signed::new(next)));
        }
        self.write_root()
    }

    /// Whether clients would accept the current root: signed by a threshold
    /// of both its own root keys and the previous root's, as a rotation
    /// requires.
    fn root_published(&self) -> bool {
        let Some(own) = self.root.signed.roles.get(&RoleName::Root) else {
            return false;
        };
        if count_valid(&self.root, own) < own.threshold {
            return false;
        }
        match self
            .previous_root
            .as_ref()
            .and_then(|previous| previous.signed.roles.get(&RoleName::Root))
        {
            Some(old) => count_valid(&self.root, old) >= old.threshold,
            None => true,
        }
    }

    /// Updates and signs every metadata file; see the module docs.
    ///
    /// # Errors
    ///
    /// [`EditError::Io`] if a file cannot be read or written,
    /// [`EditError::UnsafePath`] if a signed directory holds an unusable
    /// name, and [`EditError::Metadata`] if signing fails.
    pub fn sign(
        &mut self,
        keyring: &Keyring,
        now: OffsetDateTime,
        options: &SignOptions,
    ) -> Result<SignReport, EditError> {
        let mut report = SignReport::default();
        let lifetimes = &options.lifetimes;
        if options.bump.contains(&RoleName::Root) && self.root_published() {
            let next = self.next_root(now, lifetimes)?;
            self.previous_root = Some(std::mem::replace(&mut self.root, Signed::new(next)));
        }
        report.files.push(self.sign_root(keyring)?);

        let delegated: BTreeSet<String> = self
            .targets
            .signed
            .delegations
            .iter()
            .flat_map(|delegation| delegation.paths.iter().cloned())
            .collect();
        let mut snapshot_meta = BTreeMap::new();
        let delegations = self.targets.signed.delegations.clone();
        for delegation in &delegations {
            let mut wanted = BTreeMap::new();
            for path in &delegation.paths {
                let file = self.dir.join(path);
                if file.is_file() {
                    wanted.insert(path.clone(), FileMeta::of(&read(&file)?));
                }
            }
            let name = delegation_name(&delegation.name);
            let metadata = self
                .delegations
                .entry(delegation.name.clone())
                .or_insert_with(|| Signed::new(empty_targets(OffsetDateTime::UNIX_EPOCH, 0)));
            let step = Step {
                keyring,
                now,
                days: lifetimes.delegation,
                force: false,
                keys: &delegation.keys,
            };
            let (signed, bytes) = step.apply(metadata, &name, |targets| {
                targets.targets = wanted;
                targets.delegations.clear();
            })?;
            write_file(&self.dir.join(metadata_file(&name)), &bytes)?;
            snapshot_meta.insert(name, version_meta(metadata.signed.version, &bytes));
            report.files.push(signed);
        }

        let wanted = self.top_level_targets(&delegated)?;
        let step = Step {
            keyring,
            now,
            days: lifetimes.targets,
            force: options.bump.contains(&RoleName::Targets),
            keys: &self.role_keys(RoleName::Targets)?.clone(),
        };
        let (signed, bytes) = step.apply(&mut self.targets, TARGETS_FILE, |targets| {
            targets.targets = wanted;
        })?;
        write_file(&self.dir.join(metadata_file(TARGETS_FILE)), &bytes)?;
        snapshot_meta.insert(
            TARGETS_FILE.to_owned(),
            version_meta(self.targets.signed.version, &bytes),
        );
        report.files.push(signed);

        let step = Step {
            keyring,
            now,
            days: lifetimes.snapshot,
            force: options.bump.contains(&RoleName::Snapshot),
            keys: &self.role_keys(RoleName::Snapshot)?.clone(),
        };
        let (signed, bytes) = step.apply(&mut self.snapshot, SNAPSHOT_FILE, |snapshot| {
            snapshot.meta = snapshot_meta;
        })?;
        write_file(&self.dir.join(SNAPSHOT_FILE), &bytes)?;
        let snapshot_meta = version_meta(self.snapshot.signed.version, &bytes);
        report.files.push(signed);

        let step = Step {
            keyring,
            now,
            days: lifetimes.timestamp,
            force: options.bump.contains(&RoleName::Timestamp),
            keys: &self.role_keys(RoleName::Timestamp)?.clone(),
        };
        let (signed, bytes) = step.apply(&mut self.timestamp, TIMESTAMP_FILE, |timestamp| {
            timestamp.snapshot = snapshot_meta;
        })?;
        write_file(&self.dir.join(TIMESTAMP_FILE), &bytes)?;
        report.files.push(signed);
        Ok(report)
    }

    fn existing_package(&self, name: &PackageName) -> Result<PackageEntry, EditError> {
        self.package(name)?
            .ok_or_else(|| EditError::UnknownPackage { name: name.clone() })
    }

    fn role_keys(&self, role: RoleName) -> Result<&RoleKeys, EditError> {
        self.root
            .signed
            .roles
            .get(&role)
            .ok_or(EditError::MissingRole {
                role: role.as_str(),
            })
    }

    fn next_root(&self, now: OffsetDateTime, lifetimes: &Lifetimes) -> Result<Root, EditError> {
        let mut next = self.root.signed.clone();
        next.version = next
            .version
            .checked_add(1)
            .ok_or_else(|| EditError::VersionOverflow {
                file: root_file(next.version),
            })?;
        next.expires = Expiry::days_after(now, lifetimes.root);
        Ok(next)
    }

    fn sign_root(&mut self, keyring: &Keyring) -> Result<SignedFile, EditError> {
        let file = root_file(self.root.signed.version);
        let mut keys = self.role_keys(RoleName::Root)?.keyids.clone();
        if let Some(old) = self
            .previous_root
            .as_ref()
            .and_then(|previous| previous.signed.roles.get(&RoleName::Root))
        {
            keys.extend(old.keyids.iter().copied());
        }
        keyring
            .sign(&mut self.root, &keys)
            .map_err(|source| EditError::Metadata {
                file: file.clone(),
                source,
            })?;
        self.write_root()?;
        // A rotation is only complete when a threshold of the previous root
        // keys signed too; each key set is reported against its own
        // threshold, as clients check them.
        let own = self.role_keys(RoleName::Root)?;
        let previous_root = self
            .previous_root
            .as_ref()
            .and_then(|previous| previous.signed.roles.get(&RoleName::Root))
            .map(|old| PreviousRootSignatures {
                valid: count_valid(&self.root, old),
                threshold: old.threshold,
            });
        Ok(SignedFile {
            file,
            version: self.root.signed.version,
            bumped: false,
            valid: count_valid(&self.root, own),
            threshold: own.threshold,
            previous_root,
        })
    }

    fn write_root(&self) -> Result<(), EditError> {
        let file = root_file(self.root.signed.version);
        let bytes = self.root.to_json().map_err(|source| EditError::Metadata {
            file: file.clone(),
            source,
        })?;
        write_file(&self.dir.join(file), &bytes)
    }

    /// Every file under the signed directories that no delegation covers.
    fn top_level_targets(
        &self,
        delegated: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, FileMeta>, EditError> {
        let mut files = BTreeMap::new();
        let config = self.dir.join(REGISTRY_FILE);
        files.insert(REGISTRY_FILE.to_owned(), FileMeta::of(&read(&config)?));
        for sub in [PACKAGES_DIR, ARTIFACTS_DIR, ADVISORIES_DIR, VEX_DIR] {
            let mut found = Vec::new();
            walk(&self.dir, &self.dir.join(sub), &mut found)?;
            for path in found {
                if !delegated.contains(&path) {
                    let meta = FileMeta::of(&read(&self.dir.join(&path))?);
                    files.insert(path, meta);
                }
            }
        }
        Ok(files)
    }
}

/// One role's update in [`Registry::sign`].
struct Step<'a> {
    keyring: &'a Keyring,
    now: OffsetDateTime,
    days: u32,
    force: bool,
    keys: &'a RoleKeys,
}

impl Step<'_> {
    /// Applies `update` to `metadata`; bumps it if that changed anything,
    /// if forced, or if it is due for renewal and can be signed; then signs
    /// it. Returns the report line and the file bytes.
    fn apply<T>(
        &self,
        metadata: &mut Signed<T>,
        file: &str,
        update: impl FnOnce(&mut T),
    ) -> Result<(SignedFile, Vec<u8>), EditError>
    where
        T: Serialize + Clone + PartialEq + Versioned + VersionedMut,
    {
        let error = |source| EditError::Metadata {
            file: file.to_owned(),
            source,
        };
        let mut candidate = metadata.signed.clone();
        update(&mut candidate);
        let changed = candidate != metadata.signed;
        let holds_key = self.keyring.holds_any(self.keys);
        // Renewing writes a new version over the published one, so only do
        // it when the keyring can sign the replacement to the role's
        // threshold; a threshold-two role renewed by one key would discard
        // the still-valid metadata and take the registry offline until the
        // other keys sign.
        let held = self
            .keys
            .keyids
            .iter()
            .filter(|key| self.keyring.holds(key))
            .count();
        let can_meet = u32::try_from(held).map_or(true, |held| held >= self.keys.threshold);
        let renew = holds_key && can_meet && due_for_renewal(&metadata.signed, self.now, self.days);
        let bumped = changed || self.force || renew;
        if bumped {
            // A version nobody has signed yet was never published, so it
            // takes the change instead of making way for another version.
            let version = if metadata.signatures.is_empty() {
                candidate.version()
            } else {
                candidate
                    .version()
                    .checked_add(1)
                    .ok_or_else(|| EditError::VersionOverflow {
                        file: file.to_owned(),
                    })?
            };
            candidate.set_version(version, Expiry::days_after(self.now, self.days));
            *metadata = Signed::new(candidate);
        }
        // Signatures by keys the role no longer lists only take up space.
        metadata
            .signatures
            .retain(|signature| self.keys.keyids.contains(&signature.keyid));
        self.keyring
            .sign(metadata, &self.keys.keyids)
            .map_err(error)?;
        let bytes = metadata.to_json().map_err(error)?;
        Ok((
            SignedFile {
                file: file.to_owned(),
                version: metadata.signed.version(),
                bumped,
                valid: count_valid(metadata, self.keys),
                threshold: self.keys.threshold,
                previous_root: None,
            },
            bytes,
        ))
    }
}

/// Metadata whose version and expiry [`Step`] can set.
trait VersionedMut {
    fn set_version(&mut self, version: u64, expires: Expiry);
}

macro_rules! versioned_mut {
    ($($t:ty),*) => {$(
        impl VersionedMut for $t {
            fn set_version(&mut self, version: u64, expires: Expiry) {
                self.version = version;
                self.expires = expires;
            }
        }
    )*};
}

versioned_mut!(Targets, Snapshot, Timestamp);

/// Whether less than half of a `days`-day lifetime is left.
fn due_for_renewal(metadata: &impl Versioned, now: OffsetDateTime, days: u32) -> bool {
    let half = time::Duration::hours(i64::from(days) * 12);
    metadata.expires().time() - now < half
}

fn count_valid<T: Serialize>(metadata: &Signed<T>, keys: &RoleKeys) -> u32 {
    match metadata.verify(&RoleKeys::new(keys.keyids.iter().copied(), u32::MAX)) {
        Err(MetadataError::Threshold { valid, .. }) => valid,
        Ok(()) => u32::MAX,
        Err(_) => 0,
    }
}

fn check_role(role: &'static str, keys: Option<&RoleKeys>) -> Result<(), EditError> {
    let (count, threshold) = keys.map_or((0, 0), |keys| (keys.keyids.len(), keys.threshold));
    if threshold == 0 || usize::try_from(threshold).map_or(true, |t| t > count) {
        return Err(EditError::BadThreshold {
            role,
            keys: count,
            threshold,
        });
    }
    Ok(())
}

fn empty_targets(now: OffsetDateTime, days: u32) -> Targets {
    Targets {
        kind: Default::default(),
        spec_version: SpecVersion::V1,
        version: 1,
        expires: Expiry::days_after(now, days),
        targets: BTreeMap::new(),
        delegations: Vec::new(),
    }
}

fn version_meta(version: u64, bytes: &[u8]) -> VersionMeta {
    VersionMeta {
        version,
        file: FileMeta::of(bytes),
    }
}

fn root_file(version: u64) -> String {
    format!("{ROOT_DIR}/{version}.json")
}

fn metadata_file(name: &str) -> String {
    format!("{METADATA_DIR}/{name}")
}

fn delegation_name(role: &str) -> String {
    format!("delegations/{role}.json")
}

fn load<T: for<'de> serde::Deserialize<'de>>(
    dir: &Path,
    file: &str,
) -> Result<Signed<T>, EditError> {
    let bytes = read(&dir.join(file))?;
    Signed::from_json(&bytes).map_err(|source| EditError::Metadata {
        file: file.to_owned(),
        source,
    })
}

/// Collects the regular files under `dir`, as paths relative to `base`.
fn walk(base: &Path, dir: &Path, found: &mut Vec<String>) -> Result<(), EditError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(EditError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| EditError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|source| EditError::Io {
            path: path.clone(),
            source,
        })?;
        if kind.is_dir() {
            walk(base, &path, found)?;
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .ok()
            .and_then(Path::to_str)
            .filter(|relative| kind.is_file() && check_path(relative).is_ok())
            .ok_or_else(|| EditError::UnsafePath { path: path.clone() })?;
        found.push(relative.to_owned());
    }
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>, EditError> {
    fs::read(path).map_err(|source| EditError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_text(path: &Path) -> Result<String, EditError> {
    fs::read_to_string(path).map_err(|source| EditError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn create_dir(path: &Path) -> Result<(), EditError> {
    fs::create_dir_all(path).map_err(|source| EditError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// A package file's path and text, checked to be what a client accepts.
fn package_text(entry: &PackageEntry) -> Result<(String, String), EditError> {
    let path = package_path(&entry.name);
    let layout = |source| EditError::Layout {
        path: path.clone(),
        source: Box::new(source),
    };
    let text = entry.to_toml().map_err(layout)?;
    // Write only what a client would accept.
    PackageEntry::from_toml(&text, &path).map_err(layout)?;
    Ok((path, text))
}

/// Best-effort removal of a file written by a batch that failed, and of
/// its directory if that is now empty. A file that cannot be removed is
/// reported by the batch's retry as already admitted.
fn remove_new_file(path: &Path) {
    if fs::remove_file(path).is_ok()
        && let Some(parent) = path.parent()
    {
        let _ = fs::remove_dir(parent);
    }
}

/// Writes `bytes` to `path` through a temporary file and a rename, so a
/// failed write (a full disk, say) leaves the old file whole instead of
/// truncated, and a batch that fails part-way can be retried.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), EditError> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    fox_core::config::replace_file(path, bytes).map_err(|source| EditError::Io {
        path: path.to_path_buf(),
        source,
    })
}
