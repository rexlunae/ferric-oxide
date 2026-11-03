use syscall::flag::{MapFlags, MAP_PRIVATE, MAP_SHARED, PROT_READ, PROT_WRITE};
use syscall::PAGE_SIZE;

use super::{release_unused, sys, FileMmapInfo, Fmaps, Unmapped};
use crate::{DiskMemory, FileSystem, Node, TreePtr};

const PAGE: u64 = PAGE_SIZE as u64;
const READ_ONLY: MapFlags = PROT_READ.union(MAP_PRIVATE);
const PRIVATE_RW: MapFlags = PROT_READ.union(PROT_WRITE).union(MAP_PRIVATE);
const SHARED_RW: MapFlags = PROT_READ.union(PROT_WRITE).union(MAP_SHARED);

type Fs = FileSystem<DiskMemory>;

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn filesystem() -> Fs {
    FileSystem::create(DiskMemory::new(64 * 1024 * 1024), None, 0, 0).unwrap()
}

fn create_file(fs: &mut Fs, name: &str, contents: &[u8]) -> TreePtr<Node> {
    let node = fs
        .tx(|tx| tx.create_node(TreePtr::root(), name, Node::MODE_FILE | 0o644, 0, 0))
        .unwrap();
    fs.tx(|tx| tx.write_node(node.ptr(), 0, contents, 0, 0))
        .unwrap();
    node.ptr()
}

fn file_contents(fs: &mut Fs, node: TreePtr<Node>) -> Vec<u8> {
    let size = fs.tx(|tx| Ok(tx.read_tree(node)?.data().size())).unwrap();
    let mut buf = vec![0; size as usize];
    let read = fs.tx(|tx| tx.read_node(node, 0, &mut buf, 0, 0)).unwrap();
    assert_eq!(read, buf.len());
    buf
}

/// An open description of `node`, as the scheme's `open` records it.
fn open(fmaps: &mut Fmaps, node: TreePtr<Node>) {
    let info = fmaps.entry(node.id()).or_default();
    if !info.in_use() {
        info.invalidate();
    }
    info.open_fds += 1;
}

/// The close of a description, as the scheme's `on_close` does it.
fn close(fmaps: &mut Fmaps, node: TreePtr<Node>) -> bool {
    let info = fmaps.get_mut(&node.id()).unwrap();
    info.open_fds -= 1;
    release_unused(fmaps, node.id())
}

fn map(
    fs: &mut Fs,
    fmaps: &mut Fmaps,
    node: TreePtr<Node>,
    flags: MapFlags,
    offset: u64,
    size: usize,
) -> *mut u8 {
    let info = fmaps.get_mut(&node.id()).unwrap();
    fs.tx(|tx| info.map(node, flags, size, offset, tx)).unwrap()
}

/// A `funmap`, followed by the release the scheme does when it dropped a mapping.
fn unmap(
    fs: &mut Fs,
    fmaps: &mut Fmaps,
    node: TreePtr<Node>,
    offset: u64,
    size: usize,
    writes_back: bool,
) -> bool {
    let info = fmaps.get_mut(&node.id()).unwrap();
    let Unmapped {
        dropped,
        written_back,
    } = fs
        .tx(|tx| info.unmap(node, offset, size, writes_back, tx))
        .unwrap();
    written_back.unwrap();
    if dropped {
        release_unused(fmaps, node.id());
    }
    dropped
}

fn mapped<'a>(addr: *mut u8, len: usize) -> &'a mut [u8] {
    // SAFETY: the tests only touch ranges they mapped, while the buffer is alive.
    unsafe { core::slice::from_raw_parts_mut(addr, len) }
}

#[test]
fn last_unmap_after_close_returns_the_buffer() {
    let mut fs = filesystem();
    let contents = pattern(10_000, 1);
    let node = create_file(&mut fs, "lib.so", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    let addr = map(&mut fs, &mut fmaps, node, READ_ONLY, 0, contents.len());
    let pages = mapped(addr, 3 * PAGE_SIZE);
    assert_eq!(&pages[..contents.len()], &contents[..]);
    assert!(pages[contents.len()..].iter().all(|&byte| byte == 0));
    assert_eq!(sys::held(), 3 * PAGE_SIZE);

    // ld.so closes the descriptor and keeps the mapping.
    assert!(!close(&mut fmaps, node));
    assert_eq!(sys::held(), 3 * PAGE_SIZE);

    assert!(unmap(&mut fs, &mut fmaps, node, 0, contents.len(), false));
    assert!(fmaps.is_empty());
    assert_eq!(sys::held(), 0);
}

#[test]
fn last_close_after_unmap_returns_the_buffer() {
    let mut fs = filesystem();
    let contents = pattern(2 * PAGE_SIZE, 2);
    let node = create_file(&mut fs, "data", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, contents.len());
    assert!(unmap(&mut fs, &mut fmaps, node, 0, contents.len(), false));
    // Kept for the next mapping while the description is open.
    assert_eq!(fmaps[&node.id()].buffer_len(), 2 * PAGE_SIZE);

    assert!(close(&mut fmaps, node));
    assert!(fmaps.is_empty());
    assert_eq!(sys::held(), 0);
}

#[test]
fn mapping_again_while_open_reuses_current_ranges() {
    let mut fs = filesystem();
    let contents = pattern(PAGE_SIZE, 3);
    let node = create_file(&mut fs, "data", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    let addr = map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    assert_eq!(mapped(addr, PAGE_SIZE), &contents[..]);
    assert_eq!(fmaps[&node.id()].mappings(), 1);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert!(close(&mut fmaps, node));
    assert_eq!(sys::held(), 0);
}

#[test]
fn mapping_after_a_write_reads_the_file_again() {
    let mut fs = filesystem();
    let node = create_file(&mut fs, "data", &pattern(PAGE_SIZE, 4));
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));

    let new = pattern(PAGE_SIZE, 5);
    let info = fmaps.get_mut(&node.id()).unwrap();
    info.begin_write();
    fs.tx(|tx| tx.write_node(node, 0, &new, 0, 0)).unwrap();
    info.write_through(0, &new);

    let addr = map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    assert_eq!(mapped(addr, PAGE_SIZE), &new[..]);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert!(close(&mut fmaps, node));
}

#[test]
fn shared_writable_stores_are_written_back_up_to_the_file_size() {
    let mut fs = filesystem();
    let mut contents = pattern(6000, 6);
    let node = create_file(&mut fs, "data", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    let addr = map(&mut fs, &mut fmaps, node, SHARED_RW, 0, 2 * PAGE_SIZE);
    assert_eq!(fmaps[&node.id()].writable_mappings(), 1);
    let pages = mapped(addr, 2 * PAGE_SIZE);
    pages[0] = 0xaa;
    pages[5999] = 0xbb;
    // Past the end of the file: not written back, and the file does not grow.
    pages[7000] = 0xcc;
    assert!(!close(&mut fmaps, node));

    assert!(unmap(&mut fs, &mut fmaps, node, 0, 2 * PAGE_SIZE, true));
    assert!(fmaps.is_empty());
    assert_eq!(sys::held(), 0);

    contents[0] = 0xaa;
    contents[5999] = 0xbb;
    assert_eq!(file_contents(&mut fs, node), contents);
}

#[test]
fn private_stores_are_not_written_back() {
    let mut fs = filesystem();
    let contents = pattern(PAGE_SIZE, 7);
    let node = create_file(&mut fs, "data", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    let addr = map(&mut fs, &mut fmaps, node, PRIVATE_RW, 0, PAGE_SIZE);
    assert_eq!(fmaps[&node.id()].writable_mappings(), 0);
    // The kernel copies a private page before a store; this stands for a store it let
    // through, which must not reach the file.
    mapped(addr, PAGE_SIZE)[0] = !contents[0];
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert!(close(&mut fmaps, node));
    assert_eq!(file_contents(&mut fs, node), contents);
}

#[test]
fn several_mappings_keep_the_buffer_until_the_last_goes() {
    let mut fs = filesystem();
    let mut contents = pattern(2 * PAGE_SIZE, 8);
    let node = create_file(&mut fs, "data", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    let shared = map(&mut fs, &mut fmaps, node, SHARED_RW, 0, 2 * PAGE_SIZE);
    let private = map(&mut fs, &mut fmaps, node, READ_ONLY, 0, 2 * PAGE_SIZE);
    assert_eq!(shared, private);
    assert!(!close(&mut fmaps, node));
    assert_eq!(fmaps[&node.id()].mappings(), 2);
    assert_eq!(fmaps[&node.id()].writable_mappings(), 1);

    mapped(shared, 2 * PAGE_SIZE)[10] = 0x11;
    assert!(unmap(&mut fs, &mut fmaps, node, 0, 2 * PAGE_SIZE, false));
    // The shared mapping still uses the range, so its store reached the file, and the
    // buffer stays.
    contents[10] = 0x11;
    assert_eq!(file_contents(&mut fs, node), contents);
    assert_eq!(fmaps[&node.id()].mappings(), 1);
    assert_eq!(fmaps[&node.id()].writable_mappings(), 1);
    assert_eq!(sys::held(), 2 * PAGE_SIZE);

    mapped(shared, 2 * PAGE_SIZE)[PAGE_SIZE] = 0x22;
    assert!(unmap(&mut fs, &mut fmaps, node, 0, 2 * PAGE_SIZE, true));
    contents[PAGE_SIZE] = 0x22;
    assert_eq!(file_contents(&mut fs, node), contents);
    assert!(fmaps.is_empty());
    assert_eq!(sys::held(), 0);
}

#[test]
fn partial_unmaps_keep_the_buffer_until_every_page_goes() {
    let mut fs = filesystem();
    let node = create_file(&mut fs, "data", &pattern(3 * PAGE_SIZE, 9));
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, 3 * PAGE_SIZE);
    assert!(!close(&mut fmaps, node));
    assert!(unmap(&mut fs, &mut fmaps, node, PAGE, PAGE_SIZE, false));
    assert_eq!(fmaps[&node.id()].mappings(), 2);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert!(!fmaps.is_empty());
    assert!(unmap(&mut fs, &mut fmaps, node, 2 * PAGE, PAGE_SIZE, false));
    assert!(fmaps.is_empty());
    assert_eq!(sys::held(), 0);
}

#[test]
fn one_unmap_drops_every_range_it_covers() {
    let mut fs = filesystem();
    let mut contents = pattern(3 * PAGE_SIZE, 10);
    let node = create_file(&mut fs, "data", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    // Different flags keep these as separate ranges in the tree.
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    let addr = map(&mut fs, &mut fmaps, node, SHARED_RW, PAGE, 2 * PAGE_SIZE);
    mapped(addr, 2 * PAGE_SIZE)[PAGE_SIZE] = 0x33;
    assert!(!close(&mut fmaps, node));

    assert!(unmap(&mut fs, &mut fmaps, node, 0, 3 * PAGE_SIZE, true));
    assert!(fmaps.is_empty());
    assert_eq!(sys::held(), 0);
    contents[2 * PAGE_SIZE] = 0x33;
    assert_eq!(file_contents(&mut fs, node), contents);
}

#[test]
fn unmapping_what_is_not_mapped_is_ignored() {
    let mut fs = filesystem();
    let node = create_file(&mut fs, "data", &pattern(PAGE_SIZE, 11));
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    assert!(!unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert!(!unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert_eq!(fmaps[&node.id()].mappings(), 0);
    assert!(close(&mut fmaps, node));
}

#[test]
fn reads_use_current_ranges_up_to_the_file_size() {
    let mut fs = filesystem();
    let contents = pattern(5000, 12);
    let node = create_file(&mut fs, "data", &contents);
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    let addr = map(&mut fs, &mut fmaps, node, SHARED_RW, 0, 2 * PAGE_SIZE);
    // A store through the shared mapping is seen by reads before any write-back.
    mapped(addr, 2 * PAGE_SIZE)[4999] = 0x44;
    let info = &fmaps[&node.id()];
    let mut buf = vec![0; 2 * PAGE_SIZE];
    let read = fs
        .tx(|tx| info.read_cached(node, &mut buf, 4000, tx))
        .unwrap();
    assert_eq!(read, Some(1000));
    assert_eq!(&buf[..999], &contents[4000..4999]);
    assert_eq!(buf[999], 0x44);
    let read = fs
        .tx(|tx| info.read_cached(node, &mut buf, 5000, tx))
        .unwrap();
    assert_eq!(read, Some(0));

    let info = &fmaps[&node.id()];
    fs.tx(|tx| info.sync(node, tx)).unwrap();
    assert_eq!(file_contents(&mut fs, node)[4999], 0x44);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, 2 * PAGE_SIZE, true));
    assert!(close(&mut fmaps, node));
}

#[test]
fn reads_past_mapped_ranges_go_to_the_file() {
    let mut fs = filesystem();
    let node = create_file(&mut fs, "data", &pattern(3 * PAGE_SIZE, 13));
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    let info = &fmaps[&node.id()];
    let mut buf = vec![0; 2 * PAGE_SIZE];
    let read = fs.tx(|tx| info.read_cached(node, &mut buf, 0, tx)).unwrap();
    assert_eq!(read, None);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert!(close(&mut fmaps, node));
}

#[test]
fn writes_reach_live_mappings() {
    let mut fs = filesystem();
    let node = create_file(&mut fs, "data", &pattern(PAGE_SIZE, 14));
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    let addr = map(&mut fs, &mut fmaps, node, SHARED_RW, 0, PAGE_SIZE);
    let info = fmaps.get_mut(&node.id()).unwrap();
    info.begin_write();
    fs.tx(|tx| tx.write_node(node, 100, b"written", 0, 0))
        .unwrap();
    info.write_through(100, b"written");
    assert_eq!(&mapped(addr, PAGE_SIZE)[100..107], b"written");
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, true));
    assert!(close(&mut fmaps, node));
    assert_eq!(&file_contents(&mut fs, node)[100..107], b"written");
}

#[test]
fn mapping_many_files_returns_to_no_buffers() {
    let mut fs = filesystem();
    let nodes: Vec<_> = (0..16)
        .map(|i| create_file(&mut fs, &format!("lib{i}.so"), &pattern(4 * PAGE_SIZE, i)))
        .collect();
    let mut fmaps = Fmaps::new();

    for _ in 0..4 {
        for &node in &nodes {
            open(&mut fmaps, node);
            map(&mut fs, &mut fmaps, node, READ_ONLY, 0, 4 * PAGE_SIZE);
            assert!(!close(&mut fmaps, node));
        }
        assert_eq!(sys::held(), nodes.len() * 4 * PAGE_SIZE);
        for &node in &nodes {
            assert!(unmap(&mut fs, &mut fmaps, node, 0, 4 * PAGE_SIZE, false));
        }
        assert!(fmaps.is_empty());
        assert_eq!(sys::held(), 0);
    }
}

#[test]
fn a_failed_map_leaves_the_mappings_alone() {
    let mut fs = filesystem();
    let node = create_file(&mut fs, "data", &pattern(PAGE_SIZE, 15));
    let mut fmaps = Fmaps::new();

    open(&mut fmaps, node);
    map(&mut fs, &mut fmaps, node, READ_ONLY, 0, PAGE_SIZE);
    let info = fmaps.get_mut(&node.id()).unwrap();
    let overflow = fs.tx(|tx| info.map(node, READ_ONLY, PAGE_SIZE, u64::MAX - 1, tx));
    assert!(overflow.is_err());
    assert_eq!(fmaps[&node.id()].mappings(), 1);
    assert!(unmap(&mut fs, &mut fmaps, node, 0, PAGE_SIZE, false));
    assert!(close(&mut fmaps, node));
    assert_eq!(sys::held(), 0);
}

#[test]
fn dropping_an_entry_returns_its_buffer() {
    let mut fs = filesystem();
    let node = create_file(&mut fs, "data", &pattern(PAGE_SIZE, 16));
    let mut info = FileMmapInfo::new();
    info.open_fds = 1;
    fs.tx(|tx| info.map(node, READ_ONLY, PAGE_SIZE, 0, tx))
        .unwrap();
    let unmapped = fs
        .tx(|tx| info.unmap(node, 0, PAGE_SIZE, false, tx))
        .unwrap();
    assert!(unmapped.dropped);
    info.open_fds = 0;
    assert_eq!(sys::held(), PAGE_SIZE);
    drop(info);
    assert_eq!(sys::held(), 0);
}
