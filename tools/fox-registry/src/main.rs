//! `fox-registry`: the maintainer and CI tool for fox package registries.
//!
//! It edits a registry checkout (see `fox_index::edit`) and verifies it as
//! a client would (`fox_index::verify`):
//!
//! - `init` creates a registry with fresh role keys;
//! - `admit` adds a package and delegates its file to its owners;
//! - `release` records a signed release tag, and `check` re-checks recorded
//!   releases against their repositories (what registry CI runs on a PR);
//! - `build` is the registry builder: it builds releases from their checked
//!   tags, signs the archives and attestations, and records them in the
//!   artifact lists;
//! - `configure` sets the artifact store and the builders in
//!   `registry.toml`;
//! - `advise` publishes or updates a security advisory;
//! - `yank` marks a release yanked;
//! - `set-role` rotates a role's keys through a new root;
//! - `sign` brings the metadata up to date and signs what the given keys
//!   can sign (after a merge, and on a schedule for the timestamp);
//! - `verify` runs the client checks.
//!
//! Edits only change files; run `sign` afterwards and commit the result.

#![forbid(unsafe_code)]

mod ci;
mod keys;
mod source;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use fox_core::{GitUrl, PackageName};
use fox_index::advisory::Advisory;
use fox_index::edit::{EditError, Keyring, Registry, SignOptions, SignReport};
use fox_index::layout::{Builder, Owner, PackageEntry, PackageKind};
use fox_index::metadata::{RoleKeys, RoleName, Root, Signed};
use fox_index::verify::{self, ClientState, VerifyError};
use semver::Version;
use time::OffsetDateTime;

use crate::keys::KeyError;
use crate::source::SourceError;

/// Maintain a fox package registry.
#[derive(Debug, Parser)]
#[command(name = "fox-registry", version)]
struct Cli {
    /// The registry checkout.
    #[arg(short = 'C', long, global = true, default_value = ".")]
    registry: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a registry with fresh role keys, and sign it.
    Init {
        /// The registry name clients see.
        #[arg(long)]
        name: String,
        /// Where to write the new keys (`<role>.key` and `<role>.pub`).
        #[arg(long)]
        keys: PathBuf,
        /// Use one key for every role, for a personal or test registry.
        #[arg(long)]
        single_key: bool,
        /// Also write a README and GitHub Actions workflows that check pull
        /// requests, build and sign after merges, and renew the timestamp.
        #[arg(long)]
        github: bool,
    },
    /// Add a package and delegate its file to its owners.
    Admit {
        /// The package's git repository.
        git: String,
        /// The package name.
        #[arg(long)]
        name: String,
        /// What the package is.
        #[arg(long, value_enum, default_value_t = Kind::App)]
        kind: Kind,
        /// One line on what it does.
        #[arg(long)]
        summary: Option<String>,
        /// The package's directory in the repository.
        #[arg(long)]
        path: Option<String>,
        /// An owner, as `ID=KEY`: KEY is an `ssh-ed25519` line or a file
        /// holding one.
        #[arg(long = "owner", required = true, value_name = "ID=KEY")]
        owners: Vec<String>,
    },
    /// Record a release from a signed tag of the package's repository.
    Release {
        /// The package.
        name: String,
        /// The tag, named after the version.
        tag: String,
        /// The git cache directory.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Check recorded releases against their repositories.
    Check {
        /// Packages to check; all of them if none are given.
        names: Vec<String>,
        /// The git cache directory.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Build releases and record them as artifacts (registry builders).
    Build {
        /// `name@version` releases to build; without any, every release
        /// that has no artifact from this builder yet.
        releases: Vec<String>,
        /// The builder's `pkgar-keys` secret key.
        #[arg(long)]
        key: PathBuf,
        /// The builder's label in `registry.toml`.
        #[arg(long)]
        label: String,
        /// Where to store the blobs, as `<dir>/blake3/<hash>`.
        #[arg(long)]
        blobs: PathBuf,
        /// The Rust target (default: this machine's).
        #[arg(long)]
        target: Option<String>,
        /// The git cache directory.
        #[arg(long)]
        cache: Option<PathBuf>,
        /// The build work directory (default: one per package under the
        /// system temporary directory).
        #[arg(long)]
        work_dir: Option<PathBuf>,
    },
    /// Change `registry.toml`: the artifact store and the builders.
    Configure {
        /// Replace the artifact store base URLs with these.
        #[arg(long = "artifact-url")]
        artifact_urls: Vec<String>,
        /// Add or replace a builder, as `LABEL=KEY` (KEY as for
        /// `set-role`).
        #[arg(long = "builder", value_name = "LABEL=KEY")]
        builders: Vec<String>,
        /// Remove a builder.
        #[arg(long = "remove-builder", value_name = "LABEL")]
        remove_builders: Vec<String>,
        /// How many builders must attest to an artifact.
        #[arg(long)]
        min_builders: Option<u32>,
    },
    /// Publish or update a security advisory from a RustSec-style TOML
    /// file; it is stored as `advisories/<id>.toml`.
    Advise {
        /// The advisory file.
        file: PathBuf,
    },
    /// Mark a release yanked, so new installs skip it.
    Yank {
        /// The package.
        name: String,
        /// The release.
        version: Version,
        /// Un-yank it instead.
        #[arg(long)]
        undo: bool,
    },
    /// Give a role new keys, through a new root version.
    SetRole {
        /// The role.
        #[arg(value_enum)]
        role: Role,
        /// A key: 64 hex digits, an `ssh-ed25519` line, or a public key
        /// file.
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
        /// Signatures required.
        #[arg(long, default_value_t = 1)]
        threshold: u32,
    },
    /// Update and sign the metadata.
    Sign {
        /// A secret key: a `pkgar-keys` file or an OpenSSH private key.
        #[arg(long = "key")]
        keys: Vec<PathBuf>,
        /// Give this role a new version even if nothing changed.
        #[arg(long, value_enum)]
        bump: Vec<Role>,
        /// Fail unless every file ends up fully signed.
        #[arg(long)]
        complete: bool,
    },
    /// Verify the registry as a client would.
    Verify {
        /// The root to trust; `metadata/root/1.json` if not given.
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Kind {
    App,
    Service,
    Library,
    Data,
    Recipe,
}

impl From<Kind> for PackageKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::App => Self::App,
            Kind::Service => Self::Service,
            Kind::Library => Self::Library,
            Kind::Data => Self::Data,
            Kind::Recipe => Self::Recipe,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Role {
    Root,
    Targets,
    Snapshot,
    Timestamp,
}

impl From<Role> for RoleName {
    fn from(role: Role) -> Self {
        match role {
            Role::Root => Self::Root,
            Role::Targets => Self::Targets,
            Role::Snapshot => Self::Snapshot,
            Role::Timestamp => Self::Timestamp,
        }
    }
}

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error("the registry does not verify")]
    Verify(#[from] VerifyError),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error("cannot check {name}")]
    Source {
        name: String,
        #[source]
        source: Box<SourceError>,
    },
    #[error("invalid package name")]
    Name(#[from] fox_core::NameError),
    #[error("invalid git URL")]
    GitUrl(#[from] fox_core::GitUrlError),
    #[error("--owner {value:?} is not ID=KEY")]
    OwnerSyntax { value: String },
    #[error("invalid package entry")]
    Layout(#[from] fox_index::layout::LayoutError),
    #[error("cannot read root {}", path.display())]
    RootFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid root {}", path.display())]
    RootParse {
        path: PathBuf,
        #[source]
        source: fox_index::metadata::MetadataError,
    },
    #[error("{incomplete} metadata files are not fully signed")]
    Incomplete { incomplete: usize },
    #[error("{failed} releases failed their checks")]
    ChecksFailed { failed: usize },
    #[error("no cache directory: set --cache or HOME")]
    NoCache,
    #[error(transparent)]
    Ci(Box<ci::CiError>),
    #[error("cannot read {}", path.display())]
    AdvisoryFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid advisory {}", path.display())]
    Advisory {
        path: PathBuf,
        #[source]
        source: fox_index::advisory::AdvisoryError,
    },
    #[error("--builder {value:?} is not LABEL=KEY")]
    BuilderSyntax { value: String },
    #[error("no builder {label} to remove")]
    NoBuilder { label: String },
    #[error("{value:?} is not name@version")]
    ReleaseSyntax { value: String },
    #[error("cannot write {}", path.display())]
    Template {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fox-registry: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), CliError> {
    let dir = cli.registry.as_path();
    let now = OffsetDateTime::now_utc();
    match &cli.command {
        Command::Init {
            name,
            keys,
            single_key,
            github,
        } => {
            init(dir, name, keys, *single_key, now)?;
            if *github {
                write_templates(dir)?;
            }
            Ok(())
        }
        Command::Build {
            releases,
            key,
            label,
            blobs,
            target,
            cache,
            work_dir,
        } => build(
            dir,
            &BuildArgs {
                releases,
                key,
                label,
                blobs,
                target: target.as_deref(),
                cache: cache.as_deref(),
                work_dir: work_dir.as_deref(),
            },
        ),
        Command::Admit {
            git,
            name,
            kind,
            summary,
            path,
            owners,
        } => {
            let entry = PackageEntry {
                name: PackageName::new(name.as_str())?,
                kind: (*kind).into(),
                summary: summary.clone(),
                git: GitUrl::new(git)?,
                mirrors: Vec::new(),
                path: path.clone(),
                owners: owners
                    .iter()
                    .map(|owner| parse_owner(owner))
                    .collect::<Result<_, _>>()?,
                releases: Vec::new(),
            };
            Registry::open(dir)?.admit(&entry)?;
            println!("admitted {name}; run `fox-registry sign` with the targets key");
            Ok(())
        }
        Command::Configure {
            artifact_urls,
            builders,
            remove_builders,
            min_builders,
        } => configure(
            dir,
            &ConfigureArgs {
                artifact_urls,
                builders,
                remove_builders,
                min_builders: *min_builders,
            },
        ),
        Command::Advise { file } => advise(dir, file),
        Command::Release { name, tag, cache } => release(dir, name, tag, cache.as_deref()),
        Command::Check { names, cache } => check(dir, names, cache.as_deref()),
        Command::Yank {
            name,
            version,
            undo,
        } => {
            Registry::open(dir)?.set_yanked(&PackageName::new(name.as_str())?, version, !undo)?;
            Ok(())
        }
        Command::SetRole {
            role,
            keys,
            threshold,
        } => {
            let keys = keys
                .iter()
                .map(|key| keys::parse_public(key))
                .collect::<Result<Vec<_>, _>>()?;
            let options = SignOptions::default();
            Registry::open(dir)?.set_role(
                (*role).into(),
                RoleKeys::new(keys, *threshold),
                now,
                &options.lifetimes,
            )?;
            println!("new root written; sign it with the old and the new root keys");
            Ok(())
        }
        Command::Sign {
            keys,
            bump,
            complete,
        } => {
            let mut keyring = Keyring::new();
            for path in keys {
                keyring.add(keys::load_secret(path)?);
            }
            let options = SignOptions {
                bump: bump.iter().map(|role| RoleName::from(*role)).collect(),
                ..SignOptions::default()
            };
            let report = Registry::open(dir)?.sign(&keyring, now, &options)?;
            print_report(&report);
            let incomplete = report.files.iter().filter(|file| !file.complete()).count();
            if *complete && incomplete > 0 {
                return Err(CliError::Incomplete { incomplete });
            }
            Ok(())
        }
        Command::Verify { root } => verify_registry(dir, root.as_deref(), now),
    }
}

fn init(
    dir: &Path,
    name: &str,
    keys_dir: &Path,
    single_key: bool,
    now: OffsetDateTime,
) -> Result<(), CliError> {
    let mut keyring = Keyring::new();
    let mut roles = std::collections::BTreeMap::new();
    if single_key {
        let key = keyring.add(keys::generate(keys_dir, "registry")?);
        roles = fox_index::edit::single_key_roles(key);
    } else {
        for role in RoleName::ALL {
            let key = keyring.add(keys::generate(keys_dir, role.as_str())?);
            roles.insert(role, RoleKeys::new([key], 1));
        }
    }
    let mut registry = Registry::init(dir, name, &roles, now)?;
    let report = registry.sign(&keyring, now, &SignOptions::default())?;
    print_report(&report);
    let root =
        std::fs::read(dir.join("metadata/root/1.json")).map_err(|source| CliError::RootFile {
            path: dir.join("metadata/root/1.json"),
            source,
        })?;
    println!(
        "registry {name} created; clients pin metadata/root/1.json, BLAKE3 {}",
        blake3::hash(&root).to_hex()
    );
    println!(
        "keys are in {}; keep the root key offline",
        keys_dir.display()
    );
    Ok(())
}

impl From<ci::CiError> for CliError {
    fn from(error: ci::CiError) -> Self {
        Self::Ci(Box::new(error))
    }
}

/// The repository template `init --github` writes.
const TEMPLATES: [(&str, &str); 4] = [
    ("README.md", include_str!("../template/README.md")),
    (
        ".github/workflows/check.yml",
        include_str!("../template/check.yml"),
    ),
    (
        ".github/workflows/sign.yml",
        include_str!("../template/sign.yml"),
    ),
    (
        ".github/workflows/timestamp.yml",
        include_str!("../template/timestamp.yml"),
    ),
];

fn write_templates(dir: &Path) -> Result<(), CliError> {
    for (path, text) in TEMPLATES {
        let path = dir.join(path);
        let error = |source| CliError::Template {
            path: path.clone(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(error)?;
        }
        std::fs::write(&path, text).map_err(error)?;
    }
    println!("wrote README.md and .github/workflows; add the key secrets the README lists");
    Ok(())
}

fn advise(dir: &Path, file: &Path) -> Result<(), CliError> {
    let text = std::fs::read_to_string(file).map_err(|source| CliError::AdvisoryFile {
        path: file.to_path_buf(),
        source,
    })?;
    let invalid = |source| CliError::Advisory {
        path: file.to_path_buf(),
        source,
    };
    let advisory = Advisory::parse(&text).map_err(invalid)?;
    let path = fox_index::advisory::advisory_path(&advisory.advisory.id).map_err(invalid)?;
    Registry::open(dir)?.write_advisory(&advisory)?;
    println!("wrote {path}; run `fox-registry sign` with the targets key");
    Ok(())
}

struct ConfigureArgs<'a> {
    artifact_urls: &'a [String],
    builders: &'a [String],
    remove_builders: &'a [String],
    min_builders: Option<u32>,
}

fn configure(dir: &Path, args: &ConfigureArgs<'_>) -> Result<(), CliError> {
    let registry = Registry::open(dir)?;
    let mut config = registry.config()?;
    if !args.artifact_urls.is_empty() {
        config.artifact_base_urls = args.artifact_urls.to_vec();
    }
    for label in args.remove_builders {
        let before = config.builders.len();
        config.builders.retain(|builder| &builder.label != label);
        if config.builders.len() == before {
            return Err(CliError::NoBuilder {
                label: label.clone(),
            });
        }
    }
    for value in args.builders {
        let (label, key) = value
            .split_once('=')
            .ok_or_else(|| CliError::BuilderSyntax {
                value: value.clone(),
            })?;
        let key = keys::parse_public(key)?;
        config.builders.retain(|builder| builder.label != label);
        config.builders.push(Builder {
            label: label.to_owned(),
            key,
        });
    }
    if let Some(min) = args.min_builders {
        config.min_builder_signatures = min;
    }
    registry.set_config(&config)?;
    println!("registry.toml updated; run `fox-registry sign` with the targets key");
    Ok(())
}

struct BuildArgs<'a> {
    releases: &'a [String],
    key: &'a Path,
    label: &'a str,
    blobs: &'a Path,
    target: Option<&'a str>,
    cache: Option<&'a Path>,
    work_dir: Option<&'a Path>,
}

/// The target `build` uses without `--target`: the one this binary runs
/// on, as the build's toolchain defaults to it too.
const HOST_TARGET: &str = env!("FOX_REGISTRY_HOST_TARGET");

fn build(dir: &Path, args: &BuildArgs<'_>) -> Result<(), CliError> {
    let registry = Registry::open(dir)?;
    let cache = cache_dir(args.cache)?;
    let target = args.target.unwrap_or(HOST_TARGET);
    let releases = if args.releases.is_empty() {
        missing_builds(&registry, args.label, target)?
    } else {
        args.releases
            .iter()
            .map(|value| parse_release(value))
            .collect::<Result<_, _>>()?
    };
    for (name, version) in releases {
        let work_dir = args.work_dir.map_or_else(
            || fox_build::default_work_dir(format!("fox-registry/{name}").as_bytes()),
            Path::to_path_buf,
        );
        let job = ci::BuildJob {
            name: &name,
            version: &version,
            key: args.key,
            label: args.label,
            blobs: args.blobs,
            work_dir: &work_dir,
            cache: &cache,
            target: Some(target.to_owned()),
        };
        let pkgar = ci::build(&registry, &job)?;
        println!("built {name} {version}: {pkgar}");
    }
    Ok(())
}

fn parse_release(value: &str) -> Result<(PackageName, Version), CliError> {
    let syntax = || CliError::ReleaseSyntax {
        value: value.to_owned(),
    };
    let (name, version) = value.split_once('@').ok_or_else(syntax)?;
    Ok((
        PackageName::new(name)?,
        version.parse().map_err(|_| syntax())?,
    ))
}

/// Every release, not yanked, that has no artifact for `target` attested
/// by `label`.
fn missing_builds(
    registry: &Registry,
    label: &str,
    target: &str,
) -> Result<Vec<(PackageName, Version)>, CliError> {
    let mut missing = Vec::new();
    for delegation in &registry.targets().signed.delegations {
        let name = PackageName::new(delegation.name.as_str())?;
        let Some(entry) = registry.package(&name)? else {
            continue;
        };
        for release in entry.releases.iter().filter(|release| !release.yanked) {
            let built = registry
                .artifacts(&name, &release.version)?
                .is_some_and(|list| {
                    list.artifacts.iter().any(|artifact| {
                        artifact.target == target
                            && artifact
                                .attestations
                                .iter()
                                .any(|attestation| attestation.builder == label)
                    })
                });
            if !built {
                missing.push((name.clone(), release.version.clone()));
            }
        }
    }
    Ok(missing)
}

fn parse_owner(value: &str) -> Result<Owner, CliError> {
    let (id, key) = value
        .split_once('=')
        .filter(|(id, _)| !id.is_empty())
        .ok_or_else(|| CliError::OwnerSyntax {
            value: value.to_owned(),
        })?;
    Ok(Owner {
        id: id.to_owned(),
        key: keys::parse_author(key)?,
    })
}

fn cache_dir(cache: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(cache) = cache {
        return Ok(cache.to_path_buf());
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or(CliError::NoCache)?;
    Ok(base.join("fox-registry").join("git"))
}

fn release(dir: &Path, name: &str, tag: &str, cache: Option<&Path>) -> Result<(), CliError> {
    let registry = Registry::open(dir)?;
    let package = PackageName::new(name)?;
    let entry = registry
        .package(&package)?
        .ok_or_else(|| EditError::UnknownPackage {
            name: package.clone(),
        })?;
    let source_error = |source| CliError::Source {
        name: name.to_owned(),
        source: Box::new(source),
    };
    let repo = source::fetch(&entry, &cache_dir(cache)?).map_err(source_error)?;
    let (release, owner) = source::release_from_tag(&entry, &repo, tag).map_err(source_error)?;
    let version = release.version.clone();
    registry.add_release(&package, release)?;
    println!("recorded {name} {version} (tag {tag}, signed by {owner}); an owner signs next");
    Ok(())
}

fn check(dir: &Path, names: &[String], cache: Option<&Path>) -> Result<(), CliError> {
    let registry = Registry::open(dir)?;
    let cache = cache_dir(cache)?;
    let names = if names.is_empty() {
        registry
            .targets()
            .signed
            .delegations
            .iter()
            .map(|delegation| PackageName::new(delegation.name.as_str()))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        names
            .iter()
            .map(|name| PackageName::new(name.as_str()))
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut failed = 0;
    for name in names {
        let entry = registry
            .package(&name)?
            .ok_or_else(|| EditError::UnknownPackage { name: name.clone() })?;
        if !owners_match_delegation(&registry, &entry) {
            eprintln!("{name}: owners differ from the delegation's keys");
            failed += 1;
        }
        let repo = match source::fetch(&entry, &cache) {
            Ok(repo) => repo,
            Err(error) => {
                report_failure(name.as_str(), &error);
                failed += entry.releases.len().max(1);
                continue;
            }
        };
        for release in &entry.releases {
            match source::check_release(&entry, &repo, release) {
                Ok(()) => println!("{name} {}: ok", release.version),
                Err(error) => {
                    report_failure(&format!("{name} {}", release.version), &error);
                    failed += 1;
                }
            }
        }
    }
    if failed > 0 {
        return Err(CliError::ChecksFailed { failed });
    }
    Ok(())
}

fn owners_match_delegation(registry: &Registry, entry: &PackageEntry) -> bool {
    let owners: BTreeSet<_> = entry.owners.iter().map(Owner::delegation_key).collect();
    registry
        .targets()
        .signed
        .delegations
        .iter()
        .find(|delegation| delegation.name == entry.name.as_str())
        .is_some_and(|delegation| delegation.keys.keyids == owners)
}

fn report_failure(what: &str, error: &SourceError) {
    eprintln!("{what}: {error}");
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}

fn print_report(report: &SignReport) {
    for file in &report.files {
        let state = if file.complete() {
            "signed"
        } else {
            "NEEDS SIGNATURES"
        };
        let bumped = if file.bumped { ", new version" } else { "" };
        println!(
            "{:<40} v{:<4} {}/{} {state}{bumped}",
            file.file,
            file.version,
            file.valid.min(file.threshold),
            file.threshold
        );
    }
}

fn verify_registry(dir: &Path, root: Option<&Path>, now: OffsetDateTime) -> Result<(), CliError> {
    let path = root.map_or_else(|| dir.join("metadata/root/1.json"), Path::to_path_buf);
    let bytes = std::fs::read(&path).map_err(|source| CliError::RootFile {
        path: path.clone(),
        source,
    })?;
    let root: Signed<Root> =
        Signed::from_json(&bytes).map_err(|source| CliError::RootParse { path, source })?;
    let mut state = ClientState::trust_root(root)?;
    let verified = verify::update(dir, &mut state, now)?;
    let config = verified.config()?;
    println!(
        "registry {} verifies: root v{}, {} packages, {} signed files",
        config.name,
        verified.root().version,
        verified.package_names().len(),
        verified.files().len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }
}
