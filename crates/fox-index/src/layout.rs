//! The registry's files: `registry.toml` and one TOML file per package.
//!
//! ```text
//! registry.toml                          name, artifact base URLs, builders
//! packages/<xx>/<name>.toml              one package; <xx> is the name's first two characters
//! artifacts/<xx>/<name>/<version>.toml   prebuilt artifacts of one release
//! advisories/<id>.toml                   security advisories
//! metadata/                              signed trust metadata (crate::metadata)
//! ```
//!
//! A package file names the repository, the owners whose SSH keys sign its
//! release tags (and, as Ed25519 keys, its delegation metadata), and its
//! releases. It is delegated to the owners, so they add releases without a
//! maintainer. Prebuilt artifacts are listed in separate files that builder
//! CI writes, covered by the top-level targets role: a build never needs an
//! owner's signature, and an artifact is trusted through its builder
//! attestations, not through the list.

use fox_core::trust::AuthorKey;
use fox_core::{Blake3Digest, CpuLevel, GitUrl, KeyId, PackageName};
use semver::Version;
use serde::{Deserialize, Serialize};

/// The configuration file at the registry root.
pub const REGISTRY_FILE: &str = "registry.toml";

/// The directory of package files.
pub const PACKAGES_DIR: &str = "packages";

/// The directory of artifact lists.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// The directory of advisories.
pub const ADVISORIES_DIR: &str = "advisories";

/// The registry format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RegistryFormat {
    /// The first format, written `"fox-registry/1"`.
    #[default]
    #[serde(rename = "fox-registry/1")]
    V1,
}

/// `registry.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RegistryConfig {
    /// Format version.
    pub format: RegistryFormat,
    /// The registry's name, as clients qualify package names with it.
    pub name: String,
    /// Base URLs of the content-addressed artifact store; an artifact is at
    /// `<base>/blake3/<hash>`. The location carries no trust.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_base_urls: Vec<String>,
    /// How many builders must attest to an artifact before clients use it.
    #[serde(default = "one")]
    pub min_builder_signatures: u32,
    /// Builders whose attestations count.
    #[serde(default, rename = "builder", skip_serializing_if = "Vec::is_empty")]
    pub builders: Vec<Builder>,
}

fn one() -> u32 {
    1
}

/// A builder of prebuilt artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Builder {
    /// Its name in attestations.
    pub label: String,
    /// Its pkgar signing key.
    pub key: KeyId,
}

/// What a package is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PackageKind {
    /// A program.
    App,
    /// A daemon started by init.
    Service,
    /// A shared library.
    Library,
    /// Data files.
    Data,
    /// A cookbook recipe.
    Recipe,
}

/// A package file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PackageEntry {
    /// The package name.
    pub name: PackageName,
    /// What it is.
    pub kind: PackageKind,
    /// One line on what it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The repository.
    pub git: GitUrl,
    /// Mirrors expected to hold the same trees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<GitUrl>,
    /// The package's directory inside the repository, for a package below
    /// the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Who signs its releases.
    #[serde(rename = "owners")]
    pub owners: Vec<Owner>,
    /// Its releases, oldest first.
    #[serde(default, rename = "release", skip_serializing_if = "Vec::is_empty")]
    pub releases: Vec<Release>,
}

/// A package owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    /// A short handle.
    pub id: String,
    /// The OpenSSH Ed25519 key that signs release tags and the package's
    /// delegation metadata.
    pub key: AuthorKey,
}

impl Owner {
    /// The owner's key as a delegation key.
    pub fn delegation_key(&self) -> KeyId {
        KeyId::from_bytes(self.key.ed25519_bytes())
    }
}

/// One release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// The version.
    pub version: Version,
    /// The signed tag.
    pub tag: String,
    /// The commit the tag names.
    pub commit: String,
    /// The commit's `fox-tree/1` hash.
    pub tree_blake3: Blake3Digest,
    /// Whether new installs skip it.
    #[serde(default)]
    pub yanked: bool,
}

/// The prebuilt artifacts of one release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactList {
    /// The package name.
    pub name: PackageName,
    /// The release.
    pub version: Version,
    /// The artifacts.
    #[serde(default, rename = "artifact")]
    pub artifacts: Vec<Artifact>,
}

/// A prebuilt artifact of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Artifact {
    /// Rust target triple.
    pub target: String,
    /// CPU level, for native x86-64 artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_level: Option<CpuLevel>,
    /// Whether the package's default features were on, so a client that
    /// asks for no particular features can use it.
    pub default_features: bool,
    /// The features asked for besides the defaults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// The feature-set hash.
    pub feature_set_hash: Blake3Digest,
    /// BLAKE3 of the `.pkgar` file.
    pub pkgar_blake3: Blake3Digest,
    /// Its size.
    pub size: u64,
    /// BLAKE3 of the SBOM inside it.
    pub sbom_blake3: Blake3Digest,
    /// One signed attestation per builder that built this exact archive.
    #[serde(default, rename = "attestation", skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<ArtifactAttestation>,
}

/// One builder's attestation of an artifact, stored as two blobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ArtifactAttestation {
    /// The builder's label in `registry.toml`.
    pub builder: String,
    /// BLAKE3 of the attestation file.
    pub attestation_blake3: Blake3Digest,
    /// BLAKE3 of its signature file.
    pub signature_blake3: Blake3Digest,
}

impl ArtifactList {
    /// The artifact with this archive hash, if the list has it.
    pub fn artifact(&self, pkgar_blake3: &Blake3Digest) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|artifact| &artifact.pkgar_blake3 == pkgar_blake3)
    }
}

/// Why a registry file is invalid.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// The file is not valid TOML of the expected shape.
    #[error("invalid {what}")]
    Parse {
        /// `registry.toml` or a package file.
        what: &'static str,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// The file could not be serialized.
    #[error("cannot serialize {what}")]
    Serialize {
        /// `registry.toml` or a package file.
        what: &'static str,
        /// The underlying error.
        #[source]
        source: toml::ser::Error,
    },
    /// An artifact list is not at the path its name and version imply.
    #[error("artifacts of {name} {version} must be at {expected}, not {path}")]
    WrongArtifactPath {
        /// The package name.
        name: PackageName,
        /// The version.
        version: Version,
        /// Where it should be.
        expected: String,
        /// Where it is.
        path: String,
    },
    /// A package file is not at the path its name implies.
    #[error("package {name} must be at {expected}, not {path}")]
    WrongPath {
        /// The package name.
        name: PackageName,
        /// Where it should be.
        expected: String,
        /// Where it is.
        path: String,
    },
    /// A package has no owners.
    #[error("package {name} has no owners")]
    NoOwners {
        /// The package name.
        name: PackageName,
    },
    /// Two releases have the same version.
    #[error("package {name} lists version {version} twice")]
    DuplicateVersion {
        /// The package name.
        name: PackageName,
        /// The version.
        version: Version,
    },
}

/// The registry path of `name`'s package file:
/// `packages/<first two characters>/<name>.toml` (one-character names use
/// `_` as the second character).
///
/// # Examples
///
/// ```
/// use fox_core::PackageName;
///
/// let name = PackageName::new("ripgrep")?;
/// assert_eq!(fox_index::layout::package_path(&name), "packages/ri/ripgrep.toml");
/// # Ok::<(), fox_core::NameError>(())
/// ```
pub fn package_path(name: &PackageName) -> String {
    let text = name.as_str();
    let mut fanout: String = text.chars().take(2).collect();
    if fanout.len() < 2 {
        fanout.push('_');
    }
    format!("{PACKAGES_DIR}/{fanout}/{text}.toml")
}

/// The registry path of the artifact list of `name` `version`:
/// `artifacts/<xx>/<name>/<version>.toml`.
pub fn artifacts_path(name: &PackageName, version: &Version) -> String {
    let package = package_path(name);
    let fanout = package
        .strip_prefix(PACKAGES_DIR)
        .and_then(|rest| rest.strip_suffix(".toml"))
        .unwrap_or_default();
    format!("{ARTIFACTS_DIR}{fanout}/{version}.toml")
}

impl ArtifactList {
    /// Parses an artifact list found at registry path `path`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Parse`] if it is invalid and
    /// [`LayoutError::WrongArtifactPath`] if `path` does not match it.
    pub fn from_toml(text: &str, path: &str) -> Result<Self, LayoutError> {
        let list: Self = toml::from_str(text).map_err(|source| LayoutError::Parse {
            what: "artifact list",
            source,
        })?;
        let expected = artifacts_path(&list.name, &list.version);
        if expected != path {
            return Err(LayoutError::WrongArtifactPath {
                name: list.name,
                version: list.version,
                expected,
                path: path.to_owned(),
            });
        }
        Ok(list)
    }

    /// Serializes the artifact list.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Serialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, LayoutError> {
        toml::to_string(self).map_err(|source| LayoutError::Serialize {
            what: "artifact list",
            source,
        })
    }
}

impl RegistryConfig {
    /// A registry named `name` with no builders.
    pub fn new(name: &str) -> Self {
        Self {
            format: RegistryFormat::V1,
            name: name.to_owned(),
            artifact_base_urls: Vec::new(),
            min_builder_signatures: 1,
            builders: Vec::new(),
        }
    }

    /// Parses `registry.toml`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Parse`] if it is invalid.
    pub fn from_toml(text: &str) -> Result<Self, LayoutError> {
        toml::from_str(text).map_err(|source| LayoutError::Parse {
            what: REGISTRY_FILE,
            source,
        })
    }

    /// Serializes `registry.toml`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Serialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, LayoutError> {
        toml::to_string(self).map_err(|source| LayoutError::Serialize {
            what: REGISTRY_FILE,
            source,
        })
    }

    /// The builder keys as a trusted key set.
    pub fn builder_keys(&self) -> fox_core::TrustedKeys {
        let mut keys = fox_core::TrustedKeys::new();
        for builder in &self.builders {
            keys.insert(builder.label.clone(), builder.key);
        }
        keys
    }
}

impl PackageEntry {
    /// Parses a package file found at registry path `path`, checking that
    /// the path matches the name, that there are owners, and that no
    /// version repeats.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for the first problem.
    pub fn from_toml(text: &str, path: &str) -> Result<Self, LayoutError> {
        let entry: Self = toml::from_str(text).map_err(|source| LayoutError::Parse {
            what: "package file",
            source,
        })?;
        entry.check(path)?;
        Ok(entry)
    }

    /// Serializes the package file.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Serialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, LayoutError> {
        toml::to_string(self).map_err(|source| LayoutError::Serialize {
            what: "package file",
            source,
        })
    }

    /// The release of `version`.
    pub fn release(&self, version: &Version) -> Option<&Release> {
        self.releases
            .iter()
            .find(|release| &release.version == version)
    }

    /// The newest release that is not yanked and not a pre-release.
    pub fn newest_release(&self) -> Option<&Release> {
        self.releases
            .iter()
            .filter(|release| !release.yanked && release.version.pre.is_empty())
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    /// The owner whose key is `key`, ignoring comments.
    pub fn owner_with_key(&self, key: &AuthorKey) -> Option<&Owner> {
        self.owners.iter().find(|owner| &owner.key == key)
    }

    fn check(&self, path: &str) -> Result<(), LayoutError> {
        let expected = package_path(&self.name);
        if expected != path {
            return Err(LayoutError::WrongPath {
                name: self.name.clone(),
                expected,
                path: path.to_owned(),
            });
        }
        if self.owners.is_empty() {
            return Err(LayoutError::NoOwners {
                name: self.name.clone(),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for release in &self.releases {
            if !seen.insert(&release.version) {
                return Err(LayoutError::DuplicateVersion {
                    name: self.name.clone(),
                    version: release.version.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const OWNER_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";

    fn entry_text() -> String {
        format!(
            r#"name = "ripgrep"
kind = "app"
summary = "Recursive regex search"
git = "https://github.com/BurntSushi/ripgrep.git"

[[owners]]
id = "burntsushi"
key = "{OWNER_KEY} burntsushi"

[[release]]
version = "14.2.0"
tag = "14.2.0"
commit = "3f1c0e5a3f1c0e5a3f1c0e5a3f1c0e5a3f1c0e5a"
tree_blake3 = "{h}"

[[release]]
version = "15.0.0-rc.1"
tag = "15.0.0-rc.1"
commit = "3f1c0e5a3f1c0e5a3f1c0e5a3f1c0e5a3f1c0e5b"
tree_blake3 = "{h}"
"#,
            h = Blake3Digest::of(b"tree")
        )
    }

    #[test]
    fn package_files_parse_and_check_their_path() {
        let entry = PackageEntry::from_toml(&entry_text(), "packages/ri/ripgrep.toml").unwrap();
        assert_eq!(entry.git.as_str(), "https://github.com/BurntSushi/ripgrep");
        assert_eq!(
            entry.newest_release().unwrap().version,
            Version::new(14, 2, 0)
        );
        assert_eq!(
            PackageEntry::from_toml(&entry.to_toml().unwrap(), "packages/ri/ripgrep.toml").unwrap(),
            entry
        );
        assert!(matches!(
            PackageEntry::from_toml(&entry_text(), "packages/rg/ripgrep.toml"),
            Err(LayoutError::WrongPath { .. })
        ));
        let twice = entry_text().replace("15.0.0-rc.1\"\ntag", "14.2.0\"\ntag");
        assert!(matches!(
            PackageEntry::from_toml(&twice, "packages/ri/ripgrep.toml"),
            Err(LayoutError::DuplicateVersion { .. })
        ));
        let unknown = entry_text() + "extra = 1\n";
        assert!(PackageEntry::from_toml(&unknown, "packages/ri/ripgrep.toml").is_err());
    }

    #[test]
    fn fanout_paths() {
        let name = |text| PackageName::new(text).unwrap();
        assert_eq!(package_path(&name("x")), "packages/x_/x.toml");
        assert_eq!(package_path(&name("fox")), "packages/fo/fox.toml");
        assert_eq!(
            artifacts_path(&name("fox"), &Version::new(1, 2, 3)),
            "artifacts/fo/fox/1.2.3.toml"
        );
    }

    #[test]
    fn registry_config_defaults() {
        let config =
            RegistryConfig::from_toml("format = \"fox-registry/1\"\nname = \"ferric\"\n").unwrap();
        assert_eq!(config.min_builder_signatures, 1);
        assert!(config.builder_keys().is_empty());
        assert_eq!(
            RegistryConfig::from_toml(&config.to_toml().unwrap()).unwrap(),
            config
        );
    }

    #[test]
    fn artifact_lists_round_trip_and_check_their_path() {
        let h = Blake3Digest::of(b"x");
        let text = format!(
            "name = \"ripgrep\"\nversion = \"14.2.0\"\n\n[[artifact]]\n\
             target = \"x86_64-unknown-redox\"\ndefault-features = true\n\
             feature-set-hash = \"{h}\"\npkgar-blake3 = \"{h}\"\nsize = 5\n\
             sbom-blake3 = \"{h}\"\n\n[[artifact.attestation]]\nbuilder = \"ci\"\n\
             attestation-blake3 = \"{h}\"\nsignature-blake3 = \"{h}\"\n"
        );
        let path = "artifacts/ri/ripgrep/14.2.0.toml";
        let list = ArtifactList::from_toml(&text, path).unwrap();
        assert_eq!(list.artifact(&h).unwrap().attestations[0].builder, "ci");
        assert_eq!(
            ArtifactList::from_toml(&list.to_toml().unwrap(), path).unwrap(),
            list
        );
        assert!(matches!(
            ArtifactList::from_toml(&text, "artifacts/ri/ripgrep/1.0.0.toml"),
            Err(LayoutError::WrongArtifactPath { .. })
        ));
    }
}
