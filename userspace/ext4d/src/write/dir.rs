//! Adding an entry to a linear directory, through a [`Txn`].
//!
//! A linear directory block is a chain of entries (`ext4_dir_entry_2`: inode, `rec_len`,
//! name length, file type, name), each `rec_len` bytes long, covering the whole block; the
//! last one usually has room left over after its name. A new entry takes the room after an
//! entry's name, or an unused entry (inode 0) long enough for it. With `metadata_csum` each
//! block ends with a 12-byte tail that looks like an unused entry (`rec_len` 12, file type
//! `0xDE`) and holds a CRC32C from the checksum seed over the inode number, `i_generation`
//! and the block before the tail. When no block has room, a block is allocated and added to
//! the directory's extent tree, holding just the new entry (and the tail).
//!
//! Hashed (`dir_index`) directories, whose blocks are found through an index the entry would
//! have to be added to, are refused with [`WriteError::UnsupportedDirectory`], and so are
//! inline-data, encrypted and case-insensitive ones, and directories without extents.
//!
//! # Specification
//!
//! "Ext4 Disk Layout" on the ext4 wiki: "Directory Entries" ("Linear (Classic)
//! Directories", the checksum tail, "Hash Tree Directories" for the flag that marks them) and
//! "Checksums". No code here comes from the Linux kernel or from e2fsprogs.

use super::extents::{self, Extent};
use super::ondisk::{DirEntry, DirTail, Record};
use super::txn::Txn;
use super::{EXTENTS_FL, I_GENERATION, WriteError, add_blocks, flags_of, size_of, store_size};
use crate::device::ReadAt;
use crate::lookup::{ENTRY_HEADER, rec_len};

/// `i_flags` that make a directory one this module does not add to: hashed, encrypted,
/// inline-data and case-insensitive.
const INDEX_FL: u32 = 0x1000;
const ENCRYPT_FL: u32 = 0x800;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const CASEFOLD_FL: u32 = 0x4000_0000;
/// The checksum tail's length and its file type byte.
const TAIL: usize = 12;
const TAIL_TYPE: u8 = 0xDE;
/// 64 KiB, the length a whole-block entry has in the largest blocks; stored as `0xFFFF`.
const BLOCK_64K: usize = 1 << 16;

/// An entry to add: its name, inode and file type (`ext4_dir_entry_2.file_type`, 0 without
/// the `filetype` feature).
pub(super) struct NewEntry<'n> {
    pub(super) name: &'n [u8],
    pub(super) inode: u32,
    pub(super) file_type: u8,
    /// Whether the name length is one byte, followed by the file type (`filetype`).
    pub(super) filetype: bool,
}

/// Adds `entry` to directory `number` (`dir`, changed in place: its size and block count
/// when a block is added).
///
/// # Errors
///
/// [`WriteError::UnsupportedDirectory`] for a directory kept in a way this module does not
/// add to, [`WriteError::BadDirectory`] for a malformed block or one without its checksum
/// tail, [`WriteError::BlockChecksum`], and, when a block has to be added, those of
/// [`Txn::allocate_block`] and [`extents::map`].
pub(super) fn add_entry<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    number: u32,
    dir: &mut [u8],
    entry: &NewEntry<'_>,
) -> Result<(), WriteError> {
    let flags = flags_of(dir)?;
    let unsupported = flags & (INDEX_FL | ENCRYPT_FL | INLINE_DATA_FL | CASEFOLD_FL);
    if unsupported != 0 || flags & EXTENTS_FL == 0 {
        return Err(WriteError::UnsupportedDirectory {
            number,
            flags: unsupported | (!flags & EXTENTS_FL),
        });
    }
    let block_size = txn.block_size() as usize;
    let seed = txn.checksum_seed();
    let area = if seed.is_some() {
        block_size - TAIL
    } else {
        block_size
    };
    let extents = extents::leaves(txn, number, dir)?;
    let blocks = size_of(dir)?.div_ceil(block_size as u64);
    for logical in 0..blocks {
        let Some(physical) = physical_of(&extents, logical) else {
            continue;
        };
        let mut block = txn.block(physical)?.to_vec();
        if let Some(seed) = seed {
            check_tail(&block, seed, number, dir, (logical, physical))?;
        }
        let added =
            insert(&mut block[..area], entry).map_err(|offset| WriteError::BadDirectory {
                number,
                block: logical,
                offset,
            })?;
        if added {
            if let Some(seed) = seed {
                put_tail(&mut block, seed, number, dir);
            }
            txn.block_mut(physical)?.copy_from_slice(&block);
            return Ok(());
        }
    }
    add_block(txn, number, dir, entry, &extents, blocks, area)
}

/// Gives the directory a new last block holding just `entry`.
fn add_block<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    number: u32,
    dir: &mut [u8],
    entry: &NewEntry<'_>,
    extents: &[Extent],
    blocks: u64,
    area: usize,
) -> Result<(), WriteError> {
    let logical = u32::try_from(blocks).map_err(|_| WriteError::FileTooLarge {
        number,
        size: blocks.saturating_mul(txn.block_size()),
    })?;
    let goal = extents
        .last()
        .map_or(0, |last| last.physical + u64::from(last.len));
    let physical = txn.allocate_block(goal)?;
    extents::map(txn, number, dir, logical, physical)?;
    let block_size = txn.block_size() as usize;
    let mut block = vec![0; block_size];
    put_entry(&mut block, 0, entry, area);
    if let Some(seed) = txn.checksum_seed() {
        put_tail(&mut block, seed, number, dir);
    }
    txn.put_block(physical, block);
    store_size(dir, (blocks + 1) * txn.block_size())?;
    add_blocks(dir, txn.block_size(), 1);
    Ok(())
}

/// The volume block holding initialized block `logical` of the directory.
fn physical_of(extents: &[Extent], logical: u64) -> Option<u64> {
    extents.iter().find_map(|extent| {
        let start = u64::from(extent.logical);
        let inside = !extent.unwritten && (start..start + u64::from(extent.len)).contains(&logical);
        inside.then(|| extent.physical + (logical - start))
    })
}

/// Puts `entry` into the entries of `area` (a block without its tail) if one has room for
/// it; whether it did, or the offset of a malformed entry.
fn insert(area: &mut [u8], entry: &NewEntry<'_>) -> Result<bool, usize> {
    let need = entry_len(entry.name.len());
    let mut offset = 0;
    while offset < area.len() {
        let header = DirEntry::read(area.get(offset..offset + ENTRY_HEADER).ok_or(offset)?);
        let inode = header.inode;
        let len = rec_len(header.rec_len, area.len());
        let name_len = name_len(header, entry.filetype);
        let fits = len >= ENTRY_HEADER
            && len.is_multiple_of(4)
            && offset + len <= area.len()
            && ENTRY_HEADER + name_len <= len;
        if !fits {
            return Err(offset);
        }
        let used = if inode == 0 { 0 } else { entry_len(name_len) };
        if len - used >= need {
            if used == 0 {
                put_entry(area, offset, entry, len);
            } else {
                put_rec_len(area, offset, used);
                put_entry(area, offset + used, entry, len - used);
            }
            return Ok(true);
        }
        offset += len;
    }
    Ok(false)
}

/// The name length `header` stores: one byte with `filetype`, and otherwise two, the file
/// type byte being the high one.
fn name_len(header: DirEntry, filetype: bool) -> usize {
    if filetype {
        usize::from(header.name_len)
    } else {
        usize::from(header.name_len) | usize::from(header.file_type) << 8
    }
}

/// The room an entry with a name of `name_len` bytes needs: the header and the name,
/// rounded up to 4 bytes.
fn entry_len(name_len: usize) -> usize {
    (ENTRY_HEADER + name_len).next_multiple_of(4)
}

/// Writes `entry` at `offset`, `len` bytes long.
fn put_entry(block: &mut [u8], offset: usize, entry: &NewEntry<'_>, len: usize) {
    let raw = &mut block[offset..offset + len];
    raw.fill(0);
    // Names are at most 255 bytes (checked by the caller), so the high byte is 0 without
    // `filetype`.
    let name_len = entry.name.len();
    DirEntry {
        inode: entry.inode,
        rec_len: stored_rec_len(len),
        name_len: name_len as u8,
        file_type: if entry.filetype { entry.file_type } else { 0 },
    }
    .write(raw);
    raw[ENTRY_HEADER..ENTRY_HEADER + name_len].copy_from_slice(entry.name);
}

fn put_rec_len(block: &mut [u8], offset: usize, len: usize) {
    let header = DirEntry::read(&block[offset..]);
    DirEntry {
        rec_len: stored_rec_len(len),
        ..header
    }
    .write(&mut block[offset..]);
}

/// `rec_len` as stored: 65536 does not fit 16 bits, so a whole 64 KiB block is stored as
/// `0xFFFF` (`crate::lookup`).
fn stored_rec_len(len: usize) -> u16 {
    if len == BLOCK_64K {
        u16::MAX
    } else {
        len as u16
    }
}

/// Checks that `block` ends with a checksum tail that holds its checksum.
fn check_tail(
    block: &[u8],
    seed: u32,
    number: u32,
    dir: &[u8],
    (logical, physical): (u64, u64),
) -> Result<(), WriteError> {
    let at = block.len() - TAIL;
    let tail = DirTail::read(&block[at..]);
    let is_tail = tail.zero_inode == 0
        && usize::from(tail.rec_len) == TAIL
        && tail.zero_name_len == 0
        && tail.file_type == TAIL_TYPE;
    if !is_tail {
        return Err(WriteError::BadDirectory {
            number,
            block: logical,
            offset: at,
        });
    }
    let stored = tail.checksum;
    let computed = block_checksum(seed, number, dir, &block[..at]);
    if stored != computed {
        return Err(WriteError::BlockChecksum {
            block: physical,
            stored,
            computed,
        });
    }
    Ok(())
}

/// Writes the checksum tail at the end of `block`.
fn put_tail(block: &mut [u8], seed: u32, number: u32, dir: &[u8]) {
    let at = block.len() - TAIL;
    let checksum = block_checksum(seed, number, dir, &block[..at]);
    DirTail {
        zero_inode: 0,
        rec_len: TAIL as u16,
        zero_name_len: 0,
        file_type: TAIL_TYPE,
        checksum,
    }
    .write(&mut block[at..]);
}

fn block_checksum(seed: u32, number: u32, dir: &[u8], entries: &[u8]) -> u32 {
    ferric_jbd2::crc32c_seeded(
        seed,
        &[
            &number.to_le_bytes(),
            &dir[I_GENERATION..I_GENERATION + 4],
            entries,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(name: &[u8], inode: u32) -> NewEntry<'_> {
        NewEntry {
            name,
            inode,
            file_type: 1,
            filetype: true,
        }
    }

    /// The (inode, rec_len, name) of each entry in `area`.
    fn entries(area: &[u8]) -> Vec<(u32, usize, Vec<u8>)> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < area.len() {
            let header = DirEntry::read(&area[offset..]);
            let len = rec_len(header.rec_len, area.len());
            let name_len = usize::from(header.name_len);
            let name = area[offset + 8..offset + 8 + name_len].to_vec();
            out.push((header.inode, len, name));
            offset += len;
        }
        out
    }

    #[test]
    fn a_new_entry_takes_the_room_after_the_last_name() {
        let mut area = vec![0; 1012];
        put_entry(&mut area, 0, &new(b".", 2), 12);
        put_entry(&mut area, 12, &new(b"..", 2), 1000);
        assert_eq!(insert(&mut area, &new(b"new.txt", 20)), Ok(true));
        assert_eq!(
            entries(&area),
            [
                (2, 12, b".".to_vec()),
                (2, 12, b"..".to_vec()),
                (20, 988, b"new.txt".to_vec())
            ]
        );
    }

    #[test]
    fn an_unused_entry_long_enough_is_reused() {
        let mut area = vec![0; 1024];
        put_entry(&mut area, 0, &new(b"gone-file", 0), 20);
        put_entry(&mut area, 20, &new(b"kept", 13), 1004);
        // 13 bytes of name need 24: not the unused 20, but the room after "kept".
        assert_eq!(insert(&mut area, &new(b"abcdefghijklm", 21)), Ok(true));
        assert_eq!(entries(&area)[0], (0, 20, b"gone-file".to_vec()));
        assert_eq!(
            entries(&area)[2],
            (21, 1004 - 12, b"abcdefghijklm".to_vec())
        );
        // 5 bytes need 16, which the unused entry has.
        assert_eq!(insert(&mut area, &new(b"short", 30)), Ok(true));
        assert_eq!(entries(&area)[0], (30, 20, b"short".to_vec()));
    }

    #[test]
    fn a_full_block_takes_nothing_and_a_bad_one_is_refused() {
        let mut area = vec![0; 1024];
        // Three 250-byte names (260 bytes each) and a 236-byte one (244) fill 1024 exactly.
        for at in 0..3 {
            put_entry(&mut area, at * 260, &new(&[b'a'; 250], 12), 260);
        }
        put_entry(&mut area, 780, &new(&[b'b'; 236], 13), 244);
        assert_eq!(insert(&mut area, &new(b"c", 14)), Ok(false));
        put_rec_len(&mut area, 0, 3);
        assert_eq!(insert(&mut area, &new(b"c", 14)), Err(0));
    }

    #[test]
    fn a_whole_64k_block_is_stored_as_0xffff() {
        let mut block = vec![0; BLOCK_64K];
        put_entry(&mut block, 0, &new(b"only", 12), BLOCK_64K);
        assert_eq!(DirEntry::read(&block).rec_len, u16::MAX);
        assert_eq!(entries(&block), [(12, BLOCK_64K, b"only".to_vec())]);
    }
}
