//! CycloneDX SBOMs for fox packages.
//!
//! The packaging design (section 7.7) makes CycloneDX JSON the stored SBOM
//! format. This crate builds one from what a verified package already
//! states about itself: its [`Manifest`], the BLAKE3 of the archive and the
//! [`FileRecord`] of every file. The document is built with the
//! [`cyclonedx-bom`](https://crates.io/crates/cyclonedx-bom) crate (the
//! library behind `cargo-cyclonedx`) and written as CycloneDX 1.5, the
//! newest version that crate writes.
//!
//! The SBOM is reproducible: it has no timestamp, and its serial number is
//! derived from the archive hash, so the same archive always yields the
//! same bytes.
//!
//! Layout of the document:
//!
//! - `metadata.component`: the package, as an application, with its
//!   version, licence, `pkg:generic/<name>@<version>` purl, BLAKE3 of the
//!   archive, VCS reference, and `fox:*` properties for the target, the
//!   signer key and the runtime dependencies;
//! - `components`: one `file` component per installed file, named by its
//!   path, with its BLAKE3.
//!
//! Dependency lists from `Cargo.lock` and `cargo auditable` data come with
//! the hermetic builds of Phase P2.
//!
//! # Examples
//!
//! ```
//! use fox_core::{Blake3Digest, FileKind, FileRecord, Manifest, PackageName};
//! use fox_sbom::PackageSbom;
//!
//! let manifest = Manifest::new(PackageName::new("hello")?, "1.0.0");
//! let files = [FileRecord {
//!     path: "usr/bin/hello".into(),
//!     kind: FileKind::File,
//!     mode: 0o755,
//!     size: 5,
//!     blake3: Blake3Digest::of(b"hello"),
//! }];
//! let sbom = PackageSbom::new(&manifest, Blake3Digest::of(b"archive"), &files);
//! let mut json = Vec::new();
//! sbom.write_cyclonedx_json(&mut json)?;
//! assert!(String::from_utf8(json)?.contains("\"bomFormat\": \"CycloneDX\""));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::io::Write;

use cyclonedx_bom::external_models::uri::Uri;
use cyclonedx_bom::models::component::Classification;
use cyclonedx_bom::models::external_reference::Uri as ReferenceUri;
use cyclonedx_bom::models::external_reference::{
    ExternalReference, ExternalReferenceType, ExternalReferences,
};
use cyclonedx_bom::models::hash::{Hash, HashAlgorithm, HashValue, Hashes};
use cyclonedx_bom::models::license::{LicenseChoice, Licenses};
use cyclonedx_bom::models::property::{Properties, Property};
use cyclonedx_bom::models::tool::{Tool, Tools};
use cyclonedx_bom::prelude::*;
use fox_core::{Blake3Digest, FileKind, FileRecord, KeyId, Manifest};

/// Why an SBOM could not be built or written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SbomError {
    /// The package licence is not a valid SPDX expression.
    #[error("licence {license:?} is not a valid SPDX expression")]
    License {
        /// The licence text from the manifest.
        license: String,
        /// The parse error.
        #[source]
        source: cyclonedx_bom::external_models::spdx::SpdxExpressionError,
    },
    /// The package name or version cannot form a purl.
    #[error("cannot build a purl for the package")]
    Purl(#[source] cyclonedx_bom::external_models::uri::UriError),
    /// The document could not be serialized or written.
    #[error("cannot write the SBOM")]
    Write(#[source] cyclonedx_bom::errors::JsonWriteError),
}

/// The inputs for one package's SBOM.
#[derive(Debug, Clone)]
pub struct PackageSbom<'a> {
    manifest: &'a Manifest,
    archive_blake3: Blake3Digest,
    files: &'a [FileRecord],
    signer: Option<KeyId>,
}

impl<'a> PackageSbom<'a> {
    /// Describes the package with `manifest`, whose archive hashes to
    /// `archive_blake3` and holds `files`.
    pub fn new(
        manifest: &'a Manifest,
        archive_blake3: Blake3Digest,
        files: &'a [FileRecord],
    ) -> Self {
        Self {
            manifest,
            archive_blake3,
            files,
            signer: None,
        }
    }

    /// Records the key that signed the archive.
    pub fn with_signer(mut self, signer: KeyId) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Builds the CycloneDX document.
    ///
    /// # Errors
    ///
    /// [`SbomError::License`] if the manifest licence is not SPDX,
    /// and [`SbomError::Purl`] if the name and version cannot form a purl.
    pub fn to_bom(&self) -> Result<Bom, SbomError> {
        let files = self.files.iter().map(file_component).collect();
        Ok(Bom {
            serial_number: Some(serial_number(self.archive_blake3)),
            metadata: Some(Metadata {
                tools: Some(Tools::List(vec![Tool {
                    name: Some(NormalizedString::new("fox")),
                    version: Some(NormalizedString::new(env!("CARGO_PKG_VERSION"))),
                    ..Tool::default()
                }])),
                component: Some(self.package_component()?),
                ..Metadata::default()
            }),
            components: Some(Components(files)),
            ..Bom::default()
        })
    }

    /// Writes the document as CycloneDX 1.5 JSON.
    ///
    /// # Errors
    ///
    /// The errors of [`PackageSbom::to_bom`], and [`SbomError::Write`] if
    /// serialization or the writer fails.
    pub fn write_cyclonedx_json(&self, writer: &mut impl Write) -> Result<(), SbomError> {
        self.to_bom()?
            .output_as_json_v1_5(writer)
            .map_err(SbomError::Write)
    }

    fn package_component(&self) -> Result<Component, SbomError> {
        let manifest = self.manifest;
        let name = manifest.name.as_str();
        let mut component = Component::new(
            Classification::Application,
            name,
            &manifest.version,
            Some(format!("pkg:{name}")),
        );
        component.description = manifest.summary.as_deref().map(NormalizedString::new);
        component.purl =
            Some(Purl::new("generic", name, &manifest.version).map_err(SbomError::Purl)?);
        component.hashes = Some(blake3_hashes(self.archive_blake3));
        if let Some(license) = &manifest.license {
            let expression = SpdxExpression::parse_lax(license.clone()).map_err(|source| {
                SbomError::License {
                    license: license.clone(),
                    source,
                }
            })?;
            component.licenses = Some(Licenses(vec![LicenseChoice::Expression(expression)]));
        }
        if let Some(source) = &manifest.source {
            component.external_references = Some(ExternalReferences(vec![ExternalReference {
                external_reference_type: ExternalReferenceType::Vcs,
                url: ReferenceUri::Url(Uri::new(&source.git)),
                comment: source
                    .commit
                    .as_ref()
                    .map(|commit| format!("commit {commit}")),
                hashes: None,
            }]));
        }
        component.properties = Some(Properties(self.properties()));
        Ok(component)
    }

    fn properties(&self) -> Vec<Property> {
        let manifest = self.manifest;
        let mut properties = Vec::new();
        let mut add = |name: &str, value: &str| {
            properties.push(Property {
                name: name.to_owned(),
                value: NormalizedString::new(value),
            });
        };
        if let Some(target) = &manifest.target {
            add("fox:target", target);
        }
        if let Some(signer) = self.signer {
            add("fox:signer", &signer.to_string());
        }
        for dep in &manifest.depends {
            add("fox:depends", dep.as_str());
        }
        properties
    }
}

fn file_component(file: &FileRecord) -> Component {
    let mut component = Component::new(
        Classification::File,
        &file.path,
        "",
        Some(format!("file:{}", file.path)),
    );
    // Files have no version of their own; the package carries it.
    component.version = None;
    component.hashes = Some(blake3_hashes(file.blake3));
    if file.kind == FileKind::Symlink {
        component.properties = Some(Properties(vec![Property {
            name: "fox:kind".to_owned(),
            value: NormalizedString::new("symlink"),
        }]));
    }
    component
}

fn blake3_hashes(digest: Blake3Digest) -> Hashes {
    Hashes(vec![Hash {
        alg: HashAlgorithm::BLAKE3,
        content: HashValue(digest.to_string()),
    }])
}

/// A version 8 (custom) UUID URN taken from the first 16 bytes of the
/// archive hash, so the serial number is stable for a given archive.
fn serial_number(archive: Blake3Digest) -> UrnUuid {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&archive.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let urn = format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    UrnUuid::new(urn).expect("a formatted hex UUID always matches the URN pattern")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_core::{PackageName, SourceInfo};

    fn manifest() -> Manifest {
        let mut manifest = Manifest::new(PackageName::new("hello").unwrap(), "1.0.0");
        manifest.license = Some("MIT OR Apache-2.0".into());
        manifest.summary = Some("Prints a greeting".into());
        manifest.target = Some("x86_64-unknown-redox".into());
        manifest.depends = vec![PackageName::new("relibc").unwrap()];
        manifest.source = Some(SourceInfo {
            git: "https://example.com/hello.git".into(),
            commit: Some("abc123".into()),
        });
        manifest
    }

    fn files() -> Vec<FileRecord> {
        vec![
            FileRecord {
                path: "usr/bin/hello".into(),
                kind: FileKind::File,
                mode: 0o755,
                size: 5,
                blake3: Blake3Digest::of(b"hello"),
            },
            FileRecord {
                path: "usr/bin/hi".into(),
                kind: FileKind::Symlink,
                mode: 0o777,
                size: 5,
                blake3: Blake3Digest::of(b"hello"),
            },
        ]
    }

    fn json(sbom: &PackageSbom<'_>) -> serde_json::Value {
        let mut out = Vec::new();
        sbom.write_cyclonedx_json(&mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn describes_package_and_files() {
        let manifest = manifest();
        let files = files();
        let archive = Blake3Digest::of(b"archive");
        let sbom =
            PackageSbom::new(&manifest, archive, &files).with_signer(KeyId::from_bytes([3; 32]));
        assert!(sbom.to_bom().unwrap().validate().passed());

        let doc = json(&sbom);
        assert_eq!(doc["specVersion"], "1.5");
        let package = &doc["metadata"]["component"];
        assert_eq!(package["name"], "hello");
        assert_eq!(package["version"], "1.0.0");
        assert_eq!(package["purl"], "pkg:generic/hello@1.0.0");
        assert_eq!(package["hashes"][0]["alg"], "BLAKE3");
        assert_eq!(package["hashes"][0]["content"], archive.to_string());
        assert_eq!(package["licenses"][0]["expression"], "MIT OR Apache-2.0");
        assert_eq!(package["externalReferences"][0]["type"], "vcs");
        let props = package["properties"].as_array().unwrap();
        assert!(
            props
                .iter()
                .any(|p| p["name"] == "fox:depends" && p["value"] == "relibc")
        );
        assert!(props.iter().any(|p| p["name"] == "fox:signer"));

        let components = doc["components"].as_array().unwrap();
        assert_eq!(components.len(), 2);
        assert_eq!(components[0]["type"], "file");
        assert_eq!(components[0]["name"], "usr/bin/hello");
        assert!(components[0].get("version").is_none());
        assert_eq!(components[1]["properties"][0]["value"], "symlink");
    }

    #[test]
    fn is_reproducible() {
        let manifest = manifest();
        let files = files();
        let sbom = PackageSbom::new(&manifest, Blake3Digest::of(b"a"), &files);
        assert_eq!(json(&sbom), json(&sbom));
        let serial = json(&sbom)["serialNumber"].as_str().unwrap().to_owned();
        assert!(serial.starts_with("urn:uuid:"));
        assert_eq!(&serial[23..24], "8");
        let other = PackageSbom::new(&manifest, Blake3Digest::of(b"b"), &files);
        assert_ne!(json(&other)["serialNumber"], serial.as_str());
    }

    #[test]
    fn rejects_bad_license() {
        let mut manifest = manifest();
        manifest.license = Some("not a (licence".into());
        let files = files();
        let sbom = PackageSbom::new(&manifest, Blake3Digest::of(b"a"), &files);
        assert!(matches!(sbom.to_bom(), Err(SbomError::License { .. })));
    }
}
