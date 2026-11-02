//! `ferric-trace follow` on a synthetic guest RAM file, as a harness runs it.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ferric_trace::clock::{Clock, ClockLine};
use ferric_trace::host::{self, DIRECTORY_WORDS, Published, RegionLayout, RingSize};
use ferric_trace::kernel::SYSCALL_EXIT;
use ferric_trace::record::WORD_BYTES;
use ferric_trace::ring::{Ring, WhenFull};

/// Where the region sits in the 4 MiB of guest RAM.
const REGION_PHYS: u64 = 0x20_0000;

/// A 4 MiB RAM file with one CPU's 4 KiB ring at [`REGION_PHYS`] holding `records`
/// `syscall.exit` records, and a copy of the directory at 1 MiB that does not hold its
/// own address.
fn ram_file(records: u64) -> tempfile::NamedTempFile {
    let layout = RegionLayout::new(1, RingSize::from_kib(4).expect("valid")).expect("fits");
    let region: Vec<AtomicU64> = (0..layout.bytes() / WORD_BYTES)
        .map(|_| AtomicU64::new(0))
        .collect();
    let entry = layout.entry(0).expect("one ring");
    let start = entry.offset as usize / WORD_BYTES;
    let ring = Ring::shared(&region[start..start + entry.bytes as usize / WORD_BYTES])
        .expect("a ring")
        .when_full(WhenFull::OverwriteOldest);
    for timestamp in 1..=records {
        ring.push(SYSCALL_EXIT.id(), timestamp, &[3, 0])
            .expect("room");
    }
    let published = Published {
        self_phys: REGION_PHYS,
        generation: 1,
        clock: ClockLine {
            clock: Clock::Tsc,
            period_fs: None,
        },
    };
    host::publish(&region[..DIRECTORY_WORDS], &published, layout.entries()).expect("fits");
    let bytes: Vec<u8> = region
        .iter()
        .flat_map(|word| word.load(Ordering::Relaxed).to_ne_bytes())
        .collect();
    let file = tempfile::NamedTempFile::new().expect("a temporary file");
    file.as_file().set_len(4 << 20).expect("sized");
    file.as_file()
        .write_all_at(&bytes, REGION_PHYS)
        .expect("written");
    file.as_file()
        .write_all_at(&bytes[..4096], 1 << 20)
        .expect("written");
    file
}

fn ferric_trace() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ferric-trace"))
}

/// Waits up to ten seconds for `path` to hold `bytes` bytes.
fn wait_for(path: &Path, bytes: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::metadata(path).map_or(0, |m| m.len()) < bytes {
        assert!(Instant::now() < deadline, "{} never filled", path.display());
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn follows_a_guest_until_sigterm() {
    let ram = ram_file(25);
    let dir = tempfile::tempdir().expect("a directory");
    let out = dir.path().join("trace");
    let child = ferric_trace()
        .args(["follow", "--live", "--interval-ms", "5", "--guest-ram"])
        .arg(ram.path())
        .arg("--out")
        .arg(&out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("runs");
    wait_for(&out.join("cpu0"), 25 * 4 * 8);
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());
    let output = child.wait_with_output().expect("exits");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains("cpu 0: 25 records (800 bytes)"), "{stdout}");
    assert!(
        stdout.contains("followed the rings at 0x200000") && stdout.contains("until stopped"),
        "{stdout}"
    );
    assert!(!out.join("INCOMPLETE").exists());

    let counts = ferric_trace()
        .args(["counts", "--tsc-mhz", "1000"])
        .arg(&out)
        .output()
        .expect("runs");
    let counts = String::from_utf8_lossy(&counts.stdout);
    assert!(
        counts.contains("counts syscall.exit cpu=0 count=25") && counts.contains("lost_during=0"),
        "{counts}"
    );
}

#[test]
fn follows_for_a_duration_at_an_address() {
    let ram = ram_file(3);
    let dir = tempfile::tempdir().expect("a directory");
    let out = dir.path().join("trace");
    let output = ferric_trace()
        .args([
            "follow",
            "--phys",
            "0x200000",
            "--ram-mib",
            "4",
            "--duration",
            "0.05",
        ])
        .arg("--guest-ram")
        .arg(ram.path())
        .arg("--out")
        .arg(&out)
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("until the duration passed"));
    let text = ferric_trace().arg("text").arg(&out).output().expect("runs");
    assert!(text.status.success(), "{text:?}");
    assert_eq!(String::from_utf8_lossy(&text.stdout).lines().count(), 3);
    assert!(out.join("host-clock").exists());
}

#[test]
fn says_when_no_directory_comes() {
    let ram = ram_file(1);
    let dir = tempfile::tempdir().expect("a directory");
    let output = ferric_trace()
        .args(["follow", "--phys", "100000", "--duration", "0"])
        .arg("--guest-ram")
        .arg(ram.path())
        .arg("--out")
        .arg(dir.path().join("trace"))
        .output()
        .expect("runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("follow failed: no trace ring directory found (the duration passed)"),
        "{stderr}"
    );
}
