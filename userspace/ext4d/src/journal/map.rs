//! Finding the journal inode's blocks: the superblock fields, its group descriptor, the
//! inode, and its extent tree or (ext3) indirect block map. `crate::lookup` maps
//! directories the same way ([`BlockMap::of_inode`]), with holes allowed.
//!
//! Written from "Ext4 Disk Layout": "The Super Block", "Block Group Descriptors", "Finding
//! an Inode", "Inode Table", "The Contents of inode.i_block" ("Direct/Indirect Block
//! Addressing" and "Extent Tree"). All fields are little-endian.
//!
//! The replay writes to the blocks this map names (the journal superblock, when it empties
//! the log), so a map read from a damaged or forged volume must not be able to point those
//! writes at anything else. Before the map is used:
//!
//! - the superblock's own checksum is checked (with `metadata_csum`), and the journal
//!   inode's `i_block` and size must match the copy the superblock keeps in `s_jnl_blocks`
//!   (when `s_jnl_backup_type` says it keeps one);
//! - the map must cover the journal from block 0 without holes, lie inside the volume and
//!   not overlap itself;
//! - it must not overlap the volume's fixed metadata: the boot block, every superblock and
//!   group descriptor table copy (with the reserved GDT blocks), and each group's block and
//!   inode bitmaps and inode table.
//!
//! The checksums of group descriptors, the inode and extent blocks are not checked here.
//! What the journal yields is checked anyway: the journal superblock has its magic number
//! (and its own checksum on version 2 and 3 journals), and every block that is replayed
//! must sit under a valid commit.

use super::JournalError;
use crate::device::{ReadAt, Window};
use crate::superblock::{self, INCOMPAT_64BIT};

/// `s_feature_incompat`: this volume is an external journal device.
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
/// `s_feature_incompat`: group descriptors are spread over the volume.
const INCOMPAT_META_BG: u32 = 0x10;
/// `s_feature_ro_compat`: metadata checksums, including the superblock's.
pub(crate) const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
/// `s_feature_ro_compat`: superblock backups only in groups 0, 1 and powers of 3, 5 and 7.
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
/// `s_feature_compat`: superblock backups only in the two groups `s_backup_bgs` names.
const COMPAT_SPARSE_SUPER2: u32 = 0x200;
/// `s_jnl_backup_type`: `s_jnl_blocks` holds a copy of the journal inode's block map.
const JNL_BACKUP_BLOCKS: u8 = 1;
/// Where the superblock keeps `s_jnl_blocks`: `i_block` (60 bytes), `i_size_high`,
/// `i_size`.
const JNL_BLOCKS_AT: usize = 0x10C;
/// Where the superblock keeps `s_checksum`.
pub(crate) const CHECKSUM_AT: usize = 0x3FC;
/// `i_flags`: `i_block` holds an extent tree.
const EXTENTS_FL: u32 = 0x8_0000;
/// The extent header's magic number.
const EXTENT_MAGIC: u16 = 0xF30A;
/// The format's maximum extent tree depth.
const MAX_DEPTH: u16 = 5;

/// The superblock fields journal recovery needs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Volume {
    pub(crate) block_size: u32,
    pub(crate) blocks: u64,
    needs_recovery: bool,
    pub(crate) incompat: u32,
    first_data_block: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    /// `s_inodes_count`.
    pub(crate) inodes: u32,
    /// The size of an on-disk inode in bytes.
    pub(crate) inode_size: u32,
    desc_size: u32,
    first_meta_bg: u32,
    reserved_gdt_blocks: u32,
    pub(crate) compat: u32,
    pub(crate) ro_compat: u32,
    backup_groups: [u32; 2],
    /// The superblock's copy of the journal inode's `i_block`, `i_size_high` and `i_size`.
    journal_backup: Option<[u8; 68]>,
    pub(crate) journal_inode: u32,
}

impl Volume {
    /// Reads the fields from the superblock's bytes.
    pub(crate) fn parse(sb: &[u8; superblock::SIZE]) -> Result<Self, JournalError> {
        let geometry = superblock::Geometry::parse(sb)?;
        let incompat = le32(sb, 0x60);
        let wide = incompat & INCOMPAT_64BIT != 0;
        // Revision 0 volumes have 128-byte inodes and no s_inode_size.
        let inode_size = if le32(sb, 0x4C) == 0 {
            128
        } else {
            u32::from(le16(sb, 0x58))
        };
        let desc_size = if wide { u32::from(le16(sb, 0xFE)) } else { 32 };
        let ro_compat = le32(sb, 0x64);
        if ro_compat & RO_COMPAT_METADATA_CSUM != 0 {
            let stored = le32(sb, CHECKSUM_AT);
            let computed = ferric_jbd2::crc32c(&[&sb[..CHECKSUM_AT]]);
            if stored != computed {
                return Err(JournalError::SuperblockChecksum { stored, computed });
            }
        }
        let journal_backup = (sb[0xFD] == JNL_BACKUP_BLOCKS).then(|| {
            let mut copy = [0; 68];
            copy.copy_from_slice(&sb[JNL_BLOCKS_AT..JNL_BLOCKS_AT + 68]);
            copy
        });
        let volume = Self {
            block_size: geometry.block_size,
            blocks: geometry.blocks,
            needs_recovery: superblock::needs_recovery(sb),
            incompat,
            first_data_block: le32(sb, 0x14),
            blocks_per_group: le32(sb, 0x20),
            inodes_per_group: le32(sb, 0x28),
            inodes: le32(sb, 0x0),
            inode_size,
            desc_size,
            first_meta_bg: le32(sb, 0x104),
            reserved_gdt_blocks: u32::from(le16(sb, 0xCE)),
            compat: le32(sb, 0x5C),
            ro_compat,
            backup_groups: [le32(sb, 0x24C), le32(sb, 0x250)],
            journal_backup,
            journal_inode: le32(sb, 0xE0),
        };
        let sane = volume.inodes_per_group != 0
            && volume.blocks_per_group != 0
            && (128..=volume.block_size).contains(&inode_size)
            && (32..=volume.block_size).contains(&desc_size);
        if !sane {
            return Err(JournalError::BadSuperblock);
        }
        Ok(volume)
    }

    /// Whether the volume has an internal journal that needs replaying; an error for a
    /// journal kept on another device.
    pub(crate) fn needs_replay(&self) -> Result<bool, JournalError> {
        if !self.needs_recovery {
            return Ok(false);
        }
        if self.incompat & INCOMPAT_JOURNAL_DEV != 0 || self.journal_inode == 0 {
            return Err(JournalError::ExternalJournal);
        }
        Ok(true)
    }

    /// The number of block groups.
    fn groups(&self) -> u64 {
        let data = self.blocks.saturating_sub(u64::from(self.first_data_block));
        data.div_ceil(u64::from(self.blocks_per_group))
    }

    /// Whether group `group` starts with a copy of the superblock and descriptor table.
    fn has_backup(&self, group: u64) -> bool {
        if group == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return self
                .backup_groups
                .iter()
                .any(|&g| g != 0 && u64::from(g) == group);
        }
        if self.ro_compat & RO_COMPAT_SPARSE_SUPER == 0 {
            return true;
        }
        group == 1 || [3, 5, 7].into_iter().any(|base| is_power_of(group, base))
    }

    /// The volume's fixed metadata as block ranges (`start..end`): the boot block and every
    /// superblock and descriptor table copy with the reserved GDT blocks, then each group's
    /// bitmaps and inode table, read from its descriptor.
    fn metadata<R: ReadAt>(&self, window: &Window<R>) -> Result<Vec<(u64, u64)>, JournalError> {
        if self.incompat & INCOMPAT_META_BG != 0 {
            return Err(JournalError::Unsupported {
                what: "replaying the journal of a meta_bg volume",
            });
        }
        let groups = self.groups();
        let block_size = u64::from(self.block_size);
        let gdt_blocks = (groups * u64::from(self.desc_size)).div_ceil(block_size);
        let header = 1 + gdt_blocks + u64::from(self.reserved_gdt_blocks);
        let table_blocks =
            (u64::from(self.inodes_per_group) * u64::from(self.inode_size)).div_ceil(block_size);
        // Block 0 holds the boot sector (and, with 1 KiB blocks, nothing else).
        let mut ranges = vec![(0, u64::from(self.first_data_block) + header)];
        let mut gdt = vec![0; self.block_size as usize];
        let per_block = block_size / u64::from(self.desc_size);
        for group in 0..groups {
            let start = u64::from(self.first_data_block) + group * u64::from(self.blocks_per_group);
            if group != 0 && self.has_backup(group) {
                ranges.push((start, start + header));
            }
            if group % per_block == 0 {
                let block = u64::from(self.first_data_block) + 1 + group / per_block;
                self.read_block(window, block, &mut gdt)?;
            }
            let at = ((group % per_block) * u64::from(self.desc_size)) as usize;
            let desc = &gdt[at..at + self.desc_size as usize];
            let field = |lo: usize, hi: usize| {
                let high = if self.desc_size >= 64 {
                    le32(desc, hi)
                } else {
                    0
                };
                u64::from(high) << 32 | u64::from(le32(desc, lo))
            };
            let (block_bitmap, inode_bitmap, table) =
                (field(0x0, 0x20), field(0x4, 0x24), field(0x8, 0x28));
            ranges.push((block_bitmap, block_bitmap + 1));
            ranges.push((inode_bitmap, inode_bitmap + 1));
            ranges.push((table, table.saturating_add(table_blocks)));
        }
        Ok(ranges)
    }

    fn block_offset(&self, block: u64) -> Result<u64, JournalError> {
        if block >= self.blocks {
            return Err(JournalError::BlockOutOfRange { block });
        }
        Ok(block * u64::from(self.block_size))
    }

    fn read_block<R: ReadAt>(
        &self,
        window: &Window<R>,
        block: u64,
        buf: &mut [u8],
    ) -> Result<(), JournalError> {
        let offset = self.block_offset(block)?;
        Ok(window.read_exact_at(buf, offset)?)
    }

    /// Reads inode `number` (the whole on-disk inode, `inode_size` bytes).
    pub(crate) fn read_inode<R: ReadAt>(
        &self,
        window: &Window<R>,
        number: u32,
    ) -> Result<Vec<u8>, JournalError> {
        let mut inode = vec![0; self.inode_size as usize];
        window.read_exact_at(&mut inode, self.inode_offset(window, number)?)?;
        Ok(inode)
    }

    /// The byte offset of inode `number` on the volume, from its group descriptor.
    pub(crate) fn inode_offset<R: ReadAt>(
        &self,
        window: &Window<R>,
        number: u32,
    ) -> Result<u64, JournalError> {
        let index = number
            .checked_sub(1)
            .filter(|_| number <= self.inodes)
            .ok_or(JournalError::BadInode { number })?;
        let group = u64::from(index / self.inodes_per_group);
        let desc_offset = group * u64::from(self.desc_size);
        let desc_block = desc_offset / u64::from(self.block_size);
        // With meta_bg, only the descriptors before s_first_meta_bg are in the usual place.
        if self.incompat & INCOMPAT_META_BG != 0 && desc_block >= u64::from(self.first_meta_bg) {
            return Err(JournalError::Unsupported {
                what: "an inode in a meta_bg group",
            });
        }
        // The descriptor table starts in the block after the superblock's.
        let gdt_block = u64::from(self.first_data_block) + 1 + desc_block;
        let mut desc = vec![0; self.desc_size as usize];
        let within = desc_offset % u64::from(self.block_size);
        window.read_exact_at(&mut desc, self.block_offset(gdt_block)? + within)?;
        let high = if self.desc_size >= 64 {
            le32(&desc, 0x28)
        } else {
            0
        };
        let inode_table = u64::from(high) << 32 | u64::from(le32(&desc, 0x8));
        let slot = u64::from(index % self.inodes_per_group) * u64::from(self.inode_size);
        Ok(self.block_offset(inode_table)? + slot)
    }
}

/// A run of the journal: `len` blocks from journal block `logical` at volume block
/// `physical`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    logical: u32,
    len: u32,
    physical: u64,
}

/// Where each block of the journal inode (or, with [`BlockMap::of_inode`], of another
/// inode) is on the volume.
#[derive(Debug)]
pub(crate) struct BlockMap {
    runs: Vec<Run>,
    /// The inode's length in blocks, from its size.
    pub(crate) len: u32,
    /// The inode mapped, for errors.
    number: u32,
    /// Whether holes are allowed (and left out of the map): not in a journal, which has
    /// none, but in an ordinary file or directory.
    holes: bool,
}

impl BlockMap {
    /// Maps the journal inode of `volume`.
    pub(crate) fn load<R: ReadAt>(
        volume: &Volume,
        window: &Window<R>,
    ) -> Result<Self, JournalError> {
        let number = volume.journal_inode;
        let inode = volume.read_inode(window, number)?;
        let size = u64::from(le32(&inode, 0x6C)) << 32 | u64::from(le32(&inode, 0x4));
        let len = u32::try_from(size / u64::from(volume.block_size))
            .map_err(|_| JournalError::BadInode { number })?;
        let i_block = &inode[0x28..0x64];
        if let Some(backup) = &volume.journal_backup {
            let same = backup[..60] == *i_block
                && backup[60..64] == inode[0x6C..0x70]
                && backup[64..68] == inode[0x4..0x8];
            if !same {
                return Err(JournalError::JournalBackupMismatch);
            }
        }
        let mut map = Self {
            runs: Vec::new(),
            len,
            number,
            holes: false,
        };
        map.walk(volume, window, &inode)?;
        map.validate(volume, window)?;
        Ok(map)
    }

    /// Maps inode `number`, whose on-disk bytes are `inode`, for reading its blocks (a
    /// directory, for a path lookup). Unlike the journal's map, holes are allowed and left
    /// out, and so are unwritten extents, which read as zeros. Nothing is checked beyond
    /// what reading needs: each block is checked to be inside the volume when it is read.
    pub(crate) fn of_inode<R: ReadAt>(
        volume: &Volume,
        window: &Window<R>,
        inode: &[u8],
        number: u32,
    ) -> Result<Self, JournalError> {
        let size = u64::from(le32(inode, 0x6C)) << 32 | u64::from(le32(inode, 0x4));
        let len = u32::try_from(size.div_ceil(u64::from(volume.block_size)))
            .map_err(|_| JournalError::BadInode { number })?;
        let mut map = Self {
            runs: Vec::new(),
            len,
            number,
            holes: true,
        };
        map.walk(volume, window, inode)?;
        Ok(map)
    }

    /// Fills the map from the inode's extent tree or block map, sorted by logical block.
    fn walk<R: ReadAt>(
        &mut self,
        volume: &Volume,
        window: &Window<R>,
        inode: &[u8],
    ) -> Result<(), JournalError> {
        let i_block = &inode[0x28..0x64];
        if le32(inode, 0x20) & EXTENTS_FL != 0 {
            self.extents(volume, window, i_block, MAX_DEPTH)?;
        } else {
            self.indirect(volume, window, i_block)?;
        }
        self.runs.sort_by_key(|run| run.logical);
        Ok(())
    }

    /// Whether volume block `block` belongs to the journal.
    pub(crate) fn contains(&self, block: u64) -> bool {
        self.runs
            .iter()
            .any(|run| (run.physical..run.physical + u64::from(run.len)).contains(&block))
    }

    /// Checks that the map covers the journal from block 0 without holes, lies inside the
    /// volume and does not overlap itself or the volume's fixed metadata.
    fn validate<R: ReadAt>(&self, volume: &Volume, window: &Window<R>) -> Result<(), JournalError> {
        let number = volume.journal_inode;
        let mut next = 0u32;
        let mut spans = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            if run.logical != next {
                return Err(JournalError::BadInode { number });
            }
            next = run
                .logical
                .checked_add(run.len)
                .ok_or(JournalError::BadInode { number })?;
            let end = run
                .physical
                .checked_add(u64::from(run.len))
                .filter(|&end| run.physical != 0 && end <= volume.blocks)
                .ok_or(JournalError::JournalOutsideVolume {
                    block: run.physical,
                })?;
            spans.push((run.physical, end));
        }
        if next < self.len {
            return Err(JournalError::BadInode { number });
        }
        spans.sort_unstable();
        if let Some(pair) = spans.windows(2).find(|pair| pair[0].1 > pair[1].0) {
            return Err(JournalError::JournalOverlap { block: pair[1].0 });
        }
        for (start, end) in volume.metadata(window)? {
            if let Some(&(run, _)) = spans.iter().find(|&&(s, e)| s < end && start < e) {
                return Err(JournalError::JournalOverlap {
                    block: run.max(start),
                });
            }
        }
        Ok(())
    }

    /// The volume block that holds journal block `block`.
    pub(crate) fn physical(&self, block: u32) -> Option<u64> {
        let at = self.runs.partition_point(|run| run.logical <= block);
        let run = self.runs.get(at.checked_sub(1)?)?;
        let into = block - run.logical;
        (into < run.len).then(|| run.physical + u64::from(into))
    }

    /// Adds block `logical` at volume block `physical`. Block 0 is a hole: skipped where
    /// holes are allowed, an error in a journal, which has none.
    fn push(&mut self, logical: u32, physical: u64) -> Result<(), JournalError> {
        if physical == 0 {
            if self.holes {
                return Ok(());
            }
            return Err(JournalError::BadInode {
                number: self.number,
            });
        }
        if let Some(last) = self.runs.last_mut()
            && last.logical + last.len == logical
            && last.physical + u64::from(last.len) == physical
        {
            last.len += 1;
            return Ok(());
        }
        self.runs.push(Run {
            logical,
            len: 1,
            physical,
        });
        Ok(())
    }

    /// Walks an extent tree node (`i_block`, or an index block's contents).
    fn extents<R: ReadAt>(
        &mut self,
        volume: &Volume,
        window: &Window<R>,
        node: &[u8],
        depth_left: u16,
    ) -> Result<(), JournalError> {
        let bad = || JournalError::BadExtentTree;
        if le16(node, 0) != EXTENT_MAGIC {
            return Err(bad());
        }
        let entries = usize::from(le16(node, 2));
        let depth = le16(node, 6);
        if depth > depth_left || 12 + entries * 12 > node.len() {
            return Err(bad());
        }
        for entry in node[12..12 + entries * 12].chunks_exact(12) {
            if depth == 0 {
                let logical = le32(entry, 0);
                let mut len = u32::from(le16(entry, 4));
                // Over 32768 marks an unwritten extent; the journal never has one, but the
                // length is the same either way. Elsewhere it reads as zeros, as a hole does.
                if len > 32768 {
                    if self.holes {
                        continue;
                    }
                    len -= 32768;
                }
                let physical = u64::from(le16(entry, 6)) << 32 | u64::from(le32(entry, 8));
                self.runs.push(Run {
                    logical,
                    len,
                    physical,
                });
            } else {
                let child = u64::from(le16(entry, 8)) << 32 | u64::from(le32(entry, 4));
                let mut block = vec![0; volume.block_size as usize];
                volume.read_block(window, child, &mut block)?;
                self.extents(volume, window, &block, depth - 1)?;
            }
        }
        Ok(())
    }

    /// Walks an ext2/ext3 block map: 12 direct blocks, then single, double and triple
    /// indirect blocks of 32-bit block numbers.
    fn indirect<R: ReadAt>(
        &mut self,
        volume: &Volume,
        window: &Window<R>,
        i_block: &[u8],
    ) -> Result<(), JournalError> {
        let mut next = 0;
        for slot in 0..12 {
            if next >= self.len {
                return Ok(());
            }
            let block = le32(i_block, slot * 4);
            self.push(next, u64::from(block))?;
            next += 1;
        }
        for (slot, levels) in [(12, 1), (13, 2), (14, 3)] {
            if next >= self.len {
                return Ok(());
            }
            let block = le32(i_block, slot * 4);
            self.indirect_level(volume, window, u64::from(block), levels, &mut next)?;
        }
        if next < self.len {
            return Err(JournalError::BadInode {
                number: self.number,
            });
        }
        Ok(())
    }

    fn indirect_level<R: ReadAt>(
        &mut self,
        volume: &Volume,
        window: &Window<R>,
        block: u64,
        levels: u32,
        next: &mut u32,
    ) -> Result<(), JournalError> {
        if block == 0 && self.holes {
            // A hole covers every block the indirect block would have mapped.
            let per_block = u64::from(volume.block_size / 4);
            let covered = per_block.saturating_pow(levels);
            *next = u32::try_from(u64::from(*next).saturating_add(covered)).unwrap_or(u32::MAX);
            return Ok(());
        }
        let mut buf = vec![0; volume.block_size as usize];
        volume.read_block(window, block, &mut buf)?;
        for entry in buf.chunks_exact(4) {
            if *next >= self.len {
                break;
            }
            let child = u64::from(le32(entry, 0));
            if levels == 1 {
                self.push(*next, child)?;
                *next += 1;
            } else {
                self.indirect_level(volume, window, child, levels - 1, next)?;
            }
        }
        Ok(())
    }
}

/// Whether `n` is a power (the first or higher) of `base`.
fn is_power_of(mut n: u64, base: u64) -> bool {
    if n < base {
        return false;
    }
    while n.is_multiple_of(base) {
        n /= base;
    }
    n == 1
}

/// The little-endian `u16` at `at`; every caller's buffer is long enough.
fn le16(raw: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([raw[at], raw[at + 1]])
}

/// The little-endian `u32` at `at`; every caller's buffer is long enough.
pub(crate) fn le32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_superblock_backups_are_in_groups_0_1_and_powers_of_3_5_7() {
        let with: Vec<u64> = (0..60)
            .filter(|&g| g == 0 || g == 1 || [3, 5, 7].into_iter().any(|b| is_power_of(g, b)))
            .collect();
        assert_eq!(with, [0, 1, 3, 5, 7, 9, 25, 27, 49]);
        assert!(!is_power_of(1, 3) && !is_power_of(15, 3) && is_power_of(343, 7));
    }

    #[test]
    fn runs_map_blocks_and_merge() {
        let mut map = BlockMap {
            runs: Vec::new(),
            len: 10,
            number: 8,
            holes: false,
        };
        for (logical, physical) in [(0, 100), (1, 101), (2, 200), (3, 201)] {
            map.push(logical, physical).expect("not a hole");
        }
        assert!(matches!(
            map.push(4, 0),
            Err(JournalError::BadInode { number: 8 })
        ));
        assert_eq!(map.runs.len(), 2);
        assert_eq!(map.physical(1), Some(101));
        assert_eq!(map.physical(3), Some(201));
        assert_eq!(map.physical(4), None);
    }

    #[test]
    fn holes_are_left_out_where_they_are_allowed() {
        let mut map = BlockMap {
            runs: Vec::new(),
            len: 4,
            number: 12,
            holes: true,
        };
        for (logical, physical) in [(0, 100), (1, 0), (2, 0), (3, 103)] {
            map.push(logical, physical).expect("a hole is allowed");
        }
        assert_eq!(map.physical(0), Some(100));
        assert_eq!(map.physical(1), None);
        assert_eq!(map.physical(2), None);
        assert_eq!(map.physical(3), Some(103));
    }
}
