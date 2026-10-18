//! `fox sync`: make a root match a package set, and record what it
//! installed in a lock file.
//!
//! A package set (`fox-packages/1`) lists the packages a root should have,
//! the registries they come from and, optionally, the install policy:
//!
//! ```toml
//! format = "fox-packages/1"
//!
//! [[registry]]
//! name = "ferric"
//! git = "https://github.com/rexlunae/ferric-registry"
//! root-blake3 = "<BLAKE3 of its metadata/root/1.json>"
//!
//! [policy]                      # optional; written to etc/fox/policy.toml
//! build = "auto"
//!
//! [packages]
//! ripgrep = {}                                   # newest release, any registry
//! nginx = { version = "^1.27", registry = "ferric", features = ["http2"] }
//! mytool = { git = "https://github.com/me/mytool", rev = "v0.3.0" }
//! ```
//!
//! The lock file (`fox-lock/1`, default `fox.lock` beside the package set)
//! records each package's version, where it came from and the BLAKE3 of the
//! archive installed, plus `request-blake3`: a digest of the build request
//! (the normalized git source, `rev`, `path`, `package`, the sorted
//! features, `default-features` and the effective target). A registry
//! package is pinned to its version and archive hash; a git package to its
//! commit (archives built locally are signed with the root's own builder
//! key, so their hash is recorded but not compared). Sync keeps a locked
//! package while the lock still satisfies the set: the version requirement
//! and registry still match and the build request is unchanged, so editing
//! a package's `rev`, features or target resolves it again. It skips a
//! package whose installed archive is the locked one, and removes packages
//! the previous lock had that the set no longer lists, but only while the
//! installed archive is still the locked one; packages installed or
//! replaced by hand are left alone. `--locked` refuses to resolve anything
//! the lock does not already pin, and checks every package before it adds
//! a registry or writes the policy; `--update` re-resolves every package.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fox_core::archive::Archive;
use fox_core::config::ConfigError;
use fox_core::policy::{POLICY_FILE, Policy};
use fox_core::registries::{Registries, RegistryEntry};
use fox_core::{Blake3Digest, GitUrl, PackageName};
use fox_store::{InstallOptions, Store};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::commands::{self, CliError, print_json};
use crate::source::{SourceError, SourceSpec};
use crate::{Cli, SourceArgs};

/// Largest package set or lock file read.
const MAX_FILE: u64 = 1024 * 1024;

/// Why a sync could not go ahead.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SyncError {
    #[error("cannot read {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is larger than 1 MiB", path.display())]
    TooLarge { path: PathBuf },
    #[error("invalid {}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("cannot write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: ConfigError,
    },
    #[error("package {name} names both a git source and a registry or version")]
    MixedSource { name: PackageName },
    #[error("package {name} has an invalid git source")]
    GitSource {
        name: PackageName,
        #[source]
        source: Box<SourceError>,
    },
    #[error("registry {name} is configured with another URL or root pin than the package set's")]
    RegistryConflict { name: PackageName },
    #[error("{name} is not pinned in the lock file, and --locked forbids resolving it")]
    NotLocked { name: PackageName },
    #[error("{name} resolved to archive {found}, but the lock pins {locked}")]
    LockedHash {
        name: PackageName,
        locked: Blake3Digest,
        found: Blake3Digest,
    },
    #[error("{name} was built from commit {found}, but the lock pins {locked}")]
    LockedCommit {
        name: PackageName,
        locked: String,
        found: String,
    },
    #[error("{name} installed {found}, which is not the package set's package")]
    WrongPackage {
        name: PackageName,
        found: PackageName,
    },
}

/// The package set format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum SetFormat {
    #[default]
    #[serde(rename = "fox-packages/1")]
    V1,
}

/// A package set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageSet {
    #[serde(default)]
    pub(crate) format: SetFormat,
    #[serde(default, rename = "registry")]
    pub(crate) registries: Vec<RegistryEntry>,
    #[serde(default)]
    pub(crate) policy: Option<Policy>,
    #[serde(default)]
    pub(crate) packages: BTreeMap<PackageName, Want>,
}

/// One wanted package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Want {
    /// A version requirement for a registry package.
    pub(crate) version: Option<VersionReq>,
    /// The registry to take it from.
    pub(crate) registry: Option<PackageName>,
    /// A git source instead of a registry.
    pub(crate) git: Option<String>,
    /// The tag or commit of the git source (default: its newest release).
    pub(crate) rev: Option<String>,
    /// The package's directory in the repository.
    pub(crate) path: Option<PathBuf>,
    /// The Cargo package, in a workspace.
    pub(crate) package: Option<String>,
    /// Cargo features.
    #[serde(default)]
    pub(crate) features: Vec<String>,
    /// Whether to enable default features.
    #[serde(default = "yes")]
    pub(crate) default_features: bool,
    /// The Rust target.
    pub(crate) target: Option<String>,
}

fn yes() -> bool {
    true
}

/// Feeds an optional field to `hasher`, length-prefixed so that no two
/// requests hash the same bytes.
fn hash_field(hasher: &mut blake3::Hasher, field: Option<&[u8]>) {
    match field {
        Some(bytes) => {
            hasher.update(&[1]);
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

/// The lock file format version, bumped on any incompatible change to
/// [`Lock`] or [`Locked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum LockFormat {
    #[default]
    #[serde(rename = "fox-lock/1")]
    V1,
}

/// A lock file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lock {
    #[serde(default)]
    pub(crate) format: LockFormat,
    #[serde(default, rename = "package", skip_serializing_if = "Vec::is_empty")]
    pub(crate) packages: Vec<Locked>,
}

/// One locked package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Locked {
    pub(crate) name: PackageName,
    pub(crate) version: String,
    /// The registry it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) registry: Option<PackageName>,
    /// The git source it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git: Option<GitUrl>,
    /// The commit it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) commit: Option<String>,
    /// The digest of the build request it was resolved for.
    pub(crate) request_blake3: Blake3Digest,
    /// The archive installed.
    pub(crate) pkgar_blake3: Blake3Digest,
}

/// A want's normalized build request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    /// The git source's identity, for a git package.
    git: Option<GitUrl>,
    /// The digest of everything that changes what is built.
    digest: Blake3Digest,
}

impl Lock {
    fn get(&self, name: &PackageName) -> Option<&Locked> {
        self.packages.iter().find(|locked| &locked.name == name)
    }
}

impl Want {
    fn check(&self, name: &PackageName) -> Result<(), SyncError> {
        if self.git.is_some() && (self.version.is_some() || self.registry.is_some()) {
            return Err(SyncError::MixedSource { name: name.clone() });
        }
        Ok(())
    }

    /// Normalizes this want's build request, as built with `base`.
    ///
    /// # Errors
    ///
    /// [`SyncError::GitSource`] if the git source is not a git URL or an
    /// existing directory.
    fn request(&self, name: &PackageName, base: &SourceArgs) -> Result<Request, SyncError> {
        let git = match &self.git {
            Some(git) => Some(
                SourceSpec::parse(git)
                    .map_err(|source| SyncError::GitSource {
                        name: name.clone(),
                        source: Box::new(source),
                    })?
                    .git()
                    .clone(),
            ),
            None => None,
        };
        let mut features: Vec<&str> = self.features.iter().map(String::as_str).collect();
        features.sort_unstable();
        features.dedup();
        let mut hasher = blake3::Hasher::new();
        hash_field(&mut hasher, git.as_ref().map(|git| git.as_str().as_bytes()));
        hash_field(&mut hasher, self.rev.as_deref().map(str::as_bytes));
        hash_field(
            &mut hasher,
            self.path
                .as_deref()
                .map(|path| path.as_os_str().as_encoded_bytes()),
        );
        hash_field(&mut hasher, self.package.as_deref().map(str::as_bytes));
        hasher.update(&(features.len() as u64).to_le_bytes());
        for feature in features {
            hash_field(&mut hasher, Some(feature.as_bytes()));
        }
        hasher.update(&[u8::from(self.default_features)]);
        let target = self.target.as_deref().or(base.target.as_deref());
        hash_field(&mut hasher, target.map(str::as_bytes));
        Ok(Request {
            git,
            digest: hasher.finalize().into(),
        })
    }

    /// Whether `locked` is still a resolution of this want's `request`.
    fn accepts(&self, locked: &Locked, request: &Request) -> bool {
        if locked.request_blake3 != request.digest {
            return false;
        }
        match &request.git {
            Some(git) => locked.git.as_ref() == Some(git) && locked.commit.is_some(),
            None => {
                let version_ok = locked.version.parse::<Version>().is_ok_and(|version| {
                    self.version
                        .as_ref()
                        .is_none_or(|req| req.matches(&version))
                });
                let registry_ok = locked.registry.is_some()
                    && (self.registry.is_none() || self.registry == locked.registry);
                version_ok && registry_ok && locked.git.is_none()
            }
        }
    }

    /// The `fox install` target for this want, pinned to `locked` if given.
    fn target(&self, name: &PackageName, locked: Option<&Locked>) -> String {
        match &self.git {
            Some(git) => match self.rev.as_deref() {
                Some(rev) => format!("{git}#{rev}"),
                None => match locked.and_then(|locked| locked.commit.as_deref()) {
                    Some(commit) => format!("{git}#{commit}"),
                    None => git.clone(),
                },
            },
            None => {
                let registry = locked
                    .and_then(|locked| locked.registry.as_ref())
                    .or(self.registry.as_ref());
                let qualified = match registry {
                    Some(registry) => format!("{registry}/{name}"),
                    None => name.to_string(),
                };
                match (locked, &self.version) {
                    (Some(locked), _) => format!("{qualified}@={}", locked.version),
                    (None, Some(req)) => format!("{qualified}@{req}"),
                    (None, None) => qualified,
                }
            }
        }
    }

    fn source_args(&self, base: &SourceArgs) -> SourceArgs {
        let mut args = base.clone();
        args.features.clone_from(&self.features);
        args.no_default_features = !self.default_features;
        if self.target.is_some() {
            args.target.clone_from(&self.target);
        }
        args.path.clone_from(&self.path);
        args.package.clone_from(&self.package);
        args
    }
}

/// Reads a bounded TOML file.
fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, SyncError> {
    let io_error = |source| SyncError::Io {
        path: path.to_path_buf(),
        source,
    };
    if fs::metadata(path).map_err(io_error)?.len() > MAX_FILE {
        return Err(SyncError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let text = fs::read_to_string(path).map_err(io_error)?;
    toml::from_str(&text).map_err(|source| SyncError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// What happened to one package.
#[derive(Debug, Serialize)]
struct ActionJson {
    name: String,
    action: &'static str,
    version: String,
}

/// The options of `fox sync`.
pub(crate) struct SyncOptions<'a> {
    pub(crate) config: &'a Path,
    pub(crate) lock: Option<&'a Path>,
    pub(crate) locked: bool,
    pub(crate) update: bool,
    pub(crate) source: &'a SourceArgs,
}

/// `fox sync`.
pub(crate) fn run(cli: &Cli, options: &SyncOptions<'_>) -> Result<ExitCode, CliError> {
    let set: PackageSet = read_toml(options.config)?;
    for (name, want) in &set.packages {
        want.check(name)?;
    }
    let lock_path = options.lock.map_or_else(
        || options.config.with_file_name("fox.lock"),
        Path::to_path_buf,
    );
    let previous: Lock = if lock_path.exists() {
        read_toml(&lock_path)?
    } else {
        Lock::default()
    };
    let mut chosen = BTreeMap::new();
    for (name, want) in &set.packages {
        let request = want.request(name, options.source)?;
        let locked = previous
            .get(name)
            .filter(|locked| !options.update && want.accepts(locked, &request));
        // Checked before anything changes, so a refused --locked sync
        // leaves the root as it was.
        if options.locked && locked.is_none() {
            return Err(SyncError::NotLocked { name: name.clone() }.into());
        }
        chosen.insert(name, (request, locked));
    }
    let store = Store::open(&cli.root)?;
    let root = store.root().to_path_buf();
    add_registries(cli, &root, &set.registries)?;
    if let Some(policy) = &set.policy {
        let path = root.join(POLICY_FILE);
        fox_core::config::save_toml(&path, policy)
            .map_err(|source| SyncError::Write { path, source })?;
    }
    let policy = Policy::load(&root)?;
    let mut base = options.source.clone();
    base.allow_unsigned |= policy.allow_unsigned_git;

    let mut lock = Lock::default();
    let mut actions = Vec::new();
    for (name, want) in &set.packages {
        let (request, locked) = &chosen[name];
        let locked = *locked;
        let installed = store.get(name)?;
        if let (Some(locked), Some(installed)) = (locked, &installed)
            && installed.archive_blake3 == Some(locked.pkgar_blake3)
        {
            lock.packages.push(locked.clone());
            actions.push(ActionJson {
                name: name.to_string(),
                action: "unchanged",
                version: locked.version.clone(),
            });
            continue;
        }
        let args = want.source_args(&base);
        let target = want.target(name, locked);
        let (pending, registry) = commands::prepare(&root, &target, &args, &policy)?;
        let keys = commands::trusted_keys_for(cli, &root)?;
        let archive = commands::open_pending(pending, &keys)?;
        let entry = lock_entry(name, request, &archive, registry, locked)?;
        commands::install_archive(cli, &store, &archive, InstallOptions::default())?;
        actions.push(ActionJson {
            name: name.to_string(),
            action: if installed.is_some() {
                "upgraded"
            } else {
                "installed"
            },
            version: entry.version.clone(),
        });
        lock.packages.push(entry);
    }
    for old in &previous.packages {
        if set.packages.contains_key(&old.name) {
            continue;
        }
        let Some(installed) = store.get(&old.name)? else {
            continue;
        };
        // A package replaced by hand since the last sync is no longer the
        // one the lock installed, so it is not sync's to remove.
        let action = if installed.archive_blake3 == Some(old.pkgar_blake3) {
            store.remove(&old.name, false)?;
            "removed"
        } else {
            "kept"
        };
        if !cli.json {
            match action {
                "removed" => println!("removed {} {}", old.name, old.version),
                _ => println!("kept {}: it was replaced after the last sync", old.name),
            }
        }
        actions.push(ActionJson {
            name: old.name.to_string(),
            action,
            version: installed.version.clone(),
        });
    }
    fox_core::config::save_toml(&lock_path, &lock).map_err(|source| SyncError::Write {
        path: lock_path.clone(),
        source,
    })?;
    if cli.json {
        print_json(&actions)?;
    } else {
        let unchanged = actions.iter().filter(|a| a.action == "unchanged").count();
        println!(
            "synced {} packages ({unchanged} unchanged); lock written to {}",
            set.packages.len(),
            lock_path.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Adds the set's registries that are not configured yet, and refuses one
/// configured differently.
fn add_registries(cli: &Cli, root: &Path, wanted: &[RegistryEntry]) -> Result<(), CliError> {
    let mut list = Registries::load(root)?;
    let mut changed = false;
    for entry in wanted {
        match list.get(entry.name.as_str()) {
            Some(existing) if existing == entry => {}
            Some(_) => {
                return Err(SyncError::RegistryConflict {
                    name: entry.name.clone(),
                }
                .into());
            }
            None => {
                crate::registry::update(root, entry, time::OffsetDateTime::now_utc())?;
                if !cli.json {
                    println!("added registry {}", entry.name);
                }
                list.add(entry.clone())?;
                changed = true;
            }
        }
    }
    if changed {
        list.save(root)?;
    }
    Ok(())
}

/// The lock entry for `archive`, checked against the previous pin.
fn lock_entry(
    name: &PackageName,
    request: &Request,
    archive: &Archive,
    registry: Option<PackageName>,
    locked: Option<&Locked>,
) -> Result<Locked, SyncError> {
    let manifest = archive.manifest();
    if &manifest.name != name {
        return Err(SyncError::WrongPackage {
            name: name.clone(),
            found: manifest.name.clone(),
        });
    }
    let commit = manifest
        .source
        .as_ref()
        .and_then(|source| source.commit.clone());
    let entry = match &request.git {
        Some(git) => Locked {
            name: name.clone(),
            version: manifest.version.clone(),
            registry: None,
            git: Some(git.clone()),
            commit,
            request_blake3: request.digest,
            pkgar_blake3: archive.blake3(),
        },
        None => Locked {
            name: name.clone(),
            version: manifest.version.clone(),
            registry,
            git: None,
            commit: None,
            request_blake3: request.digest,
            pkgar_blake3: archive.blake3(),
        },
    };
    if let Some(locked) = locked {
        if request.git.is_some() {
            if locked.commit != entry.commit {
                return Err(SyncError::LockedCommit {
                    name: name.clone(),
                    locked: locked.commit.clone().unwrap_or_default(),
                    found: entry.commit.unwrap_or_default(),
                });
            }
        } else if locked.pkgar_blake3 != entry.pkgar_blake3 {
            return Err(SyncError::LockedHash {
                name: name.clone(),
                locked: locked.pkgar_blake3,
                found: entry.pkgar_blake3,
            });
        }
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(version: &str, registry: Option<&str>, request: &Request) -> Locked {
        Locked {
            name: PackageName::new("tiny").unwrap(),
            version: version.to_owned(),
            registry: registry.map(|name| PackageName::new(name).unwrap()),
            git: request.git.clone(),
            commit: request.git.as_ref().map(|_| "c0ffee".to_owned()),
            request_blake3: request.digest,
            pkgar_blake3: Blake3Digest::of(b"archive"),
        }
    }

    fn base() -> SourceArgs {
        SourceArgs {
            trust_new_key: false,
            allow_unsigned: false,
            target: None,
            package: None,
            path: None,
            features: Vec::new(),
            no_default_features: false,
            work_dir: None,
        }
    }

    #[test]
    fn package_sets_parse_and_check() {
        let set: PackageSet = toml::from_str(
            "[packages]\ntiny = {}\nbig = { version = \"^1\", registry = \"ferric\", \
             features = [\"x\"], default-features = false }\n\
             tool = { git = \"https://example.com/tool\", rev = \"v1.0.0\" }\n",
        )
        .unwrap();
        assert_eq!(set.packages.len(), 3);
        let big = &set.packages[&PackageName::new("big").unwrap()];
        assert!(!big.default_features);
        assert!(set.packages[&PackageName::new("tiny").unwrap()].default_features);
        assert!(toml::from_str::<PackageSet>("[packages]\nx = { other = 1 }\n").is_err());
        let mixed = Want {
            git: Some("https://example.com/x".into()),
            version: Some("^1".parse().unwrap()),
            ..Want::default()
        };
        assert!(matches!(
            mixed.check(&PackageName::new("x").unwrap()),
            Err(SyncError::MixedSource { .. })
        ));
    }

    #[test]
    fn locks_pin_targets_while_they_satisfy_the_set() {
        let name = PackageName::new("tiny").unwrap();
        let want = Want {
            version: Some("^0.3".parse().unwrap()),
            ..Want::default()
        };
        let request = want.request(&name, &base()).unwrap();
        let pinned = locked("0.3.1", Some("test"), &request);
        assert!(want.accepts(&pinned, &request));
        assert_eq!(want.target(&name, Some(&pinned)), "test/tiny@=0.3.1");
        assert_eq!(want.target(&name, None), "tiny@^0.3");
        assert!(!want.accepts(&locked("0.4.0", Some("test"), &request), &request));
        let other_registry = Want {
            registry: Some(PackageName::new("other").unwrap()),
            ..Want::default()
        };
        let other_request = other_registry.request(&name, &base()).unwrap();
        assert!(!other_registry.accepts(&pinned, &other_request));

        let git = Want {
            git: Some("https://example.com/tiny".into()),
            ..Want::default()
        };
        let git_request = git.request(&name, &base()).unwrap();
        let built = locked("0.3.0", None, &git_request);
        assert!(git.accepts(&built, &git_request));
        assert!(!git.accepts(&pinned, &git_request));
        assert_eq!(
            git.target(&name, Some(&built)),
            "https://example.com/tiny#c0ffee"
        );

        let lock = Lock {
            format: LockFormat::V1,
            packages: vec![pinned, built],
        };
        let text = toml::to_string(&lock).unwrap();
        assert!(text.contains("format = \"fox-lock/1\""), "{text}");
        assert!(text.contains("request-blake3 = "), "{text}");
        assert_eq!(toml::from_str::<Lock>(&text).unwrap(), lock);
    }

    #[test]
    fn changing_what_is_built_invalidates_the_lock() {
        let name = PackageName::new("tiny").unwrap();
        let want = Want {
            git: Some("https://example.com/tiny".into()),
            rev: Some("v1.0.0".into()),
            features: vec!["b".into(), "a".into()],
            default_features: true,
            ..Want::default()
        };
        let request = want.request(&name, &base()).unwrap();
        let pinned = locked("1.0.0", None, &request);
        assert!(want.accepts(&pinned, &request));

        // Feature order and repeats do not change the request.
        let reordered = Want {
            features: vec!["a".into(), "b".into(), "a".into()],
            ..want.clone()
        };
        assert_eq!(reordered.request(&name, &base()).unwrap(), request);

        let changes = [
            Want {
                rev: Some("v1.1.0".into()),
                ..want.clone()
            },
            Want {
                features: vec!["a".into()],
                ..want.clone()
            },
            Want {
                default_features: false,
                ..want.clone()
            },
            Want {
                target: Some("x86_64-unknown-redox".into()),
                ..want.clone()
            },
            Want {
                path: Some("crates/tiny".into()),
                ..want.clone()
            },
            Want {
                package: Some("tiny-cli".into()),
                ..want.clone()
            },
        ];
        for changed in changes {
            let changed_request = changed.request(&name, &base()).unwrap();
            assert!(!changed.accepts(&pinned, &changed_request), "{changed:?}");
        }
        // So does the command line's target, when the set names none.
        let mut cross = base();
        cross.target = Some("x86_64-unknown-redox".into());
        let cross_request = want.request(&name, &cross).unwrap();
        assert!(!want.accepts(&pinned, &cross_request));
    }

    #[test]
    fn local_git_sources_lock_by_their_canonical_url() {
        let name = PackageName::new("tiny").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let want = Want {
            git: Some(dir.path().to_str().unwrap().to_owned()),
            ..Want::default()
        };
        let request = want.request(&name, &base()).unwrap();
        let git = request.git.as_ref().unwrap();
        assert!(git.as_str().starts_with("file://"), "{}", git.as_str());

        // Another directory is another source, even with the same request.
        let other = tempfile::tempdir().unwrap();
        let moved = Want {
            git: Some(other.path().to_str().unwrap().to_owned()),
            ..Want::default()
        };
        let moved_request = moved.request(&name, &base()).unwrap();
        assert!(!moved.accepts(&locked("0.1.0", None, &request), &moved_request));

        let missing = Want {
            git: Some(dir.path().join("missing").to_str().unwrap().to_owned()),
            ..Want::default()
        };
        assert!(matches!(
            missing.request(&name, &base()),
            Err(SyncError::GitSource { .. })
        ));
    }
}
