//! Recovery on journals written by e2fsprogs: the journal inodes of three of ext4d's test
//! volumes (`userspace/ext4d/tests/fixtures/make-fixtures.sh` writes them and lists their
//! transactions). ext4d's own tests replay the whole volumes and compare them with
//! `e2fsck`'s replay. The last test writes a transaction of its own over what e2fsprogs
//! left in each log.

use std::convert::Infallible;
use std::io::Read;
use std::path::Path;

use ferric_jbd2::{
    ChecksumKind, CommitTime, JournalRead, JournalWrite, MAGIC, Recovery, Superblock, Writer,
};

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

impl JournalWrite for Journal {
    fn write_block(&mut self, block: u32, buf: &[u8]) -> Result<(), Infallible> {
        let start = block as usize * self.block_size;
        self.bytes[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn write_home(&mut self, _target: u64, _buf: &[u8]) -> Result<(), Infallible> {
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}

/// After e2fsprogs' transactions are replayed and the log emptied, a transaction written
/// over them is all a scan finds: what is left of theirs after its commit block has older
/// sequence numbers.
#[test]
fn a_transaction_written_over_an_emptied_log_is_all_that_replays() {
    for name in ["journal-v3", "journal-v2", "journal-ext3"] {
        let (mut journal, sb) = load(name);
        let recovery = Recovery::scan(&journal, &sb).expect("valid");
        let mut raw = journal.bytes[..journal.block_size].to_vec();
        sb.mark_recovered(&mut raw, recovery.next_sequence())
            .expect("a superblock");
        journal.bytes[..raw.len()].copy_from_slice(&raw);
        let mut writer = Writer::new(&raw).expect("an empty journal");
        let mut transaction = writer.transaction();
        let data = vec![0x5A; journal.block_size];
        transaction.log(77, &data).expect("a block");
        let committed = writer
            .commit(&mut journal, transaction, CommitTime::default())
            .expect("committed");
        assert_eq!(committed.sequence(), 8, "{name}");
        let sb = Superblock::parse(&journal.bytes).expect("valid");
        let recovery = Recovery::scan(&journal, &sb).expect("valid");
        assert_eq!(recovery.transactions(), 1, "{name}");
        let targets: Vec<u64> = recovery.writes().iter().map(|w| w.target).collect();
        assert_eq!(targets, [77], "{name}");
        let mut buf = vec![0; journal.block_size];
        recovery
            .read(&journal, &sb, &recovery.writes()[0], &mut buf)
            .expect("intact");
        assert_eq!(buf, data, "{name}");
        writer
            .checkpoint(&mut journal, committed)
            .expect("checkpointed");
        let sb = Superblock::parse(&journal.bytes).expect("valid");
        assert_eq!((sb.needs_recovery(), sb.sequence()), (false, 9), "{name}");
    }
}
