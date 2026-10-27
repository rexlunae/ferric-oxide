//! Finding the inode a path names, so the scheme's changing calls (features.toml
//! `fs.ext4_write`) know which inode to change, and reading an inode's times ([`times`]),
//! so `fstat` can report the change time, which `ext4-view` does not expose, beside access
//! and modification times read from the same copy of the inode. It is in every build.
//!
//! `ext4-view` resolves paths but does not say which inode a path ends at. [`inode_of`]
//! walks the path itself, from the root directory (inode 2), reading each directory's
//! blocks through the same inode block map the journal code uses
//! (`journal::map::BlockMap::of_inode`) and scanning their entries for the next name.
//!
//! The scan reads every directory as a linear one. That covers hashed (`dir_index`)
//! directories too: their leaf blocks are ordinary linear blocks, and their index blocks
//! are laid out to read as entries that are not in use (the root block's `..` entry spans
//! the index, and each interior block starts with a free entry covering the whole block),
//! so that readers which know nothing of the index still work. The checksum tail
//! `metadata_csum` puts at the end of a block is also a free entry. Directory block
//! checksums are not checked: nothing here writes a directory block.
//!
//! The paths are the normalized ones [`crate::fs::Fs`] keeps for its handles, which
//! `ext4-view` has already resolved: absolute, with symbolic links expanded and no `.` or
//! `..`. A final symbolic link (opened with `O_SYMLINK`, or `O_STAT` and `O_NOFOLLOW`) names
//! the link's own inode.
//!
//! Directories kept inside the inode (`inline_data`) and encrypted directories are refused
//! with [`LookupError::Unsupported`]. Names are compared byte for byte, also in
//! case-insensitive (`casefold`) directories, where a name that differs only in case from
//! the stored one is not found.
//!
//! # Specification
//!
//! Written from "Ext4 Disk Layout" on the ext4 wiki at kernel.org: "Directory Entries"
//! ("Linear (Classic) Directories", the `ext4_dir_entry` and `ext4_dir_entry_2` layouts,
//! "Hash Tree Directories" on how index blocks read to a linear reader, and the checksum
//! tail), "Inode Table" (`i_mode`, `i_flags`) and "Special inodes" (the root directory is
//! inode 2). No code here comes from the Linux kernel or from e2fsprogs.
//!
//! One encoding is not in that page: with 64 KiB blocks, an entry that spans a whole block
//! has a length of 65536, which does not fit `rec_len`'s 16 bits. `mke2fs` (e2fsprogs 1.47)
//! stores it as `0xFFFF` (an empty `lost+found` block on a volume without
//! `metadata_csum`), and `e2fsck -fn` accepts both that and 0; both are read as
//! 65536 in 64 KiB blocks. This was found by looking at the bytes of volumes `mke2fs` made
//! and at what `e2fsck` accepts, not from either's source. The `dir-64k` fixture
//! (`tests/fixtures/make-fixtures.sh`) has such an entry naming a file.

use thiserror::Error;

use crate::device::{ReadAt, Window};
use crate::journal::map::{BlockMap, Volume};
use crate::journal::{JournalError, read_volume_superblock};

/// The root directory's inode number.
pub const ROOT_INODE: u32 = 2;
/// `s_feature_incompat`: directory entries record the file type, and `name_len` is one
/// byte.
pub(crate) const INCOMPAT_FILETYPE: u32 = 0x2;
/// Where an inode keeps `i_mode`.
const I_MODE: usize = 0x0;
/// Where an inode keeps `i_flags`.
const I_FLAGS: usize = 0x20;
/// The file type bits of `i_mode`, and their value for a directory.
const S_IFMT: u16 = 0xF000;
const S_IFDIR: u16 = 0x4000;
/// `i_flags`: the inode is encrypted.
const ENCRYPT_FL: u32 = 0x800;
/// `i_flags`: the inode keeps its contents in itself (`inline_data`).
const INLINE_DATA_FL: u32 = 0x1000_0000;
/// The fixed part of a directory entry: inode, `rec_len`, `name_len` (and file type).
pub(crate) const ENTRY_HEADER: usize = 8;

/// Why a path's inode could not be found.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LookupError {
    /// The volume, an inode or a block map could not be read, or is malformed.
    #[error("cannot read the volume")]
    Volume(#[from] JournalError),
    /// The path is not one [`crate::fs::Fs`] keeps: it is not absolute or has a `.` or `..`.
    #[error("the path is not absolute and normalized")]
    NotNormalized,
    /// A directory on the path has no entry with the next name.
    #[error("directory inode {directory} has no entry {name:?}")]
    NotFound {
        /// The directory's inode number.
        directory: u32,
        /// The name looked for.
        name: String,
    },
    /// An inode the path goes through is not a directory.
    #[error("inode {number} is not a directory")]
    NotADirectory {
        /// The inode number.
        number: u32,
    },
    /// A directory on the path is stored in a way this module does not read.
    #[error("inode {number}: {what} are not supported")]
    Unsupported {
        /// The directory's inode number.
        number: u32,
        /// What kind of directory it is.
        what: &'static str,
    },
    /// A directory block holds an entry whose lengths do not fit the block.
    #[error("directory inode {directory} has a malformed entry at byte {offset} of block {block}")]
    BadEntry {
        /// The directory's inode number.
        directory: u32,
        /// The directory's logical block.
        block: u32,
        /// Where the entry starts in the block.
        offset: usize,
    },
}

/// The number of the inode `path` names on the volume `window` holds.
///
/// `path` is absolute and normalized: `/` for the root, otherwise names separated by single
/// slashes, with no `.` or `..` (empty components, as in a trailing slash, are skipped).
///
/// # Errors
///
/// [`LookupError::NotNormalized`] for any other path, [`LookupError::NotFound`] and
/// [`LookupError::NotADirectory`] when the path does not name an inode,
/// [`LookupError::Unsupported`] for inline-data and encrypted directories,
/// [`LookupError::BadEntry`] for a malformed directory block, and [`LookupError::Volume`]
/// when the volume cannot be read or an inode or its block map is malformed.
///
/// # Examples
///
/// ```no_run
/// use ext4d::device::Window;
/// use ext4d::lookup;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let window = Window::whole(std::fs::File::open("disk.ext4")?)?;
/// assert_eq!(lookup::inode_of(&window, "/")?, lookup::ROOT_INODE);
/// let hello = lookup::inode_of(&window, "/hello.txt")?;
/// # let _ = hello;
/// # Ok(())
/// # }
/// ```
pub fn inode_of<R: ReadAt>(window: &Window<R>, path: &str) -> Result<u32, LookupError> {
    let rest = path.strip_prefix('/').ok_or(LookupError::NotNormalized)?;
    let names: Vec<&str> = rest.split('/').filter(|name| !name.is_empty()).collect();
    if names.iter().any(|&name| name == "." || name == "..") {
        return Err(LookupError::NotNormalized);
    }
    let sb = read_volume_superblock(window)?;
    let volume = Volume::parse(&sb)?;
    let filetype = volume.incompat & INCOMPAT_FILETYPE != 0;
    let mut current = ROOT_INODE;
    for name in names {
        current = find(&volume, window, current, name.as_bytes(), filetype)?.ok_or_else(|| {
            LookupError::NotFound {
                directory: current,
                name: name.to_owned(),
            }
        })?;
    }
    Ok(current)
}

/// An inode's timestamps, each as seconds since the epoch and nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodeTimes {
    /// `i_atime`, with `i_atime_extra` where the inode has it.
    pub accessed: (i64, u32),
    /// `i_mtime`, with `i_mtime_extra` where the inode has it.
    pub modified: (i64, u32),
    /// `i_ctime`, with `i_ctime_extra` where the inode has it.
    pub changed: (i64, u32),
}

/// The access, modification and change times of inode `number` on the volume `window`
/// holds, all from one read of the inode, so that they always come from the same version
/// of it.
///
/// # Errors
///
/// [`LookupError::Volume`] when the volume cannot be read or the inode number is out of
/// range.
pub fn times<R: ReadAt>(window: &Window<R>, number: u32) -> Result<InodeTimes, LookupError> {
    let volume = Volume::parse(&read_volume_superblock(window)?)?;
    let inode = volume.read_inode(window, number)?;
    Ok(crate::inode::times(&inode))
}

/// The inode the entry `name` of directory `directory` names, if it has one.
pub(crate) fn find<R: ReadAt>(
    volume: &Volume,
    window: &Window<R>,
    directory: u32,
    name: &[u8],
    filetype: bool,
) -> Result<Option<u32>, LookupError> {
    let inode = volume.read_inode(window, directory)?;
    if u16::from_le_bytes([inode[I_MODE], inode[I_MODE + 1]]) & S_IFMT != S_IFDIR {
        return Err(LookupError::NotADirectory { number: directory });
    }
    let flags = le32(&inode, I_FLAGS);
    let unsupported = if flags & INLINE_DATA_FL != 0 {
        Some("inline-data directories")
    } else if flags & ENCRYPT_FL != 0 {
        Some("encrypted directories")
    } else {
        None
    };
    if let Some(what) = unsupported {
        return Err(LookupError::Unsupported {
            number: directory,
            what,
        });
    }
    let map = BlockMap::of_inode(volume, window, &inode, directory)?;
    let block_size = u64::from(volume.block_size);
    let mut buf = vec![0; volume.block_size as usize];
    for logical in 0..map.len {
        // A hole holds no entries.
        let Some(physical) = map.physical(logical) else {
            continue;
        };
        if physical >= volume.blocks {
            return Err(JournalError::BlockOutOfRange { block: physical }.into());
        }
        window
            .read_exact_at(&mut buf, physical * block_size)
            .map_err(JournalError::from)?;
        let found =
            find_in_block(&buf, name, filetype).map_err(|offset| LookupError::BadEntry {
                directory,
                block: logical,
                offset,
            })?;
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

/// The inode of the entry named `name` in one linear directory block, or the offset of the
/// first entry whose lengths do not fit.
///
/// Each entry is `inode` (`u32`), `rec_len` (`u16`, the distance to the next entry) and the
/// name's length: a `u8` followed by the file type with `filetype`, a `u16` without. Entries
/// whose inode is 0 are not in use.
fn find_in_block(block: &[u8], name: &[u8], filetype: bool) -> Result<Option<u32>, usize> {
    let mut offset = 0;
    while offset < block.len() {
        let entry = block.get(offset..offset + ENTRY_HEADER).ok_or(offset)?;
        let inode = le32(entry, 0);
        let rec_len = rec_len(u16::from_le_bytes([entry[4], entry[5]]), block.len());
        let name_len = if filetype {
            usize::from(entry[6])
        } else {
            usize::from(u16::from_le_bytes([entry[6], entry[7]]))
        };
        let fits = rec_len >= ENTRY_HEADER
            && rec_len.is_multiple_of(4)
            && offset + rec_len <= block.len()
            && ENTRY_HEADER + name_len <= rec_len;
        if !fits {
            return Err(offset);
        }
        let stored = &block[offset + ENTRY_HEADER..offset + ENTRY_HEADER + name_len];
        if inode != 0 && stored == name {
            return Ok(Some(inode));
        }
        offset += rec_len;
    }
    Ok(None)
}

/// The block size whose whole-block entries need [`rec_len`]'s special case.
const BLOCK_64K: usize = 1 << 16;

/// The length a stored `rec_len` stands for in a directory block of `block_len` bytes: as
/// stored, except in 64 KiB blocks, where `0xFFFF` and 0 stand for 65536 (an entry over the
/// whole block), which 16 bits cannot hold. No other length in such a block can be 0xFFFF
/// (lengths are multiples of 4) or 0 (no entry is empty).
pub(crate) fn rec_len(stored: u16, block_len: usize) -> usize {
    if block_len == BLOCK_64K && matches!(stored, 0 | u16::MAX) {
        BLOCK_64K
    } else {
        usize::from(stored)
    }
}

/// The little-endian `u32` at `at`; every caller's buffer is long enough.
fn le32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `ext4_dir_entry_2`, padded to `rec_len`.
    fn entry(inode: u32, rec_len: u16, name: &[u8], file_type: u8) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&inode.to_le_bytes());
        raw.extend_from_slice(&rec_len.to_le_bytes());
        raw.push(name.len() as u8);
        raw.push(file_type);
        raw.extend_from_slice(name);
        raw.resize(usize::from(rec_len), 0);
        raw
    }

    fn block(entries: &[Vec<u8>]) -> Vec<u8> {
        entries.concat()
    }

    #[test]
    fn finds_a_name_in_a_linear_block() {
        let raw = block(&[
            entry(2, 12, b".", 2),
            entry(2, 12, b"..", 2),
            entry(12, 20, b"hello.txt", 1),
            entry(13, 1024 - 44, b"dir", 2),
        ]);
        assert_eq!(find_in_block(&raw, b"hello.txt", true), Ok(Some(12)));
        assert_eq!(find_in_block(&raw, b"dir", true), Ok(Some(13)));
        assert_eq!(find_in_block(&raw, b"hello", true), Ok(None));
        assert_eq!(find_in_block(&raw, b"missing", true), Ok(None));
    }

    #[test]
    fn unused_entries_and_the_checksum_tail_are_skipped() {
        // A deleted entry keeps its name with inode 0; the metadata_csum tail is an unused
        // 12-byte entry with file type 0xDE.
        let raw = block(&[
            entry(0, 20, b"gone.txt", 1),
            entry(14, 1024 - 32, b"kept", 1),
            entry(0, 12, b"", 0xDE),
        ]);
        assert_eq!(find_in_block(&raw, b"gone.txt", true), Ok(None));
        assert_eq!(find_in_block(&raw, b"kept", true), Ok(Some(14)));
    }

    #[test]
    fn hash_tree_index_blocks_read_as_unused() {
        // The root block: `.`, then `..` spanning the rest, with the index inside it.
        let mut root = block(&[entry(2, 12, b".", 2), entry(2, 1012, b"..", 2)]);
        root[24..32].copy_from_slice(b"f000\0\0\0\0");
        assert_eq!(find_in_block(&root, b"f000", true), Ok(None));
        // An interior node: one unused entry over the whole block.
        let mut node = entry(0, 1024, b"", 0);
        node[8..16].copy_from_slice(b"f001f002");
        assert_eq!(find_in_block(&node, b"f001", true), Ok(None));
    }

    #[test]
    fn without_filetype_the_name_length_is_16_bits() {
        let mut raw = entry(12, 1024, b"hello.txt", 0);
        raw[6..8].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(find_in_block(&raw, b"hello.txt", false), Ok(Some(12)));
        // Read with filetype, the same bytes give a 9-byte name with type 0: still found.
        assert_eq!(find_in_block(&raw, b"hello.txt", true), Ok(Some(12)));
        // 0x0409 bytes of name do not fit the entry.
        raw[7] = 4;
        assert_eq!(find_in_block(&raw, b"hello.txt", false), Err(0));
    }

    #[test]
    fn malformed_entries_are_refused() {
        // rec_len too small, not a multiple of 4, past the block, shorter than the name.
        for (rec_len, name_len) in [(4u16, 0u8), (14, 1), (2048, 1), (12, 5)] {
            let mut raw = block(&[entry(2, 12, b".", 2), entry(12, 1012, b"x", 1)]);
            raw[16..18].copy_from_slice(&rec_len.to_le_bytes());
            raw[18] = name_len;
            assert_eq!(
                find_in_block(&raw, b"zzz", true),
                Err(12),
                "{rec_len} {name_len}"
            );
        }
        // A block that ends inside an entry's header.
        let raw = block(&[entry(2, 12, b".", 2)]);
        let mut short = raw.clone();
        short.extend_from_slice(&[0; 4]);
        assert_eq!(find_in_block(&short, b"zzz", true), Err(12));
        // A zero rec_len would loop forever if it were accepted.
        assert_eq!(find_in_block(&[0; 16], b"zzz", true), Err(0));
    }

    #[test]
    fn a_64k_block_spanned_by_one_entry_stores_its_length_as_0xffff_or_0() {
        for stored in [u16::MAX, 0] {
            let mut raw = vec![0; BLOCK_64K];
            raw[..4].copy_from_slice(&13u32.to_le_bytes());
            raw[4..6].copy_from_slice(&stored.to_le_bytes());
            raw[6] = 1;
            raw[7] = 1;
            raw[8] = b'b';
            assert_eq!(find_in_block(&raw, b"b", true), Ok(Some(13)), "{stored:#x}");
            assert_eq!(find_in_block(&raw, b"c", true), Ok(None), "{stored:#x}");
            // Only a 64 KiB block has the special case: elsewhere they are malformed.
            let mut small = raw[..1024].to_vec();
            small[4..6].copy_from_slice(&stored.to_le_bytes());
            assert_eq!(find_in_block(&small, b"b", true), Err(0), "{stored:#x}");
        }
        // After another entry, 65536 bytes do not fit what is left of the block.
        let mut raw = vec![0; BLOCK_64K];
        raw[..12].copy_from_slice(&entry(2, 12, b".", 2));
        raw[12 + 4..12 + 6].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(find_in_block(&raw, b"zzz", true), Err(12));
        assert_eq!(rec_len(0xFFF4, BLOCK_64K), 0xFFF4);
    }
}
