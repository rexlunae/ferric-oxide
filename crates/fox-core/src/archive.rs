//! Opening and fully verifying `.pkgar` files.
//!
//! [`Archive::open`] performs every check `fox` makes before it trusts a
//! package file:
//!
//! 1. the header is signed by a [`TrustedKeys`] key and
//!    the signature verifies ([`verify_head`]);
//! 2. the entry table matches the signed hash, and every entry path is safe;
//! 3. every entry's data, decompressed when the archive is LZMA2, hashes to
//!    the BLAKE3 value in its entry and has the size its framing claims;
//! 4. the package metadata is found and parsed.
//!
//! Metadata comes from the manifest inside the archive
//! ([`manifest_path`](crate::manifest_path)), which the signature covers.
//! Archives the cookbook builds today have no manifest; for those the
//! cookbook's metadata file next to the archive (`<name>.toml`, as written
//! by `repo cook`) is read instead. That file is not signed, but its
//! `blake3` field must equal the signed entry-table hash, so it cannot be
//! moved to another archive. [`MetadataSource`] records which one was used.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use pkgar::ext::PackageSrcExt;
use pkgar_core::{HEADER_SIZE, PackageSrc, Packaging};

use crate::{
    Blake3Digest, FileRecord, MANIFEST_DIR, Manifest, ManifestError, NameError, PackageName,
    TrustedKeys, VerifiedHead, VerifyError, head_size, verify_head,
};

/// Manifests larger than this are refused, so a hostile archive cannot make
/// `fox` buffer an arbitrary amount of data.
const MAX_MANIFEST_SIZE: usize = 64 * 1024;

/// Where an archive's [`Manifest`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataSource {
    /// The manifest inside the archive, covered by the header signature.
    Embedded,
    /// The cookbook metadata file at this path, bound to the archive by its
    /// `blake3` field but not itself signed.
    Sidecar(PathBuf),
}

/// Why an archive failed to open or verify.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ArchiveError {
    /// The archive could not be opened or read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The archive.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The header or entry table failed verification.
    #[error("{} failed verification", path.display())]
    Verify {
        /// The archive.
        path: PathBuf,
        /// What failed.
        #[source]
        source: VerifyError,
    },
    /// The `pkgar` library could not read the archive.
    #[error("cannot read package data in {}", path.display())]
    Pkgar {
        /// The archive.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<pkgar::Error>,
    },
    /// The archive changed on disk while it was being read.
    #[error("{} changed while it was being verified", path.display())]
    Changed {
        /// The archive.
        path: PathBuf,
    },
    /// An entry's data does not match the hash in the signed entry table.
    #[error("{entry}: content hash {actual} does not match signed hash {expected}")]
    EntryHash {
        /// The entry path.
        entry: String,
        /// The signed hash.
        expected: Blake3Digest,
        /// The hash of the data in the archive.
        actual: Blake3Digest,
    },
    /// An entry's data is shorter or longer than its framing says.
    #[error("{entry}: data is {actual} bytes, expected {expected}")]
    EntrySize {
        /// The entry path.
        entry: String,
        /// The size recorded in the archive.
        expected: u64,
        /// The size read.
        actual: u64,
    },
    /// The archive holds more than one manifest.
    #[error("archive holds two manifests: {first} and {second}")]
    MultipleManifests {
        /// The first manifest path.
        first: String,
        /// The second manifest path.
        second: String,
    },
    /// The manifest is larger than 64 KiB.
    #[error("manifest {entry} is larger than 64 KiB")]
    ManifestTooLarge {
        /// The manifest path.
        entry: String,
    },
    /// The manifest is not valid UTF-8.
    #[error("manifest {entry} is not UTF-8")]
    ManifestNotUtf8 {
        /// The manifest path.
        entry: String,
    },
    /// The manifest could not be parsed.
    #[error("invalid manifest {entry}")]
    Manifest {
        /// The manifest path.
        entry: String,
        /// The parse error.
        #[source]
        source: ManifestError,
    },
    /// The manifest's file name and its `name` field disagree.
    #[error("manifest {entry} names package {name}")]
    ManifestNameMismatch {
        /// The manifest path.
        entry: String,
        /// The name inside the manifest.
        name: PackageName,
    },
    /// The archive has no manifest and no cookbook metadata file.
    #[error("{} has no manifest and there is no metadata file at {}", archive.display(), sidecar.display())]
    NoMetadata {
        /// The archive.
        archive: PathBuf,
        /// Where the metadata file was looked for.
        sidecar: PathBuf,
    },
    /// The cookbook metadata file could not be read.
    #[error("cannot read package metadata {}", path.display())]
    Sidecar {
        /// The metadata file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<pkg::PackageError>,
    },
    /// The cookbook metadata file belongs to a different archive.
    #[error("metadata {} is for entry table {found:?}, archive has {expected}", path.display())]
    SidecarMismatch {
        /// The metadata file.
        path: PathBuf,
        /// The archive's signed entry-table hash.
        expected: Blake3Digest,
        /// The `blake3` value in the metadata file.
        found: String,
    },
    /// The cookbook metadata names a package or dependency fox cannot use.
    #[error("metadata {} has an unusable package name {name:?}", path.display())]
    SidecarName {
        /// The metadata file.
        path: PathBuf,
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        #[source]
        source: NameError,
    },
}

/// A `.pkgar` file whose signature, entry table and every entry's data have
/// been verified, with its metadata.
///
/// # Examples
///
/// ```no_run
/// use fox_core::archive::Archive;
/// use fox_core::keys::load_key_file;
/// use fox_core::TrustedKeys;
/// use std::path::Path;
///
/// let key = load_key_file(Path::new("build/id_ed25519.pub.toml"))?;
/// let mut keys = TrustedKeys::new();
/// keys.insert(key.label, key.key);
/// let archive = Archive::open(Path::new("hello.pkgar"), &keys)?;
/// println!("{} {}", archive.manifest().name, archive.manifest().version);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Archive {
    path: PathBuf,
    head: VerifiedHead,
    blake3: Blake3Digest,
    size: u64,
    manifest: Manifest,
    metadata: MetadataSource,
    files: Vec<FileRecord>,
}

impl Archive {
    /// Opens `path` and verifies all of it against `keys`.
    ///
    /// # Errors
    ///
    /// - [`ArchiveError::Io`] if the file cannot be read;
    /// - [`ArchiveError::Verify`] if the header or entry table fails
    ///   [`verify_head`];
    /// - [`ArchiveError::Pkgar`], [`ArchiveError::EntryHash`] or
    ///   [`ArchiveError::EntrySize`] if entry data is unreadable or does not
    ///   match;
    /// - [`ArchiveError::Changed`] if the file changes while it is read;
    /// - the manifest and sidecar variants if the metadata is missing or
    ///   invalid.
    pub fn open(path: &Path, keys: &TrustedKeys) -> Result<Self, ArchiveError> {
        let head = read_head(path, keys)?;
        let mut package = open_package_file(path, &head)?;
        let (files, manifest_text) = hash_entries(path, &head, &mut package)?;
        let (blake3, size) = hash_file(path)?;
        let (manifest, metadata) = match manifest_text {
            Some((entry, text)) => (parse_embedded(&entry, &text)?, MetadataSource::Embedded),
            None => {
                let sidecar = path.with_extension("toml");
                let manifest = read_sidecar(path, &sidecar, &head)?;
                (manifest, MetadataSource::Sidecar(sidecar))
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            head,
            blake3,
            size,
            manifest,
            metadata,
            files,
        })
    }

    /// The archive path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The verified head.
    pub fn head(&self) -> &VerifiedHead {
        &self.head
    }

    /// BLAKE3 of the whole archive file: its content address.
    pub fn blake3(&self) -> Blake3Digest {
        self.blake3
    }

    /// Size of the archive file in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The package metadata.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Where the metadata came from.
    pub fn metadata_source(&self) -> &MetadataSource {
        &self.metadata
    }

    /// Every file in the archive, verified, in archive order.
    pub fn files(&self) -> &[FileRecord] {
        &self.files
    }

    /// Reopens the archive with `pkgar` for extraction, checking that it is
    /// still the archive that was verified.
    ///
    /// `pkgar` checks each entry's hash again as it extracts it, so a file
    /// swapped for another archive signed by the same key is caught here,
    /// and data changed afterwards is caught during extraction.
    ///
    /// # Errors
    ///
    /// [`ArchiveError::Pkgar`] if the file no longer opens with the signer's
    /// key, and [`ArchiveError::Changed`] if its header differs.
    pub fn reopen(&self) -> Result<pkgar::PackageFile, ArchiveError> {
        open_package_file(&self.path, &self.head)
    }
}

/// Reads and verifies the header and entry table.
fn read_head(path: &Path, keys: &TrustedKeys) -> Result<VerifiedHead, ArchiveError> {
    let io_error = |source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    };
    let verify_error = |source| ArchiveError::Verify {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(io_error)?;
    let mut header = [0; HEADER_SIZE];
    read_up_to(&mut file, &mut header).map_err(io_error)?;
    let size = head_size(&header, keys).map_err(verify_error)?;
    // Check the claimed entry table fits in the file before allocating it.
    let file_len = file.metadata().map_err(io_error)?.len();
    if u64::try_from(size).map_or(true, |size| size > file_len) {
        return Err(verify_error(VerifyError::Truncated {
            needed: size,
            actual: usize::try_from(file_len).unwrap_or(usize::MAX),
        }));
    }
    let mut head = header.to_vec();
    head.resize(size, 0);
    let read = HEADER_SIZE + read_up_to(&mut file, &mut head[HEADER_SIZE..]).map_err(io_error)?;
    head.truncate(read);
    verify_head(&head, keys).map_err(verify_error)
}

/// Reads until `buf` is full or the file ends, returning the bytes read.
fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Opens the archive with `pkgar` using the verified signer's key and
/// checks the header is the one verified.
fn open_package_file(path: &Path, head: &VerifiedHead) -> Result<pkgar::PackageFile, ArchiveError> {
    let package =
        pkgar::PackageFile::new(path, head.signer().key.as_bytes()).map_err(|source| {
            ArchiveError::Pkgar {
                path: path.to_path_buf(),
                source: Box::new(source),
            }
        })?;
    let (now, verified) = (package.header(), head.header());
    if now.signature != verified.signature || now.blake3 != verified.blake3 {
        return Err(ArchiveError::Changed {
            path: path.to_path_buf(),
        });
    }
    Ok(package)
}

/// A manifest found while hashing: its entry path and contents.
type ManifestText = Option<(String, Vec<u8>)>;

/// Hashes every entry's data and collects the manifest, if any.
fn hash_entries(
    path: &Path,
    head: &VerifiedHead,
    package: &mut pkgar::PackageFile,
) -> Result<(Vec<FileRecord>, ManifestText), ArchiveError> {
    let pkgar_error = |source| ArchiveError::Pkgar {
        path: path.to_path_buf(),
        source: Box::new(source),
    };
    let compressed = head.packaging() == Packaging::LZMA2;
    let mut files = Vec::with_capacity(head.raw_entries().len());
    let mut manifest: ManifestText = None;
    let mut buf = vec![0; 64 * 1024];
    for entry in head.entries() {
        let is_manifest = is_manifest_path(entry.path);
        if let (true, Some((first, _))) = (is_manifest, &manifest) {
            return Err(ArchiveError::MultipleManifests {
                first: first.clone(),
                second: entry.path.to_owned(),
            });
        }
        let mut reader = package.data_reader(entry.entry).map_err(pkgar_error)?;
        let mut hasher = blake3::Hasher::new();
        let mut size = 0u64;
        let mut captured = Vec::new();
        loop {
            let count = reader.read(&mut buf).map_err(|source| ArchiveError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            if count == 0 {
                break;
            }
            hasher.update(&buf[..count]);
            size += count as u64;
            if is_manifest {
                if captured.len() + count > MAX_MANIFEST_SIZE {
                    return Err(ArchiveError::ManifestTooLarge {
                        entry: entry.path.to_owned(),
                    });
                }
                captured.extend_from_slice(&buf[..count]);
            }
        }
        let expected_size = if compressed {
            reader.unpacked_size
        } else {
            reader.data_size
        };
        reader.finish(package).map_err(pkgar_error)?;
        let actual = Blake3Digest::from(hasher.finalize());
        if actual != entry.blake3 {
            return Err(ArchiveError::EntryHash {
                entry: entry.path.to_owned(),
                expected: entry.blake3,
                actual,
            });
        }
        if size != expected_size {
            return Err(ArchiveError::EntrySize {
                entry: entry.path.to_owned(),
                expected: expected_size,
                actual: size,
            });
        }
        if is_manifest {
            manifest = Some((entry.path.to_owned(), captured));
        }
        files.push(FileRecord {
            path: entry.path.to_owned(),
            kind: entry.kind,
            mode: entry.mode,
            size,
            blake3: actual,
        });
    }
    Ok((files, manifest))
}

/// Whether `path` is a manifest: a `.toml` file directly in
/// [`MANIFEST_DIR`].
fn is_manifest_path(path: &str) -> bool {
    path.strip_prefix(MANIFEST_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|file| !file.contains('/') && file.ends_with(".toml"))
}

/// Hashes the whole archive file.
fn hash_file(path: &Path) -> Result<(Blake3Digest, u64), ArchiveError> {
    let io_error = |source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(io_error)?;
    let mut hasher = blake3::Hasher::new();
    let size = io::copy(&mut file, &mut hasher).map_err(io_error)?;
    Ok((hasher.finalize().into(), size))
}

/// Parses an embedded manifest and checks it names itself.
fn parse_embedded(entry: &str, text: &[u8]) -> Result<Manifest, ArchiveError> {
    let text = std::str::from_utf8(text).map_err(|_| ArchiveError::ManifestNotUtf8 {
        entry: entry.to_owned(),
    })?;
    let manifest = Manifest::from_toml(text).map_err(|source| ArchiveError::Manifest {
        entry: entry.to_owned(),
        source,
    })?;
    let file_name = entry.rsplit('/').next().unwrap_or(entry);
    if file_name.strip_suffix(".toml") != Some(manifest.name.as_str()) {
        return Err(ArchiveError::ManifestNameMismatch {
            entry: entry.to_owned(),
            name: manifest.name,
        });
    }
    Ok(manifest)
}

/// Reads the cookbook metadata file and checks it belongs to this archive.
fn read_sidecar(
    archive: &Path,
    sidecar: &Path,
    head: &VerifiedHead,
) -> Result<Manifest, ArchiveError> {
    if !sidecar.is_file() {
        return Err(ArchiveError::NoMetadata {
            archive: archive.to_path_buf(),
            sidecar: sidecar.to_path_buf(),
        });
    }
    let package = pkg::Package::from_file(sidecar).map_err(|source| ArchiveError::Sidecar {
        path: sidecar.to_path_buf(),
        source: Box::new(source),
    })?;
    let expected = head.entries_blake3();
    if package.blake3.parse::<Blake3Digest>().ok() != Some(expected) {
        return Err(ArchiveError::SidecarMismatch {
            path: sidecar.to_path_buf(),
            expected,
            found: package.blake3,
        });
    }
    let name = sidecar_name(sidecar, package.name.as_str())?;
    // Recipes without a detectable version are written as "TODO" or left
    // empty by the cookbook; keep a non-empty placeholder.
    let version = if package.version.is_empty() {
        "unknown".to_owned()
    } else {
        package.version
    };
    let mut manifest = Manifest::new(name, version);
    if !package.target.is_empty() {
        manifest.target = Some(package.target);
    }
    manifest.depends = package
        .depends
        .iter()
        .map(|dep| sidecar_name(sidecar, &resolve_relative(&manifest.name, dep.as_str())))
        .collect::<Result<_, _>>()?;
    Ok(manifest)
}

/// Resolves a cookbook dependency written relative to its own recipe:
/// `.runtime` in `llvm21.dev` names `llvm21.runtime`, as the cookbook's
/// `PackageName::with_suffix` does. Other names are returned unchanged.
fn resolve_relative(package: &PackageName, dep: &str) -> String {
    match dep.strip_prefix('.') {
        Some(suffix) => {
            let base = package.as_str().split('.').next().unwrap_or_default();
            format!("{base}.{suffix}")
        }
        None => dep.to_owned(),
    }
}

fn sidecar_name(sidecar: &Path, name: &str) -> Result<PackageName, ArchiveError> {
    PackageName::new(name).map_err(|source| ArchiveError::SidecarName {
        path: sidecar.to_path_buf(),
        name: name.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_dependencies() {
        let dev = PackageName::new("llvm21.dev").unwrap();
        assert_eq!(resolve_relative(&dev, ".runtime"), "llvm21.runtime");
        assert_eq!(resolve_relative(&dev, "zlib"), "zlib");
        let plain = PackageName::new("llvm21").unwrap();
        assert_eq!(resolve_relative(&plain, ".runtime"), "llvm21.runtime");
    }

    #[test]
    fn manifest_paths() {
        assert!(is_manifest_path("usr/share/fox/manifest/hello.toml"));
        assert!(!is_manifest_path("usr/share/fox/manifest/sub/hello.toml"));
        assert!(!is_manifest_path("usr/share/fox/manifestx/hello.toml"));
        assert!(!is_manifest_path("usr/share/fox/manifest/hello.txt"));
    }
}
