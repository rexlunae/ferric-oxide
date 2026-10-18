//! Signed fixture packages for tests (feature `test-fixtures`).
//!
//! Tests in the fox crates build real `.pkgar` files with a fresh key
//! instead of checking binary fixtures into the tree, so the fixtures always
//! match the `pkgar` version in `Cargo.lock`. Packing goes through the same
//! `pkgar::create_with_flags` the cookbook uses.
//!
//! # Examples
//!
//! ```
//! use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
//! use fox_core::{Manifest, PackageName, archive::Archive};
//!
//! let dir = tempfile::tempdir()?;
//! let key = FixtureKey::generate(dir.path(), "test")?;
//! let manifest = Manifest::new(PackageName::new("hello")?, "1.0.0");
//! let entries = [FixtureEntry::file("usr/bin/hello", b"#!/bin/sh\necho hi\n", 0o755)];
//! let path = build_package(dir.path(), &key, Some(&manifest), &entries, false)?;
//! let archive = Archive::open(&path, &key.trusted())?;
//! assert_eq!(archive.manifest().version, "1.0.0");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use pkgar_core::{Architecture, HeaderFlags, Packaging};

use crate::{KeyId, Manifest, TrustedKeys, manifest_path};

/// Why a fixture could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FixtureError {
    /// Writing the staging tree failed.
    #[error("cannot write fixture file {}", path.display())]
    Io {
        /// The file being written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Saving the key pair failed.
    #[error("cannot save fixture key")]
    Key(#[source] pkgar_keys::Error),
    /// Packing the archive failed.
    #[error("cannot pack fixture archive")]
    Pack(#[source] Box<pkgar::Error>),
    /// The manifest could not be serialized.
    #[error("cannot serialize fixture manifest")]
    Manifest(#[source] crate::ManifestError),
}

/// A freshly generated, unencrypted signing key pair saved in a directory.
#[derive(Debug, Clone)]
pub struct FixtureKey {
    /// The `pkgar-keys` secret key file.
    pub secret_path: PathBuf,
    /// The `pkgar-keys` public key file.
    pub public_path: PathBuf,
    /// The public key.
    pub key: KeyId,
    /// The label used for [`FixtureKey::trusted`].
    pub label: String,
}

impl FixtureKey {
    /// Generates a key pair and saves it as `<dir>/<label>.toml` (secret)
    /// and `<dir>/<label>.pub.toml` (public).
    ///
    /// # Errors
    ///
    /// [`FixtureError::Key`] if a key file cannot be written.
    pub fn generate(dir: &Path, label: &str) -> Result<Self, FixtureError> {
        let (public, secret) = pkgar_keys::SecretKeyFile::new();
        let secret_path = dir.join(format!("{label}.toml"));
        let public_path = dir.join(format!("{label}.pub.toml"));
        secret.save(&secret_path).map_err(FixtureError::Key)?;
        public.save(&public_path).map_err(FixtureError::Key)?;
        Ok(Self {
            secret_path,
            public_path,
            key: KeyId::from_bytes(public.pkey),
            label: label.to_owned(),
        })
    }

    /// A key set trusting only this key.
    pub fn trusted(&self) -> TrustedKeys {
        let mut keys = TrustedKeys::new();
        keys.insert(self.label.clone(), self.key);
        keys
    }
}

/// One file to put in a fixture package.
#[derive(Debug, Clone, Copy)]
pub enum FixtureEntry<'a> {
    /// A regular file.
    File {
        /// Path relative to the install root.
        path: &'a str,
        /// File contents.
        contents: &'a [u8],
        /// Permission bits.
        mode: u32,
    },
    /// A symbolic link.
    Symlink {
        /// Path relative to the install root.
        path: &'a str,
        /// Link target.
        target: &'a str,
    },
}

impl<'a> FixtureEntry<'a> {
    /// A regular file.
    pub fn file(path: &'a str, contents: &'a [u8], mode: u32) -> Self {
        Self::File {
            path,
            contents,
            mode,
        }
    }

    /// A symbolic link.
    pub fn symlink(path: &'a str, target: &'a str) -> Self {
        Self::Symlink { path, target }
    }
}

/// Builds `<dir>/<name>.pkgar` signed with `key`, holding `entries` and, if
/// given, `manifest` at its manifest path. Without a manifest the archive is
/// named `package.pkgar`. `compressed` selects LZMA2 packaging.
///
/// # Errors
///
/// [`FixtureError::Io`] if the staging tree cannot be written,
/// [`FixtureError::Manifest`] if the manifest cannot be serialized and
/// [`FixtureError::Pack`] if packing fails.
pub fn build_package(
    dir: &Path,
    key: &FixtureKey,
    manifest: Option<&Manifest>,
    entries: &[FixtureEntry<'_>],
    compressed: bool,
) -> Result<PathBuf, FixtureError> {
    let stem = manifest.map_or("package", |m| m.name.as_str());
    let stage = dir.join(format!("{stem}.stage"));
    if stage.exists() {
        fs::remove_dir_all(&stage).map_err(io_error(&stage))?;
    }
    fs::create_dir_all(&stage).map_err(io_error(&stage))?;
    if let Some(manifest) = manifest {
        let text = manifest.to_toml().map_err(FixtureError::Manifest)?;
        write_file(
            &stage.join(manifest_path(&manifest.name)),
            text.as_bytes(),
            0o644,
        )?;
    }
    for entry in entries {
        match *entry {
            FixtureEntry::File {
                path,
                contents,
                mode,
            } => write_file(&stage.join(path), contents, mode)?,
            FixtureEntry::Symlink { path, target } => {
                let path = stage.join(path);
                create_parent(&path)?;
                symlink(target, &path).map_err(io_error(&path))?;
            }
        }
    }
    let archive = dir.join(format!("{stem}.pkgar"));
    let packaging = if compressed {
        Packaging::LZMA2
    } else {
        Packaging::Uncompressed
    };
    pkgar::create_with_flags(
        &key.secret_path,
        &archive,
        &stage,
        HeaderFlags::latest(Architecture::Independent, packaging),
    )
    .map_err(|error| FixtureError::Pack(Box::new(error)))?;
    Ok(archive)
}

fn write_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), FixtureError> {
    create_parent(path)?;
    fs::write(path, contents).map_err(io_error(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_error(path))
}

fn create_parent(path: &Path) -> Result<(), FixtureError> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent).map_err(io_error(parent)),
        None => Ok(()),
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> FixtureError + '_ {
    move |source| FixtureError::Io {
        path: path.to_path_buf(),
        source,
    }
}
