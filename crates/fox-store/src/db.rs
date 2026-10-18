//! The installed-package database, `<root>/var/lib/fox/installed.toml`.
//!
//! ```toml
//! format = "fox-installed/1"
//!
//! [[package]]
//! name = "hello"
//! version = "1.0.0"
//! archive-blake3 = "5d3a…"
//! entries-blake3 = "77d0…"
//! signer = "a1b2…"
//! signer-label = "ferric-ci-1"
//! metadata = "embedded"
//!
//! [[package.files]]
//! path = "usr/bin/hello"
//! kind = "file"
//! mode = 493
//! size = 31
//! blake3 = "e40f…"
//! ```
//!
//! The file is rewritten whole on every change, through a temporary file
//! and a rename, so a crash leaves either the old or the new version.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use fox_core::{Blake3Digest, FileRecord, KeyId, PackageName};
use serde::{Deserialize, Serialize};

use crate::StoreError;

/// Database format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum Format {
    #[default]
    #[serde(rename = "fox-installed/1")]
    V1,
}

/// Where an installed package's metadata came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MetadataKind {
    /// The signed manifest inside the archive.
    Embedded,
    /// The cookbook's unsigned metadata file next to the archive.
    Sidecar,
    /// Adopted from `pkg`'s installed-package state, which names the
    /// package but signs nothing about it (see [`Store::adopt_pkg`]).
    ///
    /// [`Store::adopt_pkg`]: crate::Store::adopt_pkg
    Legacy,
}

/// Whether a package is fully installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PackageState {
    /// Installed and in use.
    #[default]
    Installed,
    /// A removal started and did not finish: some of its files may be
    /// gone. Running the removal again completes it.
    Removing,
}

impl PackageState {
    fn is_installed(&self) -> bool {
        *self == Self::Installed
    }
}

/// One installed package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct InstalledPackage {
    /// Package name.
    pub name: PackageName,
    /// Installed version.
    pub version: String,
    /// Target triple, if the package recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Runtime dependencies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<PackageName>,
    /// BLAKE3 of the archive it was installed from. Unknown for packages
    /// adopted from `pkg`, which keeps only the signed head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_blake3: Option<Blake3Digest>,
    /// BLAKE3 of the archive's signed entry table.
    pub entries_blake3: Blake3Digest,
    /// Key that signed the archive.
    pub signer: KeyId,
    /// Label the signer was trusted under at install time.
    pub signer_label: String,
    /// Where the metadata came from.
    pub metadata: MetadataKind,
    /// Whether a removal is in progress.
    #[serde(default, skip_serializing_if = "PackageState::is_installed")]
    pub state: PackageState,
    /// Every installed file.
    #[serde(default)]
    pub files: Vec<FileRecord>,
}

/// The whole database.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Database {
    format: Format,
    /// Set once `pkg`'s state has been adopted, so a later adoption does
    /// not bring back packages removed since.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) legacy_adopted: bool,
    #[serde(default, rename = "package")]
    pub(crate) packages: Vec<InstalledPackage>,
}

// serde's skip_serializing_if passes a reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

impl Database {
    /// Reads the database, or returns an empty one if the file is missing.
    pub(crate) fn load(path: &Path) -> Result<Self, StoreError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(StoreError::io(path, source)),
        };
        toml::from_str(&text).map_err(|source| StoreError::Database {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Writes the database atomically.
    pub(crate) fn save(&mut self, path: &Path) -> Result<(), StoreError> {
        self.packages.sort_by(|a, b| a.name.cmp(&b.name));
        let text = toml::to_string(self).map_err(StoreError::DatabaseSerialize)?;
        let tmp = path.with_extension("toml.tmp");
        let mut file = File::create(&tmp).map_err(|e| StoreError::io(&tmp, e))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| StoreError::io(&tmp, e))?;
        fs::rename(&tmp, path).map_err(|e| StoreError::io(path, e))
    }

    /// The package called `name`, mutably.
    pub(crate) fn get_mut(&mut self, name: &PackageName) -> Option<&mut InstalledPackage> {
        self.packages.iter_mut().find(|p| &p.name == name)
    }

    /// The package called `name`.
    pub(crate) fn get(&self, name: &PackageName) -> Option<&InstalledPackage> {
        self.packages.iter().find(|p| &p.name == name)
    }

    /// Removes and returns the package called `name`.
    pub(crate) fn take(&mut self, name: &PackageName) -> Option<InstalledPackage> {
        let index = self.packages.iter().position(|p| &p.name == name)?;
        Some(self.packages.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_core::FileKind;

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.toml");
        assert!(Database::load(&path).unwrap().packages.is_empty());

        let mut db = Database::default();
        db.packages.push(InstalledPackage {
            name: PackageName::new("hello").unwrap(),
            version: "1.0".into(),
            target: None,
            depends: vec![PackageName::new("relibc").unwrap()],
            archive_blake3: Some(Blake3Digest::of(b"a")),
            entries_blake3: Blake3Digest::of(b"e"),
            signer: KeyId::from_bytes([9; 32]),
            signer_label: "ci".into(),
            metadata: MetadataKind::Embedded,
            state: PackageState::Removing,
            files: vec![FileRecord {
                path: "usr/bin/hello".into(),
                kind: FileKind::File,
                mode: 0o755,
                size: 3,
                blake3: Blake3Digest::of(b"abc"),
            }],
        });
        db.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("format = \"fox-installed/1\""));
        assert!(text.contains("state = \"removing\""));
        let loaded = Database::load(&path).unwrap();
        assert_eq!(loaded.packages, db.packages);
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn rejects_corrupt_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installed.toml");
        fs::write(&path, "format = \"something-else\"").unwrap();
        assert!(matches!(
            Database::load(&path),
            Err(StoreError::Database { .. })
        ));
    }
}
