//! The daemon's command line over the block-layer images (crates/blocklayer/tests/fixtures/),
//! read through real files: what the scheme serves for each stack.

#![cfg(all(feature = "luks", feature = "lvm"))]

use std::path::PathBuf;

use ferric_blockd::Files;
use ferric_blockd::assemble::{AssembleError, assemble};
use ferric_blockd::device::FileDevice;
use ferric_blockd::spec::Spec;
use ferric_blockd::volume::Volume;
use ferric_blocklayer::{BlockDevice, Stack};

fn fixture(name: &str) -> String {
    format!(
        "{}/../../crates/blocklayer/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// A key file holding `passphrase`, unique to the test.
fn key_file(test: &str, passphrase: &[u8]) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}.key"));
    std::fs::write(&path, passphrase).expect("key file");
    path
}

fn pattern(seed: u64, start: u64, len: usize) -> Vec<u8> {
    (start..start + len as u64)
        .map(|i| ((i * 7 + i / 4096 + seed) % 251) as u8)
        .collect()
}

fn serve(args: &[String]) -> Volume<Stack<FileDevice>> {
    let spec = Spec::parse(args.iter().cloned()).expect("parses");
    Volume::new(assemble(&spec.steps, &mut Files).expect("assembles"))
}

/// Reads through a handle opened as root, as the scheme's `openat` makes one.
fn read_all(volume: &mut Volume<impl BlockDevice>, offset: u64, len: usize) -> Vec<u8> {
    let handle = volume.open("", syscall::flag::O_RDONLY, 0).expect("opens");
    let mut buf = vec![0; len];
    let n = volume.read(handle, offset, &mut buf).expect("reads");
    volume.close(handle);
    buf.truncate(n);
    buf
}

#[test]
fn lvm_on_luks() {
    let key = key_file("lvm_on_luks", b"correct horse");
    let mut volume = serve(&[
        "inner".into(),
        format!("dev:{}", fixture("lvm-on-luks.img")),
        format!("luks:{}", key.display()),
        "lvm:inner".into(),
    ]);
    assert_eq!(volume.size(), 64 * 1024);
    assert_eq!(read_all(&mut volume, 0, 8192), pattern(6, 0, 8192));
    // Reads stop at the end of the volume.
    assert_eq!(read_all(&mut volume, 64 * 1024 - 10, 100).len(), 10);
}

#[test]
fn luks_on_lvm_through_a_slice() {
    let key = key_file("luks_on_lvm", b"correct horse");
    let mut volume = serve(&[
        "sealed".into(),
        format!("dev:{}", fixture("lvm.img")),
        "slice:0:-".into(),
        "lvm:sealed".into(),
        format!("luks:{}", key.display()),
        "slice:4096:4096".into(),
    ]);
    assert_eq!(volume.size(), 4096);
    assert_eq!(read_all(&mut volume, 0, 4096), pattern(5, 4096, 4096));
}

#[test]
fn a_trailing_newline_is_part_of_the_passphrase() {
    let key = key_file("newline", b"correct horse\n");
    let spec = Spec::parse([
        "v".into(),
        format!("dev:{}", fixture("lvm-on-luks.img")),
        format!("luks:{}", key.display()),
    ])
    .expect("parses");
    let err = assemble(&spec.steps, &mut Files).map(|_| ()).unwrap_err();
    assert!(matches!(err, AssembleError::Luks { step: 2, .. }));
    let mut chain = vec![err.to_string()];
    let mut next = std::error::Error::source(&err);
    while let Some(e) = next {
        chain.push(e.to_string());
        next = e.source();
    }
    assert_eq!(
        chain,
        [
            "cannot open the LUKS volume of step 2",
            "the LUKS volume key cannot be recovered",
            "the passphrase does not open any LUKS keyslot",
        ]
    );
}

/// A caller that is not root gets no decrypted data: not by opening the volume, and not
/// through the scheme root handle.
#[test]
fn unprivileged_callers_read_nothing() {
    let key = key_file("unprivileged", b"correct horse");
    let mut volume = serve(&[
        "inner".into(),
        format!("dev:{}", fixture("lvm-on-luks.img")),
        format!("luks:{}", key.display()),
        "lvm:inner".into(),
    ]);
    let errno = |e: ferric_blockd::volume::Error<_>| e.errno();
    assert_eq!(
        volume
            .open("", syscall::flag::O_RDONLY, 1000)
            .map_err(errno),
        Err(syscall::error::EACCES)
    );
    let mut buf = [0u8; 64];
    assert_eq!(
        volume
            .read(ferric_blockd::volume::ROOT, 0, &mut buf)
            .map_err(errno),
        Err(syscall::error::EBADF)
    );
    assert_eq!(buf, [0; 64]);
}
