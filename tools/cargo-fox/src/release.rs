//! `cargo fox release`: bump the version, commit, and make the signed
//! release tag that `fox install <git-url>` verifies.
//!
//! The version is set in the package's `Cargo.toml` (or in
//! `[workspace.package]` of the workspace root when the package inherits
//! it), `Cargo.lock` is updated for the workspace members only, the change
//! is committed with `git commit`, and annotated tag `v<version>` is made on
//! the commit. With `--signing-key`, the tag is signed here with that
//! OpenSSH Ed25519 key, the way `git tag -s` signs with `gpg.format = ssh`;
//! without it, `git tag -s` signs with the user's git configuration (which
//! must use SSH signing). Either way the tag is then read back and its
//! signature verified. Nothing is pushed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use fox_git::{GitError, Repository, TagSigner};
use semver::Version;
use toml_edit::{DocumentMut, Item, value};

/// Why a release could not be made.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReleaseError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{} is not inside the work tree of a git repository", path.display())]
    NoWorkTree { path: PathBuf },
    #[error("tracked files have uncommitted changes; commit or stash them first")]
    Dirty,
    #[error("tag {tag} already exists")]
    TagExists { tag: String },
    #[error("cannot read or write {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot parse {}", path.display())]
    Toml {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("{} has no package version to set", path.display())]
    NoVersion { path: PathBuf },
    #[error("{} inherits its version but no workspace root above it sets one", path.display())]
    NoWorkspaceVersion { path: PathBuf },
    #[error("`{command}` failed: {stderr}")]
    Command { command: String, stderr: String },
    #[error("cannot run `{command}`")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("cannot read the signing key {}", path.display())]
    SigningKey {
        path: PathBuf,
        #[source]
        source: ssh_key::Error,
    },
    #[error("tag {tag} is not SSH-signed; set gpg.format = ssh in git, or pass --signing-key")]
    NotSshSigned { tag: String },
}

/// What [`release`] did.
pub(crate) struct Released {
    pub tag: String,
    pub commit: String,
    pub fingerprint: String,
    pub bumped: bool,
}

/// Releases `version` of the package whose manifest is `manifest`.
pub(crate) fn release(
    manifest: &Path,
    version: &Version,
    signing_key: Option<&Path>,
) -> Result<Released, ReleaseError> {
    let manifest = canonical(manifest)?;
    let package_dir = manifest.parent().unwrap_or(Path::new("/")).to_path_buf();
    let repo = Repository::discover(&package_dir)?;
    let root = repo
        .workdir()
        .map(Path::to_path_buf)
        .ok_or_else(|| ReleaseError::NoWorkTree {
            path: manifest.clone(),
        })?;
    if !run(
        &root,
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        return Err(ReleaseError::Dirty);
    }
    let tag = format!("v{version}");
    if repo.resolve(&format!("refs/tags/{tag}")).is_ok() {
        return Err(ReleaseError::TagExists { tag });
    }
    let key = signing_key.map(read_key).transpose()?;

    let bumped = set_version(&manifest, version)?;
    if bumped {
        let manifest_arg = manifest.to_string_lossy();
        run(
            &root,
            "cargo",
            &[
                "update",
                "--workspace",
                "--offline",
                "--manifest-path",
                &manifest_arg,
            ],
        )?;
        let message = format!("Release {tag}");
        run(&root, "git", &["commit", "-q", "-a", "-m", &message])?;
    }

    let repo = Repository::discover(&package_dir)?;
    let commit = repo.resolve("HEAD")?;
    let message = format!("Release {tag}\n");
    let tagged = match &key {
        Some(key) => {
            let tagger = run(&root, "git", &["var", "GIT_COMMITTER_IDENT"])?;
            repo.create_signed_tag(&tag, &commit, &tagger, &message, key)?
        }
        None => {
            run(&root, "git", &["tag", "-s", "-m", &message, &tag])?;
            Repository::discover(&package_dir)?.tag(&tag)?
        }
    };
    let TagSigner::Ssh(signer) = tagged.signer() else {
        return Err(ReleaseError::NotSshSigned { tag });
    };
    tagged.verify(&signer)?;
    Ok(Released {
        tag,
        commit: commit.id(),
        fingerprint: signer.fingerprint(),
        bumped,
    })
}

fn canonical(path: &Path) -> Result<PathBuf, ReleaseError> {
    path.canonicalize().map_err(|source| ReleaseError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_key(path: &Path) -> Result<ssh_key::PrivateKey, ReleaseError> {
    ssh_key::PrivateKey::read_openssh_file(path).map_err(|source| ReleaseError::SigningKey {
        path: path.to_path_buf(),
        source,
    })
}

/// Sets the package version, in `[workspace.package]` of the nearest
/// workspace root when the package inherits it. Returns whether the
/// version changed.
fn set_version(manifest: &Path, version: &Version) -> Result<bool, ReleaseError> {
    let mut doc = read_toml(manifest)?;
    let package_version = doc
        .get("package")
        .and_then(|package| package.get("version"))
        .ok_or_else(|| ReleaseError::NoVersion {
            path: manifest.to_path_buf(),
        })?;
    let inherited = package_version
        .get("workspace")
        .and_then(Item::as_bool)
        .unwrap_or(false);
    if !inherited {
        return update_version(manifest, &mut doc, &["package", "version"], version);
    }
    let mut dir = manifest.parent().and_then(Path::parent);
    while let Some(candidate) = dir {
        let path = candidate.join("Cargo.toml");
        if path.is_file() {
            let mut root = read_toml(&path)?;
            let has_version = root
                .get("workspace")
                .and_then(|workspace| workspace.get("package"))
                .and_then(|package| package.get("version"))
                .is_some();
            if has_version {
                return update_version(
                    &path,
                    &mut root,
                    &["workspace", "package", "version"],
                    version,
                );
            }
        }
        dir = candidate.parent();
    }
    Err(ReleaseError::NoWorkspaceVersion {
        path: manifest.to_path_buf(),
    })
}

fn read_toml(path: &Path) -> Result<DocumentMut, ReleaseError> {
    let text = fs::read_to_string(path).map_err(|source| ReleaseError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse().map_err(|source| ReleaseError::Toml {
        path: path.to_path_buf(),
        source,
    })
}

fn update_version(
    path: &Path,
    doc: &mut DocumentMut,
    keys: &[&str],
    version: &Version,
) -> Result<bool, ReleaseError> {
    let (last, parents) = keys.split_last().expect("keys are not empty");
    let mut item = doc.as_item_mut();
    for key in parents {
        item = &mut item[*key];
    }
    let text = version.to_string();
    if item[*last].as_str() == Some(text.as_str()) {
        return Ok(false);
    }
    item[*last] = value(text);
    fs::write(path, doc.to_string()).map_err(|source| ReleaseError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(true)
}

/// Runs a command in `dir` and returns its trimmed standard output.
fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String, ReleaseError> {
    let command = format!("{program} {}", args.join(" "));
    // Cargo tells subcommands which cargo runs them.
    let program = match program {
        "cargo" => std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        other => other.into(),
    };
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|source| ReleaseError::Spawn {
            command: command.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(ReleaseError::Command {
            command,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_package_and_inherited_versions() {
        let dir = tempfile::tempdir().unwrap();
        let member = dir.path().join("member");
        fs::create_dir(&member).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\n\n[workspace.package]\n# kept\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"m\"\nversion.workspace = true\n",
        )
        .unwrap();
        let v = Version::new(0, 2, 0);
        assert!(set_version(&member.join("Cargo.toml"), &v).unwrap());
        let root = fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(root.contains("# kept\nversion = \"0.2.0\"\n"), "{root}");
        assert!(!set_version(&member.join("Cargo.toml"), &v).unwrap());

        let plain = dir.path().join("plain.toml");
        fs::write(
            &plain,
            "[package]\nname = \"p\"\nversion = \"1.0.0\" # note\n",
        )
        .unwrap();
        assert!(set_version(&plain, &Version::new(1, 0, 1)).unwrap());
        assert!(
            fs::read_to_string(&plain)
                .unwrap()
                .contains("version = \"1.0.1\"")
        );

        fs::write(&plain, "[package]\nname = \"p\"\n").unwrap();
        assert!(matches!(
            set_version(&plain, &v),
            Err(ReleaseError::NoVersion { .. })
        ));
    }
}
