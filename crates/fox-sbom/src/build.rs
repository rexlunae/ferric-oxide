//! The SBOM of a hermetic build.

use std::collections::BTreeSet;
use std::io::Write;

use cyclonedx_bom::models::component::{Classification, Scope};
use cyclonedx_bom::models::dependency::{Dependencies, Dependency};
use cyclonedx_bom::models::external_reference::ExternalReferences;
use cyclonedx_bom::models::hash::{Hash, HashAlgorithm, HashValue, Hashes};
use cyclonedx_bom::models::license::{License, LicenseChoice, Licenses};
use cyclonedx_bom::models::property::{Properties, Property};
use cyclonedx_bom::prelude::*;
use fox_core::{Blake3Digest, CpuLevel, FileRecord, Manifest, Profile};

use crate::{
    SbomError, file_component, package_component, package_ref, property, serial_number, tool_list,
    vcs_reference,
};

/// Bytes hashed before the build identity to form the serial number.
const SERIAL_CONTEXT: &[u8] = b"fox-build-sbom/1\0";

/// What was built, and how: the inputs that identify the artifact.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo<'a> {
    /// `fox-tree/1` hash of the source tree.
    pub tree_blake3: Blake3Digest,
    /// BLAKE3 of the `Cargo.lock` used.
    pub cargo_lock_blake3: Blake3Digest,
    /// Rust target triple.
    pub target: &'a str,
    /// CPU level, for native x86-64 builds.
    pub cpu_level: Option<CpuLevel>,
    /// The package's enabled Cargo features.
    pub features: &'a [String],
    /// Hash of the features and the lockfile.
    pub feature_set_hash: Blake3Digest,
    /// Toolchain id.
    pub toolchain_id: &'a str,
    /// Cargo profile.
    pub profile: Profile,
}

/// Where a crate came from, as `Cargo.lock` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CrateSource {
    /// A registry, such as crates.io.
    Registry {
        /// The registry index URL.
        url: String,
        /// The SHA-256 checksum `Cargo.lock` pins, as hex.
        checksum: Option<String>,
    },
    /// A git repository at a commit.
    Git {
        /// The repository URL.
        url: String,
        /// The pinned commit.
        commit: String,
    },
    /// A path inside the source tree (the package's own workspace).
    Path,
}

/// One Rust crate in the build's dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateDependency {
    /// A unique id for the crate, such as Cargo's package id. It becomes the
    /// component's `bom-ref`.
    pub id: String,
    /// Crate name.
    pub name: String,
    /// Crate version.
    pub version: String,
    /// Where it came from.
    pub source: CrateSource,
    /// Its licence, usually an SPDX expression.
    pub license: Option<String>,
    /// Whether it is only used at build time (build scripts and
    /// procedural macros), not linked into the output.
    pub build_only: bool,
    /// Ids of the crates it depends on.
    pub depends: Vec<String>,
}

/// The inputs for a build's SBOM.
///
/// # Examples
///
/// ```
/// use fox_core::{Blake3Digest, FileKind, FileRecord, Manifest, PackageName, Profile};
/// use fox_sbom::{BuildInfo, BuildSbom, CrateDependency, CrateSource};
///
/// let manifest = Manifest::new(PackageName::new("hello")?, "1.0.0");
/// let digest = Blake3Digest::of(b"x");
/// let info = BuildInfo {
///     tree_blake3: digest,
///     cargo_lock_blake3: digest,
///     target: "x86_64-unknown-redox",
///     cpu_level: None,
///     features: &[],
///     feature_set_hash: digest,
///     toolchain_id: "rust-1.93.0:00",
///     profile: Profile::Release,
/// };
/// let crates = [CrateDependency {
///     id: "libc 0.2.0".into(),
///     name: "libc".into(),
///     version: "0.2.0".into(),
///     source: CrateSource::Registry {
///         url: "https://github.com/rust-lang/crates.io-index".into(),
///         checksum: Some("00".repeat(32)),
///     },
///     license: Some("MIT OR Apache-2.0".into()),
///     build_only: false,
///     depends: vec![],
/// }];
/// let roots = ["libc 0.2.0".to_owned()];
/// let sbom = BuildSbom::new(&manifest, info, &roots, &crates, &[]);
/// let mut json = Vec::new();
/// sbom.write_cyclonedx_json(&mut json)?;
/// assert!(String::from_utf8(json)?.contains("pkg:cargo/libc@0.2.0"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct BuildSbom<'a> {
    manifest: &'a Manifest,
    info: BuildInfo<'a>,
    roots: &'a [String],
    crates: &'a [CrateDependency],
    files: &'a [FileRecord],
}

impl<'a> BuildSbom<'a> {
    /// Describes the package with `manifest`, built as `info`, whose direct
    /// dependencies are the crates with ids `roots`, whose full graph is
    /// `crates`, and which installs `files`.
    pub fn new(
        manifest: &'a Manifest,
        info: BuildInfo<'a>,
        roots: &'a [String],
        crates: &'a [CrateDependency],
        files: &'a [FileRecord],
    ) -> Self {
        Self {
            manifest,
            info,
            roots,
            crates,
            files,
        }
    }

    /// Builds the CycloneDX document.
    ///
    /// # Errors
    ///
    /// [`SbomError::License`] if the package licence is not SPDX,
    /// [`SbomError::Purl`] if a name and version cannot form a purl, and
    /// [`SbomError::UnknownCrate`] if a dependency id names no crate in the
    /// graph.
    pub fn to_bom(&self) -> Result<Bom, SbomError> {
        self.check_refs()?;
        let mut package = package_component(self.manifest)?;
        package.properties = Some(Properties(self.properties()));
        let mut components = self
            .crates
            .iter()
            .map(crate_component)
            .collect::<Result<Vec<_>, _>>()?;
        components.extend(self.files.iter().map(file_component));
        Ok(Bom {
            serial_number: Some(serial_number(self.identity())),
            metadata: Some(Metadata {
                tools: Some(tool_list()),
                component: Some(package),
                ..Metadata::default()
            }),
            components: Some(Components(components)),
            dependencies: Some(self.dependencies()),
            ..Bom::default()
        })
    }

    /// Writes the document as CycloneDX 1.5 JSON.
    ///
    /// # Errors
    ///
    /// The errors of [`BuildSbom::to_bom`], and [`SbomError::Write`] if
    /// serialization or the writer fails.
    pub fn write_cyclonedx_json(&self, writer: &mut impl Write) -> Result<(), SbomError> {
        self.to_bom()?
            .output_as_json_v1_5(writer)
            .map_err(SbomError::Write)
    }

    /// Checks every dependency id names a crate in the graph.
    fn check_refs(&self) -> Result<(), SbomError> {
        let known: BTreeSet<&str> = self.crates.iter().map(|c| c.id.as_str()).collect();
        let all_refs = self
            .roots
            .iter()
            .chain(self.crates.iter().flat_map(|c| c.depends.iter()));
        for id in all_refs {
            if !known.contains(id.as_str()) {
                return Err(SbomError::UnknownCrate { id: id.clone() });
            }
        }
        Ok(())
    }

    /// The digest the serial number is taken from: everything that
    /// identifies the artifact.
    fn identity(&self) -> Blake3Digest {
        let info = &self.info;
        let mut hasher = blake3::Hasher::new();
        hasher.update(SERIAL_CONTEXT);
        for part in [
            self.manifest.name.as_str(),
            &self.manifest.version,
            info.target,
            info.cpu_level.map_or("", CpuLevel::as_str),
            info.toolchain_id,
            info.profile.as_str(),
        ] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        hasher.update(info.tree_blake3.as_bytes());
        hasher.update(info.feature_set_hash.as_bytes());
        hasher.finalize().into()
    }

    fn properties(&self) -> Vec<Property> {
        let info = &self.info;
        let mut properties = vec![
            property("fox:tree_blake3", &info.tree_blake3.to_string()),
            property("fox:cargo_lock_blake3", &info.cargo_lock_blake3.to_string()),
            property("fox:target", info.target),
        ];
        if let Some(level) = info.cpu_level {
            properties.push(property("fox:cpu_level", level.as_str()));
        }
        properties.extend(info.features.iter().map(|f| property("fox:feature", f)));
        properties.push(property(
            "fox:feature_set_hash",
            &info.feature_set_hash.to_string(),
        ));
        properties.push(property("fox:toolchain", info.toolchain_id));
        properties.push(property("fox:profile", info.profile.as_str()));
        properties.extend(
            self.manifest
                .depends
                .iter()
                .map(|dep| property("fox:depends", dep.as_str())),
        );
        properties
    }

    fn dependencies(&self) -> Dependencies {
        let mut list = vec![Dependency {
            dependency_ref: package_ref(self.manifest),
            dependencies: self.roots.to_vec(),
        }];
        list.extend(self.crates.iter().map(|c| Dependency {
            dependency_ref: c.id.clone(),
            dependencies: c.depends.clone(),
        }));
        Dependencies(list)
    }
}

fn crate_component(krate: &CrateDependency) -> Result<Component, SbomError> {
    let mut component = Component::new(
        Classification::Library,
        &krate.name,
        &krate.version,
        Some(krate.id.clone()),
    );
    component.scope = Some(if krate.build_only {
        Scope::Excluded
    } else {
        Scope::Required
    });
    component.licenses = krate.license.as_deref().map(crate_license);
    let mut properties = Vec::new();
    match &krate.source {
        CrateSource::Registry { url, checksum } => {
            component.purl =
                Some(Purl::new("cargo", &krate.name, &krate.version).map_err(SbomError::Purl)?);
            component.hashes = checksum.as_ref().map(|sum| {
                Hashes(vec![Hash {
                    alg: HashAlgorithm::SHA_256,
                    content: HashValue(sum.clone()),
                }])
            });
            properties.push(property("fox:registry", url));
        }
        CrateSource::Git { url, commit } => {
            component.external_references =
                Some(ExternalReferences(vec![vcs_reference(url, Some(commit))]));
            properties.push(property("fox:source", "git"));
        }
        CrateSource::Path => properties.push(property("fox:source", "path")),
    }
    if krate.build_only {
        properties.push(property("fox:dep-kind", "build"));
    }
    component.properties = Some(Properties(properties));
    Ok(component)
}

/// A crate's licence: an SPDX expression where it parses (old `MIT/Apache-2.0`
/// style included), otherwise kept as a named licence. Dependency licences
/// are recorded as published, so an odd one must not fail the SBOM.
fn crate_license(license: &str) -> Licenses {
    let choice = match SpdxExpression::parse_lax(license.to_owned()) {
        Ok(expression) => LicenseChoice::Expression(expression),
        Err(_) => LicenseChoice::License(License::named_license(license)),
    };
    Licenses(vec![choice])
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_core::{FileKind, PackageName, SourceInfo};

    fn manifest() -> Manifest {
        let mut manifest = Manifest::new(PackageName::new("hello").unwrap(), "1.0.0");
        manifest.license = Some("MIT".into());
        manifest.source = Some(SourceInfo {
            git: "https://example.com/hello.git".into(),
            commit: Some("ab".repeat(20)),
        });
        manifest
    }

    fn crates() -> Vec<CrateDependency> {
        vec![
            CrateDependency {
                id: "registry+https://github.com/rust-lang/crates.io-index#libc@0.2.1".into(),
                name: "libc".into(),
                version: "0.2.1".into(),
                source: CrateSource::Registry {
                    url: "https://github.com/rust-lang/crates.io-index".into(),
                    checksum: Some("11".repeat(32)),
                },
                license: Some("MIT/Apache-2.0".into()),
                build_only: false,
                depends: vec![],
            },
            CrateDependency {
                id: "git+https://example.com/cc#cc@1.0.0".into(),
                name: "cc".into(),
                version: "1.0.0".into(),
                source: CrateSource::Git {
                    url: "https://example.com/cc".into(),
                    commit: "cd".repeat(20),
                },
                license: Some("Custom licence, see LICENSE".into()),
                build_only: true,
                depends: vec![
                    "registry+https://github.com/rust-lang/crates.io-index#libc@0.2.1".into(),
                ],
            },
        ]
    }

    fn files() -> Vec<FileRecord> {
        vec![FileRecord {
            path: "usr/bin/hello".into(),
            kind: FileKind::File,
            mode: 0o755,
            size: 2,
            blake3: Blake3Digest::of(b"hi"),
        }]
    }

    fn info(features: &[String]) -> BuildInfo<'_> {
        BuildInfo {
            tree_blake3: Blake3Digest::of(b"tree"),
            cargo_lock_blake3: Blake3Digest::of(b"lock"),
            target: "x86_64-unknown-redox",
            cpu_level: Some(CpuLevel::X86_64V3),
            features,
            feature_set_hash: Blake3Digest::of(b"features"),
            toolchain_id: "rust-1.93.0:00",
            profile: Profile::Release,
        }
    }

    fn json(sbom: &BuildSbom<'_>) -> serde_json::Value {
        let mut out = Vec::new();
        sbom.write_cyclonedx_json(&mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn lists_crates_files_and_the_graph() {
        let manifest = manifest();
        let crates = crates();
        let files = files();
        let features = vec!["default".to_owned()];
        let roots = vec![crates[0].id.clone(), crates[1].id.clone()];
        let sbom = BuildSbom::new(&manifest, info(&features), &roots, &crates, &files);
        assert!(sbom.to_bom().unwrap().validate().passed());

        let doc = json(&sbom);
        let package = &doc["metadata"]["component"];
        assert_eq!(package["bom-ref"], "pkg:hello");
        assert!(package.get("hashes").is_none());
        let props = package["properties"].as_array().unwrap();
        let prop = |name: &str| {
            props
                .iter()
                .find(|p| p["name"] == name)
                .map(|p| p["value"].clone())
        };
        assert_eq!(prop("fox:cpu_level").unwrap(), "x86_64-v3");
        assert_eq!(prop("fox:feature").unwrap(), "default");
        assert_eq!(
            prop("fox:tree_blake3").unwrap(),
            Blake3Digest::of(b"tree").to_string()
        );

        let components = doc["components"].as_array().unwrap();
        assert_eq!(components.len(), 3);
        let libc = &components[0];
        assert_eq!(libc["purl"], "pkg:cargo/libc@0.2.1");
        assert_eq!(libc["hashes"][0]["alg"], "SHA-256");
        assert_eq!(libc["scope"], "required");
        assert_eq!(libc["licenses"][0]["expression"], "MIT OR Apache-2.0");
        let cc = &components[1];
        assert!(cc.get("purl").is_none());
        assert_eq!(cc["scope"], "excluded");
        assert_eq!(cc["externalReferences"][0]["type"], "vcs");
        assert_eq!(
            cc["licenses"][0]["license"]["name"],
            "Custom licence, see LICENSE"
        );
        assert_eq!(components[2]["type"], "file");

        let deps = doc["dependencies"].as_array().unwrap();
        assert_eq!(deps[0]["ref"], "pkg:hello");
        assert_eq!(deps[0]["dependsOn"].as_array().unwrap().len(), 2);
        assert_eq!(deps[2]["dependsOn"][0], crates[0].id.as_str());
    }

    #[test]
    fn serial_follows_the_build_identity() {
        let manifest = manifest();
        let crates = crates();
        let none: Vec<String> = Vec::new();
        let default = vec!["default".to_owned()];
        let a = BuildSbom::new(&manifest, info(&none), &[], &crates, &[]);
        assert_eq!(json(&a), json(&a));
        let mut other = info(&default);
        other.cpu_level = Some(CpuLevel::X86_64V4);
        let b = BuildSbom::new(&manifest, other, &[], &crates, &[]);
        assert_ne!(json(&a)["serialNumber"], json(&b)["serialNumber"]);
    }

    #[test]
    fn rejects_dangling_dependency_ids() {
        let manifest = manifest();
        let crates = crates();
        let roots = vec!["nope".to_owned()];
        let none: Vec<String> = Vec::new();
        let sbom = BuildSbom::new(&manifest, info(&none), &roots, &crates, &[]);
        assert!(matches!(
            sbom.to_bom(),
            Err(SbomError::UnknownCrate { id }) if id == "nope"
        ));
    }
}
