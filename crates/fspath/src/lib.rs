//! Paths for the file system daemons that serve a volume as a Redox scheme.
//!
//! A scheme handle names a file by its path from the volume's root, so that relative opens
//! and `fpath` work without the daemon keeping directory handles open. Two helpers make that
//! work the same way in `fatd` and `ext4d`, which both use this crate:
//!
//! - [`normalize`] resolves a path against a directory as text: `.` and empty components
//!   vanish, `..` goes up one level and stops at the root.
//! - [`path_ino`] turns a normalized path into a stable inode number, for formats or readers
//!   that have none to give (FAT has no inodes at all).
//!
//! The crate is `no_std` with `alloc`, so it carries no operating-system dependency.
//!
//! # Examples
//!
//! ```
//! use ferric_fspath::{normalize, path_ino};
//!
//! let path = normalize("/EFI/BOOT", "../redox/./kernel");
//! assert_eq!(path, "/EFI/redox/kernel");
//! assert_eq!(path_ino("/"), 2);
//! assert_eq!(path_ino(&path), path_ino("/EFI/redox/kernel"));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The inode number [`path_ino`] gives the root directory: 2, as ext4 and most Unix file
/// systems number theirs.
pub const ROOT_INO: u64 = 2;

/// `path` resolved against the directory `start` (both from the root), with `.` and `..`
/// resolved as text and no empty components. The result starts with `/`; `..` at the root
/// stays at the root. An absolute `path` ignores `start`.
pub fn normalize(start: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let full = if path.starts_with('/') {
        [path, ""]
    } else {
        [start, path]
    };
    for component in full.iter().flat_map(|p| p.split('/')) {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

/// A stable number for the file at the normalized `path`: [`ROOT_INO`] for the root, and a
/// 64-bit FNV-1a hash of the path with the top bit set, clear of the small numbers file
/// systems reserve, for the rest. Different paths can collide only as 63-bit hashes do, and
/// two paths to one file (hard links) get different numbers.
pub fn path_ino(path: &str) -> u64 {
    if path == "/" {
        return ROOT_INO;
    }
    let hash = path.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    hash | 1 << 63
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_as_text() {
        assert_eq!(normalize("/", ""), "/");
        assert_eq!(normalize("/", "a/b"), "/a/b");
        assert_eq!(normalize("/a", "b/../c"), "/a/c");
        assert_eq!(normalize("/a/b", "/x/./y"), "/x/y");
        assert_eq!(normalize("/", "../../.."), "/");
        assert_eq!(normalize("/a", "..//b/"), "/b");
    }

    #[test]
    fn inode_numbers_are_stable_and_clear_of_reserved_ones() {
        assert_eq!(path_ino("/"), ROOT_INO);
        assert_eq!(path_ino("/a"), path_ino("/a"));
        assert_ne!(path_ino("/a"), path_ino("/b"));
        assert!(path_ino("/a") > 1 << 32);
    }

    #[test]
    fn the_hash_is_fnv1a() {
        // FNV-1a of "a" is 0xaf63dc4c8601ec8c; the top bit is already set.
        assert_eq!(path_ino("a"), 0xaf63_dc4c_8601_ec8c);
    }
}
