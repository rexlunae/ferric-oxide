//! Git repository URLs in the normalized form `fox` uses as a source's
//! identity.
//!
//! The same repository is often written several ways: with or without a
//! trailing `.git` or `/`, with the host in a different case. Trust pins
//! ([`crate::trust`]), fetch caches and attestation checks all compare
//! [`GitUrl`]s, so each of those spellings maps to one string:
//!
//! - the scheme and host are lowercased;
//! - one trailing `/` and then one trailing `.git` are removed from the path;
//! - scp-like `user@host:path` URLs keep that form, with the host
//!   lowercased.
//!
//! Only `https`, `http`, `ssh`, `git` and `file` URLs and the scp-like form
//! are accepted. URLs that carry a password (`user:secret@host`) are
//! refused, so a secret is never written to a trust file or an attestation.

use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Schemes `fox` fetches from.
const SCHEMES: [&str; 5] = ["https", "http", "ssh", "git", "file"];

/// Why a string is not an accepted git URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GitUrlError {
    /// The URL is empty or contains whitespace or control characters.
    #[error("git URL is empty or contains whitespace or control characters")]
    Malformed,
    /// The scheme is not one `fox` fetches from.
    #[error("unsupported git URL scheme {scheme:?}")]
    Scheme {
        /// The scheme, lowercased.
        scheme: String,
    },
    /// The URL has no host (or, for `file://`, no path).
    #[error("git URL has no host or path")]
    MissingPart,
    /// The URL carries a password.
    #[error("git URL carries a password")]
    Password,
}

/// A normalized git repository URL.
///
/// # Examples
///
/// ```
/// use fox_core::GitUrl;
///
/// let a: GitUrl = "https://GitHub.com/rexlunae/hello.git".parse()?;
/// let b: GitUrl = "https://github.com/rexlunae/hello/".parse()?;
/// assert_eq!(a, b);
/// assert_eq!(a.as_str(), "https://github.com/rexlunae/hello");
/// # Ok::<(), fox_core::GitUrlError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitUrl(String);

impl GitUrl {
    /// Normalizes `url`.
    ///
    /// # Errors
    ///
    /// [`GitUrlError`] if `url` is not an accepted git URL; see its
    /// variants.
    pub fn new(url: &str) -> Result<Self, GitUrlError> {
        if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(GitUrlError::Malformed);
        }
        match url.split_once("://") {
            Some((scheme, rest)) => normalize_scheme_url(scheme, rest),
            None => normalize_scp(url),
        }
        .map(Self)
    }

    /// The normalized URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is a `file://` URL.
    pub fn is_local(&self) -> bool {
        self.0.starts_with("file://")
    }
}

fn normalize_scheme_url(scheme: &str, rest: &str) -> Result<String, GitUrlError> {
    let scheme = scheme.to_ascii_lowercase();
    if !SCHEMES.contains(&scheme.as_str()) {
        return Err(GitUrlError::Scheme { scheme });
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    let authority = normalize_authority(authority)?;
    let path = trim_path(path);
    let missing = if scheme == "file" {
        path.is_empty()
    } else {
        authority.is_empty()
    };
    if missing {
        return Err(GitUrlError::MissingPart);
    }
    Ok(alloc::format!("{scheme}://{authority}{path}"))
}

fn normalize_scp(url: &str) -> Result<String, GitUrlError> {
    let (authority, path) = url.split_once(':').ok_or(GitUrlError::MissingPart)?;
    // `host:path` with a `/` before the colon is a local path, not scp form.
    if authority.contains('/') {
        return Err(GitUrlError::MissingPart);
    }
    let authority = normalize_authority(authority)?;
    let path = trim_path(path);
    if authority.is_empty() || path.is_empty() {
        return Err(GitUrlError::MissingPart);
    }
    Ok(alloc::format!("{authority}:{path}"))
}

/// Lowercases the host and refuses a password in the user part.
fn normalize_authority(authority: &str) -> Result<String, GitUrlError> {
    let (user, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    let host = host.to_ascii_lowercase();
    match user {
        Some(user) if user.contains(':') => Err(GitUrlError::Password),
        Some(user) => Ok(alloc::format!("{user}@{host}")),
        None => Ok(host),
    }
}

fn trim_path(path: &str) -> &str {
    let path = path.strip_suffix('/').unwrap_or(path);
    path.strip_suffix(".git").unwrap_or(path)
}

impl fmt::Display for GitUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GitUrl {
    type Err = GitUrlError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl Serialize for GitUrl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GitUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::new(&text).map_err(serde::de::Error::custom)
    }
}

impl AsRef<str> for GitUrl {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<GitUrl> for String {
    fn from(url: GitUrl) -> Self {
        url.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(url: &str) -> Result<String, GitUrlError> {
        GitUrl::new(url).map(String::from)
    }

    #[test]
    fn spellings_of_one_repository_agree() {
        for url in [
            "https://github.com/a/b",
            "https://github.com/a/b.git",
            "https://github.com/a/b/",
            "https://github.com/a/b.git/",
            "HTTPS://GitHub.COM/a/b",
        ] {
            assert_eq!(norm(url).unwrap(), "https://github.com/a/b", "{url}");
        }
        // The path keeps its case: servers may treat it as significant.
        assert_eq!(norm("https://h/A/B").unwrap(), "https://h/A/B");
    }

    #[test]
    fn scp_and_file_forms() {
        assert_eq!(
            norm("git@GitHub.com:a/b.git").unwrap(),
            "git@github.com:a/b"
        );
        assert_eq!(norm("file:///srv/git/x.git").unwrap(), "file:///srv/git/x");
        assert_eq!(
            norm("ssh://git@Host:2222/a/b").unwrap(),
            "ssh://git@host:2222/a/b"
        );
        assert!(GitUrl::new("file:///srv/x").unwrap().is_local());
    }

    #[test]
    fn refusals() {
        assert_eq!(norm(""), Err(GitUrlError::Malformed));
        assert_eq!(norm("https://h/a b"), Err(GitUrlError::Malformed));
        assert_eq!(
            norm("ftp://h/a"),
            Err(GitUrlError::Scheme {
                scheme: "ftp".into()
            })
        );
        assert_eq!(norm("https:///a"), Err(GitUrlError::MissingPart));
        assert_eq!(norm("file://"), Err(GitUrlError::MissingPart));
        assert_eq!(norm("/local/path"), Err(GitUrlError::MissingPart));
        assert_eq!(norm("./a:b"), Err(GitUrlError::MissingPart));
        assert_eq!(norm("https://u:pw@h/a"), Err(GitUrlError::Password));
        assert_eq!(norm("https://u@h/a").unwrap(), "https://u@h/a");
    }

    #[test]
    fn serde_round_trip_normalizes() {
        #[derive(Deserialize, Serialize)]
        struct T {
            git: GitUrl,
        }
        let t: T = toml::from_str("git = \"https://H/a.git\"").unwrap();
        assert_eq!(t.git.as_str(), "https://h/a");
        assert!(toml::from_str::<T>("git = \"ftp://h/a\"").is_err());
    }
}
