//! The in-place package store and installed-package database for `fox`.
//!
//! This is the "simpler first version" of the store in the packaging design
//! (section 7.4): files are extracted straight into the install root, as
//! upstream `pkg` does, and each package's file list is recorded in
//! `<root>/var/lib/fox/installed.toml`. The content-addressed store with
//! generations replaces it in Phase P5 without changing archives.
//!
//! Under the root, the store keeps:
//!
//! - `var/lib/fox/installed.toml`: the database ([`InstalledPackage`] per
//!   package);
//! - `var/lib/fox/heads/<name>.pkgar_head`: each package's signed pkgar
//!   head, so [`Store::verify`] can re-check the signature and re-hash the
//!   installed files long after the archive is gone;
//! - `var/lib/fox/lock`: held while the database is changed.
//!
//! Install rules:
//!
//! - archives must already be verified ([`Archive::open`]);
//! - a path owned by another package is a conflict;
//! - an existing path owned by no package is replaced only when its content
//!   already matches, or with [`InstallOptions::force`];
//! - nothing is written through a symlink inside the root, so a package
//!   cannot redirect another's files outside it;
//! - reinstalling or upgrading a package removes files the new version no
//!   longer has, unless they were changed locally;
//! - removing a package keeps locally changed files, and refuses while
//!   another package depends on it unless forced.
//!
//! # Examples
//!
//! ```
//! use fox_core::archive::Archive;
//! use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
//! use fox_core::{Manifest, PackageName};
//! use fox_store::{InstallOptions, Store};
//!
//! let work = tempfile::tempdir()?;
//! let root = tempfile::tempdir()?;
//! let key = FixtureKey::generate(work.path(), "test")?;
//! let manifest = Manifest::new(PackageName::new("hello")?, "1.0.0");
//! let entries = [FixtureEntry::file("usr/bin/hello", b"hi\n", 0o755)];
//! let path = build_package(work.path(), &key, Some(&manifest), &entries, false)?;
//!
//! let store = Store::open(root.path())?;
//! let archive = Archive::open(&path, &key.trusted())?;
//! store.install(&archive, InstallOptions::default())?;
//! assert!(root.path().join("usr/bin/hello").is_file());
//! assert_eq!(store.list()?[0].name.as_str(), "hello");
//! assert!(store.verify(&manifest.name, &key.trusted())?.problems.is_empty());
//! store.remove(&manifest.name, false)?;
//! assert!(!root.path().join("usr/bin/hello").exists());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod db;
mod fsutil;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use fox_core::archive::{Archive, ArchiveError, MetadataSource};
use fox_core::{FileKind, PackageName, TrustedKeys, VerifyError, verify_head};
use pkgar::Transaction;

use db::Database;
pub use db::{InstalledPackage, MetadataKind};
use fsutil::{OnDisk, Removal, inspect, remove_if_unchanged, symlinked_parent};

/// Directory, relative to the root, of the store's own state.
pub const STATE_DIR: &str = "var/lib/fox";

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The root is not a directory.
    #[error("install root {} is not a directory", path.display())]
    RootNotDir {
        /// The root.
        path: PathBuf,
    },
    /// A file-system operation failed.
    #[error("I/O error on {}", path.display())]
    Io {
        /// The path being accessed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The database file is not valid.
    #[error("installed-package database {} is invalid", path.display())]
    Database {
        /// The database file.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: toml::de::Error,
    },
    /// The database could not be serialized.
    #[error("cannot serialize the installed-package database")]
    DatabaseSerialize(#[source] toml::ser::Error),
    /// The package is not installed.
    #[error("package {0} is not installed")]
    NotInstalled(PackageName),
    /// A file belongs to another installed package.
    #[error("{path} already belongs to package {owner}")]
    FileConflict {
        /// The contested path.
        path: String,
        /// The package that owns it.
        owner: PackageName,
    },
    /// A file exists that no package owns, with different content.
    #[error("{path} exists and belongs to no package (use --force to replace it)")]
    UnownedFile {
        /// The existing path.
        path: String,
    },
    /// A directory, or a special file, is where a file would go.
    #[error("{path} exists and is not a regular file or symlink")]
    NotAFile {
        /// The existing path.
        path: String,
    },
    /// A parent of a package path is a symlink inside the root.
    #[error("{path} would be written through the symlink {}", link.display())]
    SymlinkInPath {
        /// The package path.
        path: String,
        /// The symlink.
        link: PathBuf,
    },
    /// Other installed packages depend on the one being removed.
    #[error("package {name} is required by {}", list(dependents))]
    RequiredBy {
        /// The package being removed.
        name: PackageName,
        /// The packages that depend on it.
        dependents: Vec<PackageName>,
    },
    /// The archive could not be reopened for extraction.
    #[error("cannot reopen the archive")]
    Archive(#[source] Box<ArchiveError>),
    /// Extracting the archive failed.
    #[error("cannot extract package {name}")]
    Extract {
        /// The package.
        name: PackageName,
        /// The underlying error.
        #[source]
        source: Box<pkgar::Error>,
    },
    /// The stored head no longer verifies.
    #[error("stored head of {name} failed verification")]
    Head {
        /// The package.
        name: PackageName,
        /// What failed.
        #[source]
        source: VerifyError,
    },
    /// The stored head and the database disagree.
    #[error("stored head of {name} does not match the database")]
    HeadMismatch {
        /// The package.
        name: PackageName,
    },
}

impl StoreError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

fn list(names: &[PackageName]) -> String {
    names
        .iter()
        .map(PackageName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Options for [`Store::install`].
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// Replace existing files that belong to no package even when their
    /// content differs.
    pub force: bool,
}

/// What [`Store::install`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The package.
    pub name: PackageName,
    /// The version installed.
    pub version: String,
    /// The version it replaced, if it was already installed.
    pub previous: Option<String>,
    /// Files written.
    pub installed: usize,
    /// Files of the previous version that were removed.
    pub removed: usize,
    /// Files of the previous version kept because they were changed.
    pub kept: Vec<String>,
    /// Dependencies that are not installed.
    pub missing_depends: Vec<PackageName>,
}

/// What [`Store::remove`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReport {
    /// The package.
    pub name: PackageName,
    /// The version removed.
    pub version: String,
    /// Files removed.
    pub removed: usize,
    /// Files already missing.
    pub missing: usize,
    /// Files kept because they were changed.
    pub kept: Vec<String>,
}

/// A problem [`Store::verify`] found with one installed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProblem {
    /// The file.
    pub path: String,
    /// What is wrong.
    pub kind: ProblemKind,
}

/// What is wrong with an installed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProblemKind {
    /// The file is gone.
    Missing,
    /// The content differs from the signed hash.
    Modified,
    /// A file became a symlink, or the reverse, or something else.
    WrongType,
    /// A parent directory was replaced by a symlink.
    SymlinkInPath,
}

/// What [`Store::verify`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// The package.
    pub name: PackageName,
    /// Files checked.
    pub checked: usize,
    /// Files that did not match.
    pub problems: Vec<FileProblem>,
}

/// An install root managed by `fox`.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// Holds the store lock until dropped.
struct Lock(#[allow(dead_code, reason = "held for its Drop")] Option<File>);

impl Store {
    /// Opens the store for the install root `root`, which must exist.
    ///
    /// # Errors
    ///
    /// [`StoreError::RootNotDir`] if `root` is not a directory.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        if !root.is_dir() {
            return Err(StoreError::RootNotDir {
                path: root.to_path_buf(),
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// The install root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    fn db_path(&self) -> PathBuf {
        self.state_dir().join("installed.toml")
    }

    fn head_path(&self, name: &PackageName) -> PathBuf {
        self.state_dir()
            .join("heads")
            .join(format!("{name}.pkgar_head"))
    }

    /// Takes the store lock, creating the state directory.
    fn lock(&self) -> Result<Lock, StoreError> {
        let heads = self.state_dir().join("heads");
        fs::create_dir_all(&heads).map_err(|e| StoreError::io(&heads, e))?;
        let path = self.state_dir().join("lock");
        let file = File::create(&path).map_err(|e| StoreError::io(&path, e))?;
        match file.lock() {
            Ok(()) => Ok(Lock(Some(file))),
            // Platforms without file locks (Redox, for now) run unlocked.
            Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(Lock(None)),
            Err(source) => Err(StoreError::io(&path, source)),
        }
    }

    /// Every installed package, by name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] or [`StoreError::Database`] if the database
    /// cannot be read.
    pub fn list(&self) -> Result<Vec<InstalledPackage>, StoreError> {
        let mut packages = Database::load(&self.db_path())?.packages;
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(packages)
    }

    /// The installed package called `name`, if any.
    ///
    /// # Errors
    ///
    /// As [`Store::list`].
    pub fn get(&self, name: &PackageName) -> Result<Option<InstalledPackage>, StoreError> {
        Ok(Database::load(&self.db_path())?.get(name).cloned())
    }

    /// Installs a verified archive, replacing an installed version of the
    /// same package.
    ///
    /// # Errors
    ///
    /// - [`StoreError::FileConflict`], [`StoreError::UnownedFile`],
    ///   [`StoreError::NotAFile`] or [`StoreError::SymlinkInPath`] if a
    ///   file cannot be placed (nothing is written in that case);
    /// - [`StoreError::Archive`] if the archive changed since it was
    ///   verified;
    /// - [`StoreError::Extract`] if extraction fails;
    /// - [`StoreError::Io`], [`StoreError::Database`] or
    ///   [`StoreError::DatabaseSerialize`] for state-file errors.
    pub fn install(
        &self,
        archive: &Archive,
        options: InstallOptions,
    ) -> Result<InstallReport, StoreError> {
        let _lock = self.lock()?;
        let db_path = self.db_path();
        let mut db = Database::load(&db_path)?;
        let manifest = archive.manifest();
        let previous = db.take(&manifest.name);
        self.check_placement(archive, &db, previous.as_ref(), options)?;

        let mut package = archive
            .reopen()
            .map_err(|error| StoreError::Archive(Box::new(error)))?;
        let extract_error = |source| StoreError::Extract {
            name: manifest.name.clone(),
            source: Box::new(source),
        };
        let mut transaction = Transaction::new();
        if let Err(error) = transaction.install_with_entries(
            &mut package,
            archive.head().raw_entries(),
            &self.root,
            true,
        ) {
            // Best effort: the original error is the one worth reporting.
            let _ = transaction.abort();
            return Err(extract_error(error));
        }
        transaction.commit().map_err(extract_error)?;

        let new_paths: BTreeSet<&str> = archive.files().iter().map(|f| f.path.as_str()).collect();
        let (mut removed, mut kept) = (0, Vec::new());
        for old in previous.iter().flat_map(|p| &p.files) {
            if new_paths.contains(old.path.as_str()) {
                continue;
            }
            match remove_if_unchanged(&self.root, &old.path, old.kind, old.blake3)? {
                Removal::Removed => removed += 1,
                Removal::Missing => {}
                Removal::Kept => kept.push(old.path.clone()),
            }
        }

        let head_path = self.head_path(&manifest.name);
        let tmp_head = head_path.with_extension("pkgar_head.tmp");
        package
            .split(&tmp_head, None::<&Path>)
            .map_err(extract_error)?;
        fs::rename(&tmp_head, &head_path).map_err(|e| StoreError::io(&head_path, e))?;

        let missing_depends = manifest
            .depends
            .iter()
            .filter(|dep| db.get(dep).is_none() && **dep != manifest.name)
            .cloned()
            .collect();
        db.packages.push(record(archive));
        db.save(&db_path)?;

        Ok(InstallReport {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            previous: previous.map(|p| p.version),
            installed: archive.files().len(),
            removed,
            kept,
            missing_depends,
        })
    }

    /// Checks every archive path can be written without clobbering
    /// something it should not.
    fn check_placement(
        &self,
        archive: &Archive,
        db: &Database,
        previous: Option<&InstalledPackage>,
        options: InstallOptions,
    ) -> Result<(), StoreError> {
        let owners: BTreeMap<&str, &PackageName> = db
            .packages
            .iter()
            .flat_map(|p| p.files.iter().map(move |f| (f.path.as_str(), &p.name)))
            .collect();
        let previous_paths: BTreeSet<&str> = previous
            .iter()
            .flat_map(|p| p.files.iter().map(|f| f.path.as_str()))
            .collect();
        for file in archive.files() {
            if let Some(owner) = owners.get(file.path.as_str()) {
                return Err(StoreError::FileConflict {
                    path: file.path.clone(),
                    owner: (*owner).clone(),
                });
            }
            if let Some(link) = symlinked_parent(&self.root, &file.path)? {
                return Err(StoreError::SymlinkInPath {
                    path: file.path.clone(),
                    link,
                });
            }
            match inspect(&self.root.join(&file.path))? {
                OnDisk::Missing => {}
                OnDisk::Directory | OnDisk::Other => {
                    return Err(StoreError::NotAFile {
                        path: file.path.clone(),
                    });
                }
                OnDisk::Hashed(kind, blake3) => {
                    let same = kind == file.kind && blake3 == file.blake3;
                    if !same && !options.force && !previous_paths.contains(file.path.as_str()) {
                        return Err(StoreError::UnownedFile {
                            path: file.path.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Removes an installed package's files and its database entry.
    ///
    /// Files changed since installation are kept and listed in the report.
    /// With `force`, the package is removed even if others depend on it.
    ///
    /// # Errors
    ///
    /// - [`StoreError::NotInstalled`] if `name` is not installed;
    /// - [`StoreError::RequiredBy`] if other packages depend on it and
    ///   `force` is false;
    /// - [`StoreError::Io`], [`StoreError::Database`] or
    ///   [`StoreError::DatabaseSerialize`] for file-system errors.
    pub fn remove(&self, name: &PackageName, force: bool) -> Result<RemoveReport, StoreError> {
        let _lock = self.lock()?;
        let db_path = self.db_path();
        let mut db = Database::load(&db_path)?;
        let package = db
            .take(name)
            .ok_or_else(|| StoreError::NotInstalled(name.clone()))?;
        let dependents: Vec<PackageName> = db
            .packages
            .iter()
            .filter(|p| p.depends.contains(name))
            .map(|p| p.name.clone())
            .collect();
        if !dependents.is_empty() && !force {
            return Err(StoreError::RequiredBy {
                name: name.clone(),
                dependents,
            });
        }
        let mut report = RemoveReport {
            name: name.clone(),
            version: package.version.clone(),
            removed: 0,
            missing: 0,
            kept: Vec::new(),
        };
        for file in &package.files {
            match remove_if_unchanged(&self.root, &file.path, file.kind, file.blake3)? {
                Removal::Removed => report.removed += 1,
                Removal::Missing => report.missing += 1,
                Removal::Kept => report.kept.push(file.path.clone()),
            }
        }
        let head = self.head_path(name);
        match fs::remove_file(&head) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(StoreError::io(&head, source)),
        }
        db.save(&db_path)?;
        Ok(report)
    }

    /// Re-checks an installed package: the stored head must still verify
    /// against `keys` and match the database, and every file must still
    /// hash to its signed value.
    ///
    /// # Errors
    ///
    /// - [`StoreError::NotInstalled`] if `name` is not installed;
    /// - [`StoreError::Head`] if the stored head fails verification (for
    ///   example because its signer is no longer trusted);
    /// - [`StoreError::HeadMismatch`] if the head and database disagree;
    /// - [`StoreError::Io`] or [`StoreError::Database`] for read errors.
    pub fn verify(
        &self,
        name: &PackageName,
        keys: &TrustedKeys,
    ) -> Result<VerifyReport, StoreError> {
        let package = self
            .get(name)?
            .ok_or_else(|| StoreError::NotInstalled(name.clone()))?;
        let head_path = self.head_path(name);
        let bytes = fs::read(&head_path).map_err(|e| StoreError::io(&head_path, e))?;
        let head = verify_head(&bytes, keys).map_err(|source| StoreError::Head {
            name: name.clone(),
            source,
        })?;
        if head.entries_blake3() != package.entries_blake3 || head.signer().key != package.signer {
            return Err(StoreError::HeadMismatch { name: name.clone() });
        }
        let mut report = VerifyReport {
            name: name.clone(),
            checked: 0,
            problems: Vec::new(),
        };
        for entry in head.entries() {
            report.checked += 1;
            if let Some(kind) = self.check_file(entry.path, entry.kind, entry.blake3)? {
                report.problems.push(FileProblem {
                    path: entry.path.to_owned(),
                    kind,
                });
            }
        }
        Ok(report)
    }

    fn check_file(
        &self,
        path: &str,
        kind: FileKind,
        blake3: fox_core::Blake3Digest,
    ) -> Result<Option<ProblemKind>, StoreError> {
        if symlinked_parent(&self.root, path)?.is_some() {
            return Ok(Some(ProblemKind::SymlinkInPath));
        }
        Ok(match inspect(&self.root.join(path))? {
            OnDisk::Missing => Some(ProblemKind::Missing),
            OnDisk::Hashed(found_kind, _) if found_kind != kind => Some(ProblemKind::WrongType),
            OnDisk::Hashed(_, found) if found != blake3 => Some(ProblemKind::Modified),
            OnDisk::Hashed(..) => None,
            OnDisk::Directory | OnDisk::Other => Some(ProblemKind::WrongType),
        })
    }
}

/// The database record for a verified archive.
fn record(archive: &Archive) -> InstalledPackage {
    let manifest = archive.manifest();
    let signer = archive.head().signer();
    InstalledPackage {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        target: manifest.target.clone(),
        depends: manifest.depends.clone(),
        archive_blake3: archive.blake3(),
        entries_blake3: archive.head().entries_blake3(),
        signer: signer.key,
        signer_label: signer.label.clone(),
        metadata: match archive.metadata_source() {
            MetadataSource::Embedded => MetadataKind::Embedded,
            _ => MetadataKind::Sidecar,
        },
        files: archive.files().to_vec(),
    }
}
