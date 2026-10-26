//! The cookbook's package metadata formats.
//!
//! The cookbook writes three TOML formats next to the archives it builds,
//! inherited from Redox's `pkg` and kept byte-compatible so existing build
//! trees and repositories still read:
//!
//! - [`PackageInfo`]: `<name>.toml` beside each `<name>.pkgar`, naming the
//!   package, its version, target, entry-table hash, sizes, source
//!   identifiers and dependencies. It is also the sidecar fox falls back to
//!   when an archive carries no signed manifest; see
//!   [`archive`](crate::archive).
//! - [`RepositoryIndex`]: `repo.toml` at the top of a repository directory,
//!   mapping every published package to its version and recording packages
//!   whose last build failed.
//! - [`SourceIdentifier`]: the source, patch, commit and time identifiers a
//!   package was built from, shared by both.
//!
//! Package references in these files are [`RecipeName`]s, which unlike an
//! installed [`PackageName`] may carry the cookbook's `host:` or `target:`
//! build selector and may name a sub-package relative to the current recipe
//! (`.dev`).
//!
//! These types replace the ones fox, the cookbook and the installer used to
//! take from the `redox-pkg` crate.
//!
//! # Examples
//!
//! ```
//! use fox_core::cookbook::PackageInfo;
//!
//! let info = PackageInfo::from_toml(
//!     r#"
//!     name = "gzdoom"
//!     version = "4.14"
//!     target = "x86_64-unknown-redox"
//!     depends = ["gtk3", "sdl2"]
//!     "#,
//! )?;
//! assert_eq!(info.depends.len(), 2);
//! assert_eq!(PackageInfo::from_toml(&info.to_toml()?)?, info);
//! # Ok::<(), fox_core::cookbook::CookbookError>(())
//! ```

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::borrow::Borrow;
use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::name::{NameError, PackageName};

/// File name of the repository index at the top of a repository directory.
pub const REPOSITORY_INDEX: &str = "repo.toml";

/// Why cookbook metadata could not be read or written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CookbookError {
    /// A package reference breaks the [`RecipeName`] rules.
    #[error("invalid package reference {0:?}")]
    InvalidName(String),
    /// The text is not valid metadata.
    #[error("cannot parse cookbook metadata")]
    Parse(#[source] toml::de::Error),
    /// The metadata could not be written as TOML.
    #[error("cannot write cookbook metadata")]
    Serialize(#[source] toml::ser::Error),
    /// A metadata file could not be read.
    #[cfg(feature = "std")]
    #[error("cannot read {}", path.display())]
    Io {
        /// The file.
        path: std::path::PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A metadata file could not be parsed.
    #[cfg(feature = "std")]
    #[error("cannot parse {}", path.display())]
    ParseFile {
        /// The file.
        path: std::path::PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
}

/// The build selector a [`RecipeName`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prefix {
    /// No selector: the package for whichever target is being built.
    Any,
    /// `host:`: the package built for the build machine.
    Host,
    /// `target:`: the package built for the image's target.
    Target,
}

impl Prefix {
    fn as_str(self) -> &'static str {
        match self {
            Self::Any => "",
            Self::Host => "host:",
            Self::Target => "target:",
        }
    }
}

/// A package reference as the cookbook writes it: `recipe`, `recipe.sub`,
/// `.sub` (a sub-package of the recipe being built), optionally prefixed by
/// `host:` or `target:`.
///
/// The rules are the cookbook's: non-empty, no `/` or NUL, at most one `.`,
/// and at most one `:`, which must belong to a `host:` or `target:` prefix.
/// [`RecipeName::package_name`] gives the installed [`PackageName`] a
/// reference stands for, which is stricter.
///
/// # Examples
///
/// ```
/// use fox_core::cookbook::{Prefix, RecipeName};
///
/// let name = RecipeName::new("host:llvm21.dev")?;
/// assert_eq!(name.prefix(), Prefix::Host);
/// assert_eq!(name.recipe(), "llvm21");
/// assert_eq!(name.suffix(), Some("dev"));
/// assert_eq!(name.with_prefix(Prefix::Any).as_str(), "llvm21.dev");
/// # Ok::<(), fox_core::cookbook::CookbookError>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RecipeName(String);

impl RecipeName {
    /// Checks and wraps a package reference.
    ///
    /// # Errors
    ///
    /// Returns [`CookbookError::InvalidName`] if `name` breaks the rules
    /// above.
    pub fn new(name: impl Into<String>) -> Result<Self, CookbookError> {
        let name = name.into();
        let colons = name.matches(':').count();
        let valid = !name.is_empty()
            && !name.contains(['/', '\0'])
            && name.matches('.').count() <= 1
            && (colons == 0
                || (colons == 1 && (name.starts_with("host:") || name.starts_with("target:"))));
        if valid {
            Ok(Self(name))
        } else {
            Err(CookbookError::InvalidName(name))
        }
    }

    /// Checks every reference in `names`.
    ///
    /// # Errors
    ///
    /// Returns [`CookbookError::InvalidName`] for the first invalid one.
    pub fn from_list<I>(names: I) -> Result<Vec<Self>, CookbookError>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        names.into_iter().map(Self::new).collect()
    }

    /// The reference as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The build selector.
    pub fn prefix(&self) -> Prefix {
        if self.0.starts_with(Prefix::Host.as_str()) {
            Prefix::Host
        } else if self.0.starts_with(Prefix::Target.as_str()) {
            Prefix::Target
        } else {
            Prefix::Any
        }
    }

    /// Whether the reference starts with `host:`.
    pub fn is_host(&self) -> bool {
        self.prefix() == Prefix::Host
    }

    /// Whether the reference starts with `target:`.
    pub fn is_target(&self) -> bool {
        self.prefix() == Prefix::Target
    }

    /// The reference without its build selector: `llvm21.dev` for
    /// `host:llvm21.dev`.
    pub fn without_prefix(&self) -> &str {
        &self.0[self.prefix().as_str().len()..]
    }

    /// The recipe part, between the selector and the `.`: `llvm21` for
    /// `host:llvm21.dev`, and empty for a relative `.dev`.
    pub fn recipe(&self) -> &str {
        let name = self.without_prefix();
        name.split_once('.').map_or(name, |(recipe, _)| recipe)
    }

    /// The sub-package after the `.`, if there is one.
    pub fn suffix(&self) -> Option<&str> {
        self.without_prefix().split_once('.').map(|(_, suffix)| suffix)
    }

    /// The same package with `prefix` as its build selector.
    pub fn with_prefix(&self, prefix: Prefix) -> Self {
        Self(format!("{}{}", prefix.as_str(), self.without_prefix()))
    }

    /// The same package with `host:` as its build selector.
    pub fn with_host(&self) -> Self {
        self.with_prefix(Prefix::Host)
    }

    /// The same package with `target:` as its build selector.
    pub fn with_target(&self) -> Self {
        self.with_prefix(Prefix::Target)
    }

    /// The same recipe with `suffix` as its sub-package (none for `None`),
    /// keeping the build selector.
    pub fn with_prefixed_suffix(&self, suffix: Option<&str>) -> Self {
        let mut name = format!("{}{}", self.prefix().as_str(), self.recipe());
        if let Some(suffix) = suffix {
            name.push('.');
            name.push_str(suffix);
        }
        Self(name)
    }

    /// The same recipe with `suffix` as its sub-package (none for `None`),
    /// dropping the build selector.
    pub fn with_suffix(&self, suffix: Option<&str>) -> Self {
        let mut name = self.recipe().to_string();
        if let Some(suffix) = suffix {
            name.push('.');
            name.push_str(suffix);
        }
        Self(name)
    }

    /// The installed package this reference names, resolving a relative
    /// `.sub` against `owner` (`.runtime` in `llvm21.dev` is
    /// `llvm21.runtime`) and dropping the build selector.
    ///
    /// # Errors
    ///
    /// Returns a [`NameError`] if the result is not a valid installed
    /// package name.
    pub fn package_name(&self, owner: Option<&PackageName>) -> Result<PackageName, NameError> {
        let name = self.without_prefix();
        match (name.strip_prefix('.'), owner) {
            (Some(suffix), Some(owner)) => {
                let base = owner.as_str().split('.').next().unwrap_or_default();
                PackageName::new(format!("{base}.{suffix}"))
            }
            _ => PackageName::new(name),
        }
    }
}

impl fmt::Display for RecipeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RecipeName {
    type Err = CookbookError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl TryFrom<String> for RecipeName {
    type Error = CookbookError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl TryFrom<&str> for RecipeName {
    type Error = CookbookError;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<RecipeName> for String {
    fn from(name: RecipeName) -> Self {
        name.0
    }
}

impl From<PackageName> for RecipeName {
    fn from(name: PackageName) -> Self {
        Self(name.into())
    }
}

impl Borrow<str> for RecipeName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Where a package's source came from, as the cookbook identifies it.
///
/// Empty fields are left out of the TOML.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceIdentifier {
    /// The git commit or the BLAKE3 hash of the source tarball.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source_identifier: String,
    /// The commit of the build-system repository the package was built from.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub commit_identifier: String,
    /// The combined hash of the recipe's patches.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub patch_identifier: String,
    /// When the source was last updated, in ISO 8601.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub time_identifier: String,
}

impl SourceIdentifier {
    /// Whether `source` and `patch` are the identifiers recorded here, so
    /// the recorded build is still current.
    pub fn is_updated(&self, source: &str, patch: &str) -> bool {
        source == self.source_identifier && patch == self.patch_identifier
    }
}

/// The metadata file beside each archive the cookbook builds
/// (`<name>.toml`).
///
/// Empty strings and zero sizes are left out of the TOML.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PackageInfo {
    /// The package.
    pub name: RecipeName,
    /// Its version; empty for a meta-package, `TODO` when the recipe did
    /// not say.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// The target triple it was built for.
    pub target: String,
    /// The BLAKE3 hash of the archive's signed entry table, in hex.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub blake3: String,
    /// See [`SourceIdentifier::source_identifier`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source_identifier: String,
    /// See [`SourceIdentifier::commit_identifier`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub commit_identifier: String,
    /// See [`SourceIdentifier::patch_identifier`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub patch_identifier: String,
    /// See [`SourceIdentifier::time_identifier`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub time_identifier: String,
    /// Total size of the files once installed, in bytes.
    #[serde(skip_serializing_if = "is_zero")]
    pub storage_size: u64,
    /// Size of the archive, in bytes.
    #[serde(skip_serializing_if = "is_zero")]
    pub network_size: u64,
    /// Packages it needs at run time.
    pub depends: Vec<RecipeName>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl PackageInfo {
    /// Parses a metadata file's text.
    ///
    /// # Errors
    ///
    /// Returns [`CookbookError::Parse`] if the text is not valid TOML of
    /// this shape, including an invalid package reference.
    pub fn from_toml(text: &str) -> Result<Self, CookbookError> {
        toml::from_str(text).map_err(CookbookError::Parse)
    }

    /// Writes the metadata as TOML.
    ///
    /// # Errors
    ///
    /// Returns [`CookbookError::Serialize`] if TOML cannot represent it.
    pub fn to_toml(&self) -> Result<String, CookbookError> {
        toml::to_string(self).map_err(CookbookError::Serialize)
    }

    /// The source identifiers recorded in the metadata.
    pub fn source(&self) -> SourceIdentifier {
        SourceIdentifier {
            source_identifier: self.source_identifier.clone(),
            commit_identifier: self.commit_identifier.clone(),
            patch_identifier: self.patch_identifier.clone(),
            time_identifier: self.time_identifier.clone(),
        }
    }

    /// Records `source` as the identifiers the package was built from.
    pub fn set_source(&mut self, source: SourceIdentifier) {
        self.source_identifier = source.source_identifier;
        self.commit_identifier = source.commit_identifier;
        self.patch_identifier = source.patch_identifier;
        self.time_identifier = source.time_identifier;
    }
}

/// A repository directory's index (`repo.toml`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepositoryIndex {
    /// Identifies the build that wrote the index.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub build_id: String,
    /// Every published package, by name, with its version.
    pub packages: BTreeMap<String, String>,
    /// Packages whose last build is missing or out of date, with the source
    /// identifiers from when that was first noticed.
    pub outdated_packages: BTreeMap<String, SourceIdentifier>,
}

impl RepositoryIndex {
    /// Parses an index's text.
    ///
    /// # Errors
    ///
    /// Returns [`CookbookError::Parse`] if the text is not a valid index.
    pub fn from_toml(text: &str) -> Result<Self, CookbookError> {
        toml::from_str(text).map_err(CookbookError::Parse)
    }

    /// Writes the index as TOML.
    ///
    /// # Errors
    ///
    /// Returns [`CookbookError::Serialize`] if TOML cannot represent it.
    pub fn to_toml(&self) -> Result<String, CookbookError> {
        toml::to_string(self).map_err(CookbookError::Serialize)
    }
}

#[cfg(feature = "std")]
mod file {
    use std::path::Path;

    use serde::de::DeserializeOwned;

    use super::{CookbookError, PackageInfo, RepositoryIndex};

    fn read<T: DeserializeOwned>(path: &Path) -> Result<T, CookbookError> {
        let text = std::fs::read_to_string(path).map_err(|source| CookbookError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| CookbookError::ParseFile {
            path: path.to_path_buf(),
            source,
        })
    }

    impl PackageInfo {
        /// Reads a `<name>.toml` metadata file.
        ///
        /// # Errors
        ///
        /// Returns [`CookbookError::Io`] if it cannot be read and
        /// [`CookbookError::ParseFile`] if it is not valid metadata.
        pub fn from_file(path: &Path) -> Result<Self, CookbookError> {
            read(path)
        }
    }

    impl RepositoryIndex {
        /// Reads a `repo.toml` index.
        ///
        /// # Errors
        ///
        /// Returns [`CookbookError::Io`] if it cannot be read and
        /// [`CookbookError::ParseFile`] if it is not a valid index.
        pub fn from_file(path: &Path) -> Result<Self, CookbookError> {
            read(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> RecipeName {
        RecipeName::new(s).unwrap()
    }

    #[test]
    fn reference_rules() {
        for good in ["foo", "foo.bar", "host:foo", "host:foo.", "target:foo", ".dev", "a-b_c+1"] {
            assert!(RecipeName::new(good).is_ok(), "{good}");
        }
        for bad in ["", "a/b", "a\0", "a.b.c", "dolphin.emu.lator", "ffmpeg:latest", "host:a:b"] {
            assert!(RecipeName::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reference_parts() {
        let cases = [
            ("foo", Prefix::Any, "foo", None),
            ("foo.bar", Prefix::Any, "foo", Some("bar")),
            ("host:foo", Prefix::Host, "foo", None),
            ("host:foo.", Prefix::Host, "foo", Some("")),
            ("target:foo.dev", Prefix::Target, "foo", Some("dev")),
            (".dev", Prefix::Any, "", Some("dev")),
        ];
        for (text, prefix, recipe, suffix) in cases {
            let n = name(text);
            assert_eq!((n.prefix(), n.recipe(), n.suffix()), (prefix, recipe, suffix), "{text}");
        }
    }

    #[test]
    fn reference_rewrites() {
        let n = name("host:llvm21.dev");
        assert_eq!(n.without_prefix(), "llvm21.dev");
        assert_eq!(n.with_target().as_str(), "target:llvm21.dev");
        assert_eq!(name("llvm21").with_host().as_str(), "host:llvm21");
        assert_eq!(n.with_prefix(Prefix::Any).as_str(), "llvm21.dev");
        assert_eq!(n.with_prefixed_suffix(Some("runtime")).as_str(), "host:llvm21.runtime");
        assert_eq!(n.with_prefixed_suffix(None).as_str(), "host:llvm21");
        assert_eq!(n.with_suffix(Some("runtime")).as_str(), "llvm21.runtime");
        assert_eq!(n.with_suffix(None).as_str(), "llvm21");
    }

    #[test]
    fn installed_names() {
        let owner = PackageName::new("llvm21.dev").unwrap();
        let resolve = |s: &str, owner| name(s).package_name(owner).map(String::from);
        assert_eq!(resolve(".runtime", Some(&owner)).unwrap(), "llvm21.runtime");
        assert_eq!(resolve("host:zlib", Some(&owner)).unwrap(), "zlib");
        assert_eq!(resolve("zlib", None).unwrap(), "zlib");
        assert!(resolve(".runtime", None).is_err());
    }

    #[test]
    fn package_info() {
        let text = r#"
            name = "gzdoom"
            version = "TODO"
            target = "x86_64-unknown-redox"
            blake3 = "ab"
            storage_size = 10
            depends = ["gtk3", "sdl2", "zmusic"]
        "#;
        let info = PackageInfo::from_toml(text).unwrap();
        assert_eq!(info.name, name("gzdoom"));
        assert_eq!(info.depends, vec![name("gtk3"), name("sdl2"), name("zmusic")]);
        assert_eq!(info.storage_size, 10);
        assert_eq!(PackageInfo::from_toml(&info.to_toml().unwrap()).unwrap(), info);
    }

    #[test]
    fn package_info_defaults() {
        let info = PackageInfo::from_toml("name = \"dev-essentials\"\ntarget = \"t\"").unwrap();
        assert!(info.version.is_empty() && info.depends.is_empty());
        let text = info.to_toml().unwrap();
        for omitted in ["version", "blake3", "storage_size", "network_size", "source_identifier"] {
            assert!(!text.contains(omitted), "{omitted} in {text}");
        }
    }

    #[test]
    fn package_info_rejects_bad_references() {
        assert!(PackageInfo::from_toml("name = \"a.b.c\"").is_err());
        assert!(PackageInfo::from_toml("name = \"mgba\"\ndepends = [\"ffmpeg:latest\"]").is_err());
    }

    #[test]
    fn source_identifiers() {
        let mut info = PackageInfo::default();
        let source = SourceIdentifier {
            source_identifier: "s".into(),
            commit_identifier: "c".into(),
            patch_identifier: "p".into(),
            time_identifier: "t".into(),
        };
        info.set_source(source.clone());
        assert_eq!(info.source(), source);
        assert!(source.is_updated("s", "p"));
        assert!(!source.is_updated("s", "q"));
    }

    #[test]
    fn repository_index() {
        let text = r#"
            build_id = "0123"

            [packages]
            foo = "1.0"

            [outdated_packages.gnu-make]
            source_identifier = "1a0e"
            commit_identifier = "da93"
            time_identifier = "2025-12-13T05:33:07Z"
        "#;
        let index = RepositoryIndex::from_toml(text).unwrap();
        assert_eq!(index.packages.get("foo").map(String::as_str), Some("1.0"));
        let outdated = &index.outdated_packages["gnu-make"];
        assert!(outdated.patch_identifier.is_empty());
        assert_eq!(RepositoryIndex::from_toml(&index.to_toml().unwrap()).unwrap(), index);
    }
}
