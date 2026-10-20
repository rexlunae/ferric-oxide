//! The SBOM of a whole install root: every installed package, its files
//! and the Rust crates its build SBOM lists.
//!
//! [`SystemSbom`] is what `fox sbom --system` writes for a running system
//! or an image root. Its `metadata.component` is the system, as an
//! operating system; each installed package is an application component
//! (`bom-ref` `pkg:<name>`) with the BLAKE3 of the archive it came from and
//! `fox:*` properties for its target, signer and dependencies. Nested under
//! each package are its files and, when the package carries a build SBOM
//! ([`read_build_sbom`]), the crates from it, with their `bom-ref`s
//! prefixed by `<name>/` so that two packages linking the same crate stay
//! distinct. The dependency graph joins the system to its packages, each
//! package to the installed packages it depends on and to its root crates,
//! and keeps the crate graph of every build SBOM.
//!
//! [`cargo_crates`] lists the crates of a build SBOM, which `fox audit`
//! matches against advisories.

use std::collections::BTreeSet;
use std::io::Write;

use cyclonedx_bom::models::component::Classification;
use cyclonedx_bom::models::component::Scope;
use cyclonedx_bom::models::dependency::{Dependencies, Dependency};
use cyclonedx_bom::models::property::{Properties, Property};
use cyclonedx_bom::prelude::*;
use fox_core::{Blake3Digest, FileRecord, KeyId, PackageName};

use crate::{SbomError, blake3_hashes, file_component, property, serial_number, tool_list};

/// Domain separation for the system SBOM's serial number.
const SERIAL_CONTEXT: &[u8] = b"fox-system-sbom/1\0";

/// Parses a build SBOM, as `fox-build` packs into each archive at
/// [`sbom_path`](fox_core::sbom_path).
///
/// # Errors
///
/// [`SbomError::Read`] if it is not CycloneDX JSON.
pub fn read_build_sbom(json: &[u8]) -> Result<Bom, SbomError> {
    Bom::parse_from_json(json).map_err(|source| SbomError::Read(Box::new(source)))
}

/// A Rust crate a package was built with.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CargoCrate {
    /// The crate name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// Whether it was used only at build time (build scripts and proc
    /// macros), so it is not linked into the package.
    pub build_only: bool,
}

/// The crates.io crates in a build SBOM: its library components with a
/// `pkg:cargo` purl whose `fox:registry` is crates.io. Git and path crates
/// have no purl, and crates from other registries share the purl type but
/// not crates.io's names, so all three are left out.
pub fn cargo_crates(bom: &Bom) -> Vec<CargoCrate> {
    let Some(components) = &bom.components else {
        return Vec::new();
    };
    components
        .0
        .iter()
        .filter(|component| {
            component
                .purl
                .as_ref()
                .is_some_and(|purl| purl.as_ref().starts_with("pkg:cargo/"))
                && from_crates_io(component)
        })
        .map(|component| CargoCrate {
            name: component.name.to_string(),
            version: component
                .version
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            build_only: component.scope == Some(Scope::Excluded),
        })
        .collect()
}

/// The index URLs Cargo records for crates.io (git and sparse).
const CRATES_IO_INDEXES: [&str; 2] = [
    "https://github.com/rust-lang/crates.io-index",
    "https://index.crates.io/",
];

fn from_crates_io(component: &cyclonedx_bom::models::component::Component) -> bool {
    component.properties.as_ref().is_some_and(|properties| {
        properties.0.iter().any(|property| {
            property.name == "fox:registry" && CRATES_IO_INDEXES.contains(&property.value.as_ref())
        })
    })
}

/// One installed package.
#[derive(Debug, Clone)]
pub struct SystemPackage<'a> {
    /// Its name.
    pub name: &'a PackageName,
    /// Its version.
    pub version: &'a str,
    /// Its target triple, if it recorded one.
    pub target: Option<&'a str>,
    /// The BLAKE3 of the archive it was installed from, when known.
    pub archive_blake3: Option<Blake3Digest>,
    /// The BLAKE3 of the archive's signed entry table.
    pub entries_blake3: Blake3Digest,
    /// The key that signed it.
    pub signer: KeyId,
    /// The label that key was trusted under.
    pub signer_label: &'a str,
    /// Its runtime dependencies.
    pub depends: &'a [PackageName],
    /// Its installed files.
    pub files: &'a [FileRecord],
    /// Its build SBOM, if it has one.
    pub build_sbom: Option<&'a Bom>,
}

/// The inputs for a system SBOM.
#[derive(Debug, Clone)]
pub struct SystemSbom<'a> {
    name: &'a str,
    packages: Vec<SystemPackage<'a>>,
}

impl<'a> SystemSbom<'a> {
    /// Describes a system called `name` (an image or host name) with these
    /// packages, sorted by name so the document does not depend on their
    /// order.
    pub fn new(name: &'a str, mut packages: Vec<SystemPackage<'a>>) -> Self {
        packages.sort_by(|a, b| a.name.cmp(b.name));
        Self { name, packages }
    }

    /// Builds the CycloneDX document.
    ///
    /// # Errors
    ///
    /// [`SbomError::Purl`] if a package name and version cannot form a
    /// purl.
    pub fn to_bom(&self) -> Result<Bom, SbomError> {
        let mut system = Component::new(
            Classification::OperatingSystem,
            self.name,
            "",
            Some(SYSTEM_REF.to_owned()),
        );
        system.version = None;
        let components = self
            .packages
            .iter()
            .map(package_component)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Bom {
            serial_number: Some(serial_number(self.identity())),
            metadata: Some(Metadata {
                tools: Some(tool_list()),
                component: Some(system),
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
    /// The errors of [`SystemSbom::to_bom`], and [`SbomError::Write`] if
    /// serialization or the writer fails.
    pub fn write_cyclonedx_json(&self, writer: &mut impl Write) -> Result<(), SbomError> {
        self.to_bom()?
            .output_as_json_v1_5(writer)
            .map_err(SbomError::Write)
    }

    /// Hashes every input that appears in the document, so two documents
    /// with different contents never share a serial number. Build SBOMs
    /// are covered through the hash in their file record.
    fn identity(&self) -> Blake3Digest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(SERIAL_CONTEXT);
        let text = |hasher: &mut blake3::Hasher, part: &str| {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        };
        text(&mut hasher, self.name);
        hasher.update(&(self.packages.len() as u64).to_le_bytes());
        for package in &self.packages {
            text(&mut hasher, package.name.as_str());
            text(&mut hasher, package.version);
            text(&mut hasher, package.target.unwrap_or(""));
            hasher.update(&[u8::from(package.archive_blake3.is_some())]);
            if let Some(archive) = package.archive_blake3 {
                hasher.update(archive.as_bytes());
            }
            hasher.update(package.entries_blake3.as_bytes());
            hasher.update(package.signer.as_bytes());
            text(&mut hasher, package.signer_label);
            hasher.update(&(package.depends.len() as u64).to_le_bytes());
            for depend in package.depends {
                text(&mut hasher, depend.as_str());
            }
            hasher.update(&(package.files.len() as u64).to_le_bytes());
            for file in package.files {
                text(&mut hasher, &file.path);
                hasher.update(&[u8::from(file.kind == fox_core::FileKind::Symlink)]);
                hasher.update(&file.mode.to_le_bytes());
                hasher.update(&file.size.to_le_bytes());
                hasher.update(file.blake3.as_bytes());
            }
        }
        hasher.finalize().into()
    }

    fn dependencies(&self) -> Dependencies {
        let installed: BTreeSet<&PackageName> =
            self.packages.iter().map(|package| package.name).collect();
        let mut list = vec![Dependency {
            dependency_ref: SYSTEM_REF.to_owned(),
            dependencies: self
                .packages
                .iter()
                .map(|package| package_ref(package.name))
                .collect(),
        }];
        for package in &self.packages {
            let own = package_ref(package.name);
            let mut depends: Vec<String> = package
                .depends
                .iter()
                .filter(|name| installed.contains(name))
                .map(package_ref)
                .collect();
            let graph = package
                .build_sbom
                .and_then(|bom| bom.dependencies.as_ref())
                .map_or(&[][..], |graph| &graph.0[..]);
            for entry in graph {
                let refs = entry
                    .dependencies
                    .iter()
                    .map(|id| crate_ref(package.name, id));
                if entry.dependency_ref == own {
                    depends.extend(refs);
                } else {
                    list.push(Dependency {
                        dependency_ref: crate_ref(package.name, &entry.dependency_ref),
                        dependencies: refs.collect(),
                    });
                }
            }
            list.push(Dependency {
                dependency_ref: own,
                dependencies: depends,
            });
        }
        Dependencies(list)
    }
}

/// The `bom-ref` of the system component.
const SYSTEM_REF: &str = "system";

fn package_ref(name: &PackageName) -> String {
    format!("pkg:{name}")
}

fn crate_ref(package: &PackageName, id: &str) -> String {
    format!("{package}/{id}")
}

fn package_component(package: &SystemPackage<'_>) -> Result<Component, SbomError> {
    let name = package.name.as_str();
    let mut component = Component::new(
        Classification::Application,
        name,
        package.version,
        Some(package_ref(package.name)),
    );
    component.purl = Some(Purl::new("generic", name, package.version).map_err(SbomError::Purl)?);
    component.hashes = package.archive_blake3.map(blake3_hashes);
    component.properties = Some(Properties(properties(package)));
    let mut nested: Vec<Component> = package.files.iter().map(file_component).collect();
    if let Some(bom) = package.build_sbom {
        if let Some(described) = bom.metadata.as_ref().and_then(|m| m.component.as_ref()) {
            component.description.clone_from(&described.description);
            component.licenses.clone_from(&described.licenses);
            component
                .external_references
                .clone_from(&described.external_references);
        }
        nested.extend(crate_components(package.name, bom));
    }
    for file in &mut nested {
        // File refs are paths, which two packages cannot share, but keep
        // every nested ref in the package's namespace all the same.
        if let Some(id) = &mut file.bom_ref
            && id.starts_with("file:")
        {
            *id = crate_ref(package.name, id);
        }
    }
    component.components = Some(Components(nested));
    Ok(component)
}

fn crate_components<'b>(
    package: &'b PackageName,
    bom: &'b Bom,
) -> impl Iterator<Item = Component> + 'b {
    bom.components
        .iter()
        .flat_map(|components| components.0.iter())
        .filter(|component| component.component_type == Classification::Library)
        .map(move |component| {
            let mut component = component.clone();
            component.bom_ref = component.bom_ref.map(|id| crate_ref(package, &id));
            component
        })
}

fn properties(package: &SystemPackage<'_>) -> Vec<Property> {
    let mut properties = Vec::new();
    if let Some(target) = package.target {
        properties.push(property("fox:target", target));
    }
    properties.push(property("fox:signer", &package.signer.to_string()));
    properties.push(property("fox:signer_label", package.signer_label));
    properties.push(property(
        "fox:entries_blake3",
        &package.entries_blake3.to_string(),
    ));
    properties.extend(
        package
            .depends
            .iter()
            .map(|dep| property("fox:depends", dep.as_str())),
    );
    properties
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildInfo, BuildSbom, CrateDependency, CrateSource};
    use fox_core::{FileKind, Manifest};

    fn file(path: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            kind: FileKind::File,
            mode: 0o755,
            size: 1,
            blake3: Blake3Digest::of(path.as_bytes()),
        }
    }

    fn build_sbom(manifest: &Manifest, files: &[FileRecord]) -> Bom {
        let crates = [
            CrateDependency {
                id: "smallcrate 0.3.1".into(),
                name: "smallcrate".into(),
                version: "0.3.1".into(),
                license: Some("MIT".into()),
                source: CrateSource::Registry {
                    url: "https://github.com/rust-lang/crates.io-index".into(),
                    checksum: None,
                },
                build_only: false,
                depends: vec!["cc 1.0.0".into(), "private 1.0.0".into()],
            },
            CrateDependency {
                id: "private 1.0.0".into(),
                name: "private".into(),
                version: "1.0.0".into(),
                license: None,
                source: CrateSource::Registry {
                    url: "https://crates.example.com/index".into(),
                    checksum: None,
                },
                build_only: false,
                depends: Vec::new(),
            },
            CrateDependency {
                id: "cc 1.0.0".into(),
                name: "cc".into(),
                version: "1.0.0".into(),
                license: None,
                source: CrateSource::Registry {
                    url: "https://github.com/rust-lang/crates.io-index".into(),
                    checksum: None,
                },
                build_only: true,
                depends: Vec::new(),
            },
        ];
        let roots = ["smallcrate 0.3.1".to_owned()];
        let info = BuildInfo {
            tree_blake3: Blake3Digest::of(b"tree"),
            cargo_lock_blake3: Blake3Digest::of(b"lock"),
            target: "x86_64-unknown-redox",
            cpu_level: None,
            features: &[],
            feature_set_hash: Blake3Digest::of(b"features"),
            toolchain_id: "rustc 1.0",
            profile: Default::default(),
        };
        let mut json = Vec::new();
        BuildSbom::new(manifest, info, &roots, &crates, files)
            .write_cyclonedx_json(&mut json)
            .unwrap();
        read_build_sbom(&json).unwrap()
    }

    #[test]
    fn merges_installed_packages() {
        let hello = PackageName::new("hello").unwrap();
        let base = PackageName::new("base").unwrap();
        let mut manifest = Manifest::new(hello.clone(), "1.0.0");
        manifest.summary = Some("Prints a greeting".into());
        let hello_files = [file("usr/bin/hello")];
        let bom = build_sbom(&manifest, &hello_files);
        let crates = cargo_crates(&bom);
        // The crate from another registry is not a crates.io crate.
        assert_eq!(crates.len(), 2);
        assert!(!crates.iter().any(|c| c.name == "private"));
        assert!(crates.iter().any(|c| c.name == "cc" && c.build_only));

        let depends = [base.clone(), PackageName::new("absent").unwrap()];
        let base_files = [file("usr/lib/base")];
        let packages = vec![
            SystemPackage {
                name: &hello,
                version: "1.0.0",
                target: Some("x86_64-unknown-redox"),
                archive_blake3: Some(Blake3Digest::of(b"archive")),
                entries_blake3: Blake3Digest::of(b"entries"),
                signer: KeyId::from_bytes([1; 32]),
                signer_label: "ci",
                depends: &depends,
                files: &hello_files,
                build_sbom: Some(&bom),
            },
            SystemPackage {
                name: &base,
                version: "0.1.0",
                target: None,
                archive_blake3: None,
                entries_blake3: Blake3Digest::of(b"base"),
                signer: KeyId::from_bytes([2; 32]),
                signer_label: "local",
                depends: &[],
                files: &base_files,
                build_sbom: None,
            },
        ];
        let sbom = SystemSbom::new("image", packages.clone());
        let built = sbom.to_bom().unwrap();
        assert!(built.validate().passed(), "{:?}", built.validate());
        let mut out = Vec::new();
        sbom.write_cyclonedx_json(&mut out).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["metadata"]["component"]["type"], "operating-system");
        let components = doc["components"].as_array().unwrap();
        assert_eq!(components[0]["name"], "base");
        let hello_doc = &components[1];
        assert_eq!(hello_doc["description"], "Prints a greeting");
        let nested = hello_doc["components"].as_array().unwrap();
        assert!(
            nested
                .iter()
                .any(|c| c["bom-ref"] == "hello/smallcrate 0.3.1")
        );
        assert!(
            nested
                .iter()
                .any(|c| c["bom-ref"] == "hello/file:usr/bin/hello")
        );
        let graph = doc["dependencies"].as_array().unwrap();
        let hello_deps = graph.iter().find(|d| d["ref"] == "pkg:hello").unwrap();
        assert_eq!(
            hello_deps["dependsOn"],
            serde_json::json!(["pkg:base", "hello/smallcrate 0.3.1"])
        );

        // A package re-signed with the same entries is another document.
        let mut resigned = packages.clone();
        resigned[1].signer = KeyId::from_bytes([3; 32]);
        let serial = |packages: Vec<SystemPackage<'_>>| {
            SystemSbom::new("image", packages)
                .to_bom()
                .unwrap()
                .serial_number
                .unwrap()
                .to_string()
        };
        assert_ne!(serial(resigned), serial(packages.clone()));

        // The same packages in another order give the same document.
        let mut reversed = packages;
        reversed.reverse();
        let mut again = Vec::new();
        SystemSbom::new("image", reversed)
            .write_cyclonedx_json(&mut again)
            .unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn rejects_non_cyclonedx() {
        assert!(matches!(read_build_sbom(b"{}"), Err(SbomError::Read(_))));
    }
}
