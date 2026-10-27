//! The times `fstat` reports on `journal-wrap-inode`, a volume that needs recovery and
//! whose one committed transaction, in a log that wraps round past the journal's end, holds
//! `hello.txt`'s inode with new times (`tests/fixtures/make-fixtures.sh`).
//!
//! `ext4-view` 1.0.0 does not follow a wrapped log, so it serves the inode as it was before
//! the transaction, while ext4d's own inode reads go through the committed journal. All
//! three times must come from one of those copies, never some from each.

mod common;

use common::load;
use ext4d::device::Window;
use ext4d::fs::{Fs, OpenMode};

type Time = (i64, u32);

fn times(image: Vec<u8>) -> (Time, Time, Time) {
    let mut fs = Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume");
    let root = fs.root().expect("the root");
    let file = fs
        .open_as(Some(root), "hello.txt", OpenMode::read(), 0, 0)
        .expect("hello.txt");
    let a = fs.attributes(file).expect("open");
    (a.accessed, a.modified, a.changed)
}

#[test]
fn a_wrapped_journal_does_not_mix_two_versions_of_an_inode() {
    let journaled = ((0x7000_0000, 0), (0x7000_0001, 0), (0x7000_0002, 0));
    assert_eq!(
        times(load("journal-wrap-inode.replayed.ext4.gz")),
        journaled,
        "after e2fsck's replay"
    );
    assert_eq!(
        times(load("journal-wrap-inode.ext4.gz")),
        journaled,
        "before any replay, read through the journal"
    );
}
