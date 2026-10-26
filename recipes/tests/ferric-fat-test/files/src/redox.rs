//! The steps, on Redox.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

/// The gzipped FAT32 fixture image, built into the test.
const IMAGE: &[u8] = include_bytes!("../../../../../userspace/fatd/tests/fixtures/fat32.img.gz");
/// Where the image is written for `fatd` to read.
const IMAGE_PATH: &str = "/tmp/ferric-fat-test.img";
/// The daemon.
const FATD: &str = "/usr/bin/fatd";
/// The scheme name, and so the mount point.
const SCHEME: &str = "ferric-fat";
const ROOT: &str = "/scheme/ferric-fat";
/// How long the daemon may take to mount.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
/// 2026-01-01 12:34:56 UTC, when make-fixtures.sh says every file was last written.
const STAMP: i64 = 1_767_270_896;

/// Why a step failed.
#[derive(Debug, Error)]
enum Failure {
    #[error("{what} failed")]
    Io {
        what: String,
        #[source]
        source: io::Error,
    },
    #[error("{what}: got {got}, expected {want}")]
    Mismatch {
        what: &'static str,
        got: String,
        want: String,
    },
    #[error("{what} succeeded on a read-only file system")]
    Changed { what: &'static str },
    #[error("{what} failed with {source}, not EROFS")]
    NotErofs {
        what: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("fatd exited before mounting ({status})")]
    Exited { status: std::process::ExitStatus },
    #[error("fatd did not mount within {MOUNT_TIMEOUT:?}")]
    Timeout,
}

fn io<T>(what: impl Into<String>, result: io::Result<T>) -> Result<T, Failure> {
    result.map_err(|source| Failure::Io {
        what: what.into(),
        source,
    })
}

fn same<T: PartialEq + std::fmt::Debug>(
    what: &'static str,
    got: T,
    want: T,
) -> Result<(), Failure> {
    if got == want {
        Ok(())
    } else {
        Err(Failure::Mismatch {
            what,
            got: format!("{got:?}"),
            want: format!("{want:?}"),
        })
    }
}

fn at(path: &str) -> String {
    format!("{ROOT}/{path}")
}

/// Unpacks the image to [`IMAGE_PATH`].
fn image() -> Result<(), Failure> {
    let mut bytes = Vec::new();
    io(
        "unpack the image",
        flate2::read::GzDecoder::new(IMAGE).read_to_end(&mut bytes),
    )?;
    io("write the image", fs::write(IMAGE_PATH, bytes))
}

/// Waits for the scheme to answer, or for the daemon to give up.
fn mount(child: &mut Child) -> Result<(), Failure> {
    let start = Instant::now();
    loop {
        if fs::metadata(ROOT).is_ok() {
            return Ok(());
        }
        if let Some(status) = io("wait for fatd", child.try_wait())? {
            return Err(Failure::Exited { status });
        }
        if start.elapsed() > MOUNT_TIMEOUT {
            return Err(Failure::Timeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn names(dir: &str) -> Result<Vec<String>, Failure> {
    let mut names = Vec::new();
    for entry in io(format!("list {dir}"), fs::read_dir(dir))? {
        let entry = io(format!("list {dir}"), entry)?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn list() -> Result<(), Failure> {
    let want = [
        "A Long File Name.txt",
        "EFI",
        "big.bin",
        "dir",
        "empty",
        "hello.txt",
        "many",
        "Ünïcødé.txt",
    ];
    same(
        "the root's entries",
        names(ROOT)?,
        want.map(String::from).to_vec(),
    )?;
    let many: Vec<String> = (0..200).map(|i| format!("f{i:03}")).collect();
    same("many/'s entries", names(&at("many"))?, many)?;
    same("empty/'s entries", names(&at("empty"))?, Vec::new())
}

fn big_bin() -> Vec<u8> {
    (0..200 * 1024u32)
        .map(|i| ((i * 7 + i / 4096) % 251) as u8)
        .collect()
}

fn read() -> Result<(), Failure> {
    let read = |path: &str| io(format!("read {path}"), fs::read(at(path)));
    let hello = b"hello from FAT\n".to_vec();
    same("hello.txt", read("hello.txt")?, hello.clone())?;
    same("HELLO.TXT", read("HELLO.TXT")?, hello)?;
    same(
        "A Long File Name.txt",
        read("A Long File Name.txt")?,
        b"long\n".to_vec(),
    )?;
    same("ALONGF~1.TXT", read("ALONGF~1.TXT")?, b"long\n".to_vec())?;
    same("Ünïcødé.txt", read("Ünïcødé.txt")?, b"unicode\n".to_vec())?;
    same(
        "dir/nested.txt",
        read("dir/nested.txt")?,
        b"nested\n".to_vec(),
    )?;
    same("many/f137", read("many/f137")?, b"f137\n".to_vec())?;
    let efi = read("efi/boot/bootx64.efi")?;
    same("bootx64.efi's start", efi.get(..2), Some(&b"MZ"[..]))?;
    if read("big.bin")? != big_bin() {
        return Err(Failure::Mismatch {
            what: "big.bin",
            got: "other bytes".into(),
            want: "the pattern make-fixtures.sh writes".into(),
        });
    }
    let mut file = io("open big.bin", File::open(at("big.bin")))?;
    io("seek big.bin", file.seek(SeekFrom::Start(100_000)))?;
    let mut buf = vec![0; 3000];
    io("read big.bin at 100000", file.read_exact(&mut buf))?;
    same(
        "big.bin at 100000",
        buf.as_slice(),
        &big_bin()[100_000..103_000],
    )
}

fn stat() -> Result<(), Failure> {
    let meta = |path: &str| io(format!("stat {path}"), fs::metadata(at(path)));
    let big = meta("big.bin")?;
    same("big.bin's size", big.len(), 200 * 1024)?;
    same("big.bin is a file", big.is_file(), true)?;
    same("big.bin's mode", big.mode() & 0o7777, 0o444)?;
    same("big.bin's mtime", big.mtime(), STAMP)?;
    let dir = meta("dir")?;
    same("dir is a directory", dir.is_dir(), true)?;
    same("dir's mode", dir.mode() & 0o7777, 0o555)?;
    same("hello.txt's size", meta("hello.txt")?.len(), 15)?;
    same(
        "a case-insensitive stat names the same file",
        meta("EFI/BOOT/BOOTX64.EFI")?.ino(),
        meta("efi/boot/bootx64.efi")?.ino(),
    )
}

/// `result` must have failed with `EROFS`.
fn erofs<T>(what: &'static str, result: io::Result<T>) -> Result<(), Failure> {
    match result {
        Ok(_) => Err(Failure::Changed { what }),
        Err(err) if err.raw_os_error() == Some(syscall::EROFS) => Ok(()),
        Err(source) => Err(Failure::NotErofs { what, source }),
    }
}

fn read_only() -> Result<(), Failure> {
    let hello = at("hello.txt");
    erofs(
        "opening hello.txt for writing",
        OpenOptions::new().write(true).open(&hello),
    )?;
    erofs(
        "truncating hello.txt",
        OpenOptions::new().write(true).truncate(true).open(&hello),
    )?;
    erofs("creating new.txt", File::create(at("new.txt")))?;
    erofs("removing hello.txt", fs::remove_file(&hello))?;
    erofs("making a directory", fs::create_dir(at("newdir")))?;
    erofs("removing empty/", fs::remove_dir(at("empty")))?;
    erofs("renaming hello.txt", fs::rename(&hello, at("moved.txt")))?;
    same(
        "hello.txt afterwards",
        io("read hello.txt", fs::read(&hello))?,
        b"hello from FAT\n".to_vec(),
    )?;
    same("new.txt exists", fs::metadata(at("new.txt")).is_ok(), false)
}

fn report(step: &str, result: Result<(), Failure>) -> bool {
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS fat-{step}");
            true
        }
        Err(err) => {
            let mut line = format!("FERRIC_TEST_FAIL fat-{step}: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                line.push_str(&format!(": {cause}"));
                source = cause.source();
            }
            println!("{line}");
            false
        }
    }
}

pub fn main() -> ExitCode {
    if !Path::new(FATD).exists() {
        println!("FERRIC_TEST_SKIP fat: no {FATD} (fs.fat is off)");
        return ExitCode::SUCCESS;
    }
    if !report("image", image()) {
        return ExitCode::FAILURE;
    }
    let mut child = match Command::new(FATD)
        .args([IMAGE_PATH, SCHEME])
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(source) => {
            report(
                "mount",
                Err(Failure::Io {
                    what: "start fatd".into(),
                    source,
                }),
            );
            return ExitCode::FAILURE;
        }
    };
    let mut ok = report("mount", mount(&mut child));
    if ok {
        ok &= report("list", list());
        ok &= report("read", read());
        ok &= report("stat", stat());
        ok &= report("erofs", read_only());
    }
    // fatd serves until it is stopped; the test is done with it.
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(IMAGE_PATH);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
