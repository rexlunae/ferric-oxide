//! What is installed under a root, with each package's build SBOM:
//! the input to `fox sbom --system` and `fox audit`.
//!
//! A package's build SBOM is the file at [`sbom_path`] that the package
//! itself installed. It is read only when the package's own file list
//! records it, and only if its contents still hash to the recorded BLAKE3,
//! so another package cannot supply or replace it.

use std::fs;
use std::path::PathBuf;

use fox_core::{Blake3Digest, PackageName, sbom_path};
use fox_sbom::Bom;
use fox_sbom::{SbomError, SystemPackage, SystemSbom, read_build_sbom};
use fox_store::{InstalledPackage, Store, StoreError};

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
    let path = store.root().join(&relative);
    let bytes = fs::read(&path).map_err(|source| InventoryError::Io {
        name: package.name.clone(),
        path: path.clone(),
        source,
    })?;
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
