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
//! fox install [--force] [--allow-unsigned-metadata] <file.pkgar>...
//!                                       verify, then install into --root
//! fox list                              installed packages
//! fox remove [--force] <name>...        remove installed packages
//! fox adopt-pkg [--allow-unsigned-metadata] [--again]
//!                                       record the packages pkg installed
//! ```
//!
//! Every command takes `--root <dir>` (default `/`) and `--json`. Trusted
//! signer keys are the `pkgar-keys` public key files in
//! `<root>/etc/fox/keys/`, plus any given with `--key`.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod commands;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

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
    /// Write a CycloneDX SBOM for an archive.
    Sbom {
        /// The archive.
        archive: PathBuf,
        /// Output format.
        #[arg(long, value_enum, default_value_t = SbomFormat::Cyclonedx)]
        format: SbomFormat,
        /// Write to this file instead of standard output.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Verify archives and install them into the root.
    Install {
        /// Replace existing files that belong to no package.
        #[arg(long)]
        force: bool,
        /// Install archives without a signed manifest, trusting the name
        /// and version in the unsigned cookbook metadata file beside them.
        #[arg(long)]
        allow_unsigned_metadata: bool,
        /// Archives to install.
        #[arg(required = true)]
        archives: Vec<PathBuf>,
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
