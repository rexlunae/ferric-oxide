//! CycloneDX VEX: statements that a vulnerability does or does not affect
//! a fox package.
//!
//! A VEX (Vulnerability Exploitability eXchange) document is a CycloneDX
//! BOM whose `vulnerabilities` array carries an impact `analysis` and the
//! products it applies to (`affects`), as described in the CycloneDX 1.4
//! and 1.5 specifications (the `vulnerability` object, its `analysis`
//! `state`, `justification` and `detail`, and `affects[].ref` and
//! `affects[].versions`). A registry publishes them so `fox audit` can set
//! aside findings that do not apply, such as a crate advisory for code a
//! package does not compile.
//!
//! [`read_vex`] turns a document into one [`VexStatement`] per
//! vulnerability and fox package. Each `affects[].ref` names a fox package
//! by its purl, `pkg:generic/<name>` or `pkg:generic/<name>@<version>` (as
//! [`PackageSbom`](crate::PackageSbom) writes it), either directly or
//! through the `bom-ref` of a component in the same document that has
//! that purl. Without a version the statement covers every version; with
//! one, or with `versions` entries, only the versions named. A `versions`
//! entry whose status is `affected` is left out. Version ranges (`vers:`)
//! are rejected rather than guessed at.
//!
//! The document is parsed with `cyclonedx-bom`, so it must be CycloneDX
//! JSON 1.4 or 1.5 (`specVersion`); 1.3 has no vulnerabilities.

use cyclonedx_bom::models::bom::Bom;
use cyclonedx_bom::models::component::Component;
use cyclonedx_bom::models::vulnerability::Vulnerability;
use cyclonedx_bom::models::vulnerability_analysis::ImpactAnalysisState;
use cyclonedx_bom::models::vulnerability_target::{Status, VersionRange, VulnerabilityTarget};

/// The purl prefix of a fox package.
const PURL_PREFIX: &str = "pkg:generic/";

/// Why a VEX document could not be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VexError {
    /// The document is not CycloneDX JSON of a supported version.
    #[error("cannot read the VEX document")]
    Read(#[source] Box<cyclonedx_bom::errors::JsonReadError>),
    /// The document has no vulnerabilities.
    #[error("the VEX document lists no vulnerabilities")]
    Empty,
    /// A vulnerability has no `id`.
    #[error("vulnerability {index} has no id")]
    NoId {
        /// Its position in `vulnerabilities`.
        index: usize,
    },
    /// A vulnerability has no `analysis.state`, or an unknown one.
    #[error("vulnerability {id} has no known analysis state")]
    NoState {
        /// The vulnerability.
        id: String,
    },
    /// A vulnerability names no product in `affects`.
    #[error("vulnerability {id} affects nothing")]
    NoTargets {
        /// The vulnerability.
        id: String,
    },
    /// An `affects[].ref` is not a fox package purl, or the `bom-ref` of a
    /// component that has one.
    #[error("vulnerability {id} affects {reference:?}, which is not a fox package")]
    Target {
        /// The vulnerability.
        id: String,
        /// The reference.
        reference: String,
    },
    /// A `versions` entry is a version range, which is not supported.
    #[error("vulnerability {id} uses version range {range:?}; list versions instead")]
    Range {
        /// The vulnerability.
        id: String,
        /// The range.
        range: String,
    },
}

/// The impact analysis of a vulnerability for a product (CycloneDX
/// `analysis.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VexState {
    /// The vulnerability has been fixed in the product.
    Resolved,
    /// Fixed, with the fix described in the component's pedigree.
    ResolvedWithPedigree,
    /// The product is affected and the flaw can be exploited.
    Exploitable,
    /// Still being investigated.
    InTriage,
    /// The vulnerability was wrongly reported for the product.
    FalsePositive,
    /// The product is not affected.
    NotAffected,
}

impl VexState {
    /// Whether the statement says the product is safe from the
    /// vulnerability, so a finding of it can be set aside.
    pub fn clears(self) -> bool {
        matches!(
            self,
            Self::Resolved | Self::ResolvedWithPedigree | Self::FalsePositive | Self::NotAffected
        )
    }

    /// The CycloneDX name, such as `not_affected`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::ResolvedWithPedigree => "resolved_with_pedigree",
            Self::Exploitable => "exploitable",
            Self::InTriage => "in_triage",
            Self::FalsePositive => "false_positive",
            Self::NotAffected => "not_affected",
        }
    }

    fn from_cyclonedx(state: &ImpactAnalysisState) -> Option<Self> {
        Some(match state {
            ImpactAnalysisState::Resolved => Self::Resolved,
            ImpactAnalysisState::ResolvedWithPedigree => Self::ResolvedWithPedigree,
            ImpactAnalysisState::Exploitable => Self::Exploitable,
            ImpactAnalysisState::InTriage => Self::InTriage,
            ImpactAnalysisState::FalsePositive => Self::FalsePositive,
            ImpactAnalysisState::NotAffected => Self::NotAffected,
            _ => return None,
        })
    }
}

/// Which versions of the package a statement covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VexVersions {
    /// Every version.
    All,
    /// Only these exact versions.
    Only(Vec<String>),
}

/// What one VEX document says about one vulnerability in one fox package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VexStatement {
    /// The vulnerability id, such as `RUSTSEC-2024-0001` or a CVE.
    pub vulnerability: String,
    /// Other ids of the same vulnerability, from its `references`.
    pub aliases: Vec<String>,
    /// The fox package.
    pub package: String,
    /// The versions of it the statement covers.
    pub versions: VexVersions,
    /// The analysis.
    pub state: VexState,
    /// Why, such as `code_not_present`, when given.
    pub justification: Option<String>,
    /// The analysis in words, when given.
    pub detail: Option<String>,
}

impl VexStatement {
    /// Whether the statement is about the vulnerability known as `id` or
    /// any of `aliases`.
    pub fn names<S: AsRef<str>>(&self, id: &str, aliases: &[S]) -> bool {
        let mine = || std::iter::once(&self.vulnerability).chain(&self.aliases);
        mine().any(|known| {
            known == id || aliases.iter().any(|alias| known.as_str() == alias.as_ref())
        })
    }

    /// Whether the statement covers `version` of `package`.
    pub fn covers(&self, package: &str, version: &str) -> bool {
        self.package == package
            && match &self.versions {
                VexVersions::All => true,
                VexVersions::Only(versions) => versions.iter().any(|known| known == version),
            }
    }
}

/// Reads the statements of a CycloneDX VEX document.
///
/// # Errors
///
/// [`VexError::Read`] if it is not CycloneDX JSON 1.4 or 1.5,
/// [`VexError::Empty`] without vulnerabilities, and the other
/// [`VexError`] variants for a vulnerability without an id, a known
/// analysis state or a fox package to apply to, or with a version range.
///
/// # Examples
///
/// ```
/// let json = br#"{
///   "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
///   "vulnerabilities": [{
///     "id": "RUSTSEC-2026-0001",
///     "analysis": {"state": "not_affected", "justification": "code_not_present"},
///     "affects": [{"ref": "pkg:generic/hello@1.0.0"}]
///   }]
/// }"#;
/// let statements = fox_sbom::vex::read_vex(json)?;
/// assert!(statements[0].covers("hello", "1.0.0"));
/// assert!(statements[0].state.clears());
/// # Ok::<(), fox_sbom::vex::VexError>(())
/// ```
pub fn read_vex(json: &[u8]) -> Result<Vec<VexStatement>, VexError> {
    let bom = Bom::parse_from_json(json).map_err(|source| VexError::Read(Box::new(source)))?;
    let vulnerabilities = bom
        .vulnerabilities
        .as_ref()
        .map(|list| list.0.as_slice())
        .unwrap_or_default();
    if vulnerabilities.is_empty() {
        return Err(VexError::Empty);
    }
    let mut statements = Vec::new();
    for (index, vulnerability) in vulnerabilities.iter().enumerate() {
        statements.extend(statements_of(&bom, index, vulnerability)?);
    }
    Ok(statements)
}

fn statements_of(
    bom: &Bom,
    index: usize,
    vulnerability: &Vulnerability,
) -> Result<Vec<VexStatement>, VexError> {
    let id = vulnerability
        .id
        .as_ref()
        .map(ToString::to_string)
        .ok_or(VexError::NoId { index })?;
    let analysis = vulnerability.vulnerability_analysis.as_ref();
    let state = analysis
        .and_then(|analysis| analysis.state.as_ref())
        .and_then(VexState::from_cyclonedx)
        .ok_or_else(|| VexError::NoState { id: id.clone() })?;
    let targets = vulnerability
        .vulnerability_targets
        .as_ref()
        .map(|targets| targets.0.as_slice())
        .unwrap_or_default();
    if targets.is_empty() {
        return Err(VexError::NoTargets { id });
    }
    let aliases: Vec<String> = vulnerability
        .vulnerability_references
        .iter()
        .flat_map(|references| &references.0)
        .map(|reference| reference.id.to_string())
        .collect();
    let justification = analysis
        .and_then(|analysis| analysis.justification.as_ref())
        .map(ToString::to_string);
    let detail = analysis.and_then(|analysis| analysis.detail.clone());
    targets
        .iter()
        .map(|target| {
            let (package, versions) = target_of(bom, &id, target)?;
            Ok(VexStatement {
                vulnerability: id.clone(),
                aliases: aliases.clone(),
                package,
                versions,
                state,
                justification: justification.clone(),
                detail: detail.clone(),
            })
        })
        .collect()
}

/// The package and versions an `affects` entry names.
fn target_of(
    bom: &Bom,
    id: &str,
    target: &VulnerabilityTarget,
) -> Result<(String, VexVersions), VexError> {
    let reference = &target.bom_ref;
    let not_a_package = || VexError::Target {
        id: id.to_owned(),
        reference: reference.clone(),
    };
    let purl = component(bom, reference)
        .and_then(|component| component.purl.as_ref())
        .map_or(reference.as_str(), AsRef::as_ref);
    let (package, version) = parse_purl(purl).ok_or_else(not_a_package)?;
    let mut versions: Vec<String> = version.into_iter().collect();
    let mut affected = Vec::new();
    let listed = target
        .versions
        .as_ref()
        .map(|list| list.0.as_slice())
        .unwrap_or_default();
    for entry in listed {
        match &entry.version_range {
            VersionRange::Version(version) if entry.status == Status::Affected => {
                affected.push(version.to_string());
            }
            VersionRange::Version(version) => versions.push(version.to_string()),
            VersionRange::Range(range) => {
                return Err(VexError::Range {
                    id: id.to_owned(),
                    range: range.to_string(),
                });
            }
            VersionRange::UndefinedVersionRange(range) => {
                return Err(VexError::Range {
                    id: id.to_owned(),
                    range: range.clone(),
                });
            }
        }
    }
    // A version listed as affected is never covered, even when the purl
    // names it too.
    versions.retain(|version| !affected.contains(version));
    let versions = if versions.is_empty() && listed.is_empty() {
        VexVersions::All
    } else {
        VexVersions::Only(versions)
    };
    Ok((package, versions))
}

/// The component of the document with this `bom-ref`.
fn component<'a>(bom: &'a Bom, reference: &str) -> Option<&'a Component> {
    let metadata = bom
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.component.as_ref());
    bom.components
        .iter()
        .flat_map(|components| &components.0)
        .chain(metadata)
        .find(|component| component.bom_ref.as_deref() == Some(reference))
}

/// The package name and version of a `pkg:generic/<name>[@<version>]`
/// purl, ignoring qualifiers and subpath.
fn parse_purl(purl: &str) -> Option<(String, Option<String>)> {
    let rest = purl.strip_prefix(PURL_PREFIX)?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (name, version) = match rest.split_once('@') {
        Some((name, version)) => (name, Some(version.to_owned())),
        None => (rest, None),
    };
    if name.is_empty() || name.contains('/') || version.as_deref() == Some("") {
        return None;
    }
    Some((name.to_owned(), version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(vulnerabilities: &str, components: &str) -> Vec<u8> {
        format!(
            r#"{{"bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
                "components": [{components}], "vulnerabilities": [{vulnerabilities}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn statements_name_packages_by_purl_or_component() {
        let json = document(
            r#"{"id": "RUSTSEC-2026-0001",
                "references": [{"id": "CVE-2026-1", "source": {"name": "NVD"}}],
                "analysis": {"state": "not_affected", "justification": "code_not_present",
                             "detail": "the feature is off"},
                "affects": [{"ref": "pkg:generic/hello"},
                            {"ref": "tiny-ref", "versions": [
                              {"version": "0.3.0", "status": "unaffected"},
                              {"version": "0.4.0", "status": "affected"}]}]},
               {"id": "FOX-2026-0002", "analysis": {"state": "exploitable"},
                "affects": [{"ref": "pkg:generic/hello@1.0.0?arch=x86_64"}]}"#,
            r#"{"type": "application", "name": "tiny", "bom-ref": "tiny-ref",
                "purl": "pkg:generic/tiny"}"#,
        );
        let statements = read_vex(&json).unwrap();
        assert_eq!(statements.len(), 3);
        let hello = &statements[0];
        assert!(hello.covers("hello", "9.9.9"));
        assert!(!hello.covers("tiny", "9.9.9"));
        assert!(hello.names("RUSTSEC-2026-0001", &[] as &[&str]));
        assert!(hello.names("GHSA-x", &["CVE-2026-1"]));
        assert!(!hello.names("RUSTSEC-2026-0009", &["CVE-2026-9"]));
        assert_eq!(hello.justification.as_deref(), Some("code_not_present"));
        assert_eq!(hello.detail.as_deref(), Some("the feature is off"));
        assert!(hello.state.clears());
        let tiny = &statements[1];
        assert!(tiny.covers("tiny", "0.3.0"));
        assert!(!tiny.covers("tiny", "0.4.0"));
        let exploitable = &statements[2];
        assert!(!exploitable.state.clears());
        assert_eq!(exploitable.state.as_str(), "exploitable");
        assert!(exploitable.covers("hello", "1.0.0"));
        assert!(!exploitable.covers("hello", "1.0.1"));
    }

    #[test]
    fn a_version_listed_as_affected_is_not_covered_even_in_the_purl() {
        let json = document(
            r#"{"id": "FOX-2026-0003", "analysis": {"state": "not_affected"},
                "affects": [{"ref": "pkg:generic/hello@1.0.0", "versions": [
                  {"version": "1.0.0", "status": "affected"},
                  {"version": "1.1.0", "status": "unaffected"}]}]}"#,
            "",
        );
        let statements = read_vex(&json).unwrap();
        assert_eq!(statements.len(), 1);
        assert!(!statements[0].covers("hello", "1.0.0"));
        assert!(statements[0].covers("hello", "1.1.0"));
    }

    #[test]
    fn unusable_statements_are_rejected() {
        let affects = r#""affects": [{"ref": "pkg:generic/hello"}]"#;
        let cases = [
            (format!(r#"{{"analysis": {{"state": "not_affected"}}, {affects}}}"#), "no id"),
            (format!(r#"{{"id": "X-1", {affects}}}"#), "analysis state"),
            (
                r#"{"id": "X-1", "analysis": {"state": "not_affected"}}"#.to_owned(),
                "affects nothing",
            ),
            (
                r#"{"id": "X-1", "analysis": {"state": "not_affected"},
                    "affects": [{"ref": "pkg:cargo/hello@1.0.0"}]}"#
                    .to_owned(),
                "not a fox package",
            ),
            (
                r#"{"id": "X-1", "analysis": {"state": "not_affected"},
                    "affects": [{"ref": "pkg:generic/hello",
                                 "versions": [{"range": "vers:generic/<2", "status": "unaffected"}]}]}"#
                    .to_owned(),
                "version range",
            ),
        ];
        for (vulnerability, message) in cases {
            let error = read_vex(&document(&vulnerability, "")).unwrap_err();
            assert!(
                error.to_string().contains(message),
                "{error} for {vulnerability}"
            );
        }
        assert!(matches!(read_vex(&document("", "")), Err(VexError::Empty)));
        assert!(matches!(read_vex(b"{}"), Err(VexError::Read(_))));
    }
}
