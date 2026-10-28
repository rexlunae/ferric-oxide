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
//! [`Store::adopt_pkg`] records the packages upstream `pkg` installed (an
//! image built by the installer, or a system that used `pkg`) from the
//! signed heads it keeps, so `fox` can take over a root without
//! reinstalling anything.
//!
//! [`deps`] adds the dependencies a package is missing from a
//! [`deps::PackageSource`], such as a directory of archives, and orders them
//! for [`Store::install_plan`].
//!
//! Install rules:
//!
//! - archives must already be verified ([`Archive::open`]);
//! - a path owned by another package is a conflict;
//! - an existing path owned by no package is replaced only when its content
//!   already matches, or with [`InstallOptions::force`];
//! - nothing is written through a symlink inside the root, so a package
//!   cannot redirect another's files outside it, and the root itself may
//!   not be a symlink;
//! - no package may write into, replace or redirect [`RESERVED_DIRS`]:
//!   the store state, the trusted keys and the trust store, so a package
//!   cannot rewrite the database, enroll a signer or pin an author key;
//! - an archive without a signed manifest has an unsigned name, so it is
//!   installed only with [`InstallOptions::allow_unsigned_metadata`], and
//!   never over a package that had a signed manifest;
//! - each file gets exactly its signed mode, whatever the umask;
//! - reinstalling or upgrading a package removes files the new version no
//!   longer has, unless they were changed locally;
//! - removing a package keeps locally changed files, and refuses while
//!   another package depends on it unless forced. It is marked as being
//!   removed before any file is touched, so an interrupted removal shows
//!   in the database and can be run again.
//!
//! Shared generated files ([`SHARED_GENERATED_FILES`]) are the one exception
//! to "a path owned by another package is a conflict". Each package builds
//! its own version of such a file from its own contents, so the database
//! records exactly one owning package, and ownership is decided once:
//!
//! - a package that is already installed keeps the file: reinstalling or
//!   upgrading it replaces the index with the version the new archive
//!   carries, and the entry stays in its record;
//! - a package that does not own it skips the entry, leaves the file alone
//!   and does not record it, so removing the skip never takes the owner's
//!   file with it;
//! - a file no installed package records is an ordinary existing file: it is
//!   kept only if its content already matches, it needs
//!   [`InstallOptions::force`] otherwise, and the installing package then
//!   records it as its own;
//! - an existing symlink where the file should go is refused
//!   ([`StoreError::IndexIsSymlink`]), since the store never writes through a
//!   symlink inside the root.
//!
//! Keeping only the owner's index means `info` lists only the pages of the
//! package that installed the file; the pages of the other packages that
//! ship one are absent from its directory listing until something rebuilds
//! the index from the installed pages. The store does not do that: the
//! format of the index, the set of packages it should list and the moment it
//! should be rebuilt are a separate feature, and a partial rebuild would
//! leave an index that lies about what is installed. [`Store::verify`]
//! checks the files a package's record lists, not the entries the rest of
//! its archived index would have.
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
//!
//! // Two packages that each build their own info index: the first keeps it
//! // and both verify against what each of them actually installed.
//! let entries = |page: &'static str, index: &'static [u8]| {
//!     [
//!         FixtureEntry::file(page, page.as_bytes(), 0o644),
//!         FixtureEntry::file("usr/share/info/dir", index, 0o644),
//!     ]
//! };
//! let mut installed = Vec::new();
//! for (name, page, index) in [
//!     ("bash", "usr/share/info/bash.info", b"bash index\n".as_slice()),
//!     ("sed", "usr/share/info/sed.info", b"sed index\n".as_slice()),
//! ] {
//!     let manifest = Manifest::new(PackageName::new(name)?, "1.0.0");
//!     let dir = work.path().join(name);
//!     std::fs::create_dir_all(&dir)?;
//!     let path = build_package(&dir, &key, Some(&manifest), &entries(page, index), false)?;
//!     let archive = Archive::open(&path, &key.trusted())?;
//!     store.install(&archive, InstallOptions::default())?;
//!     assert!(store.verify(&manifest.name, &key.trusted())?.problems.is_empty());
//!     installed.push(manifest.name);
//! }
//! assert_eq!(
//!     std::fs::read(root.path().join("usr/share/info/dir"))?,
//!     b"bash index\n"
//! );
//! store.remove(&installed[1], false)?;
//! assert!(root.path().join("usr/share/info/dir").exists());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod adopt;
mod db;
pub mod deps;
mod fsutil;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fox_core::archive::{Archive, ArchiveError, MetadataSource};
use fox_core::keys::TRUSTED_KEYS_DIR;
use fox_core::policy::POLICY_FILE;
use fox_core::registries::REGISTRIES_FILE;
use fox_core::trust::TRUST_FILE;
use fox_core::{FileKind, FileRecord, PackageName, TrustedKeys, VerifyError, verify_head};
use pkgar::Transaction;

pub use adopt::{
    AdoptOptions, AdoptReport, AdoptedPackage, PKG_HEADS_DIR, PKG_STATE, SkipReason, SkippedPackage,
};
use db::Database;
pub use db::{InstalledPackage, MetadataKind, Origin, PackageState};
use fsutil::{OnDisk, Removal, inspect, remove_if_unchanged, symlinked_parent};

/// Directory, relative to the root, of the store's own state.
pub const STATE_DIR: &str = "var/lib/fox";

/// Paths no package may write into, replace or redirect: the store's own
/// state (which also holds the local builder key and the registry
/// checkouts), the trusted signer keys, the trust store, the install policy
/// and the registry list.
pub const RESERVED_DIRS: [&str; 5] = [
    STATE_DIR,
    TRUSTED_KEYS_DIR,
    TRUST_FILE,
    POLICY_FILE,
    REGISTRIES_FILE,
];

/// Paths whose content each package generates for itself, so several packages
/// legitimately produce a different version of the same path.
///
/// `/usr/share/info/dir` is GNU info's directory index: every package that
/// installs info pages writes it, built from that package's own entries. The
/// package that installs it first owns it and keeps it across reinstalls and
/// upgrades; a later package that carries its own version skips it rather
/// than failing the install, and does not record it as one of its files. A
/// path in this list is not a conflict with another package, but the file is
/// still never written through a symlink. See the module documentation for
/// the full rule.
pub const SHARED_GENERATED_FILES: [&str; 1] = ["usr/share/info/dir"];

/// Whether `path` is one of [`SHARED_GENERATED_FILES`].
fn is_shared_generated(path: &str) -> bool {
    SHARED_GENERATED_FILES.contains(&path)
}

/// Whether this install places `path`: every archive entry is installed
/// except a shared generated file that another installed package owns.
fn takes_entry(
    path: &str,
    owners: &BTreeMap<&str, &PackageName>,
    installing: &PackageName,
) -> bool {
    !is_shared_generated(path) || owners.get(path).is_none_or(|owner| *owner == installing)
}

/// Mode bits the store sets and checks on installed files (permissions,
/// setuid, setgid and sticky), the bits pkgar signs.
const MODE_BITS: u32 = 0o7777;

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
    /// The root is a symlink. Following it would install outside the
    /// path that was named.
    #[error("install root {} is a symlink", path.display())]
    RootIsSymlink {
        /// The root.
        path: PathBuf,
    },
    /// A package path is inside, or is a parent of, a directory only
    /// `fox` itself may write ([`RESERVED_DIRS`]).
    #[error("{path} is reserved: packages may not write to {reserved}")]
    ReservedPath {
        /// The package path.
        path: String,
        /// The reserved directory it touches.
        reserved: &'static str,
    },
    /// The archive has no signed manifest, so its name and version come
    /// from an unsigned file next to it.
    #[error(
        "package {name} has no signed manifest; its name is unsigned (use --allow-unsigned-metadata to install it)"
    )]
    UnsignedMetadata {
        /// The name the unsigned metadata claims.
        name: PackageName,
    },
    /// An archive with unsigned metadata would replace a package that was
    /// installed with a signed manifest.
    #[error(
        "package {name} was installed with a signed manifest; an unsigned name cannot replace it"
    )]
    UnsignedReplacesSigned {
        /// The installed package.
        name: PackageName,
    },
    /// An earlier removal of the package did not finish.
    #[error("removal of package {name} did not finish; run the removal again")]
    RemovalIncomplete {
        /// The package.
        name: PackageName,
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
    /// `pkg`'s installed-package state is not valid.
    #[error("pkg state {} is invalid", path.display())]
    PkgState {
        /// The state file.
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
    /// The path of a shared generated file that no package owns is a symlink.
    /// Replacing it would write wherever it points, and pkgar would follow
    /// it too, so the install refuses instead.
    #[error("{path} is a symlink; a shared generated file is not written through one")]
    IndexIsSymlink {
        /// The shared generated path.
        path: String,
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
    /// Accept an archive without a signed manifest, whose name, version
    /// and dependencies come from the unsigned cookbook metadata file.
    /// Even then it cannot replace a package installed with a signed
    /// manifest.
    pub allow_unsigned_metadata: bool,
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
    /// Files written, which is every archive entry except a shared generated
    /// file another installed package owns (see [`SHARED_GENERATED_FILES`]).
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
    /// The file's mode bits differ from the signed ones.
    Mode {
        /// The signed mode bits.
        expected: u32,
        /// The mode bits on disk.
        found: u32,
    },
}

impl core::fmt::Display for ProblemKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::Modified => f.write_str("modified"),
            Self::WrongType => f.write_str("wrong file type"),
            Self::SymlinkInPath => f.write_str("symlink in path"),
            Self::Mode { expected, found } => {
                write!(f, "mode {found:o}, expected {expected:o}")
            }
        }
    }
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
    /// The root is canonicalized, so every later path is absolute and
    /// stays the same if the working directory or a parent link changes.
    ///
    /// # Errors
    ///
    /// - [`StoreError::RootIsSymlink`] if `root` itself is a symlink;
    /// - [`StoreError::RootNotDir`] if `root` is not a directory;
    /// - [`StoreError::Io`] if it cannot be inspected or canonicalized.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let meta = match fs::symlink_metadata(root) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::RootNotDir {
                    path: root.to_path_buf(),
                });
            }
            Err(source) => return Err(StoreError::io(root, source)),
        };
        if meta.file_type().is_symlink() {
            return Err(StoreError::RootIsSymlink {
                path: root.to_path_buf(),
            });
        }
        if !meta.is_dir() {
            return Err(StoreError::RootNotDir {
                path: root.to_path_buf(),
            });
        }
        let root = fs::canonicalize(root).map_err(|e| StoreError::io(root, e))?;
        Ok(Self { root })
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
    /// - [`StoreError::UnsignedMetadata`] or
    ///   [`StoreError::UnsignedReplacesSigned`] if the archive has no
    ///   signed manifest and that is not allowed;
    /// - [`StoreError::ReservedPath`], [`StoreError::FileConflict`],
    ///   [`StoreError::UnownedFile`], [`StoreError::NotAFile`],
    ///   [`StoreError::SymlinkInPath`] or [`StoreError::IndexIsSymlink`] if a
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
        self.install_from(archive, options, None)
    }

    /// Installs a verified archive as [`Store::install`] does, recording
    /// `origin` as the registry it came from. The record replaces the
    /// previous version's, so reinstalling from anywhere else clears the
    /// origin.
    ///
    /// # Errors
    ///
    /// As [`Store::install`].
    pub fn install_from(
        &self,
        archive: &Archive,
        options: InstallOptions,
        origin: Option<Origin>,
    ) -> Result<InstallReport, StoreError> {
        let _lock = self.lock()?;
        let db_path = self.db_path();
        let mut db = Database::load(&db_path)?;
        let manifest = archive.manifest();
        let previous = db.take(&manifest.name);
        check_identity(archive, previous.as_ref(), options)?;
        let owners = file_owners(&db);
        self.check_placement(archive, &owners, previous.as_ref(), options)?;

        // A shared generated file stays with the package that already owns it:
        // dropping another package's entry from the transaction leaves the file
        // and its recorded owner alone, and keeps it out of this package's
        // record. The owner's own entry is always placed, so reinstalling or
        // upgrading the owner replaces the index with the version its archive
        // carries.
        let entries: Vec<pkgar_core::Entry> = archive
            .head()
            .raw_entries()
            .iter()
            .filter(|entry| {
                std::str::from_utf8(entry.path_bytes())
                    .is_ok_and(|path| takes_entry(path, &owners, &manifest.name))
            })
            .cloned()
            .collect();

        let mut package = archive
            .reopen()
            .map_err(|error| StoreError::Archive(Box::new(error)))?;
        let extract_error = |source| StoreError::Extract {
            name: manifest.name.clone(),
            source: Box::new(source),
        };
        let mut transaction = Transaction::new();
        if let Err(error) =
            transaction.install_with_entries(&mut package, &entries, &self.root, true)
        {
            // Best effort: the original error is the one worth reporting.
            let _ = transaction.abort();
            return Err(extract_error(error));
        }
        transaction.commit().map_err(extract_error)?;
        self.apply_modes(archive, &entries)?;

        let new_paths: BTreeSet<&str> = archive.files().iter().map(|f| f.path.as_str()).collect();
        let installed_paths: BTreeSet<&str> = entries
            .iter()
            .filter_map(|entry| std::str::from_utf8(entry.path_bytes()).ok())
            .collect();
        let (mut removed, mut kept) = (0, Vec::new());
        for old in previous.iter().flat_map(|p| &p.files) {
            // Only a file this install placed can be superseded: one this
            // package skipped belongs to another package, and its owner keeps
            // it even if this archive no longer carries it.
            if new_paths.contains(old.path.as_str()) && installed_paths.contains(old.path.as_str())
            {
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
        // fox owns the name now: pkg's stale entry for it must not be
        // retried by adoption, even after this package is removed.
        db.legacy_pending.remove(manifest.name.as_str());
        db.packages.push(InstalledPackage {
            origin,
            ..record(archive, &entries)
        });
        db.save(&db_path)?;

        Ok(InstallReport {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            previous: previous.map(|p| p.version),
            installed: installed_paths.len(),
            removed,
            kept,
            missing_depends,
        })
    }

    /// Checks every archive path can be written without clobbering
    /// something it should not. `owners` is [`file_owners`] of the database
    /// without the package being installed.
    fn check_placement(
        &self,
        archive: &Archive,
        owners: &BTreeMap<&str, &PackageName>,
        previous: Option<&InstalledPackage>,
        options: InstallOptions,
    ) -> Result<(), StoreError> {
        let installing = &archive.manifest().name;
        let previous_paths: BTreeSet<&str> = previous
            .iter()
            .flat_map(|p| p.files.iter().map(|f| f.path.as_str()))
            .collect();
        for file in archive.files() {
            if let Some(reserved) = reserved_dir(&file.path) {
                return Err(StoreError::ReservedPath {
                    path: file.path.clone(),
                    reserved,
                });
            }
            let shared = is_shared_generated(&file.path);
            match owners.get(file.path.as_str()) {
                // A shared generated file keeps the owner it has, because each
                // package builds a different version by design; this package
                // skips it. Any other file another package records is a
                // conflict.
                Some(owner) if shared && **owner != *installing => continue,
                Some(owner) => {
                    return Err(StoreError::FileConflict {
                        path: file.path.clone(),
                        owner: (*owner).clone(),
                    });
                }
                None => {}
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
                // pkgar would write a regular file over a symlink's target,
                // and could be made to write outside the root that way, so a
                // shared generated file no package owns must not be a symlink.
                OnDisk::Hashed(FileKind::Symlink, _) if shared => {
                    return Err(StoreError::IndexIsSymlink {
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

    /// Sets every installed regular file to its signed mode bits. pkgar
    /// creates files through the process umask, which can drop bits. Only
    /// the entries this install placed are touched: a shared generated file
    /// this package skipped keeps the owner's mode.
    fn apply_modes(
        &self,
        archive: &Archive,
        entries: &[pkgar_core::Entry],
    ) -> Result<(), StoreError> {
        let installed: BTreeSet<&str> = entries
            .iter()
            .filter_map(|entry| std::str::from_utf8(entry.path_bytes()).ok())
            .collect();
        for file in archive
            .files()
            .iter()
            .filter(|f| f.kind == FileKind::File && installed.contains(f.path.as_str()))
        {
            let path = self.root.join(&file.path);
            let permissions = fs::Permissions::from_mode(file.mode & MODE_BITS);
            fs::set_permissions(&path, permissions).map_err(|e| StoreError::io(&path, e))?;
        }
        Ok(())
    }

    /// Removes an installed package's files and its database entry.
    ///
    /// Files changed since installation are kept and listed in the report.
    /// With `force`, the package is removed even if others depend on it.
    ///
    /// The package is marked [`PackageState::Removing`] in the database
    /// before any file is touched and dropped from it only after the last
    /// one, so an error part way leaves a record that says so. Running the
    /// removal again finishes it; files already gone count as missing.
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
            .get(name)
            .cloned()
            .ok_or_else(|| StoreError::NotInstalled(name.clone()))?;
        // A removal that already started was already allowed.
        if package.state == PackageState::Installed {
            let dependents: Vec<PackageName> = db
                .packages
                .iter()
                .filter(|p| &p.name != name && p.depends.contains(name))
                .map(|p| p.name.clone())
                .collect();
            if !dependents.is_empty() && !force {
                return Err(StoreError::RequiredBy {
                    name: name.clone(),
                    dependents,
                });
            }
            if let Some(record) = db.get_mut(name) {
                record.state = PackageState::Removing;
            }
            db.save(&db_path)?;
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
        db.take(name);
        db.save(&db_path)?;
        Ok(report)
    }

    /// Re-checks an installed package: the stored head must still verify
    /// against `keys` and match the database, and every file the database
    /// records for it must still hash to its signed value.
    ///
    /// The whole signed head is checked, but only the entries the package
    /// installed are re-hashed. A shared generated file that this package
    /// skipped when it was installed belongs to another package, so the
    /// version of it in this package's head is not what should be on disk
    /// and is not reported as a problem.
    ///
    /// # Errors
    ///
    /// - [`StoreError::NotInstalled`] if `name` is not installed;
    /// - [`StoreError::RemovalIncomplete`] if a removal of it did not
    ///   finish;
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
        if package.state == PackageState::Removing {
            return Err(StoreError::RemovalIncomplete { name: name.clone() });
        }
        let head_path = self.head_path(name);
        let bytes = fs::read(&head_path).map_err(|e| StoreError::io(&head_path, e))?;
        let head = verify_head(&bytes, keys).map_err(|source| StoreError::Head {
            name: name.clone(),
            source,
        })?;
        if head.entries_blake3() != package.entries_blake3 || head.signer().key != package.signer {
            return Err(StoreError::HeadMismatch { name: name.clone() });
        }
        let files: BTreeMap<&str, &FileRecord> = package
            .files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        let mut report = VerifyReport {
            name: name.clone(),
            checked: 0,
            problems: Vec::new(),
        };
        for entry in head.entries() {
            // A file installed by no other package is not checked, and a
            // recorded file the head does not describe has no signed value to
            // check it against; the record is what was installed either way.
            let Some(file) = files.get(entry.path) else {
                continue;
            };
            report.checked += 1;
            if let Some(kind) = self.check_file(file)? {
                report.problems.push(FileProblem {
                    path: entry.path.to_owned(),
                    kind,
                });
            }
        }
        Ok(report)
    }

    /// Checks the recorded file the package's head describes is still there,
    /// with its recorded kind, content and mode.
    fn check_file(&self, file: &FileRecord) -> Result<Option<ProblemKind>, StoreError> {
        if symlinked_parent(&self.root, &file.path)?.is_some() {
            return Ok(Some(ProblemKind::SymlinkInPath));
        }
        let full = self.root.join(&file.path);
        Ok(match inspect(&full)? {
            OnDisk::Missing => Some(ProblemKind::Missing),
            OnDisk::Hashed(found_kind, _) if found_kind != file.kind => {
                Some(ProblemKind::WrongType)
            }
            OnDisk::Hashed(_, found) if found != file.blake3 => Some(ProblemKind::Modified),
            // Symlink modes carry no meaning, so only files are compared.
            OnDisk::Hashed(FileKind::File, _) => {
                let found = fs::symlink_metadata(&full)
                    .map_err(|e| StoreError::io(&full, e))?
                    .permissions()
                    .mode()
                    & MODE_BITS;
                let expected = file.mode & MODE_BITS;
                (found != expected).then_some(ProblemKind::Mode { expected, found })
            }
            OnDisk::Hashed(..) => None,
            OnDisk::Directory | OnDisk::Other => Some(ProblemKind::WrongType),
        })
    }
}

/// Every path an installed package records, and the package that records it.
fn file_owners(db: &Database) -> BTreeMap<&str, &PackageName> {
    db.packages
        .iter()
        .flat_map(|package| {
            package
                .files
                .iter()
                .map(move |file| (file.path.as_str(), &package.name))
        })
        .collect()
}

/// The reserved directory `path` would write into, replace or redirect.
/// A path equal to or under a reserved directory writes into it; a path
/// that is a parent of one (a symlink entry `var/lib`, say) would redirect
/// it.
fn reserved_dir(path: &str) -> Option<&'static str> {
    RESERVED_DIRS.into_iter().find(|dir| {
        let inside = path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        let parent = dir
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with('/'));
        inside || parent
    })
}

/// Refuses unsigned package identities unless they are allowed, and
/// never lets one replace a package that had a signed one.
fn check_identity(
    archive: &Archive,
    previous: Option<&InstalledPackage>,
    options: InstallOptions,
) -> Result<(), StoreError> {
    if matches!(archive.metadata_source(), MetadataSource::Embedded) {
        return Ok(());
    }
    let name = &archive.manifest().name;
    if !options.allow_unsigned_metadata {
        return Err(StoreError::UnsignedMetadata { name: name.clone() });
    }
    if previous.is_some_and(|p| p.metadata == MetadataKind::Embedded) {
        return Err(StoreError::UnsignedReplacesSigned { name: name.clone() });
    }
    Ok(())
}

/// The database record for a verified archive.
fn record(archive: &Archive, installed: &[pkgar_core::Entry]) -> InstalledPackage {
    let manifest = archive.manifest();
    let signer = archive.head().signer();
    InstalledPackage {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        target: manifest.target.clone(),
        depends: manifest.depends.clone(),
        archive_blake3: Some(archive.blake3()),
        entries_blake3: archive.head().entries_blake3(),
        signer: signer.key,
        signer_label: signer.label.clone(),
        metadata: match archive.metadata_source() {
            MetadataSource::Embedded => MetadataKind::Embedded,
            _ => MetadataKind::Sidecar,
        },
        state: PackageState::Installed,
        origin: None,
        // Only the files this install placed belong to this package, both
        // because that is what the store verifies and because a shared
        // generated file installed by another package stays that one's.
        files: archive
            .files()
            .iter()
            .filter(|file| {
                installed
                    .iter()
                    .any(|entry| entry.path_bytes() == file.path.as_bytes())
            })
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_paths() {
        for path in [
            "var/lib/fox",
            "var/lib/fox/installed.toml",
            "var/lib/fox/heads/x.pkgar_head",
            "etc/fox/keys/attacker.toml",
            "etc/fox/trust.toml",
            "etc/fox/policy.toml",
            "etc/fox/registries.toml",
            "var/lib/fox/registries/ferric/state.json",
            "var/lib/fox/local-builder.pub.toml",
            "var",
            "var/lib",
            "etc",
            "etc/fox",
        ] {
            assert!(reserved_dir(path).is_some(), "{path}");
        }
        for path in [
            "var/lib/foxglove",
            "etc/foxrc",
            "etc/fox/fox.toml",
            "etc/fox/trust.toml.d",
            "usr/bin/fox",
            "var/log/x",
        ] {
            assert_eq!(reserved_dir(path), None, "{path}");
        }
    }

    /// The GNU info directory index is written by every package that installs
    /// info pages, from that package's own entries, so several packages produce
    /// a different one. It is an index of what is installed rather than content
    /// that belongs to a package, so the first one installed keeps it.
    #[test]
    fn only_the_info_directory_is_a_shared_generated_file() {
        assert!(is_shared_generated("usr/share/info/dir"));
        for path in [
            // The package's own info pages stay its own.
            "usr/share/info/bash.info",
            "usr/share/info/sed.info",
            // A directory named "dir" elsewhere is ordinary content.
            "usr/share/doc/dir",
            "usr/bin/dir",
            "etc/dir",
        ] {
            assert!(!is_shared_generated(path), "{path}");
        }
    }
}
