//! `fox`, the Ferric Oxide package manager.
//!
//! It works on local `.pkgar` files and an in-place install root, and can
//! take over a root that upstream `pkg` populated.
//!
//! ```text
//! fox verify [--require-attestation] <file.pkgar>...
//!                                       check signature, every hash and the
//!                                       build attestation beside the archive
//! fox verify --installed [<name>...]    re-check installed packages
//! fox sbom <file.pkgar>                 CycloneDX JSON for a package
//! fox sbom --system [--name <name>]     CycloneDX JSON for the whole root
//! fox audit [--rustsec <dir>]           installed packages and crates that
//!                                       an advisory names
//! fox install [--force] [--allow-unsigned-metadata] [source options]
//!             <file.pkgar | git-url[#ref] | dir[#ref] | [registry/]name[@req]>...
//!                                       verify (building git sources from
//!                                       their signed release tag), then
//!                                       install into --root
//! fox build [source options] [--out <dir>] <git-url[#ref] | dir[#ref]>
//!                                       build a git source into a signed
//!                                       archive with an attestation
//! fox trust list | add-author <git-url> <key> | add-builder <label> <key>
//!         | remove-author <git-url> | remove-builder <label>
//!                                       pinned author keys and trusted
//!                                       builders (etc/fox/trust.toml)
//! fox registry add <name> <git-url> --root-blake3 <hash>
//!            | list | update [<name>...] | remove <name>
//!                                       configured package registries
//! fox search <query>                    packages in the registries
//! fox info [registry/]name              owners, releases and artifacts
//! fox sync --config <set.toml> [--lock <fox.lock>] [--locked | --update]
//!                                       make the root match a package set
//! fox list                              installed packages
//! fox remove [--force] <name>...        remove installed packages
//! fox adopt-pkg [--allow-unsigned-metadata] [--again]
//!                                       record the packages pkg installed
//! ```
//!
//! Every command takes `--root <dir>` (default `/`) and `--json`. Trusted
//! signer keys are the `pkgar-keys` public key files in
//! `<root>/etc/fox/keys/`, the root's local builder key, plus any given with
//! `--key`.
//!
//! A git source is fetched into `<root>/var/lib/fox/git/`, and its newest
//! release tag (or the tag or commit after `#`) is checked: the tag must be
//! SSH-signed by the key pinned for the repository in
//! `<root>/etc/fox/trust.toml`. The first time a repository is seen, fox
//! asks before pinning its key (trust on first use); `--trust-new-key`
//! accepts it without asking. The commit is then built hermetically with
//! `fox-build` and signed with the root's local builder key, or, with
//! `--prebuilt`, a prebuilt archive of the same commit and tree from a
//! builder listed in the trust store is installed instead.
//!
//! A package name is looked up in the registries of
//! `<root>/etc/fox/registries.toml`, verified against their signed
//! metadata. Its newest matching release is installed from a prebuilt
//! artifact that enough of the registry's builders attest to, or built from
//! its tag, which must be signed by one of the package's owners;
//! `<root>/etc/fox/policy.toml` decides which.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod audit;
mod commands;
mod cpu;
mod inventory;
mod prebuilt;
mod registry;
mod source;
mod sync;
mod trust;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// The Ferric Oxide package manager.
#[derive(Debug, Parser)]
#[command(name = "fox", version, about)]
struct Cli {
    /// Install root to act on (an image, a chroot, or `/`).
    #[arg(long, global = true, default_value = "/")]
    root: PathBuf,

    /// Also trust this pkgar public key file (repeatable).
    #[arg(long = "key", global = true, value_name = "FILE")]
    keys: Vec<PathBuf>,

    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check archive signatures and hashes, or re-check installed packages.
    Verify {
        /// Check installed packages by name (all of them if none are named).
        #[arg(long)]
        installed: bool,
        /// Fail archives that have no build attestation beside them. An
        /// attestation that is present is always checked.
        #[arg(long, conflicts_with = "installed")]
        require_attestation: bool,
        /// Archives to check, or package names with `--installed`.
        targets: Vec<String>,
    },
    /// Write a CycloneDX SBOM for an archive, or with `--system` for every
    /// package installed in the root.
    Sbom {
        /// The archive.
        #[arg(required_unless_present = "system", conflicts_with = "system")]
        archive: Option<PathBuf>,
        /// Describe the installed system instead: every package, its files
        /// and the crates in its build SBOM.
        #[arg(long)]
        system: bool,
        /// The system's name in the SBOM.
        #[arg(long, requires = "system", default_value = "ferric-oxide")]
        name: String,
        /// Output format.
        #[arg(long, value_enum, default_value_t = SbomFormat::Cyclonedx)]
        format: SbomFormat,
        /// Write to this file instead of standard output.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Verify archives, or build git sources, and install them into the
    /// root.
    Install {
        /// Replace existing files that belong to no package.
        #[arg(long)]
        force: bool,
        /// Install archives without a signed manifest, trusting the name
        /// and version in the unsigned cookbook metadata file beside them.
        #[arg(long)]
        allow_unsigned_metadata: bool,
        /// Install this prebuilt archive of the (single) git source instead
        /// of building it. Its attestation must be signed by a builder in
        /// the trust store and name the same repository, commit and tree.
        #[arg(long, value_name = "FILE")]
        prebuilt: Option<PathBuf>,
        #[command(flatten)]
        source: SourceArgs,
        /// Archives (`.pkgar` files), git URLs or local repositories, each
        /// optionally followed by `#<tag or commit>`, or registry package
        /// names, `[registry/]name[@version requirement]`.
        #[arg(required = true)]
        targets: Vec<String>,
    },
    /// Make the root match a package set (`fox-packages/1`): add its
    /// registries, write its policy, install its packages and remove those
    /// the previous lock had that it no longer lists; then write the lock.
    Sync {
        /// The package set.
        #[arg(long, value_name = "FILE")]
        config: PathBuf,
        /// The lock file (default: `fox.lock` beside the package set).
        #[arg(long, value_name = "FILE")]
        lock: Option<PathBuf>,
        /// Install only what the lock pins; fail on anything else.
        #[arg(long, conflicts_with = "update")]
        locked: bool,
        /// Resolve every package again, ignoring the lock.
        #[arg(long)]
        update: bool,
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Build a git source into a signed archive, attestation and SBOM,
    /// without installing it.
    Build {
        /// Output directory.
        #[arg(long, default_value = ".", value_name = "DIR")]
        out: PathBuf,
        #[command(flatten)]
        source: SourceArgs,
        /// A git URL or local repository, optionally followed by
        /// `#<tag or commit>`.
        #[arg(value_name = "SOURCE")]
        spec: String,
    },
    /// Show or change the trust store.
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
    /// List installed packages.
    List,
    /// Record the packages upstream `pkg` installed in the root, from the
    /// signed heads it keeps. Runs once per root, then only retries the
    /// packages it skipped (hashing their files first); files are not
    /// changed.
    AdoptPkg {
        /// Adopt packages without a signed manifest under the name `pkg`
        /// gives them.
        #[arg(long)]
        allow_unsigned_metadata: bool,
        /// Adopt every package in pkg's state, even if this root's state
        /// was adopted before.
        #[arg(long)]
        again: bool,
    },
    /// Manage package registries.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// List installed packages, and crates inside them, that a security
    /// advisory names; fails if there are any.
    Audit {
        /// Also check a checkout of the RustSec advisory database.
        #[arg(long, value_name = "DIR")]
        rustsec: Option<PathBuf>,
        /// Report every finding, ignoring the registries' VEX statements.
        #[arg(long)]
        no_vex: bool,
    },
    /// Search the registries' package names and summaries.
    Search {
        /// Text to look for.
        query: String,
    },
    /// Show a registry package's owners, releases and artifacts.
    Info {
        /// `[registry/]name`.
        name: String,
    },
    /// Remove installed packages.
    Remove {
        /// Remove even if other packages depend on it.
        #[arg(long)]
        force: bool,
        /// Packages to remove.
        #[arg(required = true)]
        names: Vec<String>,
    },
}

/// How git sources are chosen, checked and built.
#[derive(Debug, Clone, Args)]
struct SourceArgs {
    /// Accept the source's author key without asking, and pin it, when the
    /// repository has no pinned key yet.
    #[arg(long)]
    trust_new_key: bool,
    /// Build an unsigned tag, a branch or a commit (a warning is printed).
    #[arg(long)]
    allow_unsigned: bool,
    /// Rust target triple (default: the toolchain's host).
    #[arg(long)]
    target: Option<String>,
    /// x86-64 level to build at and to pick prebuilt variants for: `v3`
    /// or `v4`. Ignored for other targets. Default for `install` and
    /// `sync`: this CPU's, capped by the policy's `max-cpu-level`, when the
    /// root is the running system, else the policy's level or v3; for
    /// `build`, v3.
    #[arg(long = "level", value_name = "LEVEL")]
    cpu_level: Option<fox_core::CpuLevel>,
    /// Package to build, when the manifest holds several.
    #[arg(long, short)]
    package: Option<String>,
    /// The package's directory inside the repository.
    #[arg(long, value_name = "DIR")]
    path: Option<PathBuf>,
    /// Cargo features to enable (comma-separated or repeated).
    #[arg(long, short = 'F', value_delimiter = ',')]
    features: Vec<String>,
    /// Do not enable the package's default features.
    #[arg(long)]
    no_default_features: bool,
    /// Build work directory (default: one per source under the system
    /// temporary directory).
    #[arg(long, value_name = "DIR")]
    work_dir: Option<PathBuf>,
}

/// `fox trust` subcommands.
#[derive(Debug, Subcommand)]
enum TrustCommand {
    /// List pinned author keys and trusted builders.
    List,
    /// Pin the key that signs a repository's release tags.
    AddAuthor {
        /// The repository URL.
        git: String,
        /// An OpenSSH Ed25519 public key, or a file holding one.
        key: String,
    },
    /// Remove a repository's pinned author key.
    RemoveAuthor {
        /// The repository URL.
        git: String,
    },
    /// Trust a third-party builder's prebuilt archives.
    AddBuilder {
        /// The builder's name.
        label: String,
        /// Its pkgar public key file.
        key: PathBuf,
    },
    /// Stop trusting a builder.
    RemoveBuilder {
        /// The builder's name.
        label: String,
    },
}

/// `fox registry` subcommands.
#[derive(Debug, Subcommand)]
enum RegistryCommand {
    /// Add a registry, fetching and verifying it.
    Add {
        /// A local name for it.
        name: String,
        /// Its git repository.
        git: String,
        /// BLAKE3 of its `metadata/root/1.json`, as its maintainers
        /// publish it.
        #[arg(long)]
        root_blake3: fox_core::Blake3Digest,
    },
    /// List the configured registries.
    List,
    /// Fetch and verify registries (all of them by default).
    Update {
        /// Registries to update.
        names: Vec<String>,
    },
    /// Remove a registry and its checkout.
    Remove {
        /// The registry.
        name: String,
    },
}

/// SBOM output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SbomFormat {
    /// CycloneDX 1.5 JSON.
    Cyclonedx,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match commands::run(&cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("fox: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        super::Cli::command().debug_assert();
    }
}
