//! The `[package.metadata.fox]` table in `Cargo.toml` (packaging design,
//! section 3.2).
//!
//! ```toml
//! [package.metadata.fox]
//! kind = "app"                          # app | service | library | data | recipe
//! summary = "Recursive regex search"
//! binaries = ["rg"]                     # which [[bin]] targets get installed
//! targets = ["native"]                  # wasm32 | wasm64 | native
//! runtime-deps = []                     # other fox packages needed at run time
//! capabilities = ["file:/home", "tty"]
//!
//! [package.metadata.fox.features]
//! "pcre2" = ["pcre2"]                   # ferric.toml option id -> cargo features
//!
//! [package.metadata.fox.files]
//! "doc/rg.1" = "/usr/share/man/man1/rg.1"
//! ```
//!
//! Every field is optional, and a package without the table builds with the
//! defaults: an `app` that installs every binary target, built natively.
//! Unknown fields are refused, so a misspelt key is not silently ignored.
//! `capabilities` and `features` are read and kept for later phases;
//! nothing enforces or maps them yet.

use std::collections::BTreeMap;

use fox_core::PackageName;
use serde::Deserialize;

/// What kind of package a crate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PackageKind {
    /// A program run by users.
    #[default]
    App,
    /// A daemon started by init.
    Service,
    /// A system library.
    Library,
    /// Data files only.
    Data,
    /// A cookbook recipe (a non-Cargo port). Built by the cookbook, not here.
    Recipe,
}

/// A kind of artifact a package can be built as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TargetKind {
    /// A wasm32 module set.
    Wasm32,
    /// A wasm64 module set.
    Wasm64,
    /// A native binary for a Rust target triple.
    Native,
}

/// The `[package.metadata.fox]` table.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub struct PackageMetadata {
    /// The kind of package.
    #[serde(default)]
    pub kind: PackageKind,
    /// One-line description; the crate's `description` when absent.
    pub summary: Option<String>,
    /// The binary targets to install; every binary target when absent.
    pub binaries: Option<Vec<String>>,
    /// What the package can be built as; `["native"]` when absent.
    pub targets: Option<Vec<TargetKind>>,
    /// Other fox packages needed at run time.
    #[serde(default)]
    pub runtime_deps: Vec<PackageName>,
    /// What the program asks for. Not enforced yet.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// `ferric.toml` option id to the crate's Cargo features. Not mapped
    /// yet; builds take Cargo features directly.
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
    /// Extra files: path in the package directory to install path.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

impl PackageMetadata {
    /// Reads the table from a package's `metadata` value as `cargo
    /// metadata` reports it. A missing `fox` key gives the defaults.
    ///
    /// # Errors
    ///
    /// The deserialization error if the table does not match the schema.
    pub fn from_cargo_metadata(metadata: &serde_json::Value) -> Result<Self, serde_json::Error> {
        match metadata.get("fox") {
            Some(table) => Self::deserialize(table),
            None => Ok(Self::default()),
        }
    }

    /// Whether the package may be built as `kind`.
    pub fn allows(&self, kind: TargetKind) -> bool {
        match &self.targets {
            Some(targets) => targets.contains(&kind),
            None => kind == TargetKind::Native,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_without_a_table() {
        let metadata = PackageMetadata::from_cargo_metadata(&json!(null)).unwrap();
        assert_eq!(metadata, PackageMetadata::default());
        assert!(metadata.allows(TargetKind::Native));
        assert!(!metadata.allows(TargetKind::Wasm32));
    }

    #[test]
    fn reads_the_design_example() {
        let metadata = PackageMetadata::from_cargo_metadata(&json!({
            "fox": {
                "kind": "service",
                "summary": "Recursive regex search",
                "binaries": ["rg"],
                "targets": ["wasm32"],
                "runtime-deps": ["pcre2"],
                "capabilities": ["tty"],
                "features": { "pcre2": ["pcre2"] },
                "files": { "doc/rg.1": "/usr/share/man/man1/rg.1" }
            },
            "other-tool": { "ignored": true }
        }))
        .unwrap();
        assert_eq!(metadata.kind, PackageKind::Service);
        assert_eq!(metadata.binaries.as_deref(), Some(&["rg".to_owned()][..]));
        assert!(!metadata.allows(TargetKind::Native));
        assert_eq!(metadata.runtime_deps[0].as_str(), "pcre2");
        assert_eq!(metadata.files["doc/rg.1"], "/usr/share/man/man1/rg.1");
    }

    #[test]
    fn refuses_unknown_fields_and_bad_values() {
        for bad in [
            json!({ "fox": { "binary": ["rg"] } }),
            json!({ "fox": { "kind": "plugin" } }),
            json!({ "fox": { "runtime-deps": ["../x"] } }),
        ] {
            assert!(PackageMetadata::from_cargo_metadata(&bad).is_err(), "{bad}");
        }
    }
}
