//! Reading and changing an inode's extent tree, through a [`Txn`]: the extents a file has
//! ([`leaves`]), mapping one more block ([`map`]) and cutting the tree at a block
//! ([`truncate`]).
//!
//! A tree is a root node in the inode's `i_block` (up to four entries) and, below it, index
//! and leaf nodes in blocks of their own. Each node starts with a 12-byte header (magic
//! `0xF30A`, entries, maximum entries, depth, generation); a leaf's entries are extents
//! (first logical block, length, 48-bit physical block; a length over 32768 marks the extent
//! unwritten), an index's entries name a child node by its first logical block. With
//! `metadata_csum`, a node in a block of its own ends, after its maximum number of entries,
//! with a CRC32C from the checksum seed over the inode number, `i_generation` and the node.
//!
//! [`map`] adds a block to the end of an extent when it continues it, and otherwise inserts
//! an extent of one block into the leaf that covers it, if that leaf has room; it never
//! grows the tree, so a file whose leaf is full is refused with
//! [`WriteError::ExtentTreeFull`]. [`truncate`] shortens or removes the extents past a
//! block, frees their blocks and the nodes left empty, and makes the root a leaf again when
//! nothing is left.
//!
//! # Specification
//!
//! "Ext4 Disk Layout" on the ext4 wiki: "Extent Tree" (`ext4_extent_header`,
//! `ext4_extent_idx`, `ext4_extent`, `ext4_extent_tail`) and "Checksums". The layout of a
//! node's checksum tail was checked against volumes made by e2fsprogs 1.47's `mke2fs`. No
//! code here comes from the Linux kernel or from e2fsprogs.

use super::ondisk::{ExtentHeader, ExtentIndex, ExtentLeaf, ExtentTail, Record};
use super::txn::Txn;
use super::{I_GENERATION, WriteError};
use crate::device::ReadAt;

/// Where an inode keeps `i_block`, and its length.
pub(super) const I_BLOCK: usize = 0x28;
const I_BLOCK_LEN: usize = 60;
/// The extent header's magic number.
const MAGIC: u16 = 0xF30A;
/// The header and each entry are 12 bytes.
const ENTRY: usize = 12;
/// Lengths above this mark an unwritten extent; this is also the longest written one.
const MAX_LEN: u32 = 32768;
/// The format's deepest tree.
const MAX_DEPTH: u16 = 5;
/// The root's entries, in `i_block`.
const ROOT_ENTRIES: u16 = 4;

/// One extent: `len` blocks of the file from block `logical`, at volume block `physical`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Extent {
    pub(super) logical: u32,
    pub(super) len: u32,
    pub(super) physical: u64,
    /// Allocated but not written: it reads as zeros.
    pub(super) unwritten: bool,
}

impl Extent {
    /// The block of the file after the extent.
    pub(super) fn end(&self) -> u64 {
        u64::from(self.logical) + u64::from(self.len)
    }
}

/// Where a node is: the inode's `i_block`, or a block of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Node {
    Root,
    Block(u64),
}

/// A node's header fields.
#[derive(Clone, Copy, Debug)]
struct Header {
    entries: u16,
    max: u16,
    depth: u16,
}

/// What the tree functions need of the inode.
struct Owner<'i> {
    number: u32,
    inode: &'i mut [u8],
}

/// Writes the empty root of a new file's tree into `inode`'s `i_block`.
pub(super) fn init_root(inode: &mut [u8]) {
    let root = &mut inode[I_BLOCK..I_BLOCK + I_BLOCK_LEN];
    root.fill(0);
    put_header(
        root,
        Header {
            entries: 0,
            max: ROOT_ENTRIES,
            depth: 0,
        },
    );
}

/// Every extent of inode `number` (`inode`), in logical order.
///
/// # Errors
///
/// [`WriteError::BadExtentTree`] for a malformed tree, [`WriteError::BlockChecksum`] for a
/// node block that fails its checksum, and [`WriteError::Device`].
pub(super) fn leaves<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    number: u32,
    inode: &[u8],
) -> Result<Vec<Extent>, WriteError> {
    let mut out = Vec::new();
    let root = inode[I_BLOCK..I_BLOCK + I_BLOCK_LEN].to_vec();
    collect(txn, number, inode, &root, MAX_DEPTH, &mut out)?;
    out.sort_by_key(|extent| extent.logical);
    if out
        .windows(2)
        .any(|pair| pair[0].end() > u64::from(pair[1].logical))
    {
        return Err(WriteError::BadExtentTree { number });
    }
    Ok(out)
}

fn collect<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    number: u32,
    inode: &[u8],
    node: &[u8],
    depth_left: u16,
    out: &mut Vec<Extent>,
) -> Result<(), WriteError> {
    let header = header(node, number)?;
    if header.depth > depth_left {
        return Err(WriteError::BadExtentTree { number });
    }
    for at in 0..usize::from(header.entries) {
        if header.depth == 0 {
            out.push(extent(node, at));
        } else {
            let child = child(node, at);
            let block = read_node(txn, number, inode, child)?;
            collect(txn, number, inode, &block, header.depth - 1, out)?;
        }
    }
    Ok(())
}

/// Maps block `logical` of inode `number` (`inode`, changed in place) to volume block
/// `physical`, which the caller has allocated: the extent before it is lengthened if
/// `physical` continues it, and otherwise a new extent is inserted into the leaf that covers
/// `logical`.
///
/// # Errors
///
/// [`WriteError::ExtentTreeFull`] when a new extent is needed and its leaf is full,
/// [`WriteError::BadExtentTree`] for a malformed tree or a block already mapped, and those
/// of [`leaves`].
pub(super) fn map<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    number: u32,
    inode: &mut [u8],
    logical: u32,
    physical: u64,
) -> Result<(), WriteError> {
    let mut owner = Owner { number, inode };
    // The path from the root to the leaf: each index node and the entry taken in it.
    let mut path = Vec::new();
    let mut at = Node::Root;
    let mut node = load(txn, &owner, at)?;
    let mut depth_left = MAX_DEPTH;
    loop {
        let head = header(&node, number)?;
        if head.depth > depth_left {
            return Err(WriteError::BadExtentTree { number });
        }
        if head.depth == 0 {
            break;
        }
        if head.entries == 0 {
            return Err(WriteError::BadExtentTree { number });
        }
        let entries = usize::from(head.entries);
        // The last child starting at or before `logical`, or the first.
        let taken = (0..entries)
            .rev()
            .find(|&i| index(&node, i).block <= logical)
            .unwrap_or(0);
        path.push((at, taken));
        at = Node::Block(child(&node, taken));
        node = load(txn, &owner, at)?;
        depth_left = head.depth - 1;
    }
    let first = insert(&mut node, number, logical, physical)?;
    store(txn, &mut owner, at, &node)?;
    if first {
        // The leaf now starts earlier: so must the index entries leading to it.
        for &(index, taken) in path.iter().rev() {
            let mut node = load(txn, &owner, index)?;
            let entry = self::index(&node, taken);
            if entry.block <= logical {
                break;
            }
            put_index(
                &mut node,
                taken,
                ExtentIndex {
                    block: logical,
                    ..entry
                },
            );
            store(txn, &mut owner, index, &node)?;
        }
    }
    Ok(())
}

/// Adds `logical` at `physical` to the leaf `node`; whether it became the leaf's first
/// entry.
fn insert(node: &mut [u8], number: u32, logical: u32, physical: u64) -> Result<bool, WriteError> {
    let head = header(node, number)?;
    let entries = usize::from(head.entries);
    let pos = (0..entries)
        .find(|&i| extent(node, i).logical > logical)
        .unwrap_or(entries);
    if pos > 0 {
        let before = extent(node, pos - 1);
        if before.end() > u64::from(logical) {
            return Err(WriteError::BadExtentTree { number });
        }
        let continues = !before.unwritten
            && before.end() == u64::from(logical)
            && before.physical + u64::from(before.len) == physical
            && before.len < MAX_LEN;
        if continues {
            put_extent(
                node,
                pos - 1,
                Extent {
                    len: before.len + 1,
                    ..before
                },
            );
            return Ok(false);
        }
    }
    if head.entries >= head.max {
        return Err(WriteError::ExtentTreeFull { number });
    }
    let from = ENTRY + pos * ENTRY;
    let to = ENTRY + entries * ENTRY;
    node.copy_within(from..to, from + ENTRY);
    put_extent(
        node,
        pos,
        Extent {
            logical,
            len: 1,
            physical,
            unwritten: false,
        },
    );
    put_header(
        node,
        Header {
            entries: head.entries + 1,
            ..head
        },
    );
    Ok(pos == 0)
}

/// Frees every block of inode `number` (`inode`, changed in place) from block `keep` on,
/// with the tree nodes left empty, and returns how many volume blocks were freed.
///
/// # Errors
///
/// Those of [`leaves`], and [`WriteError::BadBlock`] for a block that cannot be freed.
pub(super) fn truncate<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    number: u32,
    inode: &mut [u8],
    keep: u32,
) -> Result<u64, WriteError> {
    let mut owner = Owner { number, inode };
    let mut root = load(txn, &owner, Node::Root)?;
    let mut freed = 0;
    cut(txn, &owner, &mut root, keep, MAX_DEPTH, &mut freed)?;
    let head = header(&root, number)?;
    if head.depth > 0 && head.entries == 0 {
        root.fill(0);
        put_header(
            &mut root,
            Header {
                entries: 0,
                max: ROOT_ENTRIES,
                depth: 0,
            },
        );
    }
    store(txn, &mut owner, Node::Root, &root)?;
    Ok(freed)
}

/// Cuts `node` at block `keep`, freeing what lies past it; the node's children are stored,
/// `node` itself is left for the caller.
fn cut<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    owner: &Owner<'_>,
    node: &mut [u8],
    keep: u32,
    depth_left: u16,
    freed: &mut u64,
) -> Result<(), WriteError> {
    let number = owner.number;
    let head = header(node, number)?;
    if head.depth > depth_left {
        return Err(WriteError::BadExtentTree { number });
    }
    let entries = usize::from(head.entries);
    let mut kept = Vec::with_capacity(entries);
    for at in 0..entries {
        let raw: [u8; ENTRY] = node[ENTRY + at * ENTRY..ENTRY * (at + 2)]
            .try_into()
            .expect("an entry is 12 bytes");
        if head.depth == 0 {
            let extent = extent(node, at);
            if extent.logical >= keep {
                free_run(txn, extent.physical, extent.len, freed)?;
            } else if extent.end() > u64::from(keep) {
                let len = keep - extent.logical;
                free_run(
                    txn,
                    extent.physical + u64::from(len),
                    extent.len - len,
                    freed,
                )?;
                let mut short = [0; ENTRY];
                put_raw_extent(&mut short, Extent { len, ..extent });
                kept.push(short);
            } else {
                kept.push(raw);
            }
            continue;
        }
        let start = index(node, at).block;
        // Only the last child that starts before `keep` can reach past it.
        let last_before = at + 1 == entries || index(node, at + 1).block >= keep;
        if start < keep && !last_before {
            kept.push(raw);
            continue;
        }
        let block = child(node, at);
        let mut child_node = read_node(txn, number, owner.inode, block)?;
        let child_keep = if start < keep { keep } else { 0 };
        cut(
            txn,
            owner,
            &mut child_node,
            child_keep,
            head.depth - 1,
            freed,
        )?;
        if header(&child_node, number)?.entries == 0 {
            txn.free_block(block)?;
            *freed += 1;
        } else {
            write_node(txn, owner, block, &mut child_node)?;
            kept.push(raw);
        }
    }
    node[ENTRY..ENTRY + entries * ENTRY].fill(0);
    for (at, raw) in kept.iter().enumerate() {
        node[ENTRY + at * ENTRY..ENTRY * (at + 2)].copy_from_slice(raw);
    }
    put_header(
        node,
        Header {
            entries: kept.len() as u16,
            ..head
        },
    );
    Ok(())
}

fn free_run<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    physical: u64,
    len: u32,
    freed: &mut u64,
) -> Result<(), WriteError> {
    for block in physical..physical + u64::from(len) {
        txn.free_block(block)?;
        *freed += 1;
    }
    Ok(())
}

/// A node's bytes: the root's from the inode, a block's from the transaction (checked).
fn load<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    owner: &Owner<'_>,
    at: Node,
) -> Result<Vec<u8>, WriteError> {
    match at {
        Node::Root => Ok(owner.inode[I_BLOCK..I_BLOCK + I_BLOCK_LEN].to_vec()),
        Node::Block(block) => read_node(txn, owner.number, owner.inode, block),
    }
}

fn store<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    owner: &mut Owner<'_>,
    at: Node,
    node: &[u8],
) -> Result<(), WriteError> {
    match at {
        Node::Root => {
            owner.inode[I_BLOCK..I_BLOCK + I_BLOCK_LEN].copy_from_slice(node);
            Ok(())
        }
        Node::Block(block) => {
            let mut node = node.to_vec();
            write_node(txn, owner, block, &mut node)
        }
    }
}

/// Reads the node in `block`, checking its header and, with `metadata_csum`, its checksum.
fn read_node<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    number: u32,
    inode: &[u8],
    block: u64,
) -> Result<Vec<u8>, WriteError> {
    let seed = txn.checksum_seed();
    let node = txn.block(block)?.to_vec();
    let head = header(&node, number)?;
    let tail = ENTRY + usize::from(head.max) * ENTRY;
    if tail + 4 > node.len() {
        return Err(WriteError::BadExtentTree { number });
    }
    if let Some(seed) = seed {
        let computed = node_checksum(seed, number, inode, &node[..tail]);
        let stored = ExtentTail::read(&node[tail..]).checksum;
        if stored != computed {
            return Err(WriteError::BlockChecksum {
                block,
                stored,
                computed,
            });
        }
    }
    Ok(node)
}

/// Stores `node` in `block`, with its checksum.
fn write_node<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    owner: &Owner<'_>,
    block: u64,
    node: &mut [u8],
) -> Result<(), WriteError> {
    if let Some(seed) = txn.checksum_seed() {
        let head = header(node, owner.number)?;
        let tail = ENTRY + usize::from(head.max) * ENTRY;
        let checksum = node_checksum(seed, owner.number, owner.inode, &node[..tail]);
        ExtentTail { checksum }.write(&mut node[tail..]);
    }
    txn.block_mut(block)?.copy_from_slice(node);
    Ok(())
}

/// A node block's checksum: CRC32C from the seed over the inode number, `i_generation` and
/// the node up to its tail.
fn node_checksum(seed: u32, number: u32, inode: &[u8], node: &[u8]) -> u32 {
    ferric_jbd2::crc32c_seeded(
        seed,
        &[
            &number.to_le_bytes(),
            &inode[I_GENERATION..I_GENERATION + 4],
            node,
        ],
    )
}

/// The header of `node`, checked to describe entries that fit it.
fn header(node: &[u8], number: u32) -> Result<Header, WriteError> {
    if node.len() < ENTRY {
        return Err(WriteError::BadExtentTree { number });
    }
    let raw = ExtentHeader::read(node);
    let head = Header {
        entries: raw.entries,
        max: raw.max,
        depth: raw.depth,
    };
    let fits = raw.magic == MAGIC
        && head.entries <= head.max
        && ENTRY + usize::from(head.max) * ENTRY <= node.len();
    if !fits {
        return Err(WriteError::BadExtentTree { number });
    }
    Ok(head)
}

fn put_header(node: &mut [u8], head: Header) {
    // eh_generation is not used by ext4; a node's is kept as it was.
    let generation = ExtentHeader::read(node).generation;
    ExtentHeader {
        magic: MAGIC,
        entries: head.entries,
        max: head.max,
        depth: head.depth,
        generation,
    }
    .write(node);
}

/// The bytes of entry `at` of a node whose header said it has room for it.
fn entry(node: &[u8], at: usize) -> &[u8] {
    &node[ENTRY + at * ENTRY..ENTRY * (at + 2)]
}

fn entry_mut(node: &mut [u8], at: usize) -> &mut [u8] {
    &mut node[ENTRY + at * ENTRY..ENTRY * (at + 2)]
}

/// Leaf entry `at`.
fn extent(node: &[u8], at: usize) -> Extent {
    let raw = ExtentLeaf::read(entry(node, at));
    let stored = u32::from(raw.len);
    let (len, unwritten) = if stored > MAX_LEN {
        (stored - MAX_LEN, true)
    } else {
        (stored, false)
    };
    Extent {
        logical: raw.block,
        len,
        physical: u64::from(raw.start_hi) << 32 | u64::from(raw.start_lo),
        unwritten,
    }
}

fn put_extent(node: &mut [u8], at: usize, extent: Extent) {
    put_raw_extent(entry_mut(node, at), extent);
}

fn put_raw_extent(raw: &mut [u8], extent: Extent) {
    let len = if extent.unwritten {
        extent.len + MAX_LEN
    } else {
        extent.len
    };
    // The callers keep lengths to MAX_LEN and physical blocks to 48 bits.
    ExtentLeaf {
        block: extent.logical,
        len: len as u16,
        start_hi: (extent.physical >> 32) as u16,
        start_lo: extent.physical as u32,
    }
    .write(raw);
}

/// Index entry `at`.
fn index(node: &[u8], at: usize) -> ExtentIndex {
    ExtentIndex::read(entry(node, at))
}

fn put_index(node: &mut [u8], at: usize, index: ExtentIndex) {
    index.write(entry_mut(node, at));
}

/// The block index entry `at` names.
fn child(node: &[u8], at: usize) -> u64 {
    let raw = index(node, at);
    u64::from(raw.leaf_hi) << 32 | u64::from(raw.leaf_lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(extents: &[Extent], max: u16) -> Vec<u8> {
        let mut node = vec![0; I_BLOCK_LEN];
        put_header(
            &mut node,
            Header {
                entries: extents.len() as u16,
                max,
                depth: 0,
            },
        );
        for (at, &e) in extents.iter().enumerate() {
            put_extent(&mut node, at, e);
        }
        node
    }

    fn ext(logical: u32, len: u32, physical: u64) -> Extent {
        Extent {
            logical,
            len,
            physical,
            unwritten: false,
        }
    }

    fn all(node: &[u8]) -> Vec<Extent> {
        let head = header(node, 12).expect("a node");
        (0..usize::from(head.entries))
            .map(|at| extent(node, at))
            .collect()
    }

    #[test]
    fn a_block_that_continues_an_extent_lengthens_it() {
        let mut node = leaf(&[ext(0, 2, 100)], 4);
        assert!(!insert(&mut node, 12, 2, 102).expect("fits"));
        assert_eq!(all(&node), [ext(0, 3, 100)]);
    }

    #[test]
    fn other_blocks_get_extents_of_their_own_in_order() {
        let mut node = leaf(&[ext(4, 1, 200)], 4);
        // After it, but elsewhere on the volume; before it; and in a hole between.
        assert!(!insert(&mut node, 12, 5, 300).expect("fits"));
        assert!(insert(&mut node, 12, 0, 50).expect("fits"));
        assert!(!insert(&mut node, 12, 2, 60).expect("fits"));
        assert_eq!(
            all(&node),
            [ext(0, 1, 50), ext(2, 1, 60), ext(4, 1, 200), ext(5, 1, 300)]
        );
        assert!(matches!(
            insert(&mut node, 12, 7, 400),
            Err(WriteError::ExtentTreeFull { number: 12 })
        ));
        // A mapped block is not mapped again.
        assert!(matches!(
            insert(&mut node, 12, 4, 999),
            Err(WriteError::BadExtentTree { .. })
        ));
    }

    #[test]
    fn unwritten_and_full_extents_are_not_lengthened() {
        let unwritten = Extent {
            unwritten: true,
            ..ext(0, 2, 100)
        };
        let mut node = leaf(&[unwritten], 4);
        insert(&mut node, 12, 2, 102).expect("fits");
        assert_eq!(all(&node), [unwritten, ext(2, 1, 102)]);
        assert_eq!(
            ExtentLeaf::read(entry(&node, 0)).len,
            2 + 32768,
            "still unwritten"
        );
        let mut node = leaf(&[ext(0, MAX_LEN, 100)], 4);
        insert(&mut node, 12, MAX_LEN, 100 + u64::from(MAX_LEN)).expect("fits");
        assert_eq!(all(&node).len(), 2);
    }

    #[test]
    fn high_physical_blocks_keep_their_upper_16_bits() {
        let far = ext(0, 1, 0x1234_5678_9ABC);
        let node = leaf(&[far], 4);
        assert_eq!(all(&node), [far]);
    }

    #[test]
    fn malformed_headers_are_refused() {
        let mut node = leaf(&[], 4);
        node[0] = 0;
        assert!(matches!(
            header(&node, 7),
            Err(WriteError::BadExtentTree { number: 7 })
        ));
        let mut node = leaf(&[], 4);
        let raw = ExtentHeader::read(&node);
        ExtentHeader { max: 5, ..raw }.write(&mut node);
        assert!(header(&node, 7).is_err(), "five entries do not fit i_block");
    }
}
