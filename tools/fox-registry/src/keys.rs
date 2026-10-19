//! Loading and generating the keys `fox-registry` signs with.
//!
//! Secret keys are either `pkgar-keys` secret key files (what `init`
//! generates for the registry roles) or OpenSSH Ed25519 private keys (what
//! package owners already sign their tags with). Both must be unencrypted:
//! keep them on offline or encrypted storage and point `--key` at them only
//! while signing.

use std::fs;
use std::path::{Path, PathBuf};

use fox_core::KeyId;
use fox_core::trust::AuthorKey;

/// Why a key could not be loaded or written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum KeyError {
    /// The file cannot be read or written.
    #[error("cannot access key file {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A `pkgar-keys` file is invalid or cannot be written.
    #[error("invalid pkgar key file {}", path.display())]
    Pkgar {
        path: PathBuf,
        #[source]
        source: pkgar_keys::Error,
    },
    /// The `pkgar-keys` secret key is encrypted.
    #[error("{} is encrypted; sign with a decrypted copy", path.display())]
    PkgarEncrypted { path: PathBuf },
    /// An OpenSSH key is invalid.
    #[error("invalid OpenSSH key in {}", path.display())]
    Ssh {
        path: PathBuf,
        #[source]
        source: ssh_key::Error,
    },
    /// The OpenSSH private key has a passphrase.
    #[error("{} has a passphrase; sign with an unencrypted copy", path.display())]
    SshEncrypted { path: PathBuf },
    /// The OpenSSH key is not Ed25519.
    #[error("{} is not an Ed25519 key", path.display())]
    NotEd25519 { path: PathBuf },
    /// A public key argument is none of the accepted forms.
    #[error("{value:?} is not a hex Ed25519 key, an ssh-ed25519 line or a public key file")]
    Unrecognized { value: String },
    /// `init` would overwrite a key.
    #[error("{} already exists", path.display())]
    Exists { path: PathBuf },
}

const OPENSSH_PRIVATE: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";

/// Loads a secret key in NaCl layout (seed then public key).
pub(crate) fn load_secret(path: &Path) -> Result<[u8; 64], KeyError> {
    let text = fs::read_to_string(path).map_err(|source| KeyError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if text.trim_start().starts_with(OPENSSH_PRIVATE) {
        return ssh_secret(path, &text);
    }
    let file = pkgar_keys::SecretKeyFile::from_str(&text).map_err(|source| KeyError::Pkgar {
        path: path.to_path_buf(),
        source,
    })?;
    file.secret_key().ok_or_else(|| KeyError::PkgarEncrypted {
        path: path.to_path_buf(),
    })
}

fn ssh_secret(path: &Path, text: &str) -> Result<[u8; 64], KeyError> {
    let key = ssh_key::PrivateKey::from_openssh(text).map_err(|source| KeyError::Ssh {
        path: path.to_path_buf(),
        source,
    })?;
    if key.is_encrypted() {
        return Err(KeyError::SshEncrypted {
            path: path.to_path_buf(),
        });
    }
    let pair = key
        .key_data()
        .ed25519()
        .ok_or_else(|| KeyError::NotEd25519 {
            path: path.to_path_buf(),
        })?;
    let mut secret = [0u8; 64];
    secret[..32].copy_from_slice(&pair.private.to_bytes());
    secret[32..].copy_from_slice(&pair.public.0);
    Ok(secret)
}

/// Parses a public key given on the command line: 64 hex digits, an
/// `ssh-ed25519` line, or a file holding a `pkgar-keys` public key or an
/// OpenSSH public key.
pub(crate) fn parse_public(value: &str) -> Result<KeyId, KeyError> {
    if let Ok(key) = value.parse::<KeyId>() {
        return Ok(key);
    }
    if let Ok(key) = AuthorKey::from_openssh(value) {
        return Ok(KeyId::from_bytes(key.ed25519_bytes()));
    }
    let path = Path::new(value);
    let text = fs::read_to_string(path).map_err(|_| KeyError::Unrecognized {
        value: value.to_owned(),
    })?;
    if let Ok(key) = AuthorKey::from_openssh(text.trim()) {
        return Ok(KeyId::from_bytes(key.ed25519_bytes()));
    }
    pkgar_keys::PublicKeyFile::from_str(&text)
        .map(|file| KeyId::from_bytes(file.pkey))
        .map_err(|source| KeyError::Pkgar {
            path: path.to_path_buf(),
            source,
        })
}

/// Parses an owner key: an `ssh-ed25519` line or a file holding one.
pub(crate) fn parse_author(value: &str) -> Result<AuthorKey, KeyError> {
    if let Ok(key) = AuthorKey::from_openssh(value) {
        return Ok(key);
    }
    let text = fs::read_to_string(value).map_err(|_| KeyError::Unrecognized {
        value: value.to_owned(),
    })?;
    AuthorKey::from_openssh(text.trim()).map_err(|_| KeyError::Unrecognized {
        value: value.to_owned(),
    })
}

/// Generates an unencrypted `pkgar-keys` pair at `<dir>/<stem>.key` and
/// `<dir>/<stem>.pub`, and returns the secret.
pub(crate) fn generate(dir: &Path, stem: &str) -> Result<[u8; 64], KeyError> {
    let secret_path = dir.join(format!("{stem}.key"));
    let public_path = dir.join(format!("{stem}.pub"));
    for path in [&secret_path, &public_path] {
        if path.exists() {
            return Err(KeyError::Exists { path: path.clone() });
        }
    }
    fs::create_dir_all(dir).map_err(|source| KeyError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let (public, secret) = pkgar_keys::SecretKeyFile::new();
    let pkgar = |path: &Path| {
        let path = path.to_path_buf();
        move |source| KeyError::Pkgar { path, source }
    };
    secret.save(&secret_path).map_err(pkgar(&secret_path))?;
    public.save(&public_path).map_err(pkgar(&public_path))?;
    load_secret(&secret_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssh_and_pkgar_keys_load_alike() {
        let dir = tempfile::tempdir().unwrap();
        let secret = generate(dir.path(), "role").unwrap();
        let public = fox_core::signing::public_key_of(&secret);
        assert_eq!(load_secret(&dir.path().join("role.key")).unwrap(), secret);
        let pub_path = dir.path().join("role.pub");
        assert_eq!(parse_public(pub_path.to_str().unwrap()).unwrap(), public);
        assert_eq!(parse_public(&public.to_string()).unwrap(), public);
        assert!(matches!(
            generate(dir.path(), "role"),
            Err(KeyError::Exists { .. })
        ));

        let pair = ssh_key::private::Ed25519Keypair::from_seed(&[3; 32]);
        let private = ssh_key::PrivateKey::from(pair);
        let path = dir.path().join("id_ed25519");
        fs::write(&path, private.to_openssh(ssh_key::LineEnding::LF).unwrap()).unwrap();
        let secret = load_secret(&path).unwrap();
        let line = private.public_key().to_openssh().unwrap();
        assert_eq!(
            parse_public(&line).unwrap(),
            fox_core::signing::public_key_of(&secret)
        );
        assert!(parse_public("nonsense").is_err());
    }
}
