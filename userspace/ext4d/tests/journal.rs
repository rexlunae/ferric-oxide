//! Journal replay (`journal-replay`) on volumes that need recovery, against e2fsprogs'
//! own replay (`tests/fixtures/make-fixtures.sh` describes the volumes and transactions).
//!
//! Each `NAME.ext4.gz` is replayed by ext4d and compared, block by block, with
//! `NAME.replayed.ext4.gz`, the same volume after `e2fsck -E journal_only`. The committed
//! images are used unless `EXT4D_FIXTURES` names a directory of fresh ones (CI makes them
//! with the script and runs these tests on them too).

#![cfg(feature = "journal-replay")]

mod common;

use common::{Crashing, le32, load, reseal};
use ext4d::device::Window;
use ext4d::fs::{Fs, OpenMode};
use ext4d::journal::{self, JournalError, Outcome};
use ferric_jbd2::RecoveryError;

const NAMES: [&str; 6] = [
    "journal-v3",
    "journal-v2",
    "journal-v2-64",
    "journal-ext3",
    "journal-4k",
    "journal-wrap",
];

fn replay(bytes: Vec<u8>) -> (Result<Outcome, JournalError>, Vec<u8>) {
    let mut window = Window::whole(bytes).expect("its length");
    let outcome = journal::replay(&mut window);
    (outcome, window.into_inner())
}

/// The fields of the volume superblock (at byte 1024) that e2fsck sets and the replay does
/// not: `s_wtime` (0x30), `s_kbytes_written` (0x178) and, over those, `s_checksum` (0x3FC).
fn without_e2fsck_bookkeeping(mut image: Vec<u8>) -> Vec<u8> {
    for (at, len) in [(0x30, 4), (0x178, 8), (0x3FC, 4)] {
        image[1024 + at..1024 + at + len].fill(0);
    }
    image
}

fn assert_same_blocks(name: &str, ours: &[u8], theirs: &[u8]) {
    assert_eq!(ours.len(), theirs.len(), "{name}: image size");
    let ours = without_e2fsck_bookkeeping(ours.to_vec());
    let theirs = without_e2fsck_bookkeeping(theirs.to_vec());
    let differ: Vec<usize> = ours
        .chunks(1024)
        .zip(theirs.chunks(1024))
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();
    assert!(
        differ.is_empty(),
        "{name}: KiB blocks {differ:?} differ from e2fsck's replay"
    );
}

fn hello(image: Vec<u8>) -> Vec<u8> {
    let mut fs = Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume");
    let root = fs.root().expect("the root");
    let file = fs
        .open_as(Some(root), "hello.txt", OpenMode::read(), 0, 0)
        .expect("hello.txt");
    let mut buf = [0; 64];
    let n = fs.read(file, 0, &mut buf).expect("readable");
    buf[..n].to_vec()
}

#[test]
fn replays_like_e2fsck() {
    for name in NAMES {
        let (outcome, ours) = replay(load(&format!("{name}.ext4.gz")));
        let outcome = outcome.unwrap_or_else(|err| panic!("{name}: {err:?}"));
        // Six transactions commit; four blocks survive the revokes (hello.txt, F2, F3, F4).
        assert_eq!(
            outcome,
            Outcome::Replayed {
                transactions: 6,
                blocks: 4,
                journal_errno: 0
            },
            "{name}"
        );
        assert_same_blocks(name, &ours, &load(&format!("{name}.replayed.ext4.gz")));
    }
}

#[test]
fn the_replayed_volume_mounts_with_the_journaled_contents() {
    for name in NAMES {
        let (outcome, ours) = replay(load(&format!("{name}.ext4.gz")));
        outcome.expect("replayed");
        assert_eq!(hello(ours), b"journaled hello\n", "{name}");
    }
}

/// What ext4-view's in-memory view of the journal (all a read-only mount has) makes of
/// these volumes, as of ext4-view 1.0.0: it refuses a journal with an escaped block, which
/// every volume here logs, and does not follow a log that wraps round, so it serves
/// journal-wrap's hello.txt as it was before the journal. The replay serves both right.
#[test]
fn without_the_replay_the_reader_cannot_serve_these_volumes() {
    for name in NAMES {
        let image = load(&format!("{name}.ext4.gz"));
        if name == "journal-wrap" {
            assert_eq!(hello(image), b"hello from ext4\n", "{name}: stale");
            continue;
        }
        let result = Fs::new(Window::whole(image).expect("its length"));
        assert!(matches!(result, Err(ext4d::Error::Ext4(_))), "{name}");
    }
}

#[test]
fn a_replayed_volume_is_clean() {
    let (first, once) = replay(load("journal-v3.ext4.gz"));
    first.expect("replayed");
    let (second, twice) = replay(once.clone());
    assert_eq!(second.expect("readable"), Outcome::Clean);
    assert_eq!(once, twice, "a clean volume is not written");
    let (clean, _) = replay(load("journal-v3.replayed.ext4.gz"));
    assert_eq!(clean.expect("readable"), Outcome::Clean);
}

#[test]
fn an_interrupted_replay_can_be_run_again() {
    let expected = load("journal-v3.replayed.ext4.gz");
    // Four blocks, the journal superblock, the volume superblock: crash before each.
    for writes_left in 0..6 {
        let crashing = Crashing {
            bytes: load("journal-v3.ext4.gz"),
            writes_left,
        };
        let mut window = Window::whole(crashing).expect("its length");
        let err = journal::replay(&mut window).expect_err("the device stops");
        assert!(matches!(err, JournalError::Device(_)), "{err:?}");
        let (outcome, ours) = replay(window.into_inner().bytes);
        let outcome = outcome.expect("replays the second time");
        // After the journal superblock is written, only the flag is left to clear.
        let blocks = if writes_left == 5 { 0 } else { 4 };
        assert!(
            matches!(outcome, Outcome::Replayed { blocks: b, .. } if b == blocks),
            "{writes_left}: {outcome:?}"
        );
        assert_same_blocks("journal-v3, interrupted", &ours, &expected);
    }
}

#[test]
fn a_corrupt_journal_block_stops_the_replay_before_any_write() {
    let image = load("journal-v3.ext4.gz");
    let replayed = load("journal-v3.replayed.ext4.gz");
    // hello.txt's journal copy is the first block after the first descriptor; find it by
    // its contents and flip a bit in it.
    let at = image
        .windows(16)
        .position(|w| w == b"journaled hello\n")
        .expect("the journal copy of hello.txt");
    assert!(
        !replayed[..at]
            .windows(16)
            .any(|w| w == b"journaled hello\n")
    );
    let mut corrupt = image.clone();
    corrupt[at + 100] ^= 1;
    let (outcome, after) = replay(corrupt.clone());
    let err = outcome.expect_err("bad checksum");
    assert!(
        matches!(
            err,
            JournalError::Recovery(RecoveryError::DataChecksum { sequence: 1, .. })
        ),
        "{err:?}"
    );
    assert_eq!(after, corrupt, "nothing was written");
}

/// Where the journal inode's `i_block` is: the copy of the superblock's `s_jnl_blocks`
/// found after the superblock (in the inode table).
fn journal_i_block(image: &[u8]) -> usize {
    let backup = &image[1024 + 0x10C..1024 + 0x10C + 60];
    2048 + image[2048..]
        .windows(60)
        .position(|w| w == backup)
        .expect("the journal inode")
}

/// The first leaf entry of the journal's extent tree, 12 bytes into `i_block`: `ee_block`,
/// `ee_len`, `ee_start_hi`, `ee_start_lo`. In the fixture it maps journal blocks 0 and 1.
const EE_START_LO: usize = 12 + 8;

/// Moves the journal's first extent to volume block `start`, in the inode and, when
/// `backup` is set, in the superblock's copy too.
fn move_journal(image: &mut [u8], start: u32, backup: bool) {
    let at = journal_i_block(image) + EE_START_LO;
    image[at..at + 4].copy_from_slice(&start.to_le_bytes());
    if backup {
        let at = 1024 + 0x10C + EE_START_LO;
        image[at..at + 4].copy_from_slice(&start.to_le_bytes());
        reseal(image);
    }
}

fn refused(image: Vec<u8>) -> JournalError {
    let (outcome, after) = replay(image.clone());
    assert_eq!(after, image, "nothing was written");
    outcome.expect_err("refused")
}

#[test]
fn a_superblock_that_fails_its_checksum_is_refused() {
    let mut image = load("journal-v3.ext4.gz");
    image[1024 + 0x200] ^= 1;
    assert!(matches!(
        refused(image),
        JournalError::SuperblockChecksum { .. }
    ));
}

#[test]
fn a_journal_inode_unlike_the_superblock_copy_is_refused() {
    let mut image = load("journal-v3.ext4.gz");
    move_journal(&mut image, 2, false);
    assert!(matches!(
        refused(image),
        JournalError::JournalBackupMismatch
    ));
}

#[test]
fn a_journal_mapped_onto_metadata_is_refused() {
    // Block 2 is the group descriptor table; 300 is past it, inside the inode tables or
    // bitmaps of the 1 KiB-block fixture's flex group.
    for start in [2, 300] {
        let mut image = load("journal-v3.ext4.gz");
        move_journal(&mut image, start, true);
        let err = refused(image);
        assert!(
            matches!(err, JournalError::JournalOverlap { .. }),
            "{start}: {err:?}"
        );
    }
}

#[test]
fn a_journal_mapped_past_the_volume_is_refused() {
    let mut image = load("journal-v3.ext4.gz");
    let blocks = le32(&image, 1024 + 0x4);
    // The first extent is two blocks long, so it now ends past the last block.
    move_journal(&mut image, blocks - 1, true);
    let err = refused(image);
    assert!(
        matches!(err, JournalError::JournalOutsideVolume { .. }),
        "{err:?}"
    );
}

#[test]
fn a_replay_into_the_journal_itself_is_refused() {
    let mut image = load("journal-v3.ext4.gz");
    let at = journal_i_block(&image) + EE_START_LO;
    let journal = le32(&image, at) as usize;
    // Point the first tag of the first descriptor (journal block 1, in the first extent)
    // at the journal superblock's own block, and reseal the descriptor's tail checksum
    // (over the journal UUID and the block).
    let descriptor = (journal + 1) * 1024;
    let target = u32::try_from(journal).expect("small");
    image[descriptor + 12..descriptor + 16].copy_from_slice(&target.to_be_bytes());
    let uuid = image[journal * 1024 + 0x30..journal * 1024 + 0x40].to_vec();
    image[descriptor + 1020..descriptor + 1024].fill(0);
    let crc = ferric_jbd2::crc32c(&[&uuid, &image[descriptor..descriptor + 1024]]);
    image[descriptor + 1020..descriptor + 1024].copy_from_slice(&crc.to_be_bytes());
    let err = refused(image);
    assert!(
        matches!(err, JournalError::TargetInJournal { block } if block == u64::from(target)),
        "{err:?}"
    );
}

#[test]
fn an_external_journal_is_refused() {
    let mut image = load("journal-v3.ext4.gz");
    // Clear s_journal_inum: the journal would then be the device s_journal_dev names.
    image[1024 + 0xE0..1024 + 0xE4].fill(0);
    reseal(&mut image);
    let (outcome, after) = replay(image.clone());
    assert!(matches!(outcome, Err(JournalError::ExternalJournal)));
    assert_eq!(after, image);
}
