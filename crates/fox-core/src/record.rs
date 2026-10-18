//! Verified file records.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::Blake3Digest;

/// What kind of file an archive entry is. pkgar stores only these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FileKind {
    /// A regular file; its hash covers its contents.
    File,
    /// A symbolic link; its hash covers the link target bytes.
    Symlink,
}

/// One file of a package whose hash has been checked against the signed
/// entry table.
///
/// The installed-package database stores these, and SBOMs list them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Path relative to the install root, with `/` separators and no `.` or
    /// `..` components.
    pub path: String,
    /// File or symlink.
    pub kind: FileKind,
    /// Permission bits (`0o7777` mask).
    pub mode: u32,
    /// Size in bytes after decompression (for a symlink, of its target).
    pub size: u64,
    /// BLAKE3 of the contents (for a symlink, of its target).
    pub blake3: Blake3Digest,
}
