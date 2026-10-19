//! The registries a root installs from: [`REGISTRIES_FILE`] under the
//! install root, format `fox-registries/1`.
//!
//! ```toml
//! format = "fox-registries/1"
//!
//! [[registry]]
//! name = "ferric"
//! git = "https://github.com/rexlunae/ferric-registry"
//! root-blake3 = "<BLAKE3 of the registry's metadata/root/1.json>"
//! ```
//!
//! Registries are searched in the order listed. `root-blake3` pins the
//! first root the client trusts; later roots are accepted only through the
//! signed rotation chain, so the pin never needs to change. The verified
//! checkout and the client's version state live under
//! [`REGISTRY_STATE_DIR`]`/<name>/`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, load_toml, save_toml};
use crate::{Blake3Digest, GitUrl, NameError, PackageName};

/// The registry list, relative to the install root.
pub const REGISTRIES_FILE: &str = "etc/fox/registries.toml";

/// Where each registry's checkout and state live, relative to the root.
pub const REGISTRY_STATE_DIR: &str = "var/lib/fox/registries";

/// The registry list format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RegistriesFormat {
    /// The first format, written `"fox-registries/1"`.
    #[default]
    #[serde(rename = "fox-registries/1")]
    V1,
}

/// One configured registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RegistryEntry {
    /// The local name, which qualifies package names (`name/package`).
    pub name: PackageName,
    /// The registry repository.
    pub git: GitUrl,
    /// BLAKE3 of the first root the client trusts.
    pub root_blake3: Blake3Digest,
}

impl RegistryEntry {
    /// The directory holding this registry's checkout and state.
    pub fn state_dir(&self, root: &Path) -> PathBuf {
        root.join(REGISTRY_STATE_DIR).join(self.name.as_str())
    }
}

/// Why the registry list could not be changed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistriesError {
    /// The file could not be read, parsed or written.
    #[error("registry list")]
    File(#[from] ConfigError),
    /// A registry with this name is already configured.
    #[error("a registry named {name} is already configured")]
    Exists {
        /// The name.
        name: PackageName,
    },
    /// An invalid registry name.
    #[error("invalid registry name")]
    Name(#[from] NameError),
}

/// The configured registries, in search order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registries {
    format: RegistriesFormat,
    #[serde(default, rename = "registry", skip_serializing_if = "Vec::is_empty")]
    registries: Vec<RegistryEntry>,
}

impl Registries {
    /// Reads `<root>/etc/fox/registries.toml`; a missing file is an empty
    /// list.
    ///
    /// # Errors
    ///
    /// [`RegistriesError::File`] if it cannot be read or is invalid.
    pub fn load(root: &Path) -> Result<Self, RegistriesError> {
        Ok(load_toml(&root.join(REGISTRIES_FILE))?)
    }

    /// Writes the list, replacing the file atomically.
    ///
    /// # Errors
    ///
    /// [`RegistriesError::File`] if it cannot be written.
    pub fn save(&self, root: &Path) -> Result<(), RegistriesError> {
        Ok(save_toml(&root.join(REGISTRIES_FILE), self)?)
    }

    /// The registries, in search order.
    pub fn entries(&self) -> &[RegistryEntry] {
        &self.registries
    }

    /// The registry called `name`.
    pub fn get(&self, name: &str) -> Option<&RegistryEntry> {
        self.registries
            .iter()
            .find(|entry| entry.name.as_str() == name)
    }

    /// Appends a registry.
    ///
    /// # Errors
    ///
    /// [`RegistriesError::Exists`] if the name is taken.
    pub fn add(&mut self, entry: RegistryEntry) -> Result<(), RegistriesError> {
        if self.get(entry.name.as_str()).is_some() {
            return Err(RegistriesError::Exists { name: entry.name });
        }
        self.registries.push(entry);
        Ok(())
    }

    /// Removes the registry called `name`, returning it.
    pub fn remove(&mut self, name: &str) -> Option<RegistryEntry> {
        let index = self
            .registries
            .iter()
            .position(|entry| entry.name.as_str() == name)?;
        Some(self.registries.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registries_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut list = Registries::load(dir.path()).unwrap();
        assert!(list.entries().is_empty());
        let entry = RegistryEntry {
            name: PackageName::new("ferric").unwrap(),
            git: GitUrl::new("https://example.com/registry.git").unwrap(),
            root_blake3: Blake3Digest::of(b"root"),
        };
        list.add(entry.clone()).unwrap();
        assert!(matches!(
            list.add(entry.clone()),
            Err(RegistriesError::Exists { .. })
        ));
        list.save(dir.path()).unwrap();
        let loaded = Registries::load(dir.path()).unwrap();
        assert_eq!(loaded, list);
        assert_eq!(
            entry.state_dir(dir.path()),
            dir.path().join("var/lib/fox/registries/ferric")
        );
        assert_eq!(list.remove("ferric"), Some(entry));
        assert!(list.remove("ferric").is_none());
    }
}
