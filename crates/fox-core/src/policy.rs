//! Install policy: [`POLICY_FILE`] under the install root.
//!
//! ```toml
//! build = "auto"                # never | auto | always
//! min-builder-signatures = 1    # registry artifacts need this many builders
//! allow-unsigned-git = false    # build unsigned tags and commits
//! allow-yanked = false          # pick yanked releases when resolving names
//! require-fresh-metadata = "24h"
//! ```
//!
//! Every key is optional and a missing file is the defaults above, which
//! are the ones `packaging-design.md` section 7.3 picked. `build = "auto"`
//! installs a registry's prebuilt artifact when enough of its builders
//! attest to it and builds from source otherwise; `never` refuses to build;
//! `always` ignores prebuilt artifacts. A registry's own
//! `min-builder-signatures` applies when it is higher than the policy's.
//! `require-fresh-metadata` is how old a registry checkout may be before
//! installing from it updates it first: a number followed by `m`, `h` or
//! `d`.

use core::fmt;
use core::str::FromStr;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{ConfigError, load_toml};

/// The policy file, relative to the install root.
pub const POLICY_FILE: &str = "etc/fox/policy.toml";

/// When `fox` builds from source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BuildPolicy {
    /// Never build; install only attested prebuilt artifacts.
    Never,
    /// Build when no prebuilt artifact is trusted.
    #[default]
    Auto,
    /// Always build, ignoring prebuilt artifacts.
    Always,
}

/// A maximum metadata age, written `30m`, `24h` or `7d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxAge(Duration);

impl MaxAge {
    /// The age as a duration.
    pub fn duration(self) -> Duration {
        self.0
    }
}

impl Default for MaxAge {
    fn default() -> Self {
        Self(Duration::from_secs(24 * 3600))
    }
}

/// A maximum age that is not a number followed by `m`, `h` or `d`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not an age like 30m, 24h or 7d")]
pub struct MaxAgeError(String);

impl FromStr for MaxAge {
    type Err = MaxAgeError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let error = || MaxAgeError(text.to_owned());
        let unit = match text.chars().last() {
            Some('m') => 60,
            Some('h') => 3600,
            Some('d') => 86_400,
            _ => return Err(error()),
        };
        let count: u64 = text[..text.len() - 1].parse().map_err(|_| error())?;
        count
            .checked_mul(unit)
            .map(|secs| Self(Duration::from_secs(secs)))
            .ok_or_else(error)
    }
}

impl fmt::Display for MaxAge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        if secs.is_multiple_of(86_400) {
            write!(f, "{}d", secs / 86_400)
        } else if secs.is_multiple_of(3600) {
            write!(f, "{}h", secs / 3600)
        } else {
            write!(f, "{}m", secs / 60)
        }
    }
}

impl Serialize for MaxAge {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MaxAge {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// The install policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Policy {
    /// When to build from source.
    pub build: BuildPolicy,
    /// Builders that must attest to a registry artifact.
    pub min_builder_signatures: u32,
    /// Whether unsigned tags and commits may be built.
    pub allow_unsigned_git: bool,
    /// Whether resolving a name may pick a yanked release.
    pub allow_yanked: bool,
    /// How old registry metadata may be before an install refreshes it.
    pub require_fresh_metadata: MaxAge,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            build: BuildPolicy::Auto,
            min_builder_signatures: 1,
            allow_unsigned_git: false,
            allow_yanked: false,
            require_fresh_metadata: MaxAge::default(),
        }
    }
}

impl Policy {
    /// Reads `<root>/etc/fox/policy.toml`; a missing file is the defaults.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] if the file exists but cannot be read or is invalid.
    pub fn load(root: &Path) -> Result<Self, ConfigError> {
        load_toml(&root.join(POLICY_FILE))
    }

    /// The builder threshold for a registry that asks for
    /// `registry_minimum`: the higher of the two, and at least 1.
    pub fn builder_threshold(&self, registry_minimum: u32) -> u32 {
        self.min_builder_signatures.max(registry_minimum).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_ages() {
        let policy: Policy = toml::from_str("").unwrap();
        assert_eq!(policy, Policy::default());
        let policy: Policy =
            toml::from_str("build = \"never\"\nrequire-fresh-metadata = \"30m\"\n").unwrap();
        assert_eq!(policy.build, BuildPolicy::Never);
        assert_eq!(policy.require_fresh_metadata.duration().as_secs(), 1800);
        assert_eq!(MaxAge::default().to_string(), "1d");
        for bad in ["", "h", "5", "5s", "-1h", "99999999999999999999d"] {
            assert!(bad.parse::<MaxAge>().is_err(), "{bad}");
        }
        assert!(toml::from_str::<Policy>("other = 1").is_err());
        assert_eq!(policy.builder_threshold(2), 2);
        assert_eq!(Policy::default().builder_threshold(0), 1);
    }
}
