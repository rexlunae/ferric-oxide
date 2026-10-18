//! The trust store: which authors sign which git repositories, and which
//! third-party builders' prebuilt archives are accepted.
//!
//! The store is one TOML file, [`TRUST_FILE`] under the install root,
//! format `fox-trust/1`:
//!
//! ```toml
//! format = "fox-trust/1"
//!
//! [[author]]
//! git = "https://github.com/example/hello"
//! key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI... alice@example.com"
//!
//! [[builder]]
//! label = "example-ci"
//! key = "<64 hex digits>"
//! ```
//!
//! An `[[author]]` entry pins the OpenSSH Ed25519 key that signs a
//! repository's release tags. `fox install <git-url>` adds one the first time
//! it sees a repository, once the user accepts the key (trust on first use);
//! afterwards a tag signed by any other key is refused until the user removes
//! the pin. Keys are compared without their comments.
//!
//! A `[[builder]]` entry trusts a builder's pkgar key for prebuilt archives
//! and their attestations. There are none by default, so `fox` accepts
//! third-party prebuilts only after the user adds a builder explicitly.
//! First-party signer keys stay in [`crate::keys::TRUSTED_KEYS_DIR`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ssh_key::{Algorithm, HashAlg, PublicKey};

use crate::{GitUrl, KeyId, TrustedKeys};

/// The trust file, relative to the install root.
pub const TRUST_FILE: &str = "etc/fox/trust.toml";

/// Largest trust file read.
const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// The trust file format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TrustFormat {
    /// The first format, written `"fox-trust/1"`.
    #[default]
    #[serde(rename = "fox-trust/1")]
    V1,
}

/// Why the trust store could not be read, written or changed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TrustError {
    /// The file could not be read or written.
    #[error("cannot access trust file {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is larger than 1 MiB.
    #[error("trust file {} is larger than 1 MiB", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The file is not a valid `fox-trust/1` file.
    #[error("invalid trust file {}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// The store could not be serialized.
    #[error("cannot serialize the trust store")]
    Serialize(#[source] toml::ser::Error),
    /// The repository is already pinned to a different author key.
    #[error("{git} is pinned to author key {pinned}, not {offered}")]
    AuthorKeyChanged {
        /// The repository.
        git: GitUrl,
        /// Fingerprint of the pinned key.
        pinned: String,
        /// Fingerprint of the other key.
        offered: String,
    },
    /// A builder label is already used for a different key.
    #[error("builder label {label:?} is already used for key {pinned}")]
    BuilderLabelTaken {
        /// The label.
        label: String,
        /// The key it names.
        pinned: KeyId,
    },
}

/// An author signing key: an OpenSSH Ed25519 public key.
///
/// Only Ed25519 keys are accepted, as that is the only signature algorithm
/// `fox` verifies.
#[derive(Debug, Clone)]
pub struct AuthorKey(PublicKey);

/// Why a string is not an accepted author key.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthorKeyError {
    /// The text is not an OpenSSH public key.
    #[error("not an OpenSSH public key")]
    Parse(#[source] ssh_key::Error),
    /// The key is not Ed25519.
    #[error("author keys must be ssh-ed25519, not {algorithm}")]
    Algorithm {
        /// The key's algorithm.
        algorithm: String,
    },
}

impl AuthorKey {
    /// Parses an OpenSSH public key line (`ssh-ed25519 AAAA… comment`).
    ///
    /// # Errors
    ///
    /// [`AuthorKeyError::Parse`] if it is not a public key, and
    /// [`AuthorKeyError::Algorithm`] if it is not Ed25519.
    pub fn from_openssh(text: &str) -> Result<Self, AuthorKeyError> {
        let key = PublicKey::from_openssh(text.trim()).map_err(AuthorKeyError::Parse)?;
        Self::new(key)
    }

    /// Wraps a public key.
    ///
    /// # Errors
    ///
    /// [`AuthorKeyError::Algorithm`] if it is not Ed25519.
    pub fn new(key: PublicKey) -> Result<Self, AuthorKeyError> {
        if key.algorithm() != Algorithm::Ed25519 {
            return Err(AuthorKeyError::Algorithm {
                algorithm: key.algorithm().to_string(),
            });
        }
        Ok(Self(key))
    }

    /// The key.
    pub fn public_key(&self) -> &PublicKey {
        &self.0
    }

    /// The `SHA256:…` fingerprint `ssh-keygen -l` shows.
    pub fn fingerprint(&self) -> String {
        self.0.fingerprint(HashAlg::Sha256).to_string()
    }

    /// Whether `other` is the same key, ignoring comments.
    pub fn same_key(&self, other: &PublicKey) -> bool {
        self.0.key_data() == other.key_data()
    }
}

impl PartialEq for AuthorKey {
    fn eq(&self, other: &Self) -> bool {
        self.same_key(&other.0)
    }
}

impl Eq for AuthorKey {}

impl Serialize for AuthorKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = self.0.to_openssh().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for AuthorKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_openssh(&text).map_err(serde::de::Error::custom)
    }
}

/// A pinned repository author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorEntry {
    /// The repository.
    pub git: GitUrl,
    /// The key that signs its release tags.
    pub key: AuthorKey,
}

/// A trusted third-party builder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderEntry {
    /// The builder's name, as attestations give it.
    pub label: String,
    /// Its pkgar signing key.
    pub key: KeyId,
}

/// What the store says about a repository and a tag's signing key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthorTrust {
    /// The repository is pinned to this key.
    Pinned,
    /// The repository has no pin yet.
    Unknown,
    /// The repository is pinned to a different key.
    Changed {
        /// The pinned key.
        pinned: AuthorKey,
    },
}

/// The trust store.
///
/// # Examples
///
/// ```
/// use fox_core::trust::{AuthorKey, AuthorTrust, TrustStore};
///
/// let key = AuthorKey::from_openssh(
///     "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti",
/// )?;
/// let git = "https://example.com/hello.git".parse()?;
/// let mut store = TrustStore::default();
/// assert_eq!(store.check_author(&git, &key), AuthorTrust::Unknown);
/// store.pin_author(git.clone(), key.clone())?;
/// assert_eq!(store.check_author(&git, &key), AuthorTrust::Pinned);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustStore {
    format: TrustFormat,
    #[serde(default, rename = "author", skip_serializing_if = "Vec::is_empty")]
    authors: Vec<AuthorEntry>,
    #[serde(default, rename = "builder", skip_serializing_if = "Vec::is_empty")]
    builders: Vec<BuilderEntry>,
}

impl TrustStore {
    /// Reads `<root>/etc/fox/trust.toml`; a missing file is an empty store.
    ///
    /// # Errors
    ///
    /// [`TrustError::Io`], [`TrustError::TooLarge`] or [`TrustError::Parse`]
    /// if the file exists but cannot be read or is invalid.
    pub fn load(root: &Path) -> Result<Self, TrustError> {
        let path = root.join(TRUST_FILE);
        let io_error = |source| TrustError::Io {
            path: path.clone(),
            source,
        };
        let size = match fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(io_error(error)),
        };
        if size > MAX_FILE_SIZE {
            return Err(TrustError::TooLarge { path });
        }
        let text = fs::read_to_string(&path).map_err(io_error)?;
        toml::from_str(&text).map_err(|source| TrustError::Parse { path, source })
    }

    /// Writes the store to `<root>/etc/fox/trust.toml`, replacing the file
    /// atomically.
    ///
    /// # Errors
    ///
    /// [`TrustError::Serialize`] or [`TrustError::Io`].
    pub fn save(&self, root: &Path) -> Result<(), TrustError> {
        let path = root.join(TRUST_FILE);
        let text = toml::to_string(self).map_err(TrustError::Serialize)?;
        let io_error = |source| TrustError::Io {
            path: path.clone(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io_error)?;
        }
        let temp = path.with_extension("toml.new");
        let mut file = fs::File::create(&temp).map_err(io_error)?;
        file.write_all(text.as_bytes()).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        fs::rename(&temp, &path).map_err(io_error)
    }

    /// The pinned authors.
    pub fn authors(&self) -> &[AuthorEntry] {
        &self.authors
    }

    /// The trusted builders.
    pub fn builders(&self) -> &[BuilderEntry] {
        &self.builders
    }

    /// The trusted builders as a key set.
    pub fn builder_keys(&self) -> TrustedKeys {
        let mut keys = TrustedKeys::new();
        for builder in &self.builders {
            keys.insert(builder.label.clone(), builder.key);
        }
        keys
    }

    /// The author pinned for `git`.
    pub fn author(&self, git: &GitUrl) -> Option<&AuthorEntry> {
        self.authors.iter().find(|entry| &entry.git == git)
    }

    /// Whether `key` is the author key pinned for `git`.
    pub fn check_author(&self, git: &GitUrl, key: &AuthorKey) -> AuthorTrust {
        match self.author(git) {
            None => AuthorTrust::Unknown,
            Some(entry) if &entry.key == key => AuthorTrust::Pinned,
            Some(entry) => AuthorTrust::Changed {
                pinned: entry.key.clone(),
            },
        }
    }

    /// Pins `git` to `key`. Pinning the key already pinned does nothing.
    ///
    /// # Errors
    ///
    /// [`TrustError::AuthorKeyChanged`] if `git` is pinned to another key;
    /// remove that pin first.
    pub fn pin_author(&mut self, git: GitUrl, key: AuthorKey) -> Result<(), TrustError> {
        match self.check_author(&git, &key) {
            AuthorTrust::Pinned => Ok(()),
            AuthorTrust::Changed { pinned } => Err(TrustError::AuthorKeyChanged {
                git,
                pinned: pinned.fingerprint(),
                offered: key.fingerprint(),
            }),
            AuthorTrust::Unknown => {
                self.authors.push(AuthorEntry { git, key });
                self.authors.sort_by(|a, b| a.git.cmp(&b.git));
                Ok(())
            }
        }
    }

    /// Removes the pin for `git`, returning whether there was one.
    pub fn remove_author(&mut self, git: &GitUrl) -> bool {
        let before = self.authors.len();
        self.authors.retain(|entry| &entry.git != git);
        self.authors.len() != before
    }

    /// Trusts `key` as builder `label`. Adding the same pair again does
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`TrustError::BuilderLabelTaken`] if `label` names another key.
    pub fn add_builder(&mut self, label: &str, key: KeyId) -> Result<(), TrustError> {
        match self.builders.iter().find(|entry| entry.label == label) {
            Some(entry) if entry.key == key => Ok(()),
            Some(entry) => Err(TrustError::BuilderLabelTaken {
                label: label.to_owned(),
                pinned: entry.key,
            }),
            None => {
                self.builders.push(BuilderEntry {
                    label: label.to_owned(),
                    key,
                });
                self.builders.sort_by(|a, b| a.label.cmp(&b.label));
                Ok(())
            }
        }
    }

    /// Removes builder `label`, returning whether there was one.
    pub fn remove_builder(&mut self, label: &str) -> bool {
        let before = self.builders.len();
        self.builders.retain(|entry| entry.label != label);
        self.builders.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti a@x";

    fn key_b() -> AuthorKey {
        let private =
            ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]));
        AuthorKey::new(private.public_key().clone()).unwrap()
    }

    fn url(text: &str) -> GitUrl {
        text.parse().unwrap()
    }

    #[test]
    fn missing_file_is_empty_and_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let mut store = TrustStore::load(root.path()).unwrap();
        assert_eq!(store, TrustStore::default());

        store
            .pin_author(
                url("https://h/a.git"),
                AuthorKey::from_openssh(KEY_A).unwrap(),
            )
            .unwrap();
        store.add_builder("ci", KeyId::from_bytes([1; 32])).unwrap();
        store.save(root.path()).unwrap();
        let text = fs::read_to_string(root.path().join(TRUST_FILE)).unwrap();
        assert!(text.starts_with("format = \"fox-trust/1\""), "{text}");
        assert!(text.contains("git = \"https://h/a\""), "{text}");
        assert_eq!(TrustStore::load(root.path()).unwrap(), store);
        assert_eq!(store.builder_keys().len(), 1);
    }

    #[test]
    fn pins_refuse_a_changed_key() {
        let mut store = TrustStore::default();
        let a = AuthorKey::from_openssh(KEY_A).unwrap();
        let git = url("https://h/a");
        store.pin_author(git.clone(), a.clone()).unwrap();
        // The comment is not part of the key.
        let same = AuthorKey::from_openssh(KEY_A.trim_end_matches(" a@x")).unwrap();
        assert_eq!(store.check_author(&git, &same), AuthorTrust::Pinned);
        store.pin_author(git.clone(), same).unwrap();

        assert_eq!(
            store.check_author(&git, &key_b()),
            AuthorTrust::Changed { pinned: a }
        );
        assert!(matches!(
            store.pin_author(git.clone(), key_b()),
            Err(TrustError::AuthorKeyChanged { .. })
        ));
        assert!(store.remove_author(&git));
        assert!(!store.remove_author(&git));
        store.pin_author(git.clone(), key_b()).unwrap();
        assert_eq!(store.authors().len(), 1);
    }

    #[test]
    fn builder_labels_are_unique() {
        let mut store = TrustStore::default();
        store.add_builder("ci", KeyId::from_bytes([1; 32])).unwrap();
        store.add_builder("ci", KeyId::from_bytes([1; 32])).unwrap();
        assert!(matches!(
            store.add_builder("ci", KeyId::from_bytes([2; 32])),
            Err(TrustError::BuilderLabelTaken { .. })
        ));
        assert!(store.remove_builder("ci"));
        assert!(store.builders().is_empty());
    }

    #[test]
    fn invalid_files_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(TRUST_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for text in [
            "format = \"fox-trust/2\"",
            "format = \"fox-trust/1\"\nextra = 1",
            "format = \"fox-trust/1\"\n[[author]]\ngit = \"https://h/a\"\nkey = \"ssh-rsa AAAA\"",
        ] {
            fs::write(&path, text).unwrap();
            assert!(
                matches!(TrustStore::load(root.path()), Err(TrustError::Parse { .. })),
                "{text}"
            );
        }
    }

    #[test]
    fn only_ed25519_author_keys() {
        assert!(matches!(
            AuthorKey::from_openssh("not a key"),
            Err(AuthorKeyError::Parse(_))
        ));
        assert_eq!(key_b().fingerprint().len(), "SHA256:".len() + 43);
    }
}
