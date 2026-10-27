//! Loading trusted signer keys from disk.
//!
//! Keys use the `pkgar-keys` public key file format (a TOML file with a
//! `pkey` hex field), the same format the cookbook writes to
//! `build/id_ed25519.pub.toml`. A system trusts every key file in
//! [`TRUSTED_KEYS_DIR`] under its root; the file stem is the key's label.
//! It also trusts its own local builder key, [`LOCAL_BUILDER_KEY`], which
//! `fox install <git-url>` creates to sign the packages it builds from
//! source.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{KeyId, TrustedKey, TrustedKeys};

/// Directory, relative to the install root, of trusted signer key files.
pub const TRUSTED_KEYS_DIR: &str = "etc/fox/keys";

/// The local builder's secret key file, relative to the install root. It
/// lives in fox's state directory, which no package may write.
pub const LOCAL_BUILDER_SECRET_KEY: &str = "var/lib/fox/local-builder.toml";

/// The local builder's public key file, relative to the install root.
pub const LOCAL_BUILDER_KEY: &str = "var/lib/fox/local-builder.pub.toml";

/// The label of the local builder key.
pub const LOCAL_BUILDER_LABEL: &str = "local-builder";

/// The label under which a system image trusts the cookbook key that
/// signed the packages it was built from.
pub const COOKBOOK_LABEL: &str = "cookbook";

/// Why trusted keys could not be loaded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeyError {
    /// A key file could not be read or parsed.
    #[error("cannot read public key file {}", path.display())]
    File {
        /// The key file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: pkgar_keys::Error,
    },
    /// The key directory could not be listed.
    #[error("cannot list key directory {}", path.display())]
    Dir {
        /// The key directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A key file could not be copied into a system's trusted keys.
    #[error("cannot install public key file {} as {}", from.display(), to.display())]
    Install {
        /// The key file.
        from: PathBuf,
        /// Where it was going.
        to: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// Reads one public key file. Its label is the file name without the
/// `.toml` (and `.pub`) extensions.
///
/// # Errors
///
/// [`KeyError::File`] if the file cannot be read or is not a public key
/// file.
pub fn load_key_file(path: &Path) -> Result<TrustedKey, KeyError> {
    let file = pkgar_keys::PublicKeyFile::open(path).map_err(|source| KeyError::File {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(TrustedKey {
        label: key_label(path),
        key: KeyId::from_bytes(file.pkey),
    })
}

/// Makes the system at `root` trust the public key file `path` under
/// `label`, by copying it to `<root>/etc/fox/keys/<label>.toml`, and
/// returns the key.
///
/// # Errors
///
/// [`KeyError::File`] if `path` is not a public key file and
/// [`KeyError::Install`] if it cannot be copied.
pub fn install_key_file(path: &Path, root: &Path, label: &str) -> Result<TrustedKey, KeyError> {
    let key = load_key_file(path)?;
    let dir = root.join(TRUSTED_KEYS_DIR);
    let target = dir.join(format!("{label}.toml"));
    let install_error = |source| KeyError::Install {
        from: path.to_path_buf(),
        to: target.clone(),
        source,
    };
    fs::create_dir_all(&dir).map_err(install_error)?;
    fs::copy(path, &target).map_err(install_error)?;
    Ok(TrustedKey {
        label: label.to_string(),
        key: key.key,
    })
}

/// Adds every `*.toml` key file in `dir` to `keys` and returns how many
/// were read. A missing directory adds nothing.
///
/// # Errors
///
/// [`KeyError::Dir`] if the directory exists but cannot be listed, and
/// [`KeyError::File`] for the first key file that cannot be read.
pub fn load_key_dir(dir: &Path, keys: &mut TrustedKeys) -> Result<usize, KeyError> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(KeyError::Dir {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut paths = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| KeyError::Dir {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            paths.push(path);
        }
    }
    // Sorted so the label kept for a key listed twice does not depend on
    // directory order.
    paths.sort();
    for path in &paths {
        let key = load_key_file(path)?;
        keys.insert(key.label, key.key);
    }
    Ok(paths.len())
}

/// The keys trusted by the system at `root`: the files in
/// `<root>/etc/fox/keys`, and the local builder key if it exists.
///
/// # Errors
///
/// As [`load_key_dir`], and [`KeyError::File`] if the local builder key
/// exists but cannot be read.
pub fn trusted_keys_for_root(root: &Path) -> Result<TrustedKeys, KeyError> {
    let mut keys = TrustedKeys::new();
    load_key_dir(&root.join(TRUSTED_KEYS_DIR), &mut keys)?;
    let local = root.join(LOCAL_BUILDER_KEY);
    if local.is_file() {
        let key = load_key_file(&local)?;
        keys.insert(LOCAL_BUILDER_LABEL, key.key);
    }
    Ok(keys)
}

fn key_label(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.strip_suffix(".toml").unwrap_or(&name);
    name.strip_suffix(".pub").unwrap_or(name).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_strip_key_file_extensions() {
        assert_eq!(key_label(Path::new("/k/ferric-ci-1.toml")), "ferric-ci-1");
        assert_eq!(key_label(Path::new("id_ed25519.pub.toml")), "id_ed25519");
    }

    #[test]
    fn missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut keys = TrustedKeys::new();
        assert_eq!(
            load_key_dir(&dir.path().join("nope"), &mut keys).unwrap(),
            0
        );
        assert!(keys.is_empty());
    }

    #[test]
    fn installed_key_is_trusted_by_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let (public, _) = pkgar_keys::SecretKeyFile::new();
        let source = dir.path().join("id_ed25519.pub.toml");
        public.save(&source).unwrap();
        let root = dir.path().join("root");

        let key = install_key_file(&source, &root, COOKBOOK_LABEL).unwrap();
        assert_eq!(key.label, COOKBOOK_LABEL);
        let trusted = trusted_keys_for_root(&root).unwrap();
        let found = trusted.get(&KeyId::from_bytes(public.pkey)).unwrap();
        assert_eq!(found.label, COOKBOOK_LABEL);

        assert!(matches!(
            install_key_file(&dir.path().join("nope.toml"), &root, "x"),
            Err(KeyError::File { .. })
        ));
    }

    #[test]
    fn loads_key_files_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        let (public, _) = pkgar_keys::SecretKeyFile::new();
        public.save(dir.path().join("ci.toml")).unwrap();
        fs::write(dir.path().join("README"), "not a key").unwrap();
        let keys = {
            let mut keys = TrustedKeys::new();
            assert_eq!(load_key_dir(dir.path(), &mut keys).unwrap(), 1);
            keys
        };
        let key = keys.get(&KeyId::from_bytes(public.pkey)).unwrap();
        assert_eq!(key.label, "ci");

        fs::write(dir.path().join("bad.toml"), "pkey = 1").unwrap();
        assert!(matches!(
            load_key_dir(dir.path(), &mut TrustedKeys::new()),
            Err(KeyError::File { .. })
        ));
    }

    #[test]
    fn root_trusts_its_local_builder() {
        let root = tempfile::tempdir().unwrap();
        assert!(trusted_keys_for_root(root.path()).unwrap().is_empty());
        let (public, _) = pkgar_keys::SecretKeyFile::new();
        let path = root.path().join(LOCAL_BUILDER_KEY);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        public.save(&path).unwrap();
        let keys = trusted_keys_for_root(root.path()).unwrap();
        let key = keys.get(&KeyId::from_bytes(public.pkey)).unwrap();
        assert_eq!(key.label, LOCAL_BUILDER_LABEL);
    }
}
