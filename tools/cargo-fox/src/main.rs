//! `cargo fox`, the package author's side of `fox`.
//!
//! ```text
//! cargo fox build   [options]               hermetic build into a staging tree
//! cargo fox package --key <secret> [options]
//!                                           build, pack a signed .pkgar, and
//!                                           write a signed attestation and SBOM
//! cargo fox keygen  <dir> [--name <name>]   make an unencrypted pkgar key pair
//! ```
//!
//! Builds take the committed tree at `--rev` (default `HEAD`) of the git
//! repository holding `--manifest-path`; uncommitted changes are not built.
//! Every command takes `--json`. `crates/fox-build` documents the pipeline.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use fox_build::{
    Auditable, Build, BuildError, BuildRequest, FeatureSelection, Packaged, Signer, build, package,
};
use fox_core::CpuLevel;
use fox_git::{GitError, Repository};
use serde::Serialize;

/// Cargo runs subcommands as `cargo-fox fox <args>`.
#[derive(Debug, Parser)]
#[command(name = "cargo", bin_name = "cargo")]
enum CargoCli {
    /// Build, package and attest fox packages.
    #[command(version, about)]
    Fox(FoxCli),
}

#[derive(Debug, Args)]
struct FoxCli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the hermetic build and stage the package's files.
    Build(BuildArgs),
    /// Build, then pack a signed archive with a signed attestation.
    Package {
        /// pkgar-keys secret key file that signs the archive and attestation.
        #[arg(long, value_name = "FILE")]
        key: PathBuf,
        /// Builder name recorded in the attestation.
        #[arg(long, default_value = "local")]
        label: String,
        /// Output directory (default: `target/fox` in the repository).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        #[command(flatten)]
        build: BuildArgs,
    },
    /// Generate an unencrypted pkgar-keys key pair: `<dir>/<name>.toml`
    /// (secret) and `<dir>/<name>.pub.toml` (public).
    Keygen {
        /// Directory for the key files.
        dir: PathBuf,
        /// Base name of the key files.
        #[arg(long, default_value = "fox")]
        name: String,
    },
}

#[derive(Debug, Args)]
struct BuildArgs {
    /// Path to the package's Cargo.toml.
    #[arg(long, default_value = "Cargo.toml", value_name = "PATH")]
    manifest_path: PathBuf,
    /// Package to build, when the manifest holds several.
    #[arg(long, short)]
    package: Option<String>,
    /// Git revision to build.
    #[arg(long, default_value = "HEAD")]
    rev: String,
    /// Rust target triple (default: the toolchain's host).
    #[arg(long)]
    target: Option<String>,
    /// CPU level for x86_64 targets (default: v3).
    #[arg(long, value_parser = parse_level)]
    level: Option<CpuLevel>,
    /// Cargo features to enable (comma-separated or repeated).
    #[arg(long, short = 'F', value_delimiter = ',')]
    features: Vec<String>,
    /// Do not enable the package's default features.
    #[arg(long)]
    no_default_features: bool,
    /// Repository URL to record (default: the origin remote).
    #[arg(long)]
    git_url: Option<String>,
    /// Work directory (default: a per-repository directory under the
    /// system temporary directory).
    #[arg(long, value_name = "DIR")]
    work_dir: Option<PathBuf>,
    /// CARGO_HOME for the build (default: `<work-dir>/cargo-home`).
    #[arg(long, value_name = "DIR")]
    cargo_home: Option<PathBuf>,
    /// Whether binaries embed their dependency list with cargo auditable.
    #[arg(long, value_enum, default_value_t = AuditableArg::Auto)]
    auditable: AuditableArg,
    /// Rustup toolchain to use instead of the source's rust-toolchain.toml.
    #[arg(long)]
    toolchain: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AuditableArg {
    /// Use cargo auditable when it is installed.
    Auto,
    /// Fail if cargo auditable is not installed.
    Require,
    /// Never use cargo auditable.
    Off,
}

fn parse_level(text: &str) -> Result<CpuLevel, fox_core::CpuLevelError> {
    text.parse()
}

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("cannot find {}", path.display())]
    Manifest {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not inside the work tree of its repository", path.display())]
    OutsideRepository { path: PathBuf },
    #[error("{} already exists", path.display())]
    KeyExists { path: PathBuf },
    #[error("cannot write key file {}", path.display())]
    Key {
        path: PathBuf,
        #[source]
        source: pkgar_keys::Error,
    },
    #[error("cannot create {}", path.display())]
    Dir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot write JSON output")]
    Json(#[from] serde_json::Error),
}

fn main() -> ExitCode {
    let CargoCli::Fox(cli) = CargoCli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo fox: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &FoxCli) -> Result<(), CliError> {
    match &cli.command {
        Command::Build(args) => {
            let (request, _) = request(args)?;
            let built = build(&request)?;
            report_build(cli.json, &built)
        }
        Command::Package {
            key,
            label,
            out,
            build: args,
        } => {
            let (request, repo_root) = request(args)?;
            let signer = Signer::load(key, label)?;
            let built = build(&request)?;
            let out = out
                .clone()
                .unwrap_or_else(|| repo_root.join("target").join("fox"));
            let packaged = package(&built, &signer, &out)?;
            report_package(cli.json, &built, &packaged)
        }
        Command::Keygen { dir, name } => keygen(cli.json, dir, name),
    }
}

/// The build request for `args`, and the repository's work tree root.
fn request(args: &BuildArgs) -> Result<(BuildRequest, PathBuf), CliError> {
    let manifest = args
        .manifest_path
        .canonicalize()
        .map_err(|source| CliError::Manifest {
            path: args.manifest_path.clone(),
            source,
        })?;
    let package_dir = manifest.parent().unwrap_or(Path::new("/")).to_path_buf();
    let repo = Repository::discover(&package_dir)?;
    let root = repo
        .workdir()
        .and_then(|dir| dir.canonicalize().ok())
        .ok_or_else(|| CliError::OutsideRepository {
            path: manifest.clone(),
        })?;
    let relative = package_dir
        .strip_prefix(&root)
        .map_err(|_| CliError::OutsideRepository {
            path: manifest.clone(),
        })?
        .to_path_buf();
    let work_dir = args
        .work_dir
        .clone()
        .unwrap_or_else(|| default_work_dir(&root));
    let mut request = BuildRequest::new(&root, &work_dir);
    request.rev = args.rev.clone();
    request.package_dir = relative;
    request.package = args.package.clone();
    request.target = args.target.clone();
    request.cpu_level = args.level;
    request.features = FeatureSelection {
        default_features: !args.no_default_features,
        features: args.features.clone(),
    };
    request.git_url = args.git_url.clone();
    request.cargo_home = args.cargo_home.clone();
    request.auditable = match args.auditable {
        AuditableArg::Auto => Auditable::Auto,
        AuditableArg::Require => Auditable::Require,
        AuditableArg::Off => Auditable::Off,
    };
    request.toolchain = args.toolchain.clone();
    Ok((request, root))
}

/// A work directory per repository under the system temporary directory.
/// It is kept between builds, so Cargo's target directory and downloads
/// are reused, and it lies outside the user's home, so no `.cargo/config`
/// above it can change the build.
fn default_work_dir(repo_root: &Path) -> PathBuf {
    let digest = blake3::hash(repo_root.as_os_str().as_encoded_bytes()).to_hex();
    std::env::temp_dir().join(format!("fox-build-{}", &digest[..16]))
}

#[derive(Serialize)]
struct BuildJson<'a> {
    name: &'a str,
    version: &'a str,
    commit: &'a str,
    tree_blake3: String,
    cargo_lock_blake3: String,
    target: &'a str,
    cpu_level: Option<&'static str>,
    features: &'a [String],
    feature_set_hash: String,
    toolchain_id: &'a str,
    auditable: bool,
    stage: &'a Path,
    files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pkgar_blake3: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attestation: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sbom: Option<&'a Path>,
}

impl<'a> BuildJson<'a> {
    fn new(built: &'a Build) -> Self {
        Self {
            name: built.manifest.name.as_str(),
            version: &built.manifest.version,
            commit: &built.source.commit,
            tree_blake3: built.source.tree_blake3.to_string(),
            cargo_lock_blake3: built.source.cargo_lock_blake3.to_string(),
            target: &built.built.target,
            cpu_level: built.built.cpu_level.map(CpuLevel::as_str),
            features: &built.built.features,
            feature_set_hash: built.built.feature_set_hash.to_string(),
            toolchain_id: &built.built.toolchain_id,
            auditable: built.built.auditable,
            stage: &built.stage,
            files: built.files.len(),
            archive: None,
            pkgar_blake3: None,
            attestation: None,
            sbom: None,
        }
    }
}

fn print_json(value: &impl Serialize) -> Result<(), CliError> {
    serde_json::to_writer_pretty(io::stdout().lock(), value)?;
    println!();
    Ok(())
}

fn report_build(json: bool, built: &Build) -> Result<(), CliError> {
    if json {
        return print_json(&BuildJson::new(built));
    }
    println!(
        "built {} {} at {} for {} ({} files)",
        built.manifest.name,
        built.manifest.version,
        &built.source.commit[..12.min(built.source.commit.len())],
        built.built.target,
        built.files.len()
    );
    println!("  tree_blake3 {}", built.source.tree_blake3);
    println!("  stage {}", built.stage.display());
    Ok(())
}

fn report_package(json: bool, built: &Build, packaged: &Packaged) -> Result<(), CliError> {
    if json {
        let mut out = BuildJson::new(built);
        out.archive = Some(&packaged.archive);
        out.pkgar_blake3 = Some(packaged.contents.output.pkgar_blake3.to_string());
        out.attestation = Some(&packaged.attestation);
        out.sbom = Some(&packaged.sbom);
        return print_json(&out);
    }
    println!(
        "packaged {} {} ({})",
        built.manifest.name, built.manifest.version, packaged.contents.output.pkgar_blake3
    );
    println!("  archive     {}", packaged.archive.display());
    println!("  attestation {}", packaged.attestation.display());
    println!("  sbom        {}", packaged.sbom.display());
    Ok(())
}

#[derive(Serialize)]
struct KeygenJson {
    secret: PathBuf,
    public: PathBuf,
    key: String,
}

fn keygen(json: bool, dir: &Path, name: &str) -> Result<(), CliError> {
    fs::create_dir_all(dir).map_err(|source| CliError::Dir {
        path: dir.to_path_buf(),
        source,
    })?;
    let secret_path = dir.join(format!("{name}.toml"));
    let public_path = dir.join(format!("{name}.pub.toml"));
    for path in [&secret_path, &public_path] {
        if path.exists() {
            return Err(CliError::KeyExists { path: path.clone() });
        }
    }
    let (public, secret) = pkgar_keys::SecretKeyFile::new();
    secret.save(&secret_path).map_err(|source| CliError::Key {
        path: secret_path.clone(),
        source,
    })?;
    public.save(&public_path).map_err(|source| CliError::Key {
        path: public_path.clone(),
        source,
    })?;
    let key = fox_core::KeyId::from_bytes(public.pkey).to_string();
    if json {
        return print_json(&KeygenJson {
            secret: secret_path,
            public: public_path,
            key,
        });
    }
    println!("secret key {}", secret_path.display());
    println!("public key {} ({key})", public_path.display());
    Ok(())
}
