//! The steps, on Redox.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

/// The daemon.
const FATD: &str = "/usr/bin/fatd";
/// How long the daemon may take to mount.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
/// 2026-01-01 12:34:56 UTC, when make-fixtures.sh says every file was last written.
const STAMP: i64 = 1_767_270_896;
/// What `fatd` says when it was built without exFAT (features.toml fs.exfat off).
const NO_EXFAT: &str = "built without exFAT support";

/// One fixture image and what differs between the formats' fixtures.
struct Volume {
    /// The step prefix, and the scheme's name after `ferric-`.
    name: &'static str,
    /// The gzipped image, built into the test.
    image: &'static [u8],
    /// What `hello.txt` holds.
    hello: &'static [u8],
    /// Whether names have 8.3 short forms (FAT does, exFAT does not).
    short_names: bool,
}

impl Volume {
    fn scheme(&self) -> String {
        format!("ferric-{}", self.name)
    }

    fn image_path(&self) -> String {
        format!("/tmp/ferric-{}-test.img", self.name)
    }

    fn at(&self, path: &str) -> String {
        format!("/scheme/{}/{path}", self.scheme())
    }
}

const FAT32: Volume = Volume {
    name: "fat",
    image: include_bytes!("../../../../../userspace/fatd/tests/fixtures/fat32.img.gz"),
    hello: b"hello from FAT\n",
    short_names: true,
};

const EXFAT: Volume = Volume {
    name: "exfat",
    image: include_bytes!("../../../../../crates/exfat/tests/fixtures/exfat-4k.img.gz"),
    hello: b"hello from exFAT\n",
    short_names: false,
};

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
    #[error("fatd exited before mounting ({status}): {stderr}")]
    Exited {
        status: std::process::ExitStatus,
        stderr: String,
    },
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

/// Unpacks the image to its path in `/tmp`.
fn image(v: &Volume) -> Result<(), Failure> {
    let mut bytes = Vec::new();
    io(
        "unpack the image",
        flate2::read::GzDecoder::new(v.image).read_to_end(&mut bytes),
    )?;
    io("write the image", fs::write(v.image_path(), bytes))
}

/// Waits for the scheme to answer, or for the daemon to give up.
fn mount(v: &Volume, child: &mut Child) -> Result<(), Failure> {
    let start = Instant::now();
    loop {
        if fs::metadata(v.at("")).is_ok() {
            return Ok(());
        }
        if let Some(status) = io("wait for fatd", child.try_wait())? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            return Err(Failure::Exited {
                status,
                stderr: stderr.trim().to_owned(),
            });
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

fn list(v: &Volume) -> Result<(), Failure> {
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
        names(&v.at(""))?,
        want.map(String::from).to_vec(),
    )?;
    let many: Vec<String> = (0..200).map(|i| format!("f{i:03}")).collect();
    same("many/'s entries", names(&v.at("many"))?, many)?;
    same("empty/'s entries", names(&v.at("empty"))?, Vec::new())
}

fn big_bin() -> Vec<u8> {
    (0..200 * 1024u32)
        .map(|i| ((i * 7 + i / 4096) % 251) as u8)
        .collect()
}

fn read(v: &Volume) -> Result<(), Failure> {
    let read = |path: &str| io(format!("read {path}"), fs::read(v.at(path)));
    let hello = v.hello.to_vec();
    same("hello.txt", read("hello.txt")?, hello.clone())?;
    same("HELLO.TXT", read("HELLO.TXT")?, hello)?;
    same(
        "A Long File Name.txt",
        read("A Long File Name.txt")?,
        b"long\n".to_vec(),
    )?;
    if v.short_names {
        same("ALONGF~1.TXT", read("ALONGF~1.TXT")?, b"long\n".to_vec())?;
    }
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
    let mut file = io("open big.bin", File::open(v.at("big.bin")))?;
    io("seek big.bin", file.seek(SeekFrom::Start(100_000)))?;
    let mut buf = vec![0; 3000];
    io("read big.bin at 100000", file.read_exact(&mut buf))?;
    same(
        "big.bin at 100000",
        buf.as_slice(),
        &big_bin()[100_000..103_000],
    )
}

fn stat(v: &Volume) -> Result<(), Failure> {
    let meta = |path: &str| io(format!("stat {path}"), fs::metadata(v.at(path)));
    let big = meta("big.bin")?;
    same("big.bin's size", big.len(), 200 * 1024)?;
    same("big.bin is a file", big.is_file(), true)?;
    same("big.bin's mode", big.mode() & 0o7777, 0o444)?;
    same("big.bin's mtime", big.mtime(), STAMP)?;
    let dir = meta("dir")?;
    same("dir is a directory", dir.is_dir(), true)?;
    same("dir's mode", dir.mode() & 0o7777, 0o555)?;
    same(
        "hello.txt's size",
        meta("hello.txt")?.len(),
        v.hello.len() as u64,
    )?;
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
        Err(err) if err.raw_os_error() == Some(libc::EROFS) => Ok(()),
        Err(source) => Err(Failure::NotErofs { what, source }),
    }
}

fn read_only(v: &Volume) -> Result<(), Failure> {
    let at = |path: &str| v.at(path);
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
        v.hello.to_vec(),
    )?;
    same("new.txt exists", fs::metadata(at("new.txt")).is_ok(), false)
}

fn report(v: &Volume, step: &str, result: Result<(), Failure>) -> bool {
    let name = v.name;
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS {name}-{step}");
            true
        }
        Err(err) => {
            let mut line = format!("FERRIC_TEST_FAIL {name}-{step}: {err}");
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

/// Mounts `v`'s image and runs every step on it. Returns whether all passed; an exFAT image
/// that this `fatd` was built not to read is a skip, not a failure.
fn check(v: &Volume) -> bool {
    if !report(v, "image", image(v)) {
        return false;
    }
    let spawned = Command::new(FATD)
        .args([v.image_path(), v.scheme()])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(source) => {
            let what = "start fatd".into();
            return report(v, "mount", Err(Failure::Io { what, source }));
        }
    };
    let mut ok = match mount(v, &mut child) {
        Err(Failure::Exited { stderr, .. }) if stderr.contains(NO_EXFAT) => {
            println!(
                "FERRIC_TEST_SKIP {}: fatd {NO_EXFAT} (fs.exfat is off)",
                v.name
            );
            let _ = fs::remove_file(v.image_path());
            return true;
        }
        mounted => report(v, "mount", mounted),
    };
    if ok {
        ok &= report(v, "list", list(v));
        ok &= report(v, "read", read(v));
        ok &= report(v, "stat", stat(v));
        ok &= report(v, "erofs", read_only(v));
    }
    // fatd serves until it is stopped; the test is done with it.
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(v.image_path());
    ok
}

pub fn main() -> ExitCode {
    if !Path::new(FATD).exists() {
        println!("FERRIC_TEST_SKIP fat: no {FATD} (fs.fat is off)");
        return ExitCode::SUCCESS;
    }
    // Both, even when the first fails, so the log shows every step.
    let fat = check(&FAT32);
    let exfat = check(&EXFAT);
    if fat && exfat {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
