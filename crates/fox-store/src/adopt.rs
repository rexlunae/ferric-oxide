//! Adopting the packages that upstream `pkg` installed.
//!
//! An image built by the installer, or a system managed with `pkg`, keeps
//! its installed-package state in two places:
//!
//! - `etc/pkg/packages.toml`: the installed package names and their
//!   dependencies ([`PKG_STATE`]);
//! - `var/lib/packages/<name>.pkgar_head`: the signed head (header and
//!   entry table) of each one ([`PKG_HEADS_DIR`]).
//!
//! [`Store::adopt_pkg`] turns that into `fox` records without touching any
//! installed file. Each head must verify against the trusted keys, exactly
//! as for [`Store::verify`], so the file list and hashes `fox` records are
//! the signed ones. The package's identity comes from its signed manifest
//! when the head lists one and the installed copy still hashes to it;
//! otherwise only `pkg`'s state names the package, which is accepted with
//! [`AdoptOptions::allow_unsigned_metadata`] and recorded as
//! [`MetadataKind::Legacy`].
//!
//! Adoption does not hash the installed files: run [`Store::verify`]
//! afterwards to compare them with the signed entries. It runs once per
//! root: the database remembers it, so packages removed with `fox` later
//! are not brought back by `pkg`'s stale state. Packages it skipped are
//! remembered as pending, and a later adoption retries those alone (once
//! their head is restored or their signer trusted, say).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use fox_core::{
    Blake3Digest, FileKind, FileRecord, Manifest, ManifestError, NameError, PackageName,
    TrustedKeys, VerifiedHead, VerifyError, manifest_path, verify_head,
};
use serde::Deserialize;

use crate::db::{Database, InstalledPackage, MetadataKind, PackageState};
use crate::fsutil::symlinked_parent;
use crate::{Store, StoreError, reserved_dir};

/// `pkg`'s installed-package state, relative to the root.
pub const PKG_STATE: &str = "etc/pkg/packages.toml";

/// Directory, relative to the root, of the heads `pkg` keeps.
pub const PKG_HEADS_DIR: &str = "var/lib/packages";

/// Options for [`Store::adopt_pkg`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AdoptOptions {
    /// Adopt packages that have no signed manifest, named only by `pkg`'s
    /// state. They are recorded as [`MetadataKind::Legacy`].
    pub allow_unsigned_metadata: bool,
    /// Adopt every package in `pkg`'s state, even if this root's state was
    /// adopted before. Without it, a later adoption only retries the
    /// packages earlier ones skipped.
    pub again: bool,
}

/// What [`Store::adopt_pkg`] did.
#[derive(Debug, Default)]
pub struct AdoptReport {
    /// The root's `pkg` state had already been adopted with nothing
    /// pending, so nothing was done.
    pub already_adopted: bool,
    /// Packages now recorded by `fox`.
    pub adopted: Vec<AdoptedPackage>,
    /// Packages left out, and why.
    pub skipped: Vec<SkippedPackage>,
}

/// One package [`Store::adopt_pkg`] recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedPackage {
    /// The package.
    pub name: PackageName,
    /// Its version (`unknown` without a signed manifest).
    pub version: String,
    /// Where its identity came from.
    pub metadata: MetadataKind,
    /// Number of files recorded.
    pub files: usize,
}

/// One package [`Store::adopt_pkg`] did not record.
#[derive(Debug)]
pub struct SkippedPackage {
    /// The name `pkg`'s state gives it.
    pub name: String,
    /// Why it was left out.
    pub reason: SkipReason,
}

/// Why [`Store::adopt_pkg`] left a package out.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SkipReason {
    /// `fox` already records a package of this name.
    #[error("already installed")]
    AlreadyInstalled,
    /// The name in `pkg`'s state is not a valid package name.
    #[error("invalid package name")]
    Name(#[source] NameError),
    /// `pkg` kept no head for it.
    #[error("no signed head at {PKG_HEADS_DIR}")]
    NoHead,
    /// The head failed verification, for example because its signer is not
    /// trusted.
    #[error("signed head failed verification")]
    Head(#[source] VerifyError),
    /// The head lists a signed manifest for another package.
    #[error("signed manifest names {found}")]
    ManifestName {
        /// The name the manifest gives.
        found: PackageName,
    },
    /// The signed manifest is not UTF-8.
    #[error("signed manifest is not UTF-8")]
    ManifestNotUtf8,
    /// The signed manifest is not valid.
    #[error("signed manifest is invalid")]
    Manifest(#[source] ManifestError),
    /// There is no signed manifest and unsigned names are not allowed.
    #[error("no signed manifest (use --allow-unsigned-metadata to adopt it by pkg's name)")]
    UnsignedMetadata,
    /// A file is in a directory only `fox` may write.
    #[error("{path} is reserved: packages may not write to {reserved}")]
    ReservedPath {
        /// The package path.
        path: String,
        /// The reserved directory.
        reserved: &'static str,
    },
    /// A file is already recorded for another package.
    #[error("{path} already belongs to package {owner}")]
    FileConflict {
        /// The contested path.
        path: String,
        /// The package that owns it.
        owner: PackageName,
    },
}

/// The parts of `pkg`'s `packages.toml` adoption reads.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PkgState {
    installed: BTreeMap<String, PkgInstalled>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PkgInstalled {
    dependencies: BTreeSet<String>,
}

/// The identity a package is adopted under.
struct Identity {
    version: String,
    target: Option<String>,
    depends: Vec<PackageName>,
    metadata: MetadataKind,
}

impl Store {
    /// Records the packages `pkg` installed in this root: those listed in
    /// [`PKG_STATE`] with a signed head in [`PKG_HEADS_DIR`] that verifies
    /// against `keys`. Each is named by its signed manifest, or with
    /// [`AdoptOptions::allow_unsigned_metadata`] by `pkg`'s state alone.
    /// No installed file is changed or hashed, and adoption runs once per
    /// root unless [`AdoptOptions::again`]. Packages that cannot be adopted
    /// are listed in the report rather than failing the whole adoption, and
    /// remembered: a later adoption retries them, and only them, so
    /// packages removed with `fox` since are not brought back.
    ///
    /// # Errors
    ///
    /// - [`StoreError::PkgState`] if `pkg`'s state file is not valid;
    /// - [`StoreError::Io`], [`StoreError::Database`] or
    ///   [`StoreError::DatabaseSerialize`] for state-file errors.
    pub fn adopt_pkg(
        &self,
        keys: &TrustedKeys,
        options: AdoptOptions,
    ) -> Result<AdoptReport, StoreError> {
        let _lock = self.lock()?;
        let db_path = self.db_path();
        let mut db = Database::load(&db_path)?;
        let mut report = AdoptReport::default();
        let retry_only = db.legacy_adopted && !options.again;
        if retry_only && db.legacy_pending.is_empty() {
            report.already_adopted = true;
            return Ok(report);
        }
        let mut state = read_pkg_state(&self.root().join(PKG_STATE))?;
        if retry_only {
            state.retain(|name, _| db.legacy_pending.contains(name));
        }
        // Rebuilt from this run's skips; a pending package that left pkg's
        // state has nothing left to adopt.
        let mut pending = BTreeSet::new();
        for (name, installed) in state {
            match self.adopt_one(&db, &name, &installed, keys, options)? {
                Ok((package, head)) => {
                    let head_path = self.head_path(&package.name);
                    write_atomic(&head_path, &head)?;
                    report.adopted.push(AdoptedPackage {
                        name: package.name.clone(),
                        version: package.version.clone(),
                        metadata: package.metadata,
                        files: package.files.len(),
                    });
                    db.packages.push(package);
                }
                // fox already records it, so it is accounted for.
                Err(reason @ SkipReason::AlreadyInstalled) => {
                    report.skipped.push(SkippedPackage { name, reason });
                }
                Err(reason) => {
                    pending.insert(name.clone());
                    report.skipped.push(SkippedPackage { name, reason });
                }
            }
        }
        db.legacy_adopted = true;
        db.legacy_pending = pending;
        db.save(&db_path)?;
        Ok(report)
    }

    /// Builds the record for one package, or says why it is skipped. The
    /// outer error is for failures that stop the whole adoption.
    fn adopt_one(
        &self,
        db: &Database,
        name: &str,
        installed: &PkgInstalled,
        keys: &TrustedKeys,
        options: AdoptOptions,
    ) -> Result<Result<(InstalledPackage, Vec<u8>), SkipReason>, StoreError> {
        let name = match PackageName::new(name) {
            Ok(name) => name,
            Err(error) => return Ok(Err(SkipReason::Name(error))),
        };
        if db.get(&name).is_some() {
            return Ok(Err(SkipReason::AlreadyInstalled));
        }
        let head_path = self
            .root()
            .join(PKG_HEADS_DIR)
            .join(format!("{name}.pkgar_head"));
        let bytes = match fs::read(&head_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Err(SkipReason::NoHead));
            }
            Err(source) => return Err(StoreError::io(&head_path, source)),
        };
        let head = match verify_head(&bytes, keys) {
            Ok(head) => head,
            Err(error) => return Ok(Err(SkipReason::Head(error))),
        };
        if let Some(reason) = placement_problem(db, &head) {
            return Ok(Err(reason));
        }
        let identity = match self.signed_identity(&name, &head)? {
            Some(Ok(identity)) => identity,
            Some(Err(reason)) => return Ok(Err(reason)),
            None if options.allow_unsigned_metadata => Identity {
                version: "unknown".to_owned(),
                target: None,
                depends: installed
                    .dependencies
                    .iter()
                    .filter_map(|dep| PackageName::new(dep.as_str()).ok())
                    .collect(),
                metadata: MetadataKind::Legacy,
            },
            None => return Ok(Err(SkipReason::UnsignedMetadata)),
        };
        let files = head
            .entries()
            .map(|entry| FileRecord {
                path: entry.path.to_owned(),
                kind: entry.kind,
                mode: entry.mode,
                size: self.installed_size(entry.path),
                blake3: entry.blake3,
            })
            .collect();
        let signer = head.signer();
        let package = InstalledPackage {
            name,
            version: identity.version,
            target: identity.target,
            depends: identity.depends,
            archive_blake3: None,
            entries_blake3: head.entries_blake3(),
            signer: signer.key,
            signer_label: signer.label.clone(),
            metadata: identity.metadata,
            state: PackageState::Installed,
            files,
        };
        Ok(Ok((package, bytes)))
    }

    /// The identity from the package's signed manifest, if the head lists
    /// one and the installed copy still matches it.
    fn signed_identity(
        &self,
        name: &PackageName,
        head: &VerifiedHead,
    ) -> Result<Option<Result<Identity, SkipReason>>, StoreError> {
        let path = manifest_path(name);
        let Some(entry) = head
            .entries()
            .find(|entry| entry.path == path && entry.kind == FileKind::File)
        else {
            return Ok(None);
        };
        if symlinked_parent(self.root(), &path)?.is_some() {
            return Ok(None);
        }
        let full = self.root().join(&path);
        let bytes = match fs::read(&full) {
            Ok(bytes) => bytes,
            // A missing or replaced manifest proves nothing; fall back to
            // the unsigned name.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::io(&full, source)),
        };
        if Blake3Digest::of(&bytes) != entry.blake3 {
            return Ok(None);
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(Some(Err(SkipReason::ManifestNotUtf8)));
        };
        let manifest = match Manifest::from_toml(text) {
            Ok(manifest) => manifest,
            Err(error) => return Ok(Some(Err(SkipReason::Manifest(error)))),
        };
        if &manifest.name != name {
            return Ok(Some(Err(SkipReason::ManifestName {
                found: manifest.name,
            })));
        }
        Ok(Some(Ok(Identity {
            version: manifest.version,
            target: manifest.target,
            depends: manifest.depends,
            metadata: MetadataKind::Embedded,
        })))
    }

    /// The size of an installed file (a symlink's target length), or 0 if
    /// it is gone. Head entries hold the stored size, which for compressed
    /// archives is not the installed one.
    fn installed_size(&self, path: &str) -> u64 {
        fs::symlink_metadata(self.root().join(path)).map_or(0, |meta| meta.len())
    }
}

/// Why the head's files cannot be recorded, if they cannot.
fn placement_problem(db: &Database, head: &VerifiedHead) -> Option<SkipReason> {
    let owners: BTreeMap<&str, &PackageName> = db
        .packages
        .iter()
        .flat_map(|p| p.files.iter().map(move |f| (f.path.as_str(), &p.name)))
        .collect();
    head.entries().find_map(|entry| {
        if let Some(reserved) = reserved_dir(entry.path) {
            return Some(SkipReason::ReservedPath {
                path: entry.path.to_owned(),
                reserved,
            });
        }
        owners
            .get(entry.path)
            .map(|owner| SkipReason::FileConflict {
                path: entry.path.to_owned(),
                owner: (*owner).clone(),
            })
    })
}

/// `pkg`'s installed packages, or none if it left no state.
fn read_pkg_state(path: &Path) -> Result<BTreeMap<String, PkgInstalled>, StoreError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => return Err(StoreError::io(path, source)),
    };
    let state: PkgState = toml::from_str(&text).map_err(|source| StoreError::PkgState {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(state.installed)
}

/// Writes `bytes` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| StoreError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| StoreError::io(path, e))
}
