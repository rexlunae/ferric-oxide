//! Signer trust and verification of pkgar heads.
//!
//! A pkgar *head* is the 136-byte header followed by the entry table (308
//! bytes per entry), as defined by the upstream `pkgar-core` crate:
//!
//! - the header holds an Ed25519 signature over the rest of the header, the
//!   signer's public key, the BLAKE3 hash of the entry table, the entry count
//!   and format flags;
//! - each entry holds a file's BLAKE3 hash, its offset and size in the data
//!   section, its mode and its NUL-padded relative path.
//!
//! A pkgar header carries the signer's public key, so a signature only means
//! something when that key is one the caller already trusts. [`verify_head`]
//! looks the key up in [`TrustedKeys`] first and only then checks the
//! signature with it. It also rejects entry tables that would be unsafe to
//! extract: absolute or `..` paths, duplicate paths, and paths nested under
//! another entry (which a symlink entry could otherwise redirect).
//!
//! Checking the file *data* against the entry hashes needs the data and, for
//! compressed archives, an LZMA decoder; that is done by `archive` with the
//! `std` feature, or by the store when it re-hashes installed files.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use pkgar_core::{DataVersion, ENTRY_SIZE, Entry, HEADER_SIZE, Header, Mode, Packaging};

use crate::{Blake3Digest, FileKind, KeyId};

/// Byte range of the signer's public key inside a pkgar header.
const HEADER_KEY: core::ops::Range<usize> = 64..96;

/// A signer key and the name it was trusted under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    /// Where the key came from, such as a key file's stem (`ferric-ci-1`).
    pub label: String,
    /// The Ed25519 public key.
    pub key: KeyId,
}

/// The set of signer keys whose packages are accepted.
///
/// # Examples
///
/// ```
/// use fox_core::{KeyId, TrustedKeys};
///
/// let mut keys = TrustedKeys::new();
/// keys.insert("ferric-ci-1", KeyId::from_bytes([1; 32]));
/// assert_eq!(keys.get(&KeyId::from_bytes([1; 32])).unwrap().label, "ferric-ci-1");
/// assert!(keys.get(&KeyId::from_bytes([2; 32])).is_none());
/// ```
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
    keys: BTreeMap<KeyId, String>,
}

impl TrustedKeys {
    /// An empty set, which trusts nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trusts `key` under `label`. A key added twice keeps its first label.
    pub fn insert(&mut self, label: impl Into<String>, key: KeyId) {
        self.keys.entry(key).or_insert_with(|| label.into());
    }

    /// Looks up a key.
    pub fn get(&self, key: &KeyId) -> Option<TrustedKey> {
        self.keys.get(key).map(|label| TrustedKey {
            label: label.clone(),
            key: *key,
        })
    }

    /// Whether no key is trusted.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Number of trusted keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Iterates over the trusted keys in key order.
    pub fn iter(&self) -> impl Iterator<Item = TrustedKey> + '_ {
        self.keys.iter().map(|(key, label)| TrustedKey {
            label: label.clone(),
            key: *key,
        })
    }
}

/// Why an entry path is unsafe or invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EntryPathError {
    /// The path is empty.
    #[error("empty path")]
    Empty,
    /// The path is not UTF-8.
    #[error("path is not UTF-8")]
    NotUtf8,
    /// The path starts with `/`.
    #[error("path is absolute")]
    Absolute,
    /// The path has an empty, `.` or `..` component.
    #[error("path has an empty, '.' or '..' component")]
    BadComponent,
    /// The path holds a control character (NUL, newline, escape, DEL and
    /// the C1 range included) or a backslash.
    #[error("path contains the character {0:?}")]
    BadCharacter(char),
}

/// Why a pkgar head failed verification.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// No signer key is trusted, so nothing can verify.
    #[error("no trusted signing keys are configured")]
    NoTrustedKeys,
    /// The data ends before the header or the entry table does.
    #[error("pkgar head is truncated: need {needed} bytes, have {actual}")]
    Truncated {
        /// Bytes needed.
        needed: usize,
        /// Bytes available.
        actual: usize,
    },
    /// The header was signed by a key that is not trusted.
    #[error("package is signed by untrusted key {key}")]
    UntrustedSigner {
        /// The key named in the header.
        key: KeyId,
    },
    /// The header signature does not verify with the key it names.
    #[error("bad header signature")]
    BadSignature(#[source] pkgar_core::Error),
    /// The header uses a format version this code does not know.
    #[error("unsupported pkgar data version {0}")]
    UnsupportedVersion(u8),
    /// The header uses a compression this code does not know.
    #[error("unsupported pkgar packaging {0}")]
    UnsupportedPackaging(u8),
    /// The entry count overflows the address space.
    #[error("entry table size overflows")]
    TooManyEntries(#[source] pkgar_core::Error),
    /// The entry table does not match the hash in the signed header.
    #[error("entry table does not match the signed header hash")]
    EntriesHash(#[source] pkgar_core::Error),
    /// An entry path is unsafe or invalid.
    #[error("entry {index} has a bad path")]
    EntryPath {
        /// Index of the entry in the table.
        index: usize,
        /// What is wrong with the path.
        #[source]
        error: EntryPathError,
    },
    /// An entry is neither a regular file nor a symlink.
    #[error("entry {path} has unsupported mode {mode:o}")]
    UnsupportedMode {
        /// Entry path.
        path: String,
        /// The raw mode.
        mode: u32,
    },
    /// Two entries have the same path.
    #[error("path {path} appears twice")]
    DuplicatePath {
        /// The repeated path.
        path: String,
    },
    /// An entry lies under another entry, which would make that entry a
    /// directory (or let a symlink entry redirect it).
    #[error("entry {path} lies under entry {parent}")]
    NestedPath {
        /// The nested entry.
        path: String,
        /// The entry it lies under.
        parent: String,
    },
}

/// An entry whose path and mode have been checked.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedEntry<'a> {
    /// The raw entry, for reading its data with `pkgar`.
    pub entry: &'a Entry,
    /// The entry path.
    pub path: &'a str,
    /// File or symlink.
    pub kind: FileKind,
    /// Permission bits.
    pub mode: u32,
    /// Expected BLAKE3 of the data.
    pub blake3: Blake3Digest,
}

/// A pkgar head whose signature, entry table hash and entry paths have
/// been checked against a set of trusted keys.
#[derive(Debug, Clone)]
pub struct VerifiedHead {
    header: Header,
    signer: TrustedKey,
    entries: Vec<Entry>,
}

impl VerifiedHead {
    /// The verified header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The trusted key that signed the header.
    pub fn signer(&self) -> &TrustedKey {
        &self.signer
    }

    /// BLAKE3 of the entry table, as signed in the header. This is the
    /// `blake3` field of a cookbook package's metadata file.
    pub fn entries_blake3(&self) -> Blake3Digest {
        Blake3Digest::from_bytes(self.header.blake3)
    }

    /// How the entry data is stored.
    pub fn packaging(&self) -> Packaging {
        self.header.flags.packaging()
    }

    /// Size of the header plus entry table, which is where the data starts.
    pub fn size(&self) -> usize {
        HEADER_SIZE + self.entries.len() * ENTRY_SIZE
    }

    /// The raw entries, in archive order.
    pub fn raw_entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entries with their checked paths and kinds, in archive order.
    pub fn entries(&self) -> impl Iterator<Item = VerifiedEntry<'_>> {
        self.entries.iter().map(|entry| {
            let (path, kind) = entry_path_and_kind(entry)
                .expect("verify_head checked every entry's path and mode");
            VerifiedEntry {
                entry,
                path,
                kind,
                mode: entry.mode & Mode::PERM.bits(),
                blake3: Blake3Digest::from_bytes(entry.blake3),
            }
        })
    }
}

/// Verifies a pkgar head (header followed by the entry table) against the
/// trusted keys.
///
/// `head` may continue past the entry table (a whole archive in memory, for
/// example); the extra bytes are ignored.
///
/// # Errors
///
/// - [`VerifyError::NoTrustedKeys`] if `keys` is empty;
/// - [`VerifyError::Truncated`] if `head` is shorter than the header and
///   entry table it describes;
/// - [`VerifyError::UntrustedSigner`] if the header names a key not in `keys`;
/// - [`VerifyError::BadSignature`] if the signature does not verify;
/// - [`VerifyError::UnsupportedVersion`] or
///   [`VerifyError::UnsupportedPackaging`] for unknown format flags;
/// - [`VerifyError::TooManyEntries`] if the entry count overflows;
/// - [`VerifyError::EntriesHash`] if the entry table does not match the
///   signed hash;
/// - [`VerifyError::EntryPath`], [`VerifyError::UnsupportedMode`],
///   [`VerifyError::DuplicatePath`] or [`VerifyError::NestedPath`] for an
///   entry that is unsafe to extract.
pub fn verify_head(head: &[u8], keys: &TrustedKeys) -> Result<VerifiedHead, VerifyError> {
    let (header, signer) = verify_header(head, keys)?;
    let total = header.total_size().map_err(VerifyError::TooManyEntries)?;
    let table = head.get(HEADER_SIZE..total).ok_or(VerifyError::Truncated {
        needed: total,
        actual: head.len(),
    })?;
    let entries = header
        .entries(table)
        .map_err(VerifyError::EntriesHash)?
        .to_vec();
    check_entries(&entries)?;

    Ok(VerifiedHead {
        header,
        signer,
        entries,
    })
}

/// The length of the head (header plus entry table) that starts with
/// `header`, after checking the header's signer and signature.
///
/// Readers use this to know how many bytes to pass to [`verify_head`]
/// after reading only the fixed-size header.
///
/// # Errors
///
/// The header errors of [`verify_head`]: [`VerifyError::NoTrustedKeys`],
/// [`VerifyError::Truncated`], [`VerifyError::UntrustedSigner`],
/// [`VerifyError::BadSignature`], [`VerifyError::UnsupportedVersion`],
/// [`VerifyError::UnsupportedPackaging`] and [`VerifyError::TooManyEntries`].
pub fn head_size(header: &[u8], keys: &TrustedKeys) -> Result<usize, VerifyError> {
    let (header, _) = verify_header(header, keys)?;
    header.total_size().map_err(VerifyError::TooManyEntries)
}

/// Checks the fixed-size header: trusted signer, signature, format flags.
fn verify_header(head: &[u8], keys: &TrustedKeys) -> Result<(Header, TrustedKey), VerifyError> {
    if keys.is_empty() {
        return Err(VerifyError::NoTrustedKeys);
    }
    let header_bytes = head.get(..HEADER_SIZE).ok_or(VerifyError::Truncated {
        needed: HEADER_SIZE,
        actual: head.len(),
    })?;
    let mut key_bytes = [0; 32];
    key_bytes.copy_from_slice(&header_bytes[HEADER_KEY]);
    let key = KeyId::from_bytes(key_bytes);
    let signer = keys.get(&key).ok_or(VerifyError::UntrustedSigner { key })?;
    let header = *Header::new(header_bytes, &key_bytes).map_err(VerifyError::BadSignature)?;

    if let DataVersion::Reserved(version) = header.flags.version() {
        return Err(VerifyError::UnsupportedVersion(version));
    }
    if let Packaging::Reserved(packaging) = header.flags.packaging() {
        return Err(VerifyError::UnsupportedPackaging(packaging));
    }
    Ok((header, signer))
}

/// Checks every entry's path and mode, and that no two entries collide.
fn check_entries(entries: &[Entry]) -> Result<(), VerifyError> {
    let mut paths = BTreeSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let (path, _) = entry_path_and_kind(entry).map_err(|bad| match bad {
            BadEntry::Path(error) => VerifyError::EntryPath { index, error },
            BadEntry::Mode(path) => VerifyError::UnsupportedMode {
                path: path.to_string(),
                mode: entry.mode,
            },
        })?;
        if !paths.insert(path) {
            return Err(VerifyError::DuplicatePath {
                path: path.to_string(),
            });
        }
    }
    for path in &paths {
        // Every proper prefix ending at a '/' is a parent directory.
        if let Some(parent) = path
            .match_indices('/')
            .map(|(at, _)| &path[..at])
            .find(|parent| paths.contains(parent))
        {
            return Err(VerifyError::NestedPath {
                path: path.to_string(),
                parent: parent.to_string(),
            });
        }
    }
    Ok(())
}

/// What makes an entry unusable, before it is tied to an index.
#[derive(Debug)]
enum BadEntry<'a> {
    Path(EntryPathError),
    Mode(&'a str),
}

/// The entry's path and kind, if both are valid.
fn entry_path_and_kind(entry: &Entry) -> Result<(&str, FileKind), BadEntry<'_>> {
    let path = check_path(entry.path_bytes()).map_err(BadEntry::Path)?;
    let kind = match Mode::from_bits_retain(entry.mode).kind() {
        Mode::FILE => FileKind::File,
        Mode::SYMLINK => FileKind::Symlink,
        _ => return Err(BadEntry::Mode(path)),
    };
    Ok((path, kind))
}

/// Checks that `bytes` is a relative path made only of normal components.
fn check_path(bytes: &[u8]) -> Result<&str, EntryPathError> {
    let path = core::str::from_utf8(bytes).map_err(|_| EntryPathError::NotUtf8)?;
    if path.is_empty() {
        return Err(EntryPathError::Empty);
    }
    if path.starts_with('/') {
        return Err(EntryPathError::Absolute);
    }
    // Control characters make paths that print or compare differently from
    // what is on disk, and a backslash is a separator to some tools.
    if let Some(bad) = path.chars().find(|c| c.is_control() || *c == '\\') {
        return Err(EntryPathError::BadCharacter(bad));
    }
    if path
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(EntryPathError::BadComponent);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rules() {
        assert_eq!(check_path(b"usr/bin/hello"), Ok("usr/bin/hello"));
        assert_eq!(check_path(b""), Err(EntryPathError::Empty));
        assert_eq!(check_path(b"/etc/passwd"), Err(EntryPathError::Absolute));
        assert_eq!(check_path(b"usr/../etc"), Err(EntryPathError::BadComponent));
        assert_eq!(check_path(b"usr//bin"), Err(EntryPathError::BadComponent));
        assert_eq!(check_path(b"./bin"), Err(EntryPathError::BadComponent));
        assert_eq!(check_path(b"bin/"), Err(EntryPathError::BadComponent));
        assert_eq!(check_path(&[0xff]), Err(EntryPathError::NotUtf8));
        for bad in ['\0', '\n', '\r', '\t', '\x1b', '\x7f', '\u{85}', '\\'] {
            let path = alloc::format!("usr/bin/a{bad}b");
            assert_eq!(
                check_path(path.as_bytes()),
                Err(EntryPathError::BadCharacter(bad)),
                "{path:?}"
            );
        }
        assert_eq!(
            check_path("usr/share/café".as_bytes()),
            Ok("usr/share/café")
        );
    }

    fn entry(path: &str, mode: u32) -> Entry {
        Entry::new_uninit(path.as_bytes(), Mode::from_bits_retain(mode)).unwrap()
    }

    #[test]
    fn rejects_duplicate_and_nested_entries() {
        let dup = [entry("a/b", 0o100644), entry("a/b", 0o100644)];
        assert!(matches!(
            check_entries(&dup),
            Err(VerifyError::DuplicatePath { .. })
        ));
        let nested = [entry("usr/lib", 0o120777), entry("usr/lib/x", 0o100644)];
        assert!(matches!(
            check_entries(&nested),
            Err(VerifyError::NestedPath { ref parent, .. }) if parent == "usr/lib"
        ));
        let siblings = [entry("usr/lib", 0o100644), entry("usr/libx", 0o100644)];
        assert!(check_entries(&siblings).is_ok());
    }

    #[test]
    fn rejects_directories_and_devices() {
        let dir = [entry("usr", 0o040755)];
        assert!(matches!(
            check_entries(&dir),
            Err(VerifyError::UnsupportedMode { .. })
        ));
    }

    #[test]
    fn short_and_untrusted_heads() {
        let mut keys = TrustedKeys::new();
        assert!(matches!(
            verify_head(&[], &keys),
            Err(VerifyError::NoTrustedKeys)
        ));
        keys.insert("k", KeyId::from_bytes([1; 32]));
        assert!(matches!(
            verify_head(&[0; 10], &keys),
            Err(VerifyError::Truncated {
                needed: HEADER_SIZE,
                actual: 10
            })
        ));
        assert!(matches!(
            verify_head(&[0; HEADER_SIZE], &keys),
            Err(VerifyError::UntrustedSigner { .. })
        ));
    }
}
