//! Security advisories: a registry's `advisories/<id>.toml` files and the
//! [RustSec advisory database](https://github.com/rustsec/advisory-db).
//!
//! Both use the RustSec advisory format (the `[advisory]` and `[versions]`
//! tables described in the advisory-db `README.md` and `CONTRIBUTING.md`).
//! A registry advisory is that TOML on its own, with the title and
//! description as fields; a RustSec advisory is a Markdown file whose
//! front matter is the TOML in a fenced `toml` block, followed by a
//! `# Title` heading and the description. Fields this crate does not use
//! (`[affected]`, `cvss`, `categories`, ...) are accepted and ignored, so
//! files written for newer versions of the format still parse.
//!
//! A registry advisory names either a fox package (`ecosystem = "fox"`,
//! the default) or a Rust crate (`ecosystem = "crates.io"`); RustSec
//! advisories always name crates. A version is affected when it matches
//! none of the `patched` and none of the `unaffected` requirements, as in
//! RustSec. Requirements are Cargo's, parsed with `semver`, so a
//! pre-release only matches a requirement that names a pre-release of the
//! same version.

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::layout::ADVISORIES_DIR;

/// Why an advisory could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdvisoryError {
    /// The TOML is not an advisory.
    #[error("invalid advisory")]
    Parse(#[source] Box<toml::de::Error>),
    /// The advisory could not be serialized.
    #[error("cannot serialize advisory")]
    Serialize(#[source] toml::ser::Error),
    /// A RustSec Markdown file has no fenced `toml` front matter.
    #[error("no ```toml front matter")]
    NoFrontMatter,
    /// The id has characters other than ASCII letters, digits and `-`.
    #[error("advisory id {id:?} may only hold letters, digits and '-'")]
    Id {
        /// The id.
        id: String,
    },
    /// A registry advisory is not at the path its id implies.
    #[error("advisory {id} must be at {expected}, not {path}")]
    WrongPath {
        /// The id.
        id: String,
        /// Where it should be.
        expected: String,
        /// Where it is.
        path: String,
    },
}

/// What an advisory is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Ecosystem {
    /// A fox package, matched against installed packages.
    #[default]
    #[serde(rename = "fox")]
    Fox,
    /// A Rust crate, matched against the crates in package SBOMs.
    #[serde(rename = "crates.io")]
    CratesIo,
}

impl Ecosystem {
    fn is_fox(&self) -> bool {
        *self == Self::Fox
    }
}

/// The `[advisory]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AdvisoryInfo {
    /// Its id, such as `FOX-2026-0001` or `RUSTSEC-2024-0001`.
    pub id: String,
    /// The affected package or crate.
    pub package: String,
    /// What `package` names.
    #[serde(default, skip_serializing_if = "Ecosystem::is_fox")]
    pub ecosystem: Ecosystem,
    /// When it was published, `YYYY-MM-DD`.
    pub date: String,
    /// A one-line summary. RustSec files carry it as the Markdown heading.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The details. RustSec files carry them as the Markdown body.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Where to read more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Other ids for the same issue, such as CVEs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Set for advisories that report no vulnerability (`unmaintained`,
    /// `unsound`, `notice`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub informational: Option<String>,
    /// The date it was withdrawn, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn: Option<String>,
}

/// The `[versions]` table.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdvisoryVersions {
    /// Versions with the fix.
    #[serde(default)]
    pub patched: Vec<VersionReq>,
    /// Versions that never had the problem.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unaffected: Vec<VersionReq>,
}

/// A security advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisory {
    /// What it is about.
    pub advisory: AdvisoryInfo,
    /// Which versions it affects.
    #[serde(default)]
    pub versions: AdvisoryVersions,
}

impl Advisory {
    /// Parses a registry advisory from `advisories/<id>.toml`, checking
    /// that `path` is where its id says it belongs.
    ///
    /// # Errors
    ///
    /// [`AdvisoryError::Parse`] if the TOML is not an advisory,
    /// [`AdvisoryError::Id`] if the id is not usable as a file name, and
    /// [`AdvisoryError::WrongPath`] if it is not at `path`.
    pub fn from_toml(text: &str, path: &str) -> Result<Self, AdvisoryError> {
        let advisory = Self::parse(text)?;
        let expected = advisory_path(&advisory.advisory.id)?;
        if expected != path {
            return Err(AdvisoryError::WrongPath {
                id: advisory.advisory.id,
                expected,
                path: path.to_owned(),
            });
        }
        Ok(advisory)
    }

    /// Parses a RustSec advisory: Markdown with the TOML in a fenced
    /// `toml` block, then a `# Title` heading and the description, which
    /// fill [`AdvisoryInfo::title`] and [`AdvisoryInfo::description`] when
    /// the TOML has none. The ecosystem is always crates.io.
    ///
    /// # Errors
    ///
    /// [`AdvisoryError::NoFrontMatter`] without the fenced block, and as
    /// [`Advisory::from_toml`] for its contents.
    pub fn from_rustsec_markdown(text: &str) -> Result<Self, AdvisoryError> {
        let start = text.find("```toml").ok_or(AdvisoryError::NoFrontMatter)?;
        let rest = &text[start + "```toml".len()..];
        let end = rest.find("\n```").ok_or(AdvisoryError::NoFrontMatter)?;
        let mut advisory = Self::parse(&rest[..end])?;
        advisory.advisory.ecosystem = Ecosystem::CratesIo;
        let body = rest[end + "\n```".len()..].trim();
        if let Some(heading) = body.strip_prefix("# ") {
            let (title, description) = heading.split_once('\n').unwrap_or((heading, ""));
            if advisory.advisory.title.is_empty() {
                title.trim().clone_into(&mut advisory.advisory.title);
            }
            if advisory.advisory.description.is_empty() {
                description
                    .trim()
                    .clone_into(&mut advisory.advisory.description);
            }
        }
        Ok(advisory)
    }

    /// Parses advisory TOML without checking where it is stored, as for an
    /// advisory about to be added to a registry.
    ///
    /// # Errors
    ///
    /// [`AdvisoryError::Parse`] if the TOML is not an advisory, and
    /// [`AdvisoryError::Id`] if the id is not usable as a file name.
    pub fn parse(text: &str) -> Result<Self, AdvisoryError> {
        let advisory: Self =
            toml::from_str(text).map_err(|error| AdvisoryError::Parse(Box::new(error)))?;
        check_id(&advisory.advisory.id)?;
        Ok(advisory)
    }

    /// Writes the advisory as registry TOML.
    ///
    /// # Errors
    ///
    /// [`AdvisoryError::Serialize`] if it cannot be serialized.
    pub fn to_toml(&self) -> Result<String, AdvisoryError> {
        toml::to_string(self).map_err(AdvisoryError::Serialize)
    }

    /// Whether it reports a vulnerability that still stands: it is neither
    /// withdrawn nor informational.
    pub fn is_active(&self) -> bool {
        self.advisory.withdrawn.is_none() && self.advisory.informational.is_none()
    }

    /// Whether `version` of the package is affected: it matches none of the
    /// patched and unaffected requirements.
    pub fn affects(&self, version: &Version) -> bool {
        let versions = &self.versions;
        !versions
            .patched
            .iter()
            .chain(&versions.unaffected)
            .any(|req| req.matches(version))
    }
}

/// Where the registry advisory `id` lives: `advisories/<id>.toml`.
///
/// # Errors
///
/// [`AdvisoryError::Id`] if `id` holds anything but ASCII letters, digits
/// and `-`.
pub fn advisory_path(id: &str) -> Result<String, AdvisoryError> {
    check_id(id)?;
    Ok(format!("{ADVISORIES_DIR}/{id}.toml"))
}

fn check_id(id: &str) -> Result<(), AdvisoryError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(AdvisoryError::Id { id: id.to_owned() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = r#"[advisory]
id = "FOX-2026-0001"
package = "hello"
date = "2026-10-01"
title = "hello prints secrets"
aliases = ["CVE-2026-0001"]

[versions]
patched = [">= 1.2.1"]
unaffected = ["< 1.0.0"]
"#;

    const RUSTSEC: &str = r#"```toml
[advisory]
id = "RUSTSEC-2026-0007"
package = "smallcrate"
date = "2026-02-03"
url = "https://example.com/smallcrate/issues/1"
categories = ["memory-corruption"]
license = "CC0-1.0"

[affected]
functions = { "smallcrate::f" = ["< 0.3.2"] }

[versions]
patched = [">= 0.3.2", "^0.2.9"]
```

# Out-of-bounds write in `f`

`f` writes past the end of its buffer
when given an empty slice.
"#;

    #[test]
    fn registry_advisories_parse_and_match() {
        let path = "advisories/FOX-2026-0001.toml";
        let advisory = Advisory::from_toml(REGISTRY, path).unwrap();
        assert_eq!(advisory.advisory.ecosystem, Ecosystem::Fox);
        assert!(advisory.is_active());
        for (version, affected) in [
            ("0.9.0", false),
            ("1.0.0", true),
            ("1.2.0", true),
            ("1.2.1", false),
        ] {
            assert_eq!(
                advisory.affects(&version.parse().unwrap()),
                affected,
                "{version}"
            );
        }
        assert_eq!(
            Advisory::from_toml(&advisory.to_toml().unwrap(), path).unwrap(),
            advisory
        );
        assert!(matches!(
            Advisory::from_toml(REGISTRY, "advisories/other.toml"),
            Err(AdvisoryError::WrongPath { .. })
        ));
        let bad_id = REGISTRY.replace("FOX-2026-0001", "../x");
        assert!(matches!(
            Advisory::from_toml(&bad_id, path),
            Err(AdvisoryError::Id { .. })
        ));
        let withdrawn = REGISTRY.replace("title", "withdrawn = \"2026-10-02\"\ntitle");
        assert!(!Advisory::from_toml(&withdrawn, path).unwrap().is_active());
    }

    #[test]
    fn rustsec_markdown_parses() {
        let advisory = Advisory::from_rustsec_markdown(RUSTSEC).unwrap();
        assert_eq!(advisory.advisory.ecosystem, Ecosystem::CratesIo);
        assert_eq!(advisory.advisory.title, "Out-of-bounds write in `f`");
        assert!(advisory.advisory.description.starts_with("`f` writes"));
        assert!(advisory.affects(&Version::new(0, 3, 1)));
        assert!(advisory.affects(&Version::new(0, 2, 8)));
        assert!(!advisory.affects(&Version::new(0, 2, 9)));
        assert!(!advisory.affects(&Version::new(0, 4, 0)));
        assert!(matches!(
            Advisory::from_rustsec_markdown("# no front matter"),
            Err(AdvisoryError::NoFrontMatter)
        ));
    }
}
