//! Recovery on journals written by e2fsprogs: the journal inodes of three of ext4d's test
//! volumes (`userspace/ext4d/tests/fixtures/make-fixtures.sh` writes them and lists their
//! transactions). ext4d's own tests replay the whole volumes and compare them with
//! `e2fsck`'s replay.

use std::convert::Infallible;
use std::io::Read;
use std::path::Path;

use ferric_jbd2::{ChecksumKind, JournalRead, MAGIC, Recovery, Superblock};

/// A journal inode's bytes.
struct Journal {
    bytes: Vec<u8>,
    block_size: usize,
}

impl JournalRead for Journal {
    type Error = Infallible;

    fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), Infallible> {
        let start = block as usize * self.block_size;
        buf.copy_from_slice(&self.bytes[start..start + buf.len()]);
        Ok(())
    }
}

fn load(name: &str) -> (Journal, Superblock) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.jbd2.gz"));
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).expect("the fixture"))
        .read_to_end(&mut bytes)
        .expect("a gzip file");
    let sb = Superblock::parse(&bytes).expect("a journal superblock");
    let journal = Journal {
        bytes,
        block_size: sb.block_size() as usize,
    };
    (journal, sb)
}

/// What the script's transactions leave, in the order the volume's blocks come (hello.txt's
/// block is below the free blocks it picked): `(first bytes, transaction)`.
const EXPECTED: [(&[u8], u32); 4] = [
    (b"journaled hello\n", 1),
    (&[0x33; 16], 4),
    (&[0x22; 16], 2),
    (b"\xc0\x3b\x39\x98escaped!esca", 6),
];

fn check(name: &str, checksum: ChecksumKind, wide: bool) {
    let (journal, sb) = load(name);
    assert!(sb.needs_recovery(), "{name}");
    assert_eq!((sb.checksum(), sb.is_64bit()), (checksum, wide), "{name}");
    let recovery = Recovery::scan(&journal, &sb).unwrap_or_else(|err| panic!("{name}: {err}"));
    // Transaction 7 is not committed.
    assert_eq!(recovery.transactions(), 6, "{name}");
    assert_eq!(recovery.next_sequence(), 8, "{name}");
    let mut buf = vec![0; sb.block_size() as usize];
    let found: Vec<(Vec<u8>, u32)> = recovery
        .writes()
        .iter()
        .map(|write| {
            recovery
                .read(&journal, &sb, write, &mut buf)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            (buf[..16].to_vec(), write.sequence)
        })
        .collect();
    let expected: Vec<(Vec<u8>, u32)> = EXPECTED
        .iter()
        .map(|&(bytes, sequence)| (bytes.to_vec(), sequence))
        .collect();
    assert_eq!(found, expected, "{name}");
    let targets: Vec<u64> = recovery.writes().iter().map(|w| w.target).collect();
    // F2, F3 and F4 are consecutive free blocks after F1, which was revoked.
    assert_eq!(targets[2] - targets[1], 1, "{name}");
    assert_eq!(targets[3] - targets[2], 1, "{name}");
    assert_eq!(MAGIC.to_be_bytes(), [0xc0, 0x3b, 0x39, 0x98]);
}

#[test]
fn version_3_checksums_and_64_bit_tags() {
    check("journal-v3", ChecksumKind::V3, true);
}

#[test]
fn version_2_checksums_and_32_bit_tags() {
    check("journal-v2", ChecksumKind::V2, false);
}

#[test]
fn no_checksums_and_32_bit_tags() {
    check("journal-ext3", ChecksumKind::None, false);
}

#[test]
fn emptying_the_log_gives_a_clean_journal() {
    let (journal, sb) = load("journal-v3");
    let recovery = Recovery::scan(&journal, &sb).expect("valid");
    let mut raw = journal.bytes[..1024].to_vec();
    sb.mark_recovered(&mut raw, recovery.next_sequence())
        .expect("a superblock");
    let after = Superblock::parse(&raw).expect("still valid");
    assert!(!after.needs_recovery());
    assert_eq!(after.sequence(), 8);
}
