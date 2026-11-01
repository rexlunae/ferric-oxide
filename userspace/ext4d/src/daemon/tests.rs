//! Host tests of the daemon's actual argument parsing and device/mount startup.

use super::*;
use ext4d::fs::OpenMode;
use std::io::Write;
use tempfile::NamedTempFile;

#[path = "../../tests/common/mod.rs"]
mod common;

fn arguments(words: &[&str]) -> Result<Args, DaemonError> {
    parse(words.iter().map(|word| (*word).to_owned()))
}

fn image(bytes: &[u8], policy: MountPolicy) -> (NamedTempFile, Args) {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    let args = Args {
        device: file.path().to_path_buf(),
        scheme: "test-ext4".to_owned(),
        offset: 0,
        length: None,
        policy,
    };
    (file, args)
}

fn hello(fs: &mut ext4d::fs::Fs) -> Vec<u8> {
    let id = fs.open(None, "hello.txt", OpenMode::read()).unwrap();
    let mut bytes = vec![0; 128];
    let n = fs.read(id, 0, &mut bytes).unwrap();
    fs.close(id);
    bytes.truncate(n);
    bytes
}

fn unchanged(file: &NamedTempFile, expected: &[u8]) {
    let actual = std::fs::read(file.path()).unwrap();
    assert_eq!(actual.len(), expected.len(), "device size changed");
    let first = actual.iter().zip(expected).position(|(a, b)| a != b);
    assert!(first.is_none(), "device changed at byte {first:?}");
}

#[test]
fn configured_policy_stays_the_default() {
    let args = arguments(&["image.ext4", "ext4"]).unwrap();
    assert_eq!(args.policy, MountPolicy::Configured);
    assert_eq!(args.offset, 0);
    assert_eq!(args.length, None);
}

#[test]
fn read_only_policy_combines_with_a_device_window() {
    for words in [
        [
            "--read-only",
            "--offset",
            "4096",
            "--length",
            "8192",
            "disk",
            "ext4",
        ],
        [
            "disk",
            "ext4",
            "--length",
            "8192",
            "--read-only",
            "--offset",
            "4096",
        ],
    ] {
        let args = arguments(&words).unwrap();
        assert_eq!(args.policy, MountPolicy::ReadOnly);
        assert_eq!(args.device, PathBuf::from("disk"));
        assert_eq!(args.scheme, "ext4");
        assert_eq!(args.offset, 4096);
        assert_eq!(args.length, Some(8192));
    }
    assert!(matches!(arguments(&["--offset"]), Err(DaemonError::Usage)));
    assert!(matches!(
        arguments(&["--read-only", "--length", "not-bytes", "disk", "ext4"]),
        Err(DaemonError::Number {
            flag: "--length",
            ..
        })
    ));
}

#[test]
fn explicit_read_only_mount_refuses_changes_with_write_support_built_in() {
    let bytes = common::load_raw("basic.ext4");
    let (file, args) = image(&bytes, MountPolicy::ReadOnly);
    let mut fs = load(&args).unwrap();
    assert!(!fs.is_writable());
    assert_eq!(hello(&mut fs), b"hello from ext4\n");
    assert!(matches!(
        fs.open(
            None,
            "hello.txt",
            OpenMode {
                write: true,
                ..OpenMode::read()
            }
        ),
        Err(ext4d::Error::ReadOnly)
    ));
    drop(fs);
    unchanged(&file, &bytes);
}

#[test]
fn configured_clean_mount_keeps_the_compiled_write_capability() {
    let bytes = common::load_raw("basic.ext4");
    let (file, args) = image(&bytes, MountPolicy::Configured);
    let mut fs = load(&args).unwrap();
    assert_eq!(fs.is_writable(), cfg!(feature = "write"));
    assert_eq!(hello(&mut fs), b"hello from ext4\n");
    drop(fs);
    unchanged(&file, &bytes);
}

#[test]
fn explicit_read_only_recovery_uses_the_journal_without_changing_the_device() {
    let bytes = common::load("journal-v3.ext4.gz");
    let (file, args) = image(&bytes, MountPolicy::ReadOnly);
    let mut fs = load(&args).unwrap();
    assert!(!fs.is_writable());
    assert!(fs.needs_recovery());
    assert_eq!(hello(&mut fs), b"journaled hello\n");
    drop(fs);
    unchanged(&file, &bytes);
}

#[cfg(feature = "journal-replay")]
#[test]
fn configured_recovery_still_replays_onto_the_device() {
    let bytes = common::load("journal-v3.ext4.gz");
    let expected = common::load("journal-v3.replayed.ext4.gz");
    let (file, args) = image(&bytes, MountPolicy::Configured);
    let mut fs = load(&args).unwrap();
    assert!(!fs.needs_recovery());
    assert_eq!(hello(&mut fs), b"journaled hello\n");
    drop(fs);
    common::assert_same_blocks(
        "daemon recovery",
        &std::fs::read(file.path()).unwrap(),
        &expected,
    );
}
