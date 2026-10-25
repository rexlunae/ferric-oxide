//! Journal replay (`journal-replay`) on volumes that need recovery, against e2fsprogs'
//! own replay (`tests/fixtures/make-fixtures.sh` describes the volumes and transactions).
//!
//! Each `NAME.ext4.gz` is replayed by ext4d and compared, block by block, with
//! `NAME.replayed.ext4.gz`, the same volume after `e2fsck -E journal_only`. The committed
//! images are used unless `EXT4D_FIXTURES` names a directory of fresh ones (CI makes them
//! with the script and runs these tests on them too).

#![cfg(feature = "journal-replay")]

use std::io::{self, Read};
use std::path::PathBuf;

use ext4d::device::{ReadAt, Window, WriteAt};
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

fn load(file: &str) -> Vec<u8> {
    let dir = std::env::var_os("EXT4D_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        PathBuf::from,
    );
    let gz = std::fs::File::open(dir.join(file)).expect("the fixture image");
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut bytes)
        .expect("a gzip file");
    bytes
}

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

/// A volume that stops taking writes after a number of them, as a crash would.
struct Crashing {
    bytes: Vec<u8>,
    writes_left: usize,
}

impl ReadAt for Crashing {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.bytes.read_exact_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        self.bytes.size()
    }
}

impl WriteAt for Crashing {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.writes_left = self
            .writes_left
            .checked_sub(1)
            .ok_or(io::ErrorKind::BrokenPipe)?;
        self.bytes.write_all_at(buf, offset)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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

#[test]
fn an_external_journal_is_refused() {
    let mut image = load("journal-v3.ext4.gz");
    // Clear s_journal_inum: the journal would then be the device s_journal_dev names.
    image[1024 + 0xE0..1024 + 0xE4].fill(0);
    let (outcome, after) = replay(image.clone());
    assert!(matches!(outcome, Err(JournalError::ExternalJournal)));
    assert_eq!(after, image);
}
