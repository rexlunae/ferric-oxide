//! The package manifest: a small TOML file inside the archive that says what
//! the package is.
//!
//! The manifest is stored as a normal archive entry at
//! [`manifest_path`]`(name)`, so the pkgar header signature covers it like
//! any installed file. It is the P1 subset of the `[package.metadata.fox]`
//! table in the design (section 3.2); fields for builds, variants and
//! capabilities arrive with the later phases.
//!
//! ```toml
//! format = "fox-manifest/1"
//! name = "hello"
//! version = "1.0.0"
//! target = "x86_64-unknown-redox"
//! summary = "Prints a greeting"
//! license = "MIT"
//! depends = ["relibc"]
//!
//! [source]
//! git = "https://github.com/example/hello"
//! commit = "3f1c0e5a"
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::PackageName;

/// Directory inside an archive (relative to the install root) that holds
/// package manifests.
pub const MANIFEST_DIR: &str = "usr/share/fox/manifest";

/// The archive path of the manifest for `name`:
/// `usr/share/fox/manifest/<name>.toml`.
///
/// # Examples
///
/// ```
/// use fox_core::{PackageName, manifest_path};
///
/// let name = PackageName::new("hello").unwrap();
/// assert_eq!(manifest_path(&name), "usr/share/fox/manifest/hello.toml");
/// ```
pub fn manifest_path(name: &PackageName) -> String {
    let mut path = String::from(MANIFEST_DIR);
    path.push('/');
    path.push_str(name.as_str());
    path.push_str(".toml");
    path
}

/// The manifest format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ManifestFormat {
    /// The first format, written `"fox-manifest/1"`.
    #[default]
    #[serde(rename = "fox-manifest/1")]
    V1,
}

/// What a package is: its name, version and descriptive metadata.
///
/// # Examples
///
/// ```
/// use fox_core::Manifest;
///
/// let manifest = Manifest::from_toml(r#"
///     format = "fox-manifest/1"
///     name = "hello"
///     version = "1.0.0"
/// "#).unwrap();
/// assert_eq!(manifest.name.as_str(), "hello");
/// assert!(manifest.depends.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Manifest {
    /// Manifest format version.
    pub format: ManifestFormat,
    /// Package name.
    pub name: PackageName,
    /// Package version. Usually semver, but recipe versions are free-form.
    pub version: String,
    /// Target triple the package was built for, if it is target-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// One-line description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Licence as an SPDX expression, such as `MIT OR Apache-2.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Packages needed at run time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<PackageName>,
    /// Where the package was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceInfo>,
}

/// The source a package was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Git repository URL.
    pub git: String,
    /// Commit id of the built revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Why a manifest could not be read or written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The text is not valid TOML or does not match the manifest schema.
    #[error("invalid manifest")]
    Parse(#[source] toml::de::Error),
    /// The manifest could not be serialized.
    #[error("cannot serialize manifest")]
    Serialize(#[source] toml::ser::Error),
    /// The `version` field is empty.
    #[error("manifest version is empty")]
    EmptyVersion,
}

impl Manifest {
    /// A minimal manifest with only a name and a version.
    pub fn new(name: PackageName, version: impl Into<String>) -> Self {
        Self {
            format: ManifestFormat::V1,
            name,
            version: version.into(),
            target: None,
            summary: None,
            license: None,
            depends: Vec::new(),
            source: None,
        }
    }

    /// Parses a manifest from TOML.
    ///
    /// # Errors
    ///
    /// [`ManifestError::Parse`] if the text is not a valid manifest, and
    /// [`ManifestError::EmptyVersion`] if `version` is empty.
    pub fn from_toml(text: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(text).map_err(ManifestError::Parse)?;
        if manifest.version.trim().is_empty() {
            return Err(ManifestError::EmptyVersion);
        }
        Ok(manifest)
    }

    /// Serializes the manifest to TOML.
    ///
    /// # Errors
    ///
    /// [`ManifestError::Serialize`] if serialization fails, which only
    /// happens for values TOML cannot represent.
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        toml::to_string(self).map_err(ManifestError::Serialize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_field() {
        let mut manifest = Manifest::new(PackageName::new("hello").unwrap(), "1.2.3");
        manifest.target = Some("x86_64-unknown-redox".into());
        manifest.summary = Some("Prints a greeting".into());
        manifest.license = Some("MIT".into());
        manifest.depends = alloc::vec![PackageName::new("relibc").unwrap()];
        manifest.source = Some(SourceInfo {
            git: "https://example.com/hello".into(),
            commit: Some("abc".into()),
        });
        let text = manifest.to_toml().unwrap();
        assert!(text.contains("format = \"fox-manifest/1\""));
        assert_eq!(Manifest::from_toml(&text).unwrap(), manifest);
    }

    #[test]
    fn rejects_unknown_format_and_bad_names() {
        let bad_format = "format = \"fox-manifest/9\"\nname = \"a\"\nversion = \"1\"";
        assert!(matches!(
            Manifest::from_toml(bad_format),
            Err(ManifestError::Parse(_))
        ));
        let bad_name = "format = \"fox-manifest/1\"\nname = \"../a\"\nversion = \"1\"";
        assert!(matches!(
            Manifest::from_toml(bad_name),
            Err(ManifestError::Parse(_))
        ));
        let empty_version = "format = \"fox-manifest/1\"\nname = \"a\"\nversion = \" \"";
        assert!(matches!(
            Manifest::from_toml(empty_version),
            Err(ManifestError::EmptyVersion)
        ));
    }
}
