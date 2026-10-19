//! `fox audit`: installed packages, and the crates inside them, that a
//! security advisory names.
//!
//! Advisories come from every configured registry (its signed
//! `advisories/`, read through the verified checkout, updated first when
//! it is older than the policy allows) and, with `--rustsec <dir>`, from a
//! checkout of the RustSec advisory database (`crates/<crate>/*.md`). A
//! `fox` advisory is matched against installed package names and
//! versions; a crates.io advisory is matched against the crates each
//! package's build SBOM lists (see [`crate::inventory`]), so a vulnerable
//! crate is found in every package that links it. Withdrawn and
//! informational advisories are skipped. A package whose version is not
//! semver cannot be compared and is reported as unchecked.
//!
//! Each registry's signed VEX documents (`vex/*.cdx.json`, see
//! [`fox_index::vex`]) can set findings aside. A statement applies to a
//! finding when it names the advisory's id or one of its aliases and
//! covers the installed package and version, and only when the package
//! was installed from that registry: the store records each package's
//! origin (the registry's name and pinned root) when `fox install` or
//! `fox sync` takes it from a registry. A registry can therefore vouch
//! only for packages installed from it, not for a same-named package from
//! another registry, a local archive or git build, a package adopted from
//! `pkg`, or one recorded before the store kept origins (whose findings
//! stay findings). A `not_affected`,
//! `false_positive` or `resolved` statement moves the finding to
//! `suppressed`; an `exploitable` or `in_triage` one keeps it a finding
//! and wins over a clearing statement from any registry. `--no-vex`
//! ignores VEX without reading it, so a broken VEX document cannot stop
//! an audit that does not use it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fox_core::policy::Policy;
use fox_core::registries::{Registries, RegistryEntry};
use fox_index::advisory::{Advisory, AdvisoryError, Ecosystem};
use fox_index::verify::VerifiedRegistry;
use fox_sbom::cargo_crates;
use fox_sbom::vex::VexStatement;
use fox_store::Store;
use semver::Version;
use serde::Serialize;
use time::OffsetDateTime;

use crate::Cli;
use crate::commands::{CliError, print_json};
use crate::inventory::{self, Inventoried};
use crate::registry::{self, RegistryError};

/// Why advisories could not be loaded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AuditError {
    #[error("no advisories to check against: add a registry or pass --rustsec")]
    NoSources,
    #[error("cannot read {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid RustSec advisory {}", path.display())]
    RustSec {
        path: PathBuf,
        #[source]
        source: AdvisoryError,
    },
}

/// An advisory and where it came from: a registry name or `rustsec`.
struct Sourced {
    source: String,
    advisory: Advisory,
}

/// A VEX statement, the registry that signed it and its file there.
struct SourcedVex {
    source: String,
    path: String,
    statement: VexStatement,
}

/// What the advisories are checked against and with.
struct Sources {
    advisories: Vec<Sourced>,
    vex: Vec<SourcedVex>,
}

/// The VEX statement applied to a finding.
#[derive(Debug, Serialize)]
struct VexJson {
    source: String,
    document: String,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    justification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct CrateJson {
    name: String,
    version: String,
    build_only: bool,
}

#[derive(Debug, Serialize)]
struct FindingJson {
    package: String,
    version: String,
    /// The crate inside the package, for a crates.io advisory.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<CrateJson>,
    advisory: String,
    source: String,
    title: String,
    aliases: Vec<String>,
    patched: Vec<String>,
    /// The VEX statement about it, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    vex: Option<VexJson>,
}

#[derive(Debug, Serialize)]
struct AuditJson {
    findings: Vec<FindingJson>,
    /// Findings a registry's VEX says do not apply.
    suppressed: Vec<FindingJson>,
    /// Packages whose version is not semver, so advisories naming them
    /// could not be compared.
    unchecked: Vec<String>,
    advisories: usize,
}

/// `fox audit [--rustsec <dir>] [--no-vex]`.
pub(crate) fn run(cli: &Cli, rustsec: Option<&Path>, no_vex: bool) -> Result<ExitCode, CliError> {
    let store = Store::open(&cli.root)?;
    let inventory = inventory::take(&store)?;
    let vex_for = (!no_vex).then_some(inventory.as_slice());
    let sources = load(&cli.root, rustsec, vex_for)?;
    let report = check(&inventory, &sources.advisories, &sources.vex);
    if cli.json {
        print_json(&report)?;
    } else {
        print_report(&report);
    }
    Ok(if report.findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Loads every source's advisories and, when `vex_for` is given, the VEX
/// statements that apply to those installed packages.
fn load(
    root: &Path,
    rustsec: Option<&Path>,
    vex_for: Option<&[Inventoried]>,
) -> Result<Sources, CliError> {
    let registries = Registries::load(root)?;
    if registries.entries().is_empty() && rustsec.is_none() {
        return Err(AuditError::NoSources.into());
    }
    let policy = Policy::load(root)?;
    let now = OffsetDateTime::now_utc();
    let mut all = Vec::new();
    let mut vex = Vec::new();
    for entry in registries.entries() {
        let verified = registry::open(root, entry, &policy, now)?;
        if let Some(inventory) = vex_for {
            vex.extend(registry_vex(&verified, entry, inventory)?);
        }
        let advisories = verified
            .advisories()
            .map_err(|source| RegistryError::Verify {
                name: entry.name.to_string(),
                source: Box::new(source),
            })?;
        all.extend(advisories.into_iter().map(|advisory| Sourced {
            source: entry.name.to_string(),
            advisory,
        }));
    }
    if let Some(dir) = rustsec {
        all.extend(
            rustsec_advisories(dir)?
                .into_iter()
                .map(|advisory| Sourced {
                    source: "rustsec".to_owned(),
                    advisory,
                }),
        );
    }
    all.retain(|sourced| sourced.advisory.is_active());
    Ok(Sources {
        advisories: all,
        vex,
    })
}

/// The registry's VEX statements about installed packages that were
/// installed from it (see the module docs). Its VEX documents are read
/// only when some package was.
fn registry_vex(
    verified: &VerifiedRegistry,
    entry: &RegistryEntry,
    inventory: &[Inventoried],
) -> Result<Vec<SourcedVex>, CliError> {
    let from_here = installed_from(inventory, entry);
    if from_here.is_empty() {
        return Ok(Vec::new());
    }
    let documents = verified
        .vex_documents()
        .map_err(|error| RegistryError::Verify {
            name: entry.name.to_string(),
            source: Box::new(error),
        })?;
    let mut statements = Vec::new();
    for document in documents {
        for statement in document.statements {
            if from_here.contains(statement.package.as_str()) {
                statements.push(SourcedVex {
                    source: entry.name.to_string(),
                    path: document.path.clone(),
                    statement,
                });
            }
        }
    }
    Ok(statements)
}

/// The names of the installed packages whose recorded origin is `entry`:
/// the same registry name and pinned root.
fn installed_from<'a>(inventory: &'a [Inventoried], entry: &RegistryEntry) -> BTreeSet<&'a str> {
    inventory
        .iter()
        .filter(|item| {
            item.package.origin.as_ref().is_some_and(|origin| {
                origin.registry == entry.name && origin.root_blake3 == entry.root_blake3
            })
        })
        .map(|item| item.package.name.as_str())
        .collect()
}

/// Every `crates/<crate>/*.md` advisory in a RustSec database checkout.
fn rustsec_advisories(dir: &Path) -> Result<Vec<Advisory>, AuditError> {
    let mut advisories = Vec::new();
    let crates = dir.join("crates");
    for krate in read_dir_sorted(&crates)? {
        if !krate.is_dir() {
            continue;
        }
        for file in read_dir_sorted(&krate)? {
            if file.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let text = fs::read_to_string(&file).map_err(|source| AuditError::Io {
                path: file.clone(),
                source,
            })?;
            let advisory = Advisory::from_rustsec_markdown(&text)
                .map_err(|source| AuditError::RustSec { path: file, source })?;
            advisories.push(advisory);
        }
    }
    Ok(advisories)
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, AuditError> {
    let io_error = |source| AuditError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut paths = fs::read_dir(dir)
        .map_err(io_error)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_error)?;
    paths.sort();
    Ok(paths)
}

fn check(inventory: &[Inventoried], advisories: &[Sourced], vex: &[SourcedVex]) -> AuditJson {
    let mut findings = Vec::new();
    let mut unchecked = Vec::new();
    for item in inventory {
        let package = &item.package;
        let name = package.name.as_str();
        match package.version.parse::<Version>() {
            Ok(version) => {
                for sourced in advisories_for(advisories, Ecosystem::Fox, name) {
                    if sourced.advisory.affects(&version) {
                        findings.push(finding(item, None, sourced));
                    }
                }
            }
            Err(_) => {
                if advisories_for(advisories, Ecosystem::Fox, name)
                    .next()
                    .is_some()
                {
                    unchecked.push(format!("{name} {}", package.version));
                }
            }
        }
        let crates = item
            .build_sbom
            .as_ref()
            .map(cargo_crates)
            .unwrap_or_default();
        for krate in crates {
            let Ok(version) = krate.version.parse::<Version>() else {
                continue;
            };
            for sourced in advisories_for(advisories, Ecosystem::CratesIo, &krate.name) {
                if sourced.advisory.affects(&version) {
                    let via = CrateJson {
                        name: krate.name.clone(),
                        version: krate.version.clone(),
                        build_only: krate.build_only,
                    };
                    findings.push(finding(item, Some(via), sourced));
                }
            }
        }
    }
    let (suppressed, findings) = findings
        .into_iter()
        .map(|finding| apply_vex(finding, vex))
        .partition::<Vec<_>, _>(|(cleared, _)| *cleared);
    AuditJson {
        findings: findings.into_iter().map(|(_, finding)| finding).collect(),
        suppressed: suppressed.into_iter().map(|(_, finding)| finding).collect(),
        unchecked,
        advisories: advisories.len(),
    }
}

/// Attaches the VEX statement that decides `finding`, and whether it
/// clears it. A statement that does not clear it wins, so a registry
/// cannot hide a vulnerability another one reports as exploitable.
fn apply_vex(mut finding: FindingJson, vex: &[SourcedVex]) -> (bool, FindingJson) {
    let mut matching = vex.iter().filter(|sourced| {
        sourced.statement.names(&finding.advisory, &finding.aliases)
            && sourced.statement.covers(&finding.package, &finding.version)
    });
    let deciding = matching
        .clone()
        .find(|sourced| !sourced.statement.state.clears())
        .or_else(|| matching.next());
    let Some(sourced) = deciding else {
        return (false, finding);
    };
    let statement = &sourced.statement;
    finding.vex = Some(VexJson {
        source: sourced.source.clone(),
        document: sourced.path.clone(),
        state: statement.state.as_str(),
        justification: statement.justification.clone(),
        detail: statement.detail.clone(),
    });
    (statement.state.clears(), finding)
}

fn advisories_for<'a>(
    advisories: &'a [Sourced],
    ecosystem: Ecosystem,
    name: &'a str,
) -> impl Iterator<Item = &'a Sourced> + 'a {
    advisories.iter().filter(move |sourced| {
        sourced.advisory.advisory.ecosystem == ecosystem
            && sourced.advisory.advisory.package == name
    })
}

fn finding(item: &Inventoried, via: Option<CrateJson>, sourced: &Sourced) -> FindingJson {
    let info = &sourced.advisory.advisory;
    FindingJson {
        package: item.package.name.to_string(),
        version: item.package.version.clone(),
        via,
        advisory: info.id.clone(),
        source: sourced.source.clone(),
        title: info.title.clone(),
        aliases: info.aliases.clone(),
        patched: sourced
            .advisory
            .versions
            .patched
            .iter()
            .map(ToString::to_string)
            .collect(),
        vex: None,
    }
}

fn print_report(report: &AuditJson) {
    for finding in &report.findings {
        print_finding(finding);
    }
    for finding in &report.suppressed {
        if let Some(vex) = &finding.vex {
            let why = vex
                .justification
                .as_ref()
                .map_or_else(String::new, |why| format!(", {why}"));
            println!(
                "{} {}: {} set aside by {} VEX ({}{why})",
                finding.package, finding.version, finding.advisory, vex.source, vex.state
            );
        }
    }
    for package in &report.unchecked {
        eprintln!("fox: cannot compare {package} with its advisories: not a semver version");
    }
    println!(
        "{} finding(s) against {} advisories; {} set aside by VEX",
        report.findings.len(),
        report.advisories,
        report.suppressed.len()
    );
}

fn print_finding(finding: &FindingJson) {
    let via = finding.via.as_ref().map_or_else(String::new, |krate| {
        let build = if krate.build_only { ", build only" } else { "" };
        format!(" (crate {} {}{build})", krate.name, krate.version)
    });
    let patched = if finding.patched.is_empty() {
        "no patched version".to_owned()
    } else {
        format!("patched: {}", finding.patched.join(", "))
    };
    let vex = finding.vex.as_ref().map_or_else(String::new, |vex| {
        format!("; {} VEX: {}", vex.source, vex.state)
    });
    println!(
        "{} {}{via}: {} [{}] {}; {patched}{vex}",
        finding.package, finding.version, finding.advisory, finding.source, finding.title
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_core::{Blake3Digest, KeyId, Manifest, PackageName, Profile};
    use fox_sbom::{BuildInfo, BuildSbom, CrateDependency, CrateSource, read_build_sbom};
    use fox_store::{InstalledPackage, MetadataKind, Origin, PackageState};

    fn rustsec(id: &str, krate: &str, patched: &str, extra: &str) -> String {
        format!(
            "```toml\n[advisory]\nid = \"{id}\"\npackage = \"{krate}\"\ndate = \"2026-01-01\"\n{extra}\n\
             [versions]\npatched = [\"{patched}\"]\n```\n\n# {krate} is broken\n\nDetails.\n"
        )
    }

    fn inventoried(name: &str, version: &str, crates: &[(&str, &str, bool)]) -> Inventoried {
        let name = PackageName::new(name).unwrap();
        let manifest = Manifest::new(name.clone(), version);
        let crates: Vec<CrateDependency> = crates
            .iter()
            .map(|(krate, version, build_only)| CrateDependency {
                id: format!("{krate} {version}"),
                name: (*krate).to_owned(),
                version: (*version).to_owned(),
                source: CrateSource::Registry {
                    url: "https://github.com/rust-lang/crates.io-index".to_owned(),
                    checksum: None,
                },
                license: None,
                build_only: *build_only,
                depends: Vec::new(),
            })
            .collect();
        let info = BuildInfo {
            tree_blake3: Blake3Digest::of(b"tree"),
            cargo_lock_blake3: Blake3Digest::of(b"lock"),
            target: "x86_64-unknown-redox",
            cpu_level: None,
            features: &[],
            feature_set_hash: Blake3Digest::of(b"features"),
            toolchain_id: "rustc",
            profile: Profile::default(),
        };
        let mut json = Vec::new();
        BuildSbom::new(&manifest, info, &[], &crates, &[])
            .write_cyclonedx_json(&mut json)
            .unwrap();
        Inventoried {
            package: InstalledPackage {
                name,
                version: version.to_owned(),
                target: None,
                depends: Vec::new(),
                archive_blake3: None,
                entries_blake3: Blake3Digest::of(b"entries"),
                signer: KeyId::from_bytes([1; 32]),
                signer_label: "ci".to_owned(),
                metadata: MetadataKind::Embedded,
                state: PackageState::Installed,
                origin: None,
                files: Vec::new(),
            },
            build_sbom: Some(read_build_sbom(&json).unwrap()),
        }
    }

    #[test]
    fn crates_inside_packages_match_rustsec_advisories() {
        let db = tempfile::tempdir().unwrap();
        for (krate, file, text) in [
            (
                "smallcrate",
                "RUSTSEC-2026-0001.md",
                rustsec("RUSTSEC-2026-0001", "smallcrate", ">= 0.3.2", ""),
            ),
            (
                "cc",
                "RUSTSEC-2026-0002.md",
                rustsec("RUSTSEC-2026-0002", "cc", ">= 2.0.0", ""),
            ),
            (
                "old",
                "RUSTSEC-2026-0003.md",
                rustsec(
                    "RUSTSEC-2026-0003",
                    "old",
                    ">= 9.0.0",
                    "withdrawn = \"2026-02-01\"",
                ),
            ),
        ] {
            let dir = db.path().join("crates").join(krate);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(file), text).unwrap();
        }
        fs::write(db.path().join("crates/smallcrate/README.txt"), "skipped").unwrap();
        let root = tempfile::tempdir().unwrap();
        let sources = load(root.path(), Some(db.path()), Some(&[])).unwrap();
        let advisories = sources.advisories;
        // The withdrawn advisory is dropped.
        assert_eq!(advisories.len(), 2);

        let inventory = [
            inventoried(
                "app",
                "1.0.0",
                &[
                    ("smallcrate", "0.3.1", false),
                    ("cc", "1.0.0", true),
                    ("old", "1.0.0", false),
                ],
            ),
            inventoried("fixed", "2.0.0", &[("smallcrate", "0.3.2", false)]),
        ];
        let report = check(&inventory, &advisories, &[]);
        let found: Vec<(&str, &str, bool)> = report
            .findings
            .iter()
            .map(|f| {
                let via = f.via.as_ref().unwrap();
                (f.package.as_str(), f.advisory.as_str(), via.build_only)
            })
            .collect();
        assert_eq!(
            found,
            [
                ("app", "RUSTSEC-2026-0001", false),
                ("app", "RUSTSEC-2026-0002", true)
            ]
        );
        assert_eq!(report.findings[0].title, "smallcrate is broken");

        let broken = db.path().join("crates/cc/RUSTSEC-2026-0004.md");
        fs::write(&broken, "no front matter").unwrap();
        assert!(matches!(
            rustsec_advisories(db.path()),
            Err(AuditError::RustSec { .. })
        ));
    }

    fn vex(source: &str, id: &str, package: &str, state: &str) -> SourcedVex {
        let json = format!(
            r#"{{"bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
                "vulnerabilities": [{{"id": "{id}", "analysis": {{"state": "{state}",
                  "justification": "code_not_present"}},
                  "affects": [{{"ref": "pkg:generic/{package}@1.0.0"}}]}}]}}"#
        );
        let statement = fox_sbom::vex::read_vex(json.as_bytes()).unwrap().remove(0);
        SourcedVex {
            source: source.to_owned(),
            path: format!("vex/{id}.cdx.json"),
            statement,
        }
    }

    #[test]
    fn vex_sets_findings_aside_unless_another_statement_disagrees() {
        let advisory = |id: &str, aliases: &str| Sourced {
            source: "test".to_owned(),
            advisory: Advisory::parse(&format!(
                "[advisory]\nid = \"{id}\"\npackage = \"app\"\ndate = \"2026-01-01\"\n\
                 aliases = [{aliases}]\n"
            ))
            .unwrap(),
        };
        let advisories = [advisory("FOX-1", ""), advisory("FOX-2", "\"CVE-2026-2\"")];
        let inventory = [
            inventoried("app", "1.0.0", &[]),
            inventoried("app2", "1.0.0", &[]),
        ];

        // By id, and by alias; other packages and versions are untouched.
        let statements = [
            vex("test", "FOX-1", "app", "not_affected"),
            vex("test", "CVE-2026-2", "app", "false_positive"),
        ];
        let report = check(&inventory, &advisories, &statements);
        assert!(report.findings.is_empty());
        assert_eq!(report.suppressed.len(), 2);
        let vex_json = report.suppressed[0].vex.as_ref().unwrap();
        assert_eq!(vex_json.state, "not_affected");
        assert_eq!(vex_json.justification.as_deref(), Some("code_not_present"));
        assert_eq!(vex_json.document, "vex/FOX-1.cdx.json");
        let report = check(
            &[inventoried("app", "1.0.1", &[])],
            &advisories,
            &statements,
        );
        assert_eq!(report.findings.len(), 2);

        // An exploitable statement from another registry wins, and stays
        // attached to the finding.
        let disagreeing = [
            vex("test", "FOX-1", "app", "not_affected"),
            vex("other", "FOX-1", "app", "exploitable"),
        ];
        let report = check(&inventory, &advisories[..1], &disagreeing);
        assert_eq!(report.findings.len(), 1);
        assert!(report.suppressed.is_empty());
        assert_eq!(report.findings[0].vex.as_ref().unwrap().source, "other");
    }

    #[test]
    fn non_semver_versions_are_unchecked() {
        let advisory = Advisory::parse(
            "[advisory]\nid = \"FOX-1\"\npackage = \"app\"\ndate = \"2026-01-01\"\n",
        )
        .unwrap();
        let advisories = [Sourced {
            source: "test".to_owned(),
            advisory,
        }];
        let report = check(&[inventoried("app", "2024.1", &[])], &advisories, &[]);
        assert!(report.findings.is_empty());
        assert_eq!(report.unchecked, ["app 2024.1"]);
        let report = check(&[inventoried("app", "1.0.0", &[])], &advisories, &[]);
        assert_eq!(report.findings.len(), 1);
    }

    #[test]
    fn vex_applies_only_to_packages_installed_from_the_registry() {
        let entry = RegistryEntry {
            name: PackageName::new("ferric").unwrap(),
            git: "https://example.com/registry.git".parse().unwrap(),
            root_blake3: Blake3Digest::of(b"root"),
        };
        let package = |name: &str, registry: &str, root: &[u8]| {
            let mut item = inventoried(name, "1.0.0", &[]);
            item.package.origin = Some(Origin {
                registry: PackageName::new(registry).unwrap(),
                root_blake3: Blake3Digest::of(root),
            });
            item
        };
        let mut local = inventoried("local", "1.0.0", &[]);
        local.package.origin = None;
        let inventory = [
            package("ours", "ferric", b"root"),
            package("theirs", "other", b"root"),
            // A different registry later configured under the same name.
            package("renamed", "ferric", b"another root"),
            local,
        ];
        assert_eq!(
            installed_from(&inventory, &entry)
                .into_iter()
                .collect::<Vec<_>>(),
            ["ours"]
        );
    }
}
