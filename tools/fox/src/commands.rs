//! The subcommands.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fox_core::VerifiedAttestation;
use fox_core::archive::{Archive, ArchiveError, MetadataSource};
use fox_core::attested::{attestation_path, verify_archive_attestation};
use fox_core::keys::{KeyError, load_key_file, trusted_keys_for_root};
use fox_core::{NameError, PackageName, TrustedKeys};
use fox_sbom::{PackageSbom, SbomError};
use fox_store::{
    AdoptOptions, InstallOptions, MetadataKind, PackageState, ProblemKind, Store, StoreError,
};
use serde::Serialize;

use crate::{Cli, Command, SbomFormat};

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    #[error("cannot load trusted keys")]
    Keys(#[from] KeyError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot build SBOM")]
    Sbom(#[from] SbomError),
    #[error("invalid package name {name:?}")]
    Name {
        name: String,
        #[source]
        source: NameError,
    },
    #[error("cannot write {}", path.display())]
    Output {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot write JSON output")]
    Json(#[from] serde_json::Error),
}

pub(crate) fn run(cli: &Cli) -> Result<ExitCode, CliError> {
    match &cli.command {
        Command::Verify {
            installed, targets, ..
        } if *installed => verify_installed(cli, targets),
        Command::Verify {
            targets,
            require_attestation,
            ..
        } => verify_archives(cli, targets, *require_attestation),
        Command::Sbom {
            archive,
            format,
            output,
        } => sbom(cli, archive, *format, output.as_deref()),
        Command::Install {
            force,
            allow_unsigned_metadata,
            archives,
        } => install(
            cli,
            archives,
            InstallOptions {
                force: *force,
                allow_unsigned_metadata: *allow_unsigned_metadata,
            },
        ),
        Command::List => list(cli),
        Command::AdoptPkg {
            allow_unsigned_metadata,
            again,
        } => adopt_pkg(
            cli,
            AdoptOptions {
                allow_unsigned_metadata: *allow_unsigned_metadata,
                again: *again,
            },
        ),
        Command::Remove { force, names } => remove(cli, names, *force),
    }
}

/// The keys under the root plus those given with `--key`.
fn trusted_keys(cli: &Cli) -> Result<TrustedKeys, CliError> {
    trusted_keys_in(cli, &cli.root)
}

/// The keys under `root` (an opened store's checked root) plus `--key`.
fn trusted_keys_in(cli: &Cli, root: &Path) -> Result<TrustedKeys, CliError> {
    let mut keys = trusted_keys_for_root(root)?;
    for path in &cli.keys {
        let key = load_key_file(path)?;
        keys.insert(key.label, key.key);
    }
    Ok(keys)
}

fn parse_name(name: &str) -> Result<PackageName, CliError> {
    PackageName::new(name).map_err(|source| CliError::Name {
        name: name.to_owned(),
        source,
    })
}

fn print_json(value: &impl Serialize) -> Result<(), CliError> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out).map_err(|source| CliError::Output {
        path: PathBuf::from("<stdout>"),
        source,
    })
}

/// Formats an error and its sources on one line.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn sidecar_warning(archive: &Archive) {
    if let MetadataSource::Sidecar(path) = archive.metadata_source() {
        eprintln!(
            "fox: warning: {} has no signed manifest; name and version come from {}",
            archive.path().display(),
            path.display()
        );
    }
}

#[derive(Serialize)]
struct ArchiveSummary {
    path: PathBuf,
    name: String,
    version: String,
    archive_blake3: String,
    entries_blake3: String,
    signer: String,
    signer_label: String,
    metadata: &'static str,
    files: usize,
}

impl ArchiveSummary {
    fn new(archive: &Archive) -> Self {
        let signer = archive.head().signer();
        Self {
            path: archive.path().to_path_buf(),
            name: archive.manifest().name.to_string(),
            version: archive.manifest().version.clone(),
            archive_blake3: archive.blake3().to_string(),
            entries_blake3: archive.head().entries_blake3().to_string(),
            signer: signer.key.to_string(),
            signer_label: signer.label.clone(),
            metadata: match archive.metadata_source() {
                MetadataSource::Embedded => "embedded",
                _ => "sidecar",
            },
            files: archive.files().len(),
        }
    }
}

#[derive(Serialize)]
struct VerifyResult {
    path: PathBuf,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    package: Option<ArchiveSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attestation: Option<AttestationSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct AttestationSummary {
    path: PathBuf,
    builder: String,
    builder_key: String,
    git: String,
    commit: String,
    tree_blake3: String,
    target: String,
    cpu_level: Option<&'static str>,
    features: Vec<String>,
    toolchain_id: String,
    sbom_blake3: String,
}

impl AttestationSummary {
    fn new(path: PathBuf, verified: &VerifiedAttestation) -> Self {
        let a = verified.attestation();
        Self {
            path,
            builder: verified.signer().label.clone(),
            builder_key: verified.signer().key.to_string(),
            git: a.source.git.clone(),
            commit: a.source.commit.clone(),
            tree_blake3: a.source.tree_blake3.to_string(),
            target: a.build.target.clone(),
            cpu_level: a.build.cpu_level.map(fox_core::CpuLevel::as_str),
            features: a.build.features.clone(),
            toolchain_id: a.build.toolchain_id.clone(),
            sbom_blake3: a.output.sbom_blake3.to_string(),
        }
    }
}

/// Why an archive's attestation check failed, for output.
enum AttestationProblem {
    Missing(PathBuf),
    Invalid(String),
}

/// Checks the attestation beside a verified archive, if there is one.
fn check_attestation(
    archive: &Archive,
    keys: &TrustedKeys,
    required: bool,
) -> Result<Option<AttestationSummary>, AttestationProblem> {
    let path = attestation_path(archive.path());
    if !path.exists() {
        return if required {
            Err(AttestationProblem::Missing(path))
        } else {
            Ok(None)
        };
    }
    verify_archive_attestation(archive, &path, keys)
        .map(|verified| Some(AttestationSummary::new(path, &verified)))
        .map_err(|error| AttestationProblem::Invalid(chain(&error)))
}

fn verify_one(
    cli: &Cli,
    path: PathBuf,
    keys: &TrustedKeys,
    require_attestation: bool,
) -> VerifyResult {
    let archive = match Archive::open(&path, keys) {
        Ok(archive) => archive,
        Err(error) => {
            if !cli.json {
                println!("FAILED {}: {}", path.display(), chain(&error));
            }
            return VerifyResult {
                path,
                ok: false,
                package: None,
                attestation: None,
                error: Some(chain(&error)),
            };
        }
    };
    let package = Some(ArchiveSummary::new(&archive));
    match check_attestation(&archive, keys, require_attestation) {
        Ok(attestation) => {
            if !cli.json {
                sidecar_warning(&archive);
                let attested = attestation
                    .as_ref()
                    .map(|a| format!(", built from {} by {}", a.commit, a.builder))
                    .unwrap_or_default();
                println!(
                    "ok {} {} {} ({} files, signed by {}{attested})",
                    path.display(),
                    archive.manifest().name,
                    archive.manifest().version,
                    archive.files().len(),
                    archive.head().signer().label
                );
            }
            VerifyResult {
                path,
                ok: true,
                package,
                attestation,
                error: None,
            }
        }
        Err(problem) => {
            let error = match problem {
                AttestationProblem::Missing(at) => {
                    format!("no build attestation at {}", at.display())
                }
                AttestationProblem::Invalid(error) => error,
            };
            if !cli.json {
                println!("FAILED {}: {error}", path.display());
            }
            VerifyResult {
                path,
                ok: false,
                package,
                attestation: None,
                error: Some(error),
            }
        }
    }
}

fn verify_archives(
    cli: &Cli,
    targets: &[String],
    require_attestation: bool,
) -> Result<ExitCode, CliError> {
    let keys = trusted_keys(cli)?;
    let results: Vec<VerifyResult> = targets
        .iter()
        .map(|target| verify_one(cli, PathBuf::from(target), &keys, require_attestation))
        .collect();
    if cli.json {
        print_json(&results)?;
    }
    Ok(exit_code(results.iter().all(|r| r.ok)))
}

#[derive(Serialize)]
struct InstalledVerifyResult {
    name: String,
    ok: bool,
    checked: usize,
    problems: Vec<ProblemJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct ProblemJson {
    path: String,
    problem: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    found_mode: Option<u32>,
}

impl ProblemJson {
    fn new(path: &str, kind: ProblemKind) -> Self {
        let (problem, modes) = match kind {
            ProblemKind::Missing => ("missing", None),
            ProblemKind::Modified => ("modified", None),
            ProblemKind::WrongType => ("wrong-type", None),
            ProblemKind::SymlinkInPath => ("symlink-in-path", None),
            ProblemKind::Mode { expected, found } => ("mode", Some((expected, found))),
            _ => ("unknown", None),
        };
        Self {
            path: path.to_owned(),
            problem,
            expected_mode: modes.map(|(expected, _)| expected),
            found_mode: modes.map(|(_, found)| found),
        }
    }
}

fn verify_installed(cli: &Cli, names: &[String]) -> Result<ExitCode, CliError> {
    let store = Store::open(&cli.root)?;
    let keys = trusted_keys_in(cli, store.root())?;
    let names = if names.is_empty() {
        store.list()?.into_iter().map(|p| p.name).collect()
    } else {
        names
            .iter()
            .map(|n| parse_name(n))
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut results = Vec::new();
    for name in names {
        let result = match store.verify(&name, &keys) {
            Ok(report) => InstalledVerifyResult {
                name: name.to_string(),
                ok: report.problems.is_empty(),
                checked: report.checked,
                problems: report
                    .problems
                    .iter()
                    .map(|p| ProblemJson::new(&p.path, p.kind))
                    .collect(),
                error: None,
            },
            Err(error) => InstalledVerifyResult {
                name: name.to_string(),
                ok: false,
                checked: 0,
                problems: Vec::new(),
                error: Some(chain(&error)),
            },
        };
        if !cli.json {
            print_installed_result(&result);
        }
        results.push(result);
    }
    if cli.json {
        print_json(&results)?;
    }
    Ok(exit_code(results.iter().all(|r| r.ok)))
}

fn print_installed_result(result: &InstalledVerifyResult) {
    match (&result.error, result.ok) {
        (Some(error), _) => println!("FAILED {}: {error}", result.name),
        (None, true) => println!("ok {} ({} files)", result.name, result.checked),
        (None, false) => {
            println!("FAILED {}", result.name);
            for problem in &result.problems {
                match (problem.expected_mode, problem.found_mode) {
                    (Some(expected), Some(found)) => println!(
                        "  {}: mode {found:04o}, signed {expected:04o}",
                        problem.path
                    ),
                    _ => println!("  {}: {}", problem.path, problem.problem),
                }
            }
        }
    }
}

fn sbom(
    cli: &Cli,
    path: &Path,
    format: SbomFormat,
    output: Option<&Path>,
) -> Result<ExitCode, CliError> {
    let keys = trusted_keys(cli)?;
    let archive = Archive::open(path, &keys)?;
    sidecar_warning(&archive);
    let sbom = PackageSbom::new(archive.manifest(), archive.blake3(), archive.files())
        .with_signer(archive.head().signer().key);
    match format {
        SbomFormat::Cyclonedx => match output {
            Some(out) => {
                let mut file = File::create(out).map_err(|source| CliError::Output {
                    path: out.to_path_buf(),
                    source,
                })?;
                sbom.write_cyclonedx_json(&mut file)?;
            }
            None => {
                let mut stdout = io::stdout().lock();
                sbom.write_cyclonedx_json(&mut stdout)?;
                writeln!(stdout).map_err(|source| CliError::Output {
                    path: PathBuf::from("<stdout>"),
                    source,
                })?;
            }
        },
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct InstallJson {
    name: String,
    version: String,
    previous: Option<String>,
    installed: usize,
    removed: usize,
    kept: Vec<String>,
    missing_depends: Vec<String>,
}

fn install(cli: &Cli, paths: &[PathBuf], options: InstallOptions) -> Result<ExitCode, CliError> {
    let store = Store::open(&cli.root)?;
    let keys = trusted_keys_in(cli, store.root())?;
    // Verify everything before installing anything.
    let archives = paths
        .iter()
        .map(|path| Archive::open(path, &keys))
        .collect::<Result<Vec<_>, _>>()?;
    let mut results = Vec::new();
    for archive in &archives {
        sidecar_warning(archive);
        let report = store.install(archive, options)?;
        if !cli.json {
            match &report.previous {
                Some(old) => println!("upgraded {} {old} -> {}", report.name, report.version),
                None => println!("installed {} {}", report.name, report.version),
            }
            for path in &report.kept {
                println!("  kept locally changed {path}");
            }
        }
        for dep in &report.missing_depends {
            eprintln!(
                "fox: warning: {} needs {dep}, which is not installed",
                report.name
            );
        }
        results.push(InstallJson {
            name: report.name.to_string(),
            version: report.version,
            previous: report.previous,
            installed: report.installed,
            removed: report.removed,
            kept: report.kept,
            missing_depends: report
                .missing_depends
                .iter()
                .map(ToString::to_string)
                .collect(),
        });
    }
    if cli.json {
        print_json(&results)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct ListJson {
    name: String,
    version: String,
    target: Option<String>,
    depends: Vec<String>,
    archive_blake3: Option<String>,
    signer: String,
    signer_label: String,
    metadata: &'static str,
    files: usize,
    removal_incomplete: bool,
}

/// How a package's identity was established, for output.
fn metadata_name(kind: MetadataKind) -> &'static str {
    match kind {
        MetadataKind::Embedded => "embedded",
        MetadataKind::Sidecar => "sidecar",
        MetadataKind::Legacy => "legacy",
        _ => "unknown",
    }
}

fn list(cli: &Cli) -> Result<ExitCode, CliError> {
    let store = Store::open(&cli.root)?;
    let packages = store.list()?;
    if cli.json {
        let rows: Vec<ListJson> = packages
            .into_iter()
            .map(|p| ListJson {
                name: p.name.to_string(),
                version: p.version,
                target: p.target,
                depends: p.depends.iter().map(ToString::to_string).collect(),
                archive_blake3: p.archive_blake3.map(|digest| digest.to_string()),
                signer: p.signer.to_string(),
                signer_label: p.signer_label,
                metadata: metadata_name(p.metadata),
                files: p.files.len(),
                removal_incomplete: p.state == PackageState::Removing,
            })
            .collect();
        print_json(&rows)?;
    } else {
        for p in packages {
            let unsigned = if p.metadata == MetadataKind::Embedded {
                ""
            } else {
                ", unsigned name"
            };
            let removing = if p.state == PackageState::Removing {
                ", removal incomplete"
            } else {
                ""
            };
            let note = format!("{unsigned}{removing}");
            println!(
                "{} {} ({} files, signed by {}{note})",
                p.name,
                p.version,
                p.files.len(),
                p.signer_label
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct AdoptJson {
    already_adopted: bool,
    adopted: Vec<AdoptedJson>,
    skipped: Vec<SkippedJson>,
}

#[derive(Serialize)]
struct AdoptedJson {
    name: String,
    version: String,
    metadata: &'static str,
    files: usize,
}

#[derive(Serialize)]
struct SkippedJson {
    name: String,
    reason: String,
}

fn adopt_pkg(cli: &Cli, options: AdoptOptions) -> Result<ExitCode, CliError> {
    let store = Store::open(&cli.root)?;
    let keys = trusted_keys_in(cli, store.root())?;
    let report = store.adopt_pkg(&keys, options)?;
    let out = AdoptJson {
        already_adopted: report.already_adopted,
        adopted: report
            .adopted
            .iter()
            .map(|p| AdoptedJson {
                name: p.name.to_string(),
                version: p.version.clone(),
                metadata: metadata_name(p.metadata),
                files: p.files,
            })
            .collect(),
        skipped: report
            .skipped
            .iter()
            .map(|p| SkippedJson {
                name: p.name.clone(),
                reason: chain(&p.reason),
            })
            .collect(),
    };
    if cli.json {
        print_json(&out)?;
    } else if out.already_adopted {
        println!("pkg's packages were already adopted");
    } else {
        for p in &out.adopted {
            println!(
                "adopted {} {} ({} files, {})",
                p.name, p.version, p.files, p.metadata
            );
        }
        for p in &out.skipped {
            println!("skipped {}: {}", p.name, p.reason);
        }
        println!(
            "adopted {} of {} packages",
            out.adopted.len(),
            out.adopted.len() + out.skipped.len()
        );
    }
    Ok(exit_code(out.skipped.is_empty()))
}

#[derive(Serialize)]
struct RemoveJson {
    name: String,
    version: String,
    removed: usize,
    missing: usize,
    kept: Vec<String>,
}

fn remove(cli: &Cli, names: &[String], force: bool) -> Result<ExitCode, CliError> {
    let store = Store::open(&cli.root)?;
    let names = names
        .iter()
        .map(|n| parse_name(n))
        .collect::<Result<Vec<_>, _>>()?;
    let mut results = Vec::new();
    for name in &names {
        let report = store.remove(name, force)?;
        if !cli.json {
            println!("removed {} {}", report.name, report.version);
            for path in &report.kept {
                println!("  kept locally changed {path}");
            }
        }
        results.push(RemoveJson {
            name: report.name.to_string(),
            version: report.version,
            removed: report.removed,
            missing: report.missing,
            kept: report.kept,
        });
    }
    if cli.json {
        print_json(&results)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn exit_code(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
