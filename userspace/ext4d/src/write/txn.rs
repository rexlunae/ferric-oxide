//! The blocks one change rewrites, gathered in memory before they are journaled, and the
//! block-group bookkeeping that allocating and freeing need: the block and inode bitmaps,
//! the group descriptors' counters, flags and checksums, and the superblock's free counts.
//!
//! A [`Txn`] reads each block it is asked for once and keeps it; [`Txn::block_mut`] marks a
//! block changed. Allocation and freeing change the bitmaps and descriptors as they go, and
//! [`Txn::finish`] then puts the checksums and the superblock's counts right and hands back
//! every changed block, for one journal transaction.
//!
//! Before a group's descriptor or bitmaps are changed, their stored checksums are checked,
//! so that a damaged group is never rewritten with checksums that would hide the damage. A
//! block is handed out only if its bitmap says it is free and it is not part of the volume's
//! fixed metadata or the journal; a block is freed only if its bitmap says it is in use and
//! it is neither of those.
//!
//! Groups whose bitmaps were never initialized (`BLOCK_UNINIT`, `INODE_UNINIT`) are passed
//! over: the volume has room elsewhere, or the allocation fails with
//! [`WriteError::NoSpace`]. The blocks reserved for root (`s_r_blocks_count`) are not held
//! back.
//!
//! # Specification
//!
//! "Ext4 Disk Layout" on the ext4 wiki: "Block Group Descriptors" (the fields, their high
//! halves with 64-byte descriptors, `bg_flags`, `bg_itable_unused`), "Block and inode
//! Bitmaps", "The Super Block" (`s_free_blocks_count`, `s_free_inodes_count`, `s_first_ino`)
//! and "Checksums": a descriptor's `bg_checksum` is the low 16 bits of CRC32C from the
//! checksum seed over the group number and the descriptor with `bg_checksum` zero
//! (`metadata_csum`), or CRC16 over the UUID, the group number and the descriptor without
//! `bg_checksum` (`gdt_csum`); a bitmap's checksum is CRC32C from the seed over its
//! `blocks_per_group / 8` or `inodes_per_group / 8` bytes, split into a low and (with 64-byte
//! descriptors) a high half. The checksums were checked against volumes made by e2fsprogs
//! 1.47's `mke2fs`. No code here comes from the Linux kernel or from e2fsprogs.

use std::collections::{BTreeMap, BTreeSet};

use crc::{CRC_16_MODBUS, Crc};

use super::ondisk::{GroupDescHi, GroupDescLo, Record};
use super::{Target, WriteError};
use crate::device::{ReadAt, Window};
use crate::journal::map::{BlockMap, le32};
use crate::superblock::{self, INCOMPAT_64BIT, INCOMPAT_RECOVER};

/// The CRC16 `gdt_csum` uses: the reflected 0x8005 polynomial from `0xFFFF`, not inverted at
/// the end.
static CRC16: Crc<u16> = Crc::<u16>::new(&CRC_16_MODBUS);

/// `bg_flags` bits.
const INODE_UNINIT: u16 = 0x1;
const BLOCK_UNINIT: u16 = 0x2;
/// Where `bg_checksum` is, which the CRC16 of `gdt_csum` leaves out.
const BG_CHECKSUM: usize = 0x1E;
/// The descriptor size from which the high halves exist.
const WIDE_DESC: u32 = 64;

/// Superblock fields.
const S_FREE_BLOCKS_LO: usize = 0x0C;
const S_FREE_INODES: usize = 0x10;
const S_REV_LEVEL: usize = 0x4C;
const S_FIRST_INO: usize = 0x54;
const S_INCOMPAT: usize = 0x60;
const S_FREE_BLOCKS_HI: usize = 0x158;
const S_CHECKSUM: usize = 0x3FC;
/// The first inode a revision 0 volume leaves to files.
const GOOD_OLD_FIRST_INO: u32 = 11;

/// A group descriptor: its bytes as stored (`s_desc_size` of them, which may be more than the
/// fields known here) and its fields, the high halves only in a 64-byte one.
#[derive(Clone)]
struct Descriptor {
    raw: Vec<u8>,
    lo: GroupDescLo,
    hi: Option<GroupDescHi>,
}

impl Descriptor {
    /// The descriptor `raw` holds; `wide` when it is at least 64 bytes long.
    fn parse(raw: Vec<u8>, wide: bool) -> Self {
        let lo = GroupDescLo::read(&raw);
        let hi = wide.then(|| GroupDescHi::read(&raw[GroupDescLo::SIZE..]));
        Self { raw, lo, hi }
    }

    /// Its bytes, with the fields as they are now.
    fn bytes(&self) -> Vec<u8> {
        let mut raw = self.raw.clone();
        self.lo.write(&mut raw);
        if let Some(hi) = self.hi {
            hi.write(&mut raw[GroupDescLo::SIZE..]);
        }
        raw
    }

    fn high<T: Default>(&self, field: impl FnOnce(&GroupDescHi) -> T) -> T {
        self.hi.as_ref().map(field).unwrap_or_default()
    }

    /// The block or inode bitmap's block.
    fn bitmap(&self, inodes: bool) -> u64 {
        let (lo, hi) = if inodes {
            (self.lo.inode_bitmap, self.high(|hi| hi.inode_bitmap))
        } else {
            (self.lo.block_bitmap, self.high(|hi| hi.block_bitmap))
        };
        u64::from(hi) << 32 | u64::from(lo)
    }

    /// The bitmap's checksum as stored: 16 bits, or 32 in a 64-byte descriptor.
    fn bitmap_csum(&self, inodes: bool) -> u32 {
        let (lo, hi) = if inodes {
            (
                self.lo.inode_bitmap_csum,
                self.high(|hi| hi.inode_bitmap_csum),
            )
        } else {
            (
                self.lo.block_bitmap_csum,
                self.high(|hi| hi.block_bitmap_csum),
            )
        };
        u32::from(hi) << 16 | u32::from(lo)
    }

    fn set_bitmap_csum(&mut self, inodes: bool, crc: u32) {
        let (lo, hi) = (crc as u16, (crc >> 16) as u16);
        if inodes {
            self.lo.inode_bitmap_csum = lo;
            if let Some(high) = &mut self.hi {
                high.inode_bitmap_csum = hi;
            }
        } else {
            self.lo.block_bitmap_csum = lo;
            if let Some(high) = &mut self.hi {
                high.block_bitmap_csum = hi;
            }
        }
    }

    fn free_blocks(&self) -> u32 {
        u32::from(self.high(|hi| hi.free_blocks)) << 16 | u32::from(self.lo.free_blocks)
    }

    fn set_free_blocks(&mut self, count: u32) {
        self.lo.free_blocks = count as u16;
        if let Some(hi) = &mut self.hi {
            hi.free_blocks = (count >> 16) as u16;
        }
    }

    fn free_inodes(&self) -> u32 {
        u32::from(self.high(|hi| hi.free_inodes)) << 16 | u32::from(self.lo.free_inodes)
    }

    fn set_free_inodes(&mut self, count: u32) {
        self.lo.free_inodes = count as u16;
        if let Some(hi) = &mut self.hi {
            hi.free_inodes = (count >> 16) as u16;
        }
    }

    fn itable_unused(&self) -> u32 {
        u32::from(self.high(|hi| hi.itable_unused)) << 16 | u32::from(self.lo.itable_unused)
    }

    fn set_itable_unused(&mut self, count: u32) {
        self.lo.itable_unused = count as u16;
        if let Some(hi) = &mut self.hi {
            hi.itable_unused = (count >> 16) as u16;
        }
    }

    fn uninit(&self, flag: u16) -> bool {
        self.lo.flags & flag != 0
    }
}

/// What each group touched so far needs at the end.
#[derive(Default)]
struct Touched {
    block_bitmap: bool,
    inode_bitmap: bool,
}

/// The blocks a change rewrites, and the group bookkeeping that goes with them.
pub(super) struct Txn<'a, R> {
    window: &'a Window<R>,
    target: &'a Target,
    /// The journal's blocks, which are never handed out or freed.
    journal: &'a BlockMap,
    /// The fixed metadata (`Volume::metadata`), read when first needed.
    metadata: Option<Vec<(u64, u64)>>,
    cache: BTreeMap<u64, Vec<u8>>,
    dirty: BTreeSet<u64>,
    groups: BTreeMap<u32, Touched>,
    free_blocks: i64,
    free_inodes: i64,
}

impl<'a, R: ReadAt> Txn<'a, R> {
    /// A change to the volume `window` holds, which `target` describes.
    pub(super) fn new(window: &'a Window<R>, target: &'a Target, journal: &'a BlockMap) -> Self {
        Self {
            window,
            target,
            journal,
            metadata: None,
            cache: BTreeMap::new(),
            dirty: BTreeSet::new(),
            groups: BTreeMap::new(),
            free_blocks: 0,
            free_inodes: 0,
        }
    }

    /// What metadata checksums start from, with `metadata_csum`.
    pub(super) fn checksum_seed(&self) -> Option<u32> {
        self.target.checksum_seed
    }

    /// The volume's block size in bytes.
    pub(super) fn block_size(&self) -> u64 {
        u64::from(self.target.volume.block_size)
    }

    fn load(&mut self, block: u64) -> Result<&mut Vec<u8>, WriteError> {
        if block >= self.target.volume.blocks {
            return Err(WriteError::BadBlock { block });
        }
        if !self.cache.contains_key(&block) {
            let mut buf = vec![0; self.target.volume.block_size as usize];
            self.window
                .read_exact_at(&mut buf, block * self.block_size())?;
            self.cache.insert(block, buf);
        }
        Ok(self.cache.get_mut(&block).expect("just inserted"))
    }

    /// Volume block `block`, as this change has left it so far.
    pub(super) fn block(&mut self, block: u64) -> Result<&[u8], WriteError> {
        Ok(self.load(block)?.as_slice())
    }

    /// Volume block `block`, to change: it is written by this change.
    pub(super) fn block_mut(&mut self, block: u64) -> Result<&mut [u8], WriteError> {
        self.dirty.insert(block);
        Ok(self.load(block)?.as_mut_slice())
    }

    /// Volume block `block` with new contents, `data` (one block long), which is not read
    /// first: a block this change has just allocated.
    pub(super) fn put_block(&mut self, block: u64, data: Vec<u8>) {
        self.dirty.insert(block);
        self.cache.insert(block, data);
    }

    /// Where inode `number` is: its inode table block and the byte offset in it.
    fn inode_home(&self, number: u32) -> Result<(u64, usize), WriteError> {
        let at = self.target.volume.inode_offset(self.window, number)?;
        Ok((at / self.block_size(), (at % self.block_size()) as usize))
    }

    /// Inode `number`'s bytes, as this change has left them so far.
    pub(super) fn inode(&mut self, number: u32) -> Result<Vec<u8>, WriteError> {
        let (home, within) = self.inode_home(number)?;
        let size = self.target.volume.inode_size as usize;
        Ok(self.block(home)?[within..within + size].to_vec())
    }

    /// Stores `inode` as inode `number`'s bytes.
    pub(super) fn put_inode(&mut self, number: u32, inode: &[u8]) -> Result<(), WriteError> {
        let (home, within) = self.inode_home(number)?;
        self.block_mut(home)?[within..within + inode.len()].copy_from_slice(inode);
        Ok(())
    }

    /// The block map of inode `number`, whose bytes are `inode`, as on the volume.
    pub(super) fn block_map(&self, number: u32, inode: &[u8]) -> Result<BlockMap, WriteError> {
        Ok(BlockMap::of_inode(
            &self.target.volume,
            self.window,
            inode,
            number,
        )?)
    }

    /// Whether `block` is part of the fixed metadata or the journal.
    pub(super) fn reserved(&mut self, block: u64) -> Result<bool, WriteError> {
        if self.metadata.is_none() {
            self.metadata = Some(self.target.volume.metadata(self.window)?);
        }
        let metadata = self.metadata.as_deref().unwrap_or_default();
        let fixed = metadata
            .iter()
            .any(|&(start, end)| (start..end).contains(&block));
        Ok(fixed || self.journal.contains(block))
    }

    /// The number of groups.
    fn group_count(&self) -> u32 {
        // Volume::parse checked blocks_per_group; the count fits, as each group has a
        // descriptor.
        u32::try_from(self.target.volume.groups()).unwrap_or(u32::MAX)
    }

    /// Group `group`'s descriptor block and the offset of the descriptor in it.
    fn desc_home(&self, group: u32) -> (u64, usize) {
        let volume = &self.target.volume;
        let at = u64::from(group) * u64::from(volume.desc_size);
        let block = u64::from(volume.first_data_block) + 1 + at / self.block_size();
        (block, (at % self.block_size()) as usize)
    }

    /// Group `group`'s descriptor, checked against its checksum the first time it is read.
    fn desc(&mut self, group: u32) -> Result<Descriptor, WriteError> {
        let (home, within) = self.desc_home(group);
        let size = self.target.volume.desc_size as usize;
        let wide = self.target.volume.desc_size >= WIDE_DESC;
        // Volume::parse checked that descriptors are at least 32 bytes.
        let desc = Descriptor::parse(self.block(home)?[within..within + size].to_vec(), wide);
        if !self.groups.contains_key(&group) {
            if let Some(computed) = self.desc_checksum(group, &desc) {
                let stored = desc.lo.checksum;
                if stored != computed {
                    return Err(WriteError::GroupChecksum {
                        group,
                        stored: u32::from(stored),
                        computed: u32::from(computed),
                    });
                }
            }
            self.groups.insert(group, Touched::default());
        }
        Ok(desc)
    }

    fn put_desc(&mut self, group: u32, desc: &Descriptor) -> Result<(), WriteError> {
        let (home, within) = self.desc_home(group);
        let bytes = desc.bytes();
        self.block_mut(home)?[within..within + bytes.len()].copy_from_slice(&bytes);
        Ok(())
    }

    /// The checksum `bg_checksum` should hold, or `None` on a volume without descriptor
    /// checksums.
    fn desc_checksum(&self, group: u32, desc: &Descriptor) -> Option<u16> {
        // The group number is checksummed as the little-endian 32 bits the format names.
        let number = group.to_le_bytes();
        if let Some(seed) = self.target.checksum_seed {
            let mut zeroed = desc.clone();
            zeroed.lo.checksum = 0;
            let crc = ferric_jbd2::crc32c_seeded(seed, &[&number, &zeroed.bytes()]);
            return Some(crc as u16);
        }
        if !self.target.gdt_csum {
            return None;
        }
        let bytes = desc.bytes();
        let mut digest = CRC16.digest();
        digest.update(&self.target.uuid);
        digest.update(&number);
        digest.update(&bytes[..BG_CHECKSUM]);
        digest.update(&bytes[BG_CHECKSUM + 2..]);
        Some(digest.finalize())
    }

    /// Group `group`'s block or inode bitmap, checked against its checksum the first time
    /// (`metadata_csum`).
    fn bitmap(&mut self, group: u32, desc: &Descriptor, inodes: bool) -> Result<u64, WriteError> {
        let block = desc.bitmap(inodes);
        let first = !self.cache.contains_key(&block);
        if first && let Some(computed) = self.bitmap_checksum(block, inodes)? {
            let stored = desc.bitmap_csum(inodes);
            let computed = if self.target.volume.desc_size >= WIDE_DESC {
                computed
            } else {
                computed & 0xFFFF
            };
            if stored != computed {
                return Err(WriteError::BitmapChecksum {
                    group,
                    stored,
                    computed,
                });
            }
        }
        Ok(block)
    }

    /// The checksum of the bitmap in `block`, or `None` without `metadata_csum`.
    fn bitmap_checksum(&mut self, block: u64, inodes: bool) -> Result<Option<u32>, WriteError> {
        let Some(seed) = self.target.checksum_seed else {
            return Ok(None);
        };
        let volume = &self.target.volume;
        let per_group = if inodes {
            volume.inodes_per_group
        } else {
            volume.blocks_per_group
        };
        let len = (per_group / 8) as usize;
        let bitmap = self.block(block)?;
        let len = len.min(bitmap.len());
        Ok(Some(ferric_jbd2::crc32c_seeded(seed, &[&bitmap[..len]])))
    }

    /// The first block of group `group`, and how many blocks it has.
    fn group_span(&self, group: u32) -> (u64, u64) {
        let volume = &self.target.volume;
        let first = u64::from(volume.first_data_block)
            + u64::from(group) * u64::from(volume.blocks_per_group);
        let count = (volume.blocks - first).min(u64::from(volume.blocks_per_group));
        (first, count)
    }

    /// The group volume block `block` is in.
    fn group_of(&self, block: u64) -> Option<u32> {
        let volume = &self.target.volume;
        let data = block.checked_sub(u64::from(volume.first_data_block))?;
        u32::try_from(data / u64::from(volume.blocks_per_group))
            .ok()
            .filter(|&group| group < self.group_count())
    }

    /// Allocates a free block, the first one at or after `goal` if there is one there, and
    /// otherwise the first free one in the groups after it, round the volume.
    ///
    /// # Errors
    ///
    /// [`WriteError::NoSpace`] when no initialized group has a free block,
    /// [`WriteError::GroupChecksum`] and [`WriteError::BitmapChecksum`] for a damaged group,
    /// and [`WriteError::Device`].
    pub(super) fn allocate_block(&mut self, goal: u64) -> Result<u64, WriteError> {
        let groups = self.group_count();
        let start = self.group_of(goal).unwrap_or(0);
        // The goal's group from the goal on, the other groups, then the goal's group
        // before the goal.
        for step in 0..=groups {
            let group = (start + step % groups.max(1)) % groups.max(1);
            let (first, count) = self.group_span(group);
            let (from, to) = match step {
                0 => (goal.saturating_sub(first).min(count), count),
                s if s == groups => (0, goal.saturating_sub(first).min(count)),
                _ => (0, count),
            };
            if let Some(block) = self.take_block(group, from, to)? {
                return Ok(block);
            }
        }
        Err(WriteError::NoSpace)
    }

    /// Takes the first free block from bit `from` to bit `to` of group `group`, if any.
    fn take_block(&mut self, group: u32, from: u64, to: u64) -> Result<Option<u64>, WriteError> {
        let mut desc = self.desc(group)?;
        let free = desc.free_blocks();
        if from >= to || free == 0 || desc.uninit(BLOCK_UNINIT) {
            return Ok(None);
        }
        let bitmap = self.bitmap(group, &desc, false)?;
        let (first, _) = self.group_span(group);
        for bit in from..to {
            let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
            if self.block(bitmap)?[byte] & mask != 0 || self.reserved(first + bit)? {
                continue;
            }
            self.block_mut(bitmap)?[byte] |= mask;
            desc.set_free_blocks(free - 1);
            self.put_desc(group, &desc)?;
            self.touch(group).block_bitmap = true;
            self.free_blocks -= 1;
            return Ok(Some(first + bit));
        }
        Ok(None)
    }

    /// Frees volume block `block`.
    ///
    /// # Errors
    ///
    /// [`WriteError::BadBlock`] for a block outside the groups, in the fixed metadata or the
    /// journal, in a group whose bitmap was never initialized, or already free; and those of
    /// [`Self::allocate_block`] for the group.
    pub(super) fn free_block(&mut self, block: u64) -> Result<(), WriteError> {
        let group = self.group_of(block).ok_or(WriteError::BadBlock { block })?;
        if self.reserved(block)? {
            return Err(WriteError::BadBlock { block });
        }
        let mut desc = self.desc(group)?;
        if desc.uninit(BLOCK_UNINIT) {
            return Err(WriteError::BadBlock { block });
        }
        let bitmap = self.bitmap(group, &desc, false)?;
        let bit = block - self.group_span(group).0;
        let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
        if self.block(bitmap)?[byte] & mask == 0 {
            return Err(WriteError::BadBlock { block });
        }
        self.block_mut(bitmap)?[byte] &= !mask;
        desc.set_free_blocks(desc.free_blocks() + 1);
        self.put_desc(group, &desc)?;
        self.touch(group).block_bitmap = true;
        self.free_blocks += 1;
        Ok(())
    }

    /// Allocates a free inode, the first one in group `goal` or the groups after it.
    ///
    /// # Errors
    ///
    /// [`WriteError::NoSpace`] when no initialized group has a free inode, and those of
    /// [`Self::allocate_block`] for the groups read.
    pub(super) fn allocate_inode(&mut self, goal: u32) -> Result<u32, WriteError> {
        let groups = self.group_count();
        for step in 0..groups {
            let group = (goal + step) % groups;
            if let Some(number) = self.take_inode(group)? {
                return Ok(number);
            }
        }
        Err(WriteError::NoSpace)
    }

    fn take_inode(&mut self, group: u32) -> Result<Option<u32>, WriteError> {
        let mut desc = self.desc(group)?;
        let free = desc.free_inodes();
        if free == 0 || desc.uninit(INODE_UNINIT) {
            return Ok(None);
        }
        let bitmap = self.bitmap(group, &desc, true)?;
        let per_group = self.target.volume.inodes_per_group;
        let first_ino = self.first_ino()?;
        for bit in 0..per_group {
            let number = group * per_group + bit + 1;
            let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
            if number < first_ino || self.block(bitmap)?[byte] & mask != 0 {
                continue;
            }
            self.block_mut(bitmap)?[byte] |= mask;
            desc.set_free_inodes(free - 1);
            if self.target.checksum_seed.is_some() || self.target.gdt_csum {
                // The inodes past the last one in use are not even read by e2fsck; this one
                // must not be among them.
                let used = per_group.saturating_sub(desc.itable_unused());
                if bit >= used {
                    desc.set_itable_unused(per_group - bit - 1);
                }
            }
            self.put_desc(group, &desc)?;
            self.touch(group).inode_bitmap = true;
            self.free_inodes -= 1;
            return Ok(Some(number));
        }
        Ok(None)
    }

    /// `s_first_ino`: the first inode a file may have.
    fn first_ino(&mut self) -> Result<u32, WriteError> {
        let sb = self.superblock()?;
        Ok(if le32(&sb, S_REV_LEVEL) == 0 {
            GOOD_OLD_FIRST_INO
        } else {
            le32(&sb, S_FIRST_INO)
        })
    }

    fn touch(&mut self, group: u32) -> &mut Touched {
        self.groups.entry(group).or_default()
    }

    /// The superblock's block and its offset in it.
    fn superblock_home(&self) -> (u64, usize) {
        let block_size = self.block_size();
        (
            superblock::OFFSET / block_size,
            (superblock::OFFSET % block_size) as usize,
        )
    }

    fn superblock(&mut self) -> Result<Vec<u8>, WriteError> {
        let (home, within) = self.superblock_home();
        Ok(self.block(home)?[within..within + superblock::SIZE].to_vec())
    }

    /// Puts the changed groups' checksums and the superblock's free counts right, and
    /// returns every block this change writes.
    ///
    /// The superblock is logged with `INCOMPAT_RECOVER` set, as it is on the disk while the
    /// transaction is in the journal, so that writing it home does not mark the volume clean
    /// before the journal is emptied.
    pub(super) fn finish(mut self) -> Result<BTreeMap<u64, Vec<u8>>, WriteError> {
        let groups: Vec<(u32, bool, bool)> = self
            .groups
            .iter()
            .map(|(&group, touched)| (group, touched.block_bitmap, touched.inode_bitmap))
            .collect();
        let mut changed_groups = false;
        for (group, blocks, inodes) in groups {
            if !blocks && !inodes {
                continue;
            }
            changed_groups = true;
            let mut desc = self.desc(group)?;
            for (touched, is_inodes) in [(blocks, false), (inodes, true)] {
                if !touched {
                    continue;
                }
                let block = desc.bitmap(is_inodes);
                if let Some(crc) = self.bitmap_checksum(block, is_inodes)? {
                    desc.set_bitmap_csum(is_inodes, crc);
                }
            }
            if let Some(crc) = self.desc_checksum(group, &desc) {
                desc.lo.checksum = crc;
            }
            self.put_desc(group, &desc)?;
        }
        if changed_groups {
            self.put_superblock_counts()?;
        }
        let Self { cache, dirty, .. } = self;
        Ok(cache
            .into_iter()
            .filter(|(block, _)| dirty.contains(block))
            .collect())
    }

    fn put_superblock_counts(&mut self) -> Result<(), WriteError> {
        let mut sb = self.superblock()?;
        let wide = le32(&sb, S_INCOMPAT) & INCOMPAT_64BIT != 0;
        let high = if wide {
            u64::from(le32(&sb, S_FREE_BLOCKS_HI))
        } else {
            0
        };
        let blocks = high << 32 | u64::from(le32(&sb, S_FREE_BLOCKS_LO));
        let blocks = blocks.saturating_add_signed(self.free_blocks);
        sb[S_FREE_BLOCKS_LO..S_FREE_BLOCKS_LO + 4].copy_from_slice(&(blocks as u32).to_le_bytes());
        if wide {
            let high = (blocks >> 32) as u32;
            sb[S_FREE_BLOCKS_HI..S_FREE_BLOCKS_HI + 4].copy_from_slice(&high.to_le_bytes());
        }
        let inodes = u64::from(le32(&sb, S_FREE_INODES)).saturating_add_signed(self.free_inodes);
        sb[S_FREE_INODES..S_FREE_INODES + 4].copy_from_slice(&(inodes as u32).to_le_bytes());
        let incompat = le32(&sb, S_INCOMPAT) | INCOMPAT_RECOVER;
        sb[S_INCOMPAT..S_INCOMPAT + 4].copy_from_slice(&incompat.to_le_bytes());
        if self.target.checksum_seed.is_some() {
            let crc = ferric_jbd2::crc32c(&[&sb[..S_CHECKSUM]]);
            sb[S_CHECKSUM..].copy_from_slice(&crc.to_le_bytes());
        }
        let (home, within) = self.superblock_home();
        self.block_mut(home)?[within..within + superblock::SIZE].copy_from_slice(&sb);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gdt_crc16_is_the_modbus_one() {
        // The reflected 0x8005 polynomial from 0xFFFF without a final inversion: the
        // standard check value of "123456789" for that CRC is 0x4B37.
        assert_eq!(CRC16.checksum(b"123456789"), 0x4B37);
    }
}
