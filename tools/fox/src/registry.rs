//! Package registries on the client: `fox registry add|list|update|remove`,
//! `fox search`, `fox info`, and looking up `fox install <name>`.
//!
//! Each configured registry (`etc/fox/registries.toml`) has a directory
//! under `var/lib/fox/registries/<name>/`: `git/`, a bare mirror of the
//! registry repository; `tree/`, the verified checkout of its default
//! branch; and `state.json`, the trusted root and the metadata versions seen
//! (`fox_index::verify::ClientState`) with the time of the last update.
//!
//! An update fetches, exports the branch head into `tree.new/`, verifies
//! that against the saved state, and only then replaces `tree/` and the
//! state. A registry whose last update is older than the policy's
//! `require-fresh-metadata` is updated before a name is looked up in it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fox_core::config::{ConfigError, write_atomic};
use fox_core::policy::Policy;
use fox_core::registries::{Registries, RegistriesError, RegistryEntry};
use fox_core::{Blake3Digest, GitUrl, PackageName};
use fox_git::GitError;
use fox_index::layout::{PackageEntry, REGISTRY_FILE, RegistryConfig};
use fox_index::metadata::{Root, Signed};
use fox_index::verify::{self, ClientState, VerifiedRegistry, VerifyError};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::commands::{CliError, print_json};
use crate::{Cli, RegistryCommand};

const GIT_DIR: &str = "git";
const TREE_DIR: &str = "tree";
const NEW_TREE_DIR: &str = "tree.new";
const STATE_FILE: &str = "state.json";
const FIRST_ROOT: &str = "metadata/root/1.json";

/// Why a registry could not be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RegistryError {
    #[error(transparent)]
    List(#[from] RegistriesError),
    #[error("cannot fetch registry {name}")]
    Git {
        name: String,
        #[source]
        source: Box<GitError>,
    },
    #[error("registry {name} does not verify")]
    Verify {
        name: String,
        #[source]
        source: Box<VerifyError>,
    },
    #[error("cannot access {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("registry state")]
    State(#[from] ConfigError),
    #[error("invalid registry state {}", path.display())]
    StateParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "the first root of {name} has BLAKE3 {found}, not the pinned {pinned}; \
         check the hash with the registry's maintainers"
    )]
    RootMismatch {
        name: String,
        pinned: Blake3Digest,
        found: Blake3Digest,
    },
    #[error("invalid first root in registry {name}")]
    RootParse {
        name: String,
        #[source]
        source: fox_index::metadata::MetadataError,
    },
    #[error("invalid registry.toml in registry {name}")]
    Config {
        name: String,
        #[source]
        source: Box<fox_index::layout::LayoutError>,
    },
    #[error("no registry named {name}")]
    Unknown { name: String },
    #[error("no registry has a package named {name}")]
    NoPackage { name: PackageName },
    #[error("{name} is in several registries ({registries}); write <registry>/{name}")]
    Ambiguous {
        name: PackageName,
        registries: String,
    },
    #[error("{name} has no release matching {req}")]
    NoRelease { name: PackageName, req: VersionReq },
    #[error("no registries are configured; add one with `fox registry add`")]
    NoRegistries,
}

/// A registry-qualified name with an optional version requirement:
/// `[registry/]name[@requirement]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageSpec {
    pub(crate) registry: Option<String>,
    pub(crate) name: PackageName,
    pub(crate) req: VersionReq,
}

impl PackageSpec {
    /// Parses `text`, or returns `None` if it is not a package spec (and so
    /// may be a URL or a path).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        if text.contains(':') || text.starts_with('.') || text.starts_with('/') {
            return None;
        }
        let (rest, req) = match text.split_once('@') {
            Some((rest, req)) => (rest, req.parse().ok()?),
            None => (text, VersionReq::STAR),
        };
        let (registry, name) = match rest.split_once('/') {
            Some((registry, name)) => (Some(PackageName::new(registry).ok()?.to_string()), name),
            None => (None, rest),
        };
        Some(Self {
            registry,
            name: PackageName::new(name).ok()?,
            req,
        })
    }
}

/// The saved state of one registry.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedState {
    /// Unix time of the last successful update.
    updated: i64,
    client: ClientState,
}

/// A package found in a registry.
pub(crate) struct Found {
    pub(crate) registry: RegistryEntry,
    pub(crate) verified: VerifiedRegistry,
    pub(crate) entry: PackageEntry,
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> RegistryError + '_ {
    move |source| RegistryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn verify_error(name: &PackageName) -> impl FnOnce(VerifyError) -> RegistryError + '_ {
    move |source| RegistryError::Verify {
        name: name.to_string(),
        source: Box::new(source),
    }
}

fn load_state(dir: &Path) -> Result<Option<SavedState>, RegistryError> {
    let path = dir.join(STATE_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(&path)(error)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| RegistryError::StateParse { path, source })
}

fn save_state(dir: &Path, state: &SavedState) -> Result<(), RegistryError> {
    let path = dir.join(STATE_FILE);
    let bytes = serde_json::to_vec_pretty(state).map_err(|source| RegistryError::StateParse {
        path: path.clone(),
        source,
    })?;
    Ok(write_atomic(&path, &bytes)?)
}

/// Fetches the registry and exports its branch head into `tree.new/`.
fn fetch_tree(entry: &RegistryEntry, dir: &Path) -> Result<PathBuf, RegistryError> {
    let git_error = |source| RegistryError::Git {
        name: entry.name.to_string(),
        source: Box::new(source),
    };
    let repo = fox_git::fetch(entry.git.as_str(), &dir.join(GIT_DIR)).map_err(git_error)?;
    let head = repo.resolve("HEAD").map_err(git_error)?;
    let tree = dir.join(NEW_TREE_DIR);
    if tree.exists() {
        fs::remove_dir_all(&tree).map_err(io_error(&tree))?;
    }
    repo.export(&head, &tree).map_err(git_error)?;
    Ok(tree)
}

/// Reads the exported first root and checks it against the pin.
fn pinned_root(entry: &RegistryEntry, tree: &Path) -> Result<Signed<Root>, RegistryError> {
    let path = tree.join(FIRST_ROOT);
    let bytes = fs::read(&path).map_err(io_error(&path))?;
    let found = Blake3Digest::of(&bytes);
    if found != entry.root_blake3 {
        return Err(RegistryError::RootMismatch {
            name: entry.name.to_string(),
            pinned: entry.root_blake3,
            found,
        });
    }
    Signed::from_json(&bytes).map_err(|source| RegistryError::RootParse {
        name: entry.name.to_string(),
        source,
    })
}

/// Fetches and verifies `entry`, then replaces its checkout and state.
pub(crate) fn update(
    root: &Path,
    entry: &RegistryEntry,
    now: OffsetDateTime,
) -> Result<VerifiedRegistry, RegistryError> {
    let dir = entry.state_dir(root);
    let tree = fetch_tree(entry, &dir)?;
    let mut client = match load_state(&dir)? {
        Some(saved) => saved.client,
        None => ClientState::trust_root(pinned_root(entry, &tree)?)
            .map_err(verify_error(&entry.name))?,
    };
    verify::update(&tree, &mut client, now).map_err(verify_error(&entry.name))?;
    let current = dir.join(TREE_DIR);
    if current.exists() {
        fs::remove_dir_all(&current).map_err(io_error(&current))?;
    }
    fs::rename(&tree, &current).map_err(io_error(&current))?;
    let saved = SavedState {
        updated: now.unix_timestamp(),
        client,
    };
    save_state(&dir, &saved)?;
    // Re-verify at its final path; the versions just saved are accepted.
    let mut client = saved.client;
    verify::update(&current, &mut client, now).map_err(verify_error(&entry.name))
}

/// The verified checkout of `entry`, updated first if it is missing or
/// older than the policy allows.
pub(crate) fn open(
    root: &Path,
    entry: &RegistryEntry,
    policy: &Policy,
    now: OffsetDateTime,
) -> Result<VerifiedRegistry, RegistryError> {
    let dir = entry.state_dir(root);
    let Some(saved) = load_state(&dir)? else {
        return update(root, entry, now);
    };
    let max_age =
        i64::try_from(policy.require_fresh_metadata.duration().as_secs()).unwrap_or(i64::MAX);
    if now.unix_timestamp().saturating_sub(saved.updated) > max_age {
        return update(root, entry, now);
    }
    let mut client = saved.client;
    verify::update(&dir.join(TREE_DIR), &mut client, now).map_err(verify_error(&entry.name))
}

/// Finds `spec`'s package in the configured registries.
pub(crate) fn find(
    root: &Path,
    spec: &PackageSpec,
    policy: &Policy,
    now: OffsetDateTime,
) -> Result<Found, RegistryError> {
    let list = Registries::load(root)?;
    if list.entries().is_empty() {
        return Err(RegistryError::NoRegistries);
    }
    let candidates: Vec<&RegistryEntry> = match &spec.registry {
        Some(name) => vec![
            list.get(name)
                .ok_or_else(|| RegistryError::Unknown { name: name.clone() })?,
        ],
        None => list.entries().iter().collect(),
    };
    let mut found = Vec::new();
    for registry in candidates {
        let verified = open(root, registry, policy, now)?;
        if let Some(entry) = verified
            .package(&spec.name)
            .map_err(verify_error(&registry.name))?
        {
            found.push(Found {
                registry: registry.clone(),
                verified,
                entry,
            });
        }
    }
    match found.len() {
        0 => Err(RegistryError::NoPackage {
            name: spec.name.clone(),
        }),
        1 => Ok(found.remove(0)),
        _ => Err(RegistryError::Ambiguous {
            name: spec.name.clone(),
            registries: found
                .iter()
                .map(|found| found.registry.name.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// The builders of every configured registry, as its last verified
/// checkout lists them, labelled `<registry>/<builder>`. A registry that has
/// never been updated contributes none.
pub(crate) fn builder_keys(root: &Path) -> Result<fox_core::TrustedKeys, RegistryError> {
    let mut keys = fox_core::TrustedKeys::new();
    for entry in Registries::load(root)?.entries() {
        let path = entry.state_dir(root).join(TREE_DIR).join(REGISTRY_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&path)(error)),
        };
        // The checkout was verified when it was written, and packages
        // cannot write under var/lib/fox.
        let config = RegistryConfig::from_toml(&text).map_err(|source| RegistryError::Config {
            name: entry.name.to_string(),
            source: Box::new(source),
        })?;
        for builder in &config.builders {
            keys.insert(format!("{}/{}", entry.name, builder.label), builder.key);
        }
    }
    Ok(keys)
}

/// `fox registry ...`.
pub(crate) fn run(cli: &Cli, command: &RegistryCommand) -> Result<ExitCode, CliError> {
    let root = &cli.root;
    let now = OffsetDateTime::now_utc();
    match command {
        RegistryCommand::Add {
            name,
            git,
            root_blake3,
        } => {
            let mut list = Registries::load(root)?;
            let entry = RegistryEntry {
                name: crate::commands::parse_name(name)?,
                git: GitUrl::new(git).map_err(|source| CliError::GitUrl {
                    url: git.clone(),
                    source,
                })?,
                root_blake3: *root_blake3,
            };
            if list.get(name).is_some() {
                return Err(RegistriesError::Exists { name: entry.name }.into());
            }
            let verified = update(root, &entry, now)?;
            list.add(entry)?;
            list.save(root)?;
            if !cli.json {
                println!(
                    "added registry {name}: {} packages, root v{}",
                    verified.package_names().len(),
                    verified.root().version
                );
            }
        }
        RegistryCommand::List => list(cli)?,
        RegistryCommand::Update { names } => {
            let list = Registries::load(root)?;
            for entry in selected(&list, names)? {
                let verified = update(root, entry, now)?;
                if !cli.json {
                    println!(
                        "updated {}: {} packages, root v{}",
                        entry.name,
                        verified.package_names().len(),
                        verified.root().version
                    );
                }
            }
        }
        RegistryCommand::Remove { name } => {
            let mut list = Registries::load(root)?;
            let entry = list
                .remove(name)
                .ok_or_else(|| RegistryError::Unknown { name: name.clone() })?;
            list.save(root)?;
            let dir = entry.state_dir(root);
            if dir.exists() {
                fs::remove_dir_all(&dir).map_err(io_error(&dir))?;
            }
            if !cli.json {
                println!("removed registry {name}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn selected<'a>(
    list: &'a Registries,
    names: &[String],
) -> Result<Vec<&'a RegistryEntry>, RegistryError> {
    if names.is_empty() {
        return Ok(list.entries().iter().collect());
    }
    names
        .iter()
        .map(|name| {
            list.get(name)
                .ok_or_else(|| RegistryError::Unknown { name: name.clone() })
        })
        .collect()
}

#[derive(Serialize)]
struct RegistryJson {
    name: String,
    git: String,
    root_blake3: String,
    updated: Option<i64>,
}

fn list(cli: &Cli) -> Result<(), CliError> {
    let list = Registries::load(&cli.root)?;
    let mut rows = Vec::new();
    for entry in list.entries() {
        let updated = load_state(&entry.state_dir(&cli.root))?.map(|saved| saved.updated);
        rows.push(RegistryJson {
            name: entry.name.to_string(),
            git: entry.git.to_string(),
            root_blake3: entry.root_blake3.to_string(),
            updated,
        });
    }
    if cli.json {
        return print_json(&rows);
    }
    for row in rows {
        let updated = row
            .updated
            .and_then(|time| OffsetDateTime::from_unix_timestamp(time).ok());
        match updated {
            Some(time) => println!("{}  {}  updated {time}", row.name, row.git),
            None => println!("{}  {}  never updated", row.name, row.git),
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct SearchJson {
    registry: String,
    name: String,
    version: Option<String>,
    summary: Option<String>,
}

/// `fox search <query>`: packages whose name or summary contains `query`.
pub(crate) fn search(cli: &Cli, query: &str) -> Result<ExitCode, CliError> {
    let policy = Policy::load(&cli.root)?;
    let now = OffsetDateTime::now_utc();
    let list = Registries::load(&cli.root)?;
    if list.entries().is_empty() {
        return Err(RegistryError::NoRegistries.into());
    }
    let query = query.to_lowercase();
    let mut rows = Vec::new();
    for registry in list.entries() {
        let verified = open(&cli.root, registry, &policy, now)?;
        for name in verified.package_names() {
            let Some(entry) = verified
                .package(&name)
                .map_err(verify_error(&registry.name))?
            else {
                continue;
            };
            let summary = entry.summary.clone().unwrap_or_default();
            if !name.as_str().contains(&query) && !summary.to_lowercase().contains(&query) {
                continue;
            }
            rows.push(SearchJson {
                registry: registry.name.to_string(),
                name: name.to_string(),
                version: entry
                    .newest_release()
                    .map(|release| release.version.to_string()),
                summary: entry.summary,
            });
        }
    }
    if cli.json {
        print_json(&rows)?;
    } else {
        for row in &rows {
            println!(
                "{}/{} {}  {}",
                row.registry,
                row.name,
                row.version.as_deref().unwrap_or("-"),
                row.summary.as_deref().unwrap_or("")
            );
        }
    }
    Ok(if rows.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[derive(Serialize)]
struct InfoJson {
    registry: String,
    name: String,
    kind: fox_index::layout::PackageKind,
    summary: Option<String>,
    git: String,
    owners: Vec<OwnerJson>,
    releases: Vec<ReleaseJson>,
}

#[derive(Serialize)]
struct OwnerJson {
    id: String,
    fingerprint: String,
}

#[derive(Serialize)]
struct ReleaseJson {
    version: String,
    tag: String,
    commit: String,
    yanked: bool,
    artifacts: Vec<ArtifactJson>,
}

#[derive(Serialize)]
struct ArtifactJson {
    target: String,
    pkgar_blake3: String,
    builders: Vec<String>,
}

/// `fox info <name>`: a package's owners, releases and artifacts.
pub(crate) fn info(cli: &Cli, spec: &str) -> Result<ExitCode, CliError> {
    let spec = PackageSpec::parse(spec).ok_or_else(|| CliError::PackageSpec {
        spec: spec.to_owned(),
    })?;
    let policy = Policy::load(&cli.root)?;
    let found = find(&cli.root, &spec, &policy, OffsetDateTime::now_utc())?;
    let mut releases = Vec::new();
    for release in found.entry.releases.iter().rev() {
        let artifacts = found
            .verified
            .artifacts(&found.entry.name, &release.version)
            .map_err(verify_error(&found.registry.name))?
            .map(|list| list.artifacts)
            .unwrap_or_default();
        releases.push(ReleaseJson {
            version: release.version.to_string(),
            tag: release.tag.clone(),
            commit: release.commit.clone(),
            yanked: release.yanked,
            artifacts: artifacts
                .into_iter()
                .map(|artifact| ArtifactJson {
                    target: artifact.target,
                    pkgar_blake3: artifact.pkgar_blake3.to_string(),
                    builders: artifact
                        .attestations
                        .into_iter()
                        .map(|attestation| attestation.builder)
                        .collect(),
                })
                .collect(),
        });
    }
    let info = InfoJson {
        registry: found.registry.name.to_string(),
        name: found.entry.name.to_string(),
        kind: found.entry.kind,
        summary: found.entry.summary.clone(),
        git: found.entry.git.to_string(),
        owners: found
            .entry
            .owners
            .iter()
            .map(|owner| OwnerJson {
                id: owner.id.clone(),
                fingerprint: owner.key.fingerprint(),
            })
            .collect(),
        releases,
    };
    if cli.json {
        print_json(&info)?;
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{}/{}  {}",
        info.registry,
        info.name,
        info.summary.as_deref().unwrap_or("")
    );
    println!("  git     {}", info.git);
    for owner in &info.owners {
        println!("  owner   {} {}", owner.id, owner.fingerprint);
    }
    for release in &info.releases {
        let yanked = if release.yanked { " (yanked)" } else { "" };
        println!("  {}{yanked}  tag {}", release.version, release.tag);
        for artifact in &release.artifacts {
            println!(
                "    {}  built by {}",
                artifact.target,
                artifact.builders.join(", ")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_specs() {
        let spec = PackageSpec::parse("ferric/ripgrep@^14").unwrap();
        assert_eq!(spec.registry.as_deref(), Some("ferric"));
        assert_eq!(spec.name.as_str(), "ripgrep");
        assert!(spec.req.matches(&semver::Version::new(14, 2, 0)));
        let spec = PackageSpec::parse("ripgrep").unwrap();
        assert_eq!(spec.registry, None);
        assert_eq!(spec.req, VersionReq::STAR);
        for not_a_spec in [
            "https://example.com/x.git",
            "./dir",
            "/abs",
            "git@host:x",
            "a/b/c",
            "x@not a req",
        ] {
            assert_eq!(PackageSpec::parse(not_a_spec), None, "{not_a_spec}");
        }
    }
}
