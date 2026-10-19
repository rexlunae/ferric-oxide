//! `fox-registry`: the maintainer and CI tool for fox package registries.
//!
//! It edits a registry checkout (see `fox_index::edit`) and verifies it as
//! a client would (`fox_index::verify`):
//!
//! - `init` creates a registry with fresh role keys;
//! - `admit` adds a package and delegates its file to its owners;
//! - `release` records a signed release tag, and `check` re-checks recorded
//!   releases against their repositories (what registry CI runs on a PR);
//! - `yank` marks a release yanked;
//! - `set-role` rotates a role's keys through a new root;
//! - `sign` brings the metadata up to date and signs what the given keys
//!   can sign (after a merge, and on a schedule for the timestamp);
//! - `verify` runs the client checks.
//!
//! Edits only change files; run `sign` afterwards and commit the result.

#![forbid(unsafe_code)]

mod keys;
mod source;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use fox_core::{GitUrl, PackageName};
use fox_index::edit::{EditError, Keyring, Registry, SignOptions, SignReport};
use fox_index::layout::{Owner, PackageEntry, PackageKind};
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
        } => init(dir, name, keys, *single_key, now),
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
        let previous = file.previous_root.map_or_else(String::new, |old| {
            format!(
                ", {}/{} previous root keys",
                old.valid.min(old.threshold),
                old.threshold
            )
        });
        println!(
            "{:<40} v{:<4} {}/{} {state}{bumped}{previous}",
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
