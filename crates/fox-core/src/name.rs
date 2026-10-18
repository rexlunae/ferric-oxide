//! Package names.

use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

/// The longest package name accepted, in bytes.
const MAX_LEN: usize = 128;

/// A package name, such as `ripgrep` or `sdl2.dev`.
///
/// Names become file names (the manifest path inside an archive, the head
/// file in the installed-package database), so they are restricted to ASCII
/// letters, digits and `-`, `_`, `+`, `.`; they start with a letter or digit
/// and contain at most one `.`, which separates an optional sub-package the
/// way cookbook package names do (`gcc13.dev`). Cookbook `host:` and
/// `target:` prefixes are build-time selectors, not installed names, and are
/// rejected.
///
/// # Examples
///
/// ```
/// use fox_core::PackageName;
///
/// assert!(PackageName::new("sdl2.dev").is_ok());
/// assert!(PackageName::new("../etc").is_err());
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackageName(String);

/// Why a string is not a valid [`PackageName`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NameError {
    /// The name is empty.
    #[error("package name is empty")]
    Empty,
    /// The name is longer than 128 bytes.
    #[error("package name is {len} bytes long, more than 128")]
    TooLong {
        /// Length of the rejected name in bytes.
        len: usize,
    },
    /// The name does not start with an ASCII letter or digit.
    #[error("package name must start with a letter or digit")]
    BadStart,
    /// The name contains a character outside `A-Za-z0-9-_+.`.
    #[error("package name contains {0:?}; only letters, digits and - _ + . are allowed")]
    BadChar(char),
    /// The name contains more than one `.`.
    #[error("package name contains more than one '.'")]
    TooManyDots,
}

impl PackageName {
    /// Checks and wraps a name.
    ///
    /// # Errors
    ///
    /// Returns a [`NameError`] saying which rule the name breaks.
    pub fn new(name: impl Into<String>) -> Result<Self, NameError> {
        let name = name.into();
        let first = name.chars().next().ok_or(NameError::Empty)?;
        if name.len() > MAX_LEN {
            return Err(NameError::TooLong { len: name.len() });
        }
        if !first.is_ascii_alphanumeric() {
            return Err(NameError::BadStart);
        }
        if let Some(bad) = name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.')))
        {
            return Err(NameError::BadChar(bad));
        }
        if name.matches('.').count() > 1 {
            return Err(NameError::TooManyDots);
        }
        Ok(Self(name))
    }

    /// The name as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PackageName {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for PackageName {
    type Error = NameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PackageName> for String {
    fn from(name: PackageName) -> Self {
        name.0
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl AsRef<str> for PackageName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_cookbook_style_names() {
        for name in [
            "ripgrep",
            "gcc13",
            "sdl2.dev",
            "libstdcxx-v3",
            "c++filt",
            "a_b",
        ] {
            assert!(PackageName::new(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn rejects_unsafe_names() {
        assert_eq!(PackageName::new(""), Err(NameError::Empty));
        assert_eq!(PackageName::new(".hidden"), Err(NameError::BadStart));
        assert_eq!(PackageName::new("-flag"), Err(NameError::BadStart));
        assert_eq!(PackageName::new("a/b"), Err(NameError::BadChar('/')));
        assert_eq!(PackageName::new("host:gcc"), Err(NameError::BadChar(':')));
        assert_eq!(PackageName::new("a.b.c"), Err(NameError::TooManyDots));
        assert!(matches!(
            PackageName::new("a".repeat(129)),
            Err(NameError::TooLong { len: 129 })
        ));
    }
}
