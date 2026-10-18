//! Hermetic Cargo builds of git sources into signed, attested fox packages.
//!
//! This is the build pipeline of the packaging design (section 5.1).
//! [`build`] turns one commit of a Cargo package into a staging tree;
//! [`package`] packs that tree into a signed `.pkgar` and writes a signed
//! build attestation beside it:
//!
//! 1. **Source.** The commit is exported from git into an empty directory
//!    (`fox-git`), so uncommitted changes and untracked files never reach
//!    the build, and the export yields the commit's `fox-tree/1` BLAKE3 hash.
//! 2. **Lockfile.** The workspace must commit its `Cargo.lock`; its BLAKE3
//!    is recorded and every Cargo step runs with `--locked` or `--frozen`.
//! 3. **Fetch.** `cargo fetch --locked` downloads dependencies into a
//!    private `CARGO_HOME`. It is the only step given network settings;
//!    later steps run with `--frozen` and `CARGO_NET_OFFLINE`.
//! 4. **Toolchain.** The source's `rust-toolchain.toml` picks the toolchain
//!    (or [`BuildRequest::toolchain`] names one), and `toolchain_id` hashes
//!    `rustc -vV` and `cargo -vV`.
//! 5. **Build.** `cargo build --frozen --release` for the target, with
//!    `SOURCE_DATE_EPOCH` set to the commit time, `--remap-path-prefix` for
//!    the source, target and Cargo directories, and `-C target-cpu` for the
//!    CPU level (x86-64-v3 by default on x86-64, as `features.toml` sets
//!    for images). With `cargo auditable` installed (or required), binaries
//!    embed their dependency list.
//! 6. **Stage.** The installed binaries (`usr/bin/<name>`), the files from
//!    `[package.metadata.fox.files]`, the signed manifest and the CycloneDX
//!    build SBOM (`fox-sbom`) go into a staging tree.
//! 7. **Pack and attest** ([`package`]). The tree is packed with `pkgar`,
//!    the archive is re-opened and verified, and an attestation binding the
//!    source, lockfile, toolchain, target, CPU level, features and SBOM to
//!    the archive is written and signed with the same key.
//!
//! Commands run from an empty environment with a private `CARGO_HOME`, and
//! [`build`] refuses to run when a `.cargo/config.toml` above the work
//! directory, or one in `CARGO_HOME`, could change the build. Network access
//! is not blocked by a sandbox yet; that comes with sandboxed builds.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use fox_build::{BuildRequest, Signer, build, package};
//!
//! let mut request = BuildRequest::new(Path::new("."), Path::new("/tmp/fox-work"));
//! request.rev = "v1.0.0".into();
//! let built = build(&request)?;
//! let signer = Signer::load(Path::new("build/id_ed25519.toml"), "local")?;
//! let packaged = package(&built, &signer, Path::new("target/fox"))?;
//! println!("{}", packaged.archive.display());
//! # Ok::<(), fox_build::BuildError>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod cargo;
mod deps;
pub mod metadata;
mod stage;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;

use cargo_metadata::{Metadata, Package, TargetKind as CargoTargetKind};
use fox_core::archive::{Archive, ArchiveError};
use fox_core::attested::{
    AttestedError, attestation_path, signature_path, verify_archive_attestation,
};
use fox_core::{
    Attestation, AttestationError, AttestationFormat, AttestationSignature, AttestedPackage,
    Blake3Digest, BuildRecord, CpuLevel, EntryPathError, FileRecord, KeyId, Manifest, NameError,
    OutputRecord, PackageName, Profile, SourceInfo, SourceRecord, TrustedKeys, feature_set_hash,
    manifest_path, sbom_path,
};
use fox_git::{GitError, Repository};
use fox_sbom::{BuildInfo, BuildSbom, SbomError};
use pkgar_core::{Architecture, HeaderFlags, Packaging};

use crate::cargo::{Network, Stderr, Tools};
use crate::metadata::{PackageKind, PackageMetadata, TargetKind};
use crate::stage::{Stage, io_error, remove_dir_if_present};

/// A Cargo or rustc step of the build, for error reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Step {
    /// `rustc -vV` and `cargo -vV`.
    ToolchainVersion,
    /// `cargo --list`, looking for `cargo auditable`.
    ListCommands,
    /// `cargo metadata --no-deps`, finding the workspace and package.
    Workspace,
    /// `cargo fetch`.
    Fetch,
    /// `cargo metadata` with dependencies.
    Metadata,
    /// `cargo build`.
    Build,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ToolchainVersion => "rustc -vV",
            Self::ListCommands => "cargo --list",
            Self::Workspace => "cargo metadata --no-deps",
            Self::Fetch => "cargo fetch",
            Self::Metadata => "cargo metadata",
            Self::Build => "cargo build",
        })
    }
}

/// Why a build or packaging step failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// Reading the source repository failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// A file or directory could not be read or written.
    #[error("cannot access {}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A tool could not be started.
    #[error("cannot run {step}")]
    Spawn {
        /// The step.
        step: Step,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A tool exited with an error.
    #[error("{step} failed ({status}){}", if stderr.is_empty() { String::new() } else { format!(":\n{stderr}") })]
    Command {
        /// The step.
        step: Step,
        /// Its exit status.
        status: ExitStatus,
        /// Its standard error, when it was captured.
        stderr: String,
    },
    /// `rustc -vV` did not print a release and host.
    #[error("cannot read the toolchain version from rustc -vV output {output:?}")]
    ToolchainVersion {
        /// What rustc printed.
        output: String,
    },
    /// `cargo metadata` printed something that is not its JSON.
    #[error("cannot parse cargo metadata output")]
    MetadataJson(#[source] cargo_metadata::Error),
    /// `cargo metadata` gave no resolved dependency graph.
    #[error("cargo metadata has no dependency graph")]
    NoResolve,
    /// The package directory is not a relative path inside the repository.
    #[error("package directory {} must be relative and stay inside the repository", path.display())]
    PackageDir {
        /// The rejected path.
        path: PathBuf,
    },
    /// There is no `Cargo.toml` in the package directory of the commit.
    #[error("the commit has no {}", path.display())]
    NoManifest {
        /// Where it was expected, relative to the repository.
        path: PathBuf,
    },
    /// No package matches the request.
    #[error("no package {name:?} in the workspace")]
    NoPackage {
        /// The requested name, or the manifest path when none was given.
        name: String,
    },
    /// The workspace has no `Cargo.lock` in the commit.
    #[error("the commit has no Cargo.lock at {}; commit it so builds are reproducible", path.display())]
    MissingLockfile {
        /// Where it was expected, relative to the repository.
        path: PathBuf,
    },
    /// `Cargo.lock` does not parse.
    #[error("cannot parse Cargo.lock")]
    Lockfile(#[source] toml::de::Error),
    /// A path dependency lies outside the exported source tree.
    #[error("path dependency {} is outside the source tree", path.display())]
    PathOutsideSource {
        /// Its manifest.
        path: PathBuf,
    },
    /// A Cargo configuration outside the source could change the build.
    #[error("{} could change the build; build in a directory without one above it", path.display())]
    OutsideCargoConfig {
        /// The configuration file.
        path: PathBuf,
    },
    /// The `[package.metadata.fox]` table is invalid.
    #[error("invalid [package.metadata.fox] in {package}")]
    Metadata {
        /// The crate.
        package: String,
        /// The schema error.
        #[source]
        source: serde_json::Error,
    },
    /// The crate name is not a valid fox package name.
    #[error("crate name {name:?} is not a valid package name")]
    PackageName {
        /// The crate name.
        name: String,
        /// Why it was rejected.
        #[source]
        source: NameError,
    },
    /// The package is a cookbook recipe.
    #[error("{package} is a recipe package; recipes build through the cookbook")]
    RecipeKind {
        /// The package.
        package: String,
    },
    /// The package does not declare native builds.
    #[error("{package} does not list \"native\" in [package.metadata.fox] targets")]
    NativeNotDeclared {
        /// The package.
        package: String,
    },
    /// A CPU level was given for a target that is not x86-64.
    #[error("CPU level {level} applies to x86_64 targets, not {target}")]
    LevelNeedsX86_64 {
        /// The level.
        level: CpuLevel,
        /// The target.
        target: String,
    },
    /// `binaries` names something that is not a binary target.
    #[error("{package} has no binary target {binary:?}")]
    UnknownBinary {
        /// The package.
        package: String,
        /// The name.
        binary: String,
    },
    /// A `files` source path is not a relative path inside the package.
    #[error("file {path:?} must be a relative path inside the package")]
    FileSource {
        /// The rejected path.
        path: String,
    },
    /// A file to install is not a regular file.
    #[error("{} is not a regular file", path.display())]
    NotAFile {
        /// The path.
        path: PathBuf,
    },
    /// An install path is not a safe archive path.
    #[error("install path {path:?} is not allowed")]
    InstallPath {
        /// The path.
        path: String,
        /// Why.
        #[source]
        source: EntryPathError,
    },
    /// Two files would be installed at one path, or one beneath another.
    #[error("install path {path:?} clashes with another file of the package")]
    DuplicateInstallPath {
        /// The path.
        path: String,
    },
    /// `cargo auditable` was required but is not installed.
    #[error("cargo auditable is required but not installed (cargo install cargo-auditable)")]
    AuditableMissing,
    /// The SBOM could not be built.
    #[error("cannot build the SBOM")]
    Sbom(#[from] SbomError),
    /// The manifest could not be serialized.
    #[error("cannot write the package manifest")]
    Manifest(#[source] fox_core::ManifestError),
    /// The signing key could not be loaded.
    #[error("cannot load signing key {}", path.display())]
    Key {
        /// The key file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: pkgar_keys::Error,
    },
    /// A key file to be generated already exists.
    #[error("key file {} already exists", path.display())]
    KeyExists {
        /// The file.
        path: PathBuf,
    },
    /// Packing the archive failed.
    #[error("cannot pack {}", path.display())]
    Pack {
        /// The archive.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<pkgar::Error>,
    },
    /// The packed archive does not verify.
    #[error("the packed archive does not verify")]
    Archive(#[source] Box<ArchiveError>),
    /// The attestation could not be written or signed.
    #[error("cannot write the attestation")]
    Attestation(#[source] AttestationError),
    /// The written attestation does not verify against the archive.
    #[error("the written attestation does not verify")]
    SelfCheck(#[source] Box<AttestedError>),
    /// The build timestamp could not be formatted.
    #[error("cannot format the build time")]
    Time(#[source] time::error::Format),
}

/// Whether binaries embed their dependency list with `cargo auditable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Auditable {
    /// Use it when it is installed. The attestation records which.
    #[default]
    Auto,
    /// Fail the build if it is not installed.
    Require,
    /// Never use it.
    Off,
}

/// Which Cargo features to enable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureSelection {
    /// Whether the package's `default` feature is enabled.
    pub default_features: bool,
    /// Further features of the package.
    pub features: Vec<String>,
}

impl Default for FeatureSelection {
    fn default() -> Self {
        Self {
            default_features: true,
            features: Vec::new(),
        }
    }
}

/// What to build.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BuildRequest {
    /// The repository, or any path inside its work tree.
    pub repository: PathBuf,
    /// The revision to build (default `HEAD`).
    pub rev: String,
    /// The package's directory relative to the repository root (default:
    /// the root).
    pub package_dir: PathBuf,
    /// The package name, when the directory's manifest is a virtual
    /// workspace or holds several packages.
    pub package: Option<String>,
    /// The Rust target triple (default: the toolchain's host).
    pub target: Option<String>,
    /// The CPU level, for x86-64 targets (default: x86-64-v3).
    pub cpu_level: Option<CpuLevel>,
    /// The Cargo features.
    pub features: FeatureSelection,
    /// The repository URL to record (default: the `origin` remote, else a
    /// `file://` URL of the repository).
    pub git_url: Option<String>,
    /// Working directory for the exported source, Cargo's target directory
    /// and the stage. The source and stage are recreated on every build.
    pub work_dir: PathBuf,
    /// `CARGO_HOME` for the build (default: `<work_dir>/cargo-home`). It
    /// only caches downloads, so it can be shared between builds.
    pub cargo_home: Option<PathBuf>,
    /// `cargo auditable` use.
    pub auditable: Auditable,
    /// A rustup toolchain name to use instead of the source's own choice.
    pub toolchain: Option<String>,
}

impl BuildRequest {
    /// A request to build `HEAD` of the repository containing `repository`,
    /// working in `work_dir`, with every other setting at its default.
    pub fn new(repository: &Path, work_dir: &Path) -> Self {
        Self {
            repository: repository.to_path_buf(),
            rev: "HEAD".to_owned(),
            package_dir: PathBuf::new(),
            package: None,
            target: None,
            cpu_level: None,
            features: FeatureSelection::default(),
            git_url: None,
            work_dir: work_dir.to_path_buf(),
            cargo_home: None,
            auditable: Auditable::default(),
            toolchain: None,
        }
    }
}

/// How an artifact was built: the build half of its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltWith {
    /// Rust target triple.
    pub target: String,
    /// CPU level, for x86-64 targets.
    pub cpu_level: Option<CpuLevel>,
    /// The package's enabled features after resolution, sorted.
    pub features: Vec<String>,
    /// [`feature_set_hash`] of the features and lockfile.
    pub feature_set_hash: Blake3Digest,
    /// Toolchain id.
    pub toolchain_id: String,
    /// Cargo profile.
    pub profile: Profile,
    /// Whether `cargo auditable` was used.
    pub auditable: bool,
}

/// A finished build: a staging tree ready to pack.
///
/// The stage lives in the request's work directory, so it is valid until
/// the next build in the same directory replaces it.
#[derive(Debug, Clone)]
pub struct Build {
    /// The staging tree.
    pub stage: PathBuf,
    /// The package manifest, also staged.
    pub manifest: Manifest,
    /// The source.
    pub source: SourceRecord,
    /// How it was built.
    pub built: BuiltWith,
    /// The CycloneDX SBOM, also staged at [`sbom_path`].
    pub sbom: Vec<u8>,
    /// Every staged file.
    pub files: Vec<FileRecord>,
}

/// A key that signs archives and attestations.
#[derive(Debug, Clone)]
pub struct Signer {
    label: String,
    secret_path: PathBuf,
    secret: [u8; 64],
    key: KeyId,
}

impl Signer {
    /// Loads a `pkgar-keys` secret key file, asking for its passphrase if
    /// it is encrypted. `label` names the builder in attestations.
    ///
    /// # Errors
    ///
    /// [`BuildError::Key`] if the file cannot be read or decrypted.
    pub fn load(path: &Path, label: &str) -> Result<Self, BuildError> {
        let key_error = |source| BuildError::Key {
            path: path.to_path_buf(),
            source,
        };
        let file = pkgar_keys::get_skey(path).map_err(key_error)?;
        let (Some(secret), Some(public)) = (file.secret_key(), file.public_key()) else {
            return Err(key_error(pkgar_keys::Error::PassphraseRequired));
        };
        Ok(Self {
            label: label.to_owned(),
            secret_path: path.to_path_buf(),
            secret,
            key: KeyId::from_bytes(public),
        })
    }

    /// Generates an unencrypted key pair, writes the secret key to
    /// `secret_path` (mode `0600`) and the public key to `public_path`, and
    /// returns a signer for it.
    ///
    /// # Errors
    ///
    /// [`BuildError::KeyExists`] if either file exists, [`BuildError::Io`]
    /// if the directory cannot be created and [`BuildError::Key`] if a file
    /// cannot be written.
    pub fn generate(
        secret_path: &Path,
        public_path: &Path,
        label: &str,
    ) -> Result<Self, BuildError> {
        for path in [secret_path, public_path] {
            if path.exists() {
                return Err(BuildError::KeyExists {
                    path: path.to_path_buf(),
                });
            }
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(io_error(dir))?;
            }
        }
        let (public, secret) = pkgar_keys::SecretKeyFile::new();
        let save_error = |path: &Path| {
            let path = path.to_path_buf();
            move |source| BuildError::Key { path, source }
        };
        secret.save(secret_path).map_err(save_error(secret_path))?;
        public.save(public_path).map_err(save_error(public_path))?;
        Self::load(secret_path, label)
    }

    /// Loads the key at `secret_path`, first generating it (as
    /// [`Signer::generate`]) if neither it nor `public_path` exists.
    ///
    /// # Errors
    ///
    /// As [`Signer::load`] and [`Signer::generate`].
    pub fn load_or_generate(
        secret_path: &Path,
        public_path: &Path,
        label: &str,
    ) -> Result<Self, BuildError> {
        if secret_path.exists() || public_path.exists() {
            Self::load(secret_path, label)
        } else {
            Self::generate(secret_path, public_path, label)
        }
    }

    /// The public key.
    pub fn key(&self) -> KeyId {
        self.key
    }

    /// The builder label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// A key set trusting only this key, under its label.
    pub fn trusted(&self) -> TrustedKeys {
        let mut keys = TrustedKeys::new();
        keys.insert(self.label.clone(), self.key);
        keys
    }
}

/// The files [`package`] wrote.
#[derive(Debug, Clone)]
pub struct Packaged {
    /// The `.pkgar` archive.
    pub archive: PathBuf,
    /// The attestation, `<stem>.attest.toml`.
    pub attestation: PathBuf,
    /// Its signature, `<stem>.attest.toml.sig`.
    pub signature: PathBuf,
    /// A copy of the SBOM, `<stem>.cdx.json`.
    pub sbom: PathBuf,
    /// The attestation's contents.
    pub contents: Attestation,
}

/// A default work directory for builds of the source named by `key` (a
/// repository path or URL): a directory per source under the system
/// temporary directory. Reusing it keeps Cargo's target directory and
/// downloads between builds, and it lies outside the user's home, so no
/// `.cargo/config.toml` above it can change the build.
pub fn default_work_dir(key: &[u8]) -> PathBuf {
    let digest = blake3::hash(key).to_hex();
    std::env::temp_dir().join(format!("fox-build-{}", &digest[..16]))
}

/// The host triple of the toolchain a build in `dir` would use with
/// `toolchain` (or, without one, the toolchain rustup selects there): the
/// target [`build`] builds for when [`BuildRequest::target`] is `None`.
/// Callers that record the target before building (a lock file, say)
/// resolve it with this and pass it explicitly.
///
/// # Errors
///
/// [`BuildError::Spawn`] or [`BuildError::Command`] if `rustc -vV` or
/// `cargo -vV` cannot run, and [`BuildError::ToolchainVersion`] if rustc's
/// output lacks a host.
pub fn toolchain_host(dir: &Path, toolchain: Option<&str>) -> Result<String, BuildError> {
    // -vV reads no Cargo configuration, so a throwaway CARGO_HOME will do.
    let cargo_home = dir.join(".fox-cargo-home");
    Tools::new(dir, &cargo_home, toolchain)
        .toolchain_info()
        .map(|info| info.host)
}

/// Runs the hermetic build described by `request`.
///
/// # Errors
///
/// [`BuildError`] for the first step that fails; see its variants.
pub fn build(request: &BuildRequest) -> Result<Build, BuildError> {
    let repo = Repository::discover(&request.repository)?;
    let commit = repo.resolve(&request.rev)?;
    let git_url = request
        .git_url
        .clone()
        .or_else(|| repo.origin_url())
        .unwrap_or_else(|| local_url(&repo, &request.repository));
    let package_dir = checked_package_dir(&request.package_dir)?;

    let work = &request.work_dir;
    fs::create_dir_all(work).map_err(io_error(work))?;
    let work = work.canonicalize().map_err(io_error(work))?;
    let cargo_home = match &request.cargo_home {
        Some(home) => {
            fs::create_dir_all(home).map_err(io_error(home))?;
            home.canonicalize().map_err(io_error(home))?
        }
        None => work.join("cargo-home"),
    };
    check_cargo_configs(&work, &cargo_home)?;

    let src = work.join("src");
    remove_dir_if_present(&src)?;
    let export = repo.export(&commit, &src)?;
    let pkg_dir = src.join(&package_dir);
    if !pkg_dir.join("Cargo.toml").is_file() {
        return Err(BuildError::NoManifest {
            path: package_dir.join("Cargo.toml"),
        });
    }

    let mut tools = Tools::new(&pkg_dir, &cargo_home, request.toolchain.as_deref());
    let toolchain = tools.toolchain_info()?;
    let target = request.target.clone().unwrap_or(toolchain.host);
    let cpu_level = cpu_level(&target, request.cpu_level)?;

    let workspace = workspace(&tools, &pkg_dir)?;
    let package = select_package(&workspace, request.package.as_deref(), &pkg_dir)?;
    let fox = PackageMetadata::from_cargo_metadata(&package.metadata).map_err(|source| {
        BuildError::Metadata {
            package: package.name.to_string(),
            source,
        }
    })?;
    check_kind(&package, &fox)?;
    let binaries = binaries(&package, &fox)?;
    let lock_path = workspace.workspace_root.as_std_path().join("Cargo.lock");
    let lock = fs::read_to_string(&lock_path).map_err(|_| BuildError::MissingLockfile {
        path: relative_to(&lock_path, &src),
    })?;
    let cargo_lock_blake3 = Blake3Digest::of(lock.as_bytes());
    let manifest_arg = package.manifest_path.as_std_path().to_path_buf();

    fetch(&tools, &manifest_arg, &target)?;
    let resolved = resolved_metadata(&tools, &manifest_arg, &target, &request.features)?;
    check_paths_inside(&resolved, &src)?;
    let features = enabled_features(&resolved, &package)?;
    let graph = deps::crate_graph(&resolved, &package.id, &lock, &src)?;
    let feature_set_hash = feature_set_hash(&features, &cargo_lock_blake3);
    let auditable = use_auditable(&tools, request.auditable)?;

    let target_dir = work.join("target");
    if !binaries.is_empty() {
        tools.set_env("CARGO_TARGET_DIR", &target_dir);
        tools.set_env("SOURCE_DATE_EPOCH", commit.committer_time().to_string());
        tools.set_env("CARGO_INCREMENTAL", "0");
        tools.set_env(
            "CARGO_ENCODED_RUSTFLAGS",
            rustflags(&src, &target_dir, &cargo_home, cpu_level),
        );
        cargo_build(
            &tools,
            &manifest_arg,
            &target,
            &binaries,
            &request.features,
            auditable,
        )?;
    }

    let manifest = package_manifest(&package, &fox, &target, &git_url, &commit.id())?;
    let source = SourceRecord {
        git: git_url,
        commit: commit.id(),
        tree_blake3: export.tree_blake3,
        cargo_lock_blake3,
        path: (!package_dir.as_os_str().is_empty())
            .then(|| package_dir.to_string_lossy().into_owned()),
    };
    let built = BuiltWith {
        target,
        cpu_level,
        features,
        feature_set_hash,
        toolchain_id: toolchain.id,
        profile: Profile::Release,
        auditable,
    };
    let release_dir = target_dir.join(&built.target).join("release");
    let mut stage = Stage::create(&work.join("stage"))?;
    for binary in &binaries {
        stage.copy(
            &format!("usr/bin/{binary}"),
            &release_dir.join(binary),
            0o755,
        )?;
    }
    for (from, to) in &fox.files {
        stage.copy(to, &package_file(package_dir_abs(&package), from)?, 0o644)?;
    }
    let manifest_text = manifest.to_toml().map_err(BuildError::Manifest)?;
    stage.write(
        &manifest_path(&manifest.name),
        manifest_text.as_bytes(),
        0o644,
    )?;
    let sbom = write_sbom(&mut stage, &manifest, &source, &built, &graph)?;
    Ok(Build {
        stage: stage.dir().to_path_buf(),
        manifest,
        source,
        built,
        sbom,
        files: stage.records()?,
    })
}

/// Packs `build` into `out_dir` as `<stem>.pkgar`, signs it with `signer`,
/// and writes the signed attestation and a copy of the SBOM beside it.
///
/// The stem is `<name>-<version>-<target>[-<level>]-<features>`, where
/// `<features>` is the first 12 hex digits of the feature-set hash.
///
/// # Errors
///
/// [`BuildError::Pack`] if packing fails, [`BuildError::Archive`] if the
/// archive does not verify, [`BuildError::Attestation`] or
/// [`BuildError::SelfCheck`] if the attestation cannot be written or does not
/// verify, and [`BuildError::Io`] for file errors.
pub fn package(build: &Build, signer: &Signer, out_dir: &Path) -> Result<Packaged, BuildError> {
    fs::create_dir_all(out_dir).map_err(io_error(out_dir))?;
    let stem = artifact_stem(build);
    let archive_path = out_dir.join(format!("{stem}.pkgar"));
    pkgar::create_with_flags(
        &signer.secret_path,
        &archive_path,
        &build.stage,
        HeaderFlags::latest(Architecture::Independent, Packaging::LZMA2),
    )
    .map_err(|source| BuildError::Pack {
        path: archive_path.clone(),
        source: Box::new(source),
    })?;
    let archive = Archive::open(&archive_path, &signer.trusted())
        .map_err(|source| BuildError::Archive(Box::new(source)))?;

    let contents = attestation(build, signer, &archive)?;
    let text = contents.to_toml().map_err(BuildError::Attestation)?;
    let signature = AttestationSignature::sign(text.as_bytes(), &signer.secret)
        .map_err(BuildError::Attestation)?;
    let attestation = attestation_path(&archive_path);
    let sig_path = signature_path(&attestation);
    fs::write(&attestation, &text).map_err(io_error(&attestation))?;
    let sig_text = signature.to_toml().map_err(BuildError::Attestation)?;
    fs::write(&sig_path, sig_text).map_err(io_error(&sig_path))?;
    let sbom = out_dir.join(format!("{stem}.cdx.json"));
    fs::write(&sbom, &build.sbom).map_err(io_error(&sbom))?;
    verify_archive_attestation(&archive, &attestation, &signer.trusted())
        .map_err(|source| BuildError::SelfCheck(Box::new(source)))?;
    Ok(Packaged {
        archive: archive_path,
        attestation,
        signature: sig_path,
        sbom,
        contents,
    })
}

fn attestation(
    build: &Build,
    signer: &Signer,
    archive: &Archive,
) -> Result<Attestation, BuildError> {
    let sbom_blake3 = Blake3Digest::of(&build.sbom);
    let built = &build.built;
    let now = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let built_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(BuildError::Time)?;
    Ok(Attestation {
        format: AttestationFormat::V1,
        package: AttestedPackage {
            name: build.manifest.name.clone(),
            version: build.manifest.version.clone(),
        },
        source: build.source.clone(),
        build: BuildRecord {
            target: built.target.clone(),
            cpu_level: built.cpu_level,
            features: built.features.clone(),
            feature_set_hash: built.feature_set_hash,
            toolchain_id: built.toolchain_id.clone(),
            profile: built.profile,
            auditable: built.auditable,
            builder: signer.label.clone(),
            builder_key: signer.key,
            built_at: built_at
                .parse()
                .expect("an RFC 3339 timestamp is a TOML datetime"),
        },
        output: OutputRecord {
            pkgar_blake3: archive.blake3(),
            pkgar_size: archive.size(),
            pkgar_header_signer: archive.head().signer().key,
            entries_blake3: archive.head().entries_blake3(),
            sbom_blake3,
        },
    })
}

/// `<name>-<version>-<target>[-<level>]-<12 hex digits of the feature-set hash>`.
fn artifact_stem(build: &Build) -> String {
    let built = &build.built;
    let level = built
        .cpu_level
        .map(|level| format!("-{level}"))
        .unwrap_or_default();
    let features = built.feature_set_hash.to_string();
    format!(
        "{}-{}-{}{level}-{}",
        build.manifest.name,
        build.manifest.version,
        built.target,
        &features[..12]
    )
}

/// A `file://` URL for a repository without an `origin` remote.
fn local_url(repo: &Repository, fallback: &Path) -> String {
    let path = repo.workdir().unwrap_or(fallback);
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", path.display())
}

fn checked_package_dir(dir: &Path) -> Result<PathBuf, BuildError> {
    if dir
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        Ok(dir
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect())
    } else {
        Err(BuildError::PackageDir {
            path: dir.to_path_buf(),
        })
    }
}

/// Refuses Cargo configuration files the build would read from outside the
/// source: in any directory above the work directory, or in `CARGO_HOME`.
fn check_cargo_configs(work: &Path, cargo_home: &Path) -> Result<(), BuildError> {
    let candidates = work
        .ancestors()
        .flat_map(|dir| [dir.join(".cargo/config.toml"), dir.join(".cargo/config")])
        .chain([cargo_home.join("config.toml"), cargo_home.join("config")]);
    for path in candidates {
        if path.exists() {
            return Err(BuildError::OutsideCargoConfig { path });
        }
    }
    Ok(())
}

fn cpu_level(target: &str, requested: Option<CpuLevel>) -> Result<Option<CpuLevel>, BuildError> {
    let x86_64 = target.starts_with("x86_64-");
    match requested {
        Some(level) if !x86_64 => Err(BuildError::LevelNeedsX86_64 {
            level,
            target: target.to_owned(),
        }),
        Some(level) => Ok(Some(level)),
        None => Ok(x86_64.then_some(CpuLevel::X86_64V3)),
    }
}

fn parse_metadata(output: &[u8]) -> Result<Metadata, BuildError> {
    let text = String::from_utf8_lossy(output);
    cargo_metadata::MetadataCommand::parse(text.as_ref()).map_err(BuildError::MetadataJson)
}

/// The workspace layout, without resolving dependencies.
fn workspace(tools: &Tools, pkg_dir: &Path) -> Result<Metadata, BuildError> {
    let mut command = tools.command("cargo", Network::Denied);
    command
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(pkg_dir.join("Cargo.toml"));
    let output = tools.run(Step::Workspace, command, Stderr::Capture)?;
    parse_metadata(&output.stdout)
}

fn select_package(
    workspace: &Metadata,
    name: Option<&str>,
    pkg_dir: &Path,
) -> Result<Package, BuildError> {
    let members = workspace.workspace_packages();
    let found = match name {
        Some(name) => members.into_iter().find(|p| p.name.as_str() == name),
        None => {
            let manifest = pkg_dir.join("Cargo.toml");
            members
                .into_iter()
                .find(|p| p.manifest_path.as_std_path() == manifest)
        }
    };
    found.cloned().ok_or_else(|| BuildError::NoPackage {
        name: name.map_or_else(
            || pkg_dir.join("Cargo.toml").display().to_string(),
            str::to_owned,
        ),
    })
}

fn check_kind(package: &Package, fox: &PackageMetadata) -> Result<(), BuildError> {
    if fox.kind == PackageKind::Recipe {
        return Err(BuildError::RecipeKind {
            package: package.name.to_string(),
        });
    }
    if !fox.allows(TargetKind::Native) {
        return Err(BuildError::NativeNotDeclared {
            package: package.name.to_string(),
        });
    }
    Ok(())
}

/// The binary targets to build and install.
fn binaries(package: &Package, fox: &PackageMetadata) -> Result<Vec<String>, BuildError> {
    let bins: Vec<&str> = package
        .targets
        .iter()
        .filter(|t| t.kind.contains(&CargoTargetKind::Bin))
        .map(|t| t.name.as_str())
        .collect();
    match &fox.binaries {
        None => Ok(bins.iter().map(|b| (*b).to_owned()).collect()),
        Some(wanted) => wanted
            .iter()
            .map(|binary| {
                if bins.contains(&binary.as_str()) {
                    Ok(binary.clone())
                } else {
                    Err(BuildError::UnknownBinary {
                        package: package.name.to_string(),
                        binary: binary.clone(),
                    })
                }
            })
            .collect(),
    }
}

fn feature_args(features: &FeatureSelection) -> Vec<OsString> {
    let mut args = Vec::new();
    if !features.default_features {
        args.push("--no-default-features".into());
    }
    if !features.features.is_empty() {
        args.push("--features".into());
        args.push(features.features.join(",").into());
    }
    args
}

fn fetch(tools: &Tools, manifest: &Path, target: &str) -> Result<(), BuildError> {
    let mut command = tools.command("cargo", Network::Allowed);
    command
        .args(["fetch", "--locked", "--target", target, "--manifest-path"])
        .arg(manifest);
    tools.run(Step::Fetch, command, Stderr::Inherit)?;
    Ok(())
}

fn resolved_metadata(
    tools: &Tools,
    manifest: &Path,
    target: &str,
    features: &FeatureSelection,
) -> Result<Metadata, BuildError> {
    let mut command = tools.command("cargo", Network::Denied);
    command
        .args(["metadata", "--format-version", "1", "--frozen"])
        .args(["--filter-platform", target, "--manifest-path"])
        .arg(manifest)
        .args(feature_args(features));
    let output = tools.run(Step::Metadata, command, Stderr::Capture)?;
    parse_metadata(&output.stdout)
}

/// Refuses path dependencies outside the exported tree: they would bring
/// files from the build host that no hash covers.
fn check_paths_inside(metadata: &Metadata, src: &Path) -> Result<(), BuildError> {
    for package in &metadata.packages {
        let path = package.manifest_path.as_std_path();
        if package.source.is_none() && !path.starts_with(src) {
            return Err(BuildError::PathOutsideSource {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn enabled_features(metadata: &Metadata, package: &Package) -> Result<Vec<String>, BuildError> {
    let resolve = metadata.resolve.as_ref().ok_or(BuildError::NoResolve)?;
    let node = resolve
        .nodes
        .iter()
        .find(|node| node.id == package.id)
        .ok_or(BuildError::NoResolve)?;
    let mut features: Vec<String> = node.features.iter().map(|f| f.to_string()).collect();
    features.sort();
    Ok(features)
}

fn use_auditable(tools: &Tools, mode: Auditable) -> Result<bool, BuildError> {
    match mode {
        Auditable::Off => Ok(false),
        Auditable::Auto => tools.has_auditable(),
        Auditable::Require => match tools.has_auditable()? {
            true => Ok(true),
            false => Err(BuildError::AuditableMissing),
        },
    }
}

/// `CARGO_ENCODED_RUSTFLAGS` for the build: path remapping so no build-host
/// path ends up in the output, and the CPU level. The source is listed last
/// because rustc applies the last matching prefix.
fn rustflags(
    src: &Path,
    target_dir: &Path,
    cargo_home: &Path,
    level: Option<CpuLevel>,
) -> OsString {
    let mut flags: Vec<OsString> = Vec::new();
    for (from, to) in [
        (cargo_home, "/fox/cargo"),
        (target_dir, "/fox/target"),
        (src, "/fox/source"),
    ] {
        let mut flag = OsString::from("--remap-path-prefix=");
        flag.push(from);
        flag.push("=");
        flag.push(to);
        flags.push(flag);
    }
    if let Some(level) = level {
        flags.push(format!("-Ctarget-cpu={}", level.target_cpu()).into());
    }
    flags.join(std::ffi::OsStr::new("\x1f"))
}

fn cargo_build(
    tools: &Tools,
    manifest: &Path,
    target: &str,
    binaries: &[String],
    features: &FeatureSelection,
    auditable: bool,
) -> Result<(), BuildError> {
    let mut command = tools.command("cargo", Network::Denied);
    if auditable {
        command.arg("auditable");
    }
    command
        .args([
            "build",
            "--frozen",
            "--release",
            "--target",
            target,
            "--manifest-path",
        ])
        .arg(manifest);
    for binary in binaries {
        command.args(["--bin", binary]);
    }
    command.args(feature_args(features));
    tools.run(Step::Build, command, Stderr::Inherit)?;
    Ok(())
}

fn package_manifest(
    package: &Package,
    fox: &PackageMetadata,
    target: &str,
    git: &str,
    commit: &str,
) -> Result<Manifest, BuildError> {
    let name =
        PackageName::new(package.name.as_str()).map_err(|source| BuildError::PackageName {
            name: package.name.to_string(),
            source,
        })?;
    let mut manifest = Manifest::new(name, package.version.to_string());
    manifest.target = Some(target.to_owned());
    manifest.summary = fox.summary.clone().or_else(|| package.description.clone());
    manifest.license = package.license.clone();
    manifest.depends = fox.runtime_deps.clone();
    manifest.source = Some(SourceInfo {
        git: git.to_owned(),
        commit: Some(commit.to_owned()),
    });
    Ok(manifest)
}

fn package_dir_abs(package: &Package) -> &Path {
    package
        .manifest_path
        .as_std_path()
        .parent()
        .expect("a manifest path has a parent directory")
}

/// A `[package.metadata.fox.files]` source, which must stay inside the
/// package directory.
fn package_file(dir: &Path, from: &str) -> Result<PathBuf, BuildError> {
    let path = Path::new(from);
    if from.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(BuildError::FileSource {
            path: from.to_owned(),
        });
    }
    Ok(dir.join(path))
}

fn write_sbom(
    stage: &mut Stage,
    manifest: &Manifest,
    source: &SourceRecord,
    built: &BuiltWith,
    graph: &deps::CrateGraph,
) -> Result<Vec<u8>, BuildError> {
    let files = stage.records()?;
    let info = BuildInfo {
        tree_blake3: source.tree_blake3,
        cargo_lock_blake3: source.cargo_lock_blake3,
        target: &built.target,
        cpu_level: built.cpu_level,
        features: &built.features,
        feature_set_hash: built.feature_set_hash,
        toolchain_id: &built.toolchain_id,
        profile: built.profile,
    };
    let mut sbom = Vec::new();
    BuildSbom::new(manifest, info, &graph.roots, &graph.crates, &files)
        .write_cyclonedx_json(&mut sbom)?;
    stage.write(&sbom_path(&manifest.name), &sbom, 0o644)?;
    Ok(sbom)
}

fn relative_to(path: &Path, base: &Path) -> PathBuf {
    path.strip_prefix(base).unwrap_or(path).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_level_defaults_to_v3_on_x86_64_only() {
        assert_eq!(
            cpu_level("x86_64-unknown-redox", None).unwrap(),
            Some(CpuLevel::X86_64V3)
        );
        assert_eq!(
            cpu_level("x86_64-unknown-linux-gnu", Some(CpuLevel::X86_64V4)).unwrap(),
            Some(CpuLevel::X86_64V4)
        );
        assert_eq!(cpu_level("aarch64-unknown-redox", None).unwrap(), None);
        assert!(matches!(
            cpu_level("aarch64-unknown-redox", Some(CpuLevel::X86_64V3)),
            Err(BuildError::LevelNeedsX86_64 { .. })
        ));
    }

    #[test]
    fn rustflags_remap_paths_and_set_the_level() {
        let flags = rustflags(
            Path::new("/w/src"),
            Path::new("/w/target"),
            Path::new("/w/cargo-home"),
            Some(CpuLevel::X86_64V3),
        );
        let flags = flags.to_string_lossy();
        let parts: Vec<&str> = flags.split('\x1f').collect();
        assert_eq!(
            parts,
            [
                "--remap-path-prefix=/w/cargo-home=/fox/cargo",
                "--remap-path-prefix=/w/target=/fox/target",
                "--remap-path-prefix=/w/src=/fox/source",
                "-Ctarget-cpu=x86-64-v3",
            ]
        );
    }

    #[test]
    fn feature_arguments() {
        assert!(feature_args(&FeatureSelection::default()).is_empty());
        let args = feature_args(&FeatureSelection {
            default_features: false,
            features: vec!["a".into(), "b".into()],
        });
        assert_eq!(args, ["--no-default-features", "--features", "a,b"]);
    }

    #[test]
    fn package_paths_stay_inside() {
        assert!(checked_package_dir(Path::new("crates/hello")).is_ok());
        assert_eq!(
            checked_package_dir(Path::new("./a")).unwrap(),
            Path::new("a")
        );
        assert!(checked_package_dir(Path::new("../x")).is_err());
        assert!(checked_package_dir(Path::new("/abs")).is_err());
        assert!(package_file(Path::new("/p"), "doc/a.1").is_ok());
        for bad in ["", "../a", "/etc/passwd", "./a"] {
            assert!(package_file(Path::new("/p"), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn finds_outside_cargo_configs() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("a/b");
        fs::create_dir_all(&work).unwrap();
        let home = dir.path().join("home");
        check_cargo_configs(&work, &home).unwrap();
        fs::create_dir_all(dir.path().join("a/.cargo")).unwrap();
        fs::write(dir.path().join("a/.cargo/config.toml"), "").unwrap();
        assert!(matches!(
            check_cargo_configs(&work, &home),
            Err(BuildError::OutsideCargoConfig { .. })
        ));
    }
}
