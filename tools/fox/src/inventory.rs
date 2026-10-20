//! What is installed under a root, with each package's build SBOM:
//! the input to `fox sbom --system` and `fox audit`.
//!
//! A package's build SBOM is the file at [`sbom_path`] that the package
//! itself installed. It is read only when the package's own file list
//! records it, and only if its contents still hash to the recorded BLAKE3,
//! so another package cannot supply or replace it. It is never read through
//! a symbolic link (the root may be an image someone else prepared), and no
//! more than its recorded size (at most 64 MiB) is read.
//!
//! Packages whose removal did not finish are left out: some of their files,
//! the SBOM among them, may already be gone.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use fox_core::{Blake3Digest, PackageName, sbom_path};
use fox_sbom::Bom;
use fox_sbom::{SbomError, SystemPackage, SystemSbom, read_build_sbom};
use fox_store::{InstalledPackage, PackageState, Store, StoreError};

/// The largest build SBOM read.
const MAX_SBOM: u64 = 64 * 1024 * 1024;

/// Why the inventory could not be taken.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InventoryError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot read the SBOM of {name} at {}", path.display())]
    Io {
        name: PackageName,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the SBOM of {name} at {} has changed since it was installed", path.display())]
    Changed { name: PackageName, path: PathBuf },
    #[error("the SBOM of {name} at {} is reached through a symbolic link", path.display())]
    Symlink { name: PackageName, path: PathBuf },
    #[error("the SBOM of {name} records {size} bytes, more than the 64 MiB read")]
    TooLarge { name: PackageName, size: u64 },
    #[error("invalid SBOM in {name}")]
    Sbom {
        name: PackageName,
        #[source]
        source: SbomError,
    },
}

/// An installed package and its build SBOM, if it has one.
pub(crate) struct Inventoried {
    pub(crate) package: InstalledPackage,
    pub(crate) build_sbom: Option<Bom>,
}

/// Every installed package under the store's root, by name.
pub(crate) fn take(store: &Store) -> Result<Vec<Inventoried>, InventoryError> {
    store
        .list()?
        .into_iter()
        .filter(|package| package.state == PackageState::Installed)
        .map(|package| {
            let build_sbom = build_sbom(store, &package)?;
            Ok(Inventoried {
                package,
                build_sbom,
            })
        })
        .collect()
}

fn build_sbom(store: &Store, package: &InstalledPackage) -> Result<Option<Bom>, InventoryError> {
    let relative = sbom_path(&package.name);
    let Some(record) = package.files.iter().find(|file| file.path == relative) else {
        return Ok(None);
    };
    if record.size > MAX_SBOM {
        return Err(InventoryError::TooLarge {
            name: package.name.clone(),
            size: record.size,
        });
    }
    let path = store.root().join(&relative);
    let bytes = read_in_root(store.root(), &relative, record.size, &package.name)?;
    if Blake3Digest::of(&bytes) != record.blake3 {
        return Err(InventoryError::Changed {
            name: package.name.clone(),
            path,
        });
    }
    read_build_sbom(&bytes)
        .map(Some)
        .map_err(|source| InventoryError::Sbom {
            name: package.name.clone(),
            source,
        })
}

/// Reads `root/relative` without following a symbolic link at any
/// component, and at most `limit` bytes (more means the file changed).
fn read_in_root(
    root: &Path,
    relative: &str,
    limit: u64,
    name: &PackageName,
) -> Result<Vec<u8>, InventoryError> {
    let mut path = root.to_path_buf();
    let io_error = |path: &Path, source| InventoryError::Io {
        name: name.clone(),
        path: path.to_path_buf(),
        source,
    };
    for part in relative.split('/') {
        path.push(part);
        let meta = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
        if meta.file_type().is_symlink() {
            return Err(InventoryError::Symlink {
                name: name.clone(),
                path,
            });
        }
    }
    let file = fs::File::open(&path).map_err(|source| io_error(&path, source))?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(&path, source))?;
    if bytes.len() as u64 > limit {
        return Err(InventoryError::Changed {
            name: name.clone(),
            path,
        });
    }
    Ok(bytes)
}

/// The system SBOM of `inventory`, naming the system `name`.
pub(crate) fn system_sbom<'a>(name: &'a str, inventory: &'a [Inventoried]) -> SystemSbom<'a> {
    let packages = inventory
        .iter()
        .map(|item| {
            let package = &item.package;
            SystemPackage {
                name: &package.name,
                version: &package.version,
                target: package.target.as_deref(),
                archive_blake3: package.archive_blake3,
                entries_blake3: package.entries_blake3,
                signer: package.signer,
                signer_label: &package.signer_label,
                depends: &package.depends,
                files: &package.files,
                build_sbom: item.build_sbom.as_ref(),
            }
        })
        .collect();
    SystemSbom::new(name, packages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sboms_are_read_inside_the_root_and_within_their_size() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let name = PackageName::new("tiny").unwrap();
        fs::create_dir_all(root.path().join("usr/share")).unwrap();
        fs::write(root.path().join("usr/share/a.json"), b"{}").unwrap();
        assert_eq!(
            read_in_root(root.path(), "usr/share/a.json", 2, &name).unwrap(),
            b"{}"
        );
        // Longer than recorded: the file changed.
        assert!(matches!(
            read_in_root(root.path(), "usr/share/a.json", 1, &name),
            Err(InventoryError::Changed { .. })
        ));
        // A directory on the way that points outside the root.
        fs::write(outside.path().join("a.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("usr/link")).unwrap();
        assert!(matches!(
            read_in_root(root.path(), "usr/link/a.json", 2, &name),
            Err(InventoryError::Symlink { .. })
        ));
    }
}
