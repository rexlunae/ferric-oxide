//! The steps, on Redox.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

/// The fixture image, built into the test.
const IMAGE: &[u8] = include_bytes!("../../../../../userspace/ext4d/tests/fixtures/basic.ext4");
/// Where the image is written for `ext4d` to read.
const IMAGE_PATH: &str = "/tmp/ferric-ext4-test.ext4";
/// The daemon.
const EXT4D: &str = "/usr/bin/ext4d";
/// The scheme name, and so the mount point.
const SCHEME: &str = "ferric-ext4";
const ROOT: &str = "/scheme/ferric-ext4";
/// How long the daemon may take to mount.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);

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
    #[error("ext4d exited before mounting ({status})")]
    Exited { status: std::process::ExitStatus },
    #[error("ext4d did not mount within {MOUNT_TIMEOUT:?}")]
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

/// Waits for `/scheme/ferric-ext4` to answer, or for the daemon to give up.
fn mount(child: &mut Child) -> Result<(), Failure> {
    let start = Instant::now();
    loop {
        if fs::metadata(ROOT).is_ok() {
            return Ok(());
        }
        if let Some(status) = io("wait for ext4d", child.try_wait())? {
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
        "big.bin",
        "dir",
        "empty",
        "hello.txt",
        "link",
        "lost+found",
        "many",
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
    same(
        "hello.txt",
        read("hello.txt")?,
        b"hello from ext4\n".to_vec(),
    )?;
    same(
        "dir/nested.txt",
        read("dir/nested.txt")?,
        b"nested\n".to_vec(),
    )?;
    same("many/f137", read("many/f137")?, b"f137\n".to_vec())?;
    same(
        "link, followed",
        read("link")?,
        b"hello from ext4\n".to_vec(),
    )?;
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
    )?;
    let target = io("read the link", fs::read_link(at("link")))?;
    same(
        "the link's target",
        target.as_path(),
        Path::new("hello.txt"),
    )
}

fn stat() -> Result<(), Failure> {
    let meta = |path: &str| io(format!("stat {path}"), fs::metadata(at(path)));
    let big = meta("big.bin")?;
    same("big.bin's size", big.len(), 200 * 1024)?;
    same("big.bin is a file", big.is_file(), true)?;
    same("dir is a directory", meta("dir")?.is_dir(), true)?;
    same("hello.txt's size", meta("hello.txt")?.len(), 16)
}

/// `result` must have failed with `EROFS`.
fn erofs<T>(what: &'static str, result: io::Result<T>) -> Result<(), Failure> {
    match result {
        Ok(_) => Err(Failure::Changed { what }),
        Err(err) if err.raw_os_error() == Some(libc::EROFS) => Ok(()),
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
        b"hello from ext4\n".to_vec(),
    )?;
    same("new.txt exists", fs::metadata(at("new.txt")).is_ok(), false)
}

fn report(step: &str, result: Result<(), Failure>) -> bool {
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS ext4-{step}");
            true
        }
        Err(err) => {
            let mut line = format!("FERRIC_TEST_FAIL ext4-{step}: {err}");
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
    if !Path::new(EXT4D).exists() {
        println!("FERRIC_TEST_SKIP ext4: no {EXT4D} (fs.ext4 is off)");
        return ExitCode::SUCCESS;
    }
    if !report("image", io("write the image", fs::write(IMAGE_PATH, IMAGE))) {
        return ExitCode::FAILURE;
    }
    let mut child = match Command::new(EXT4D)
        .args([IMAGE_PATH, SCHEME])
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(source) => {
            report(
                "mount",
                Err(Failure::Io {
                    what: "start ext4d".into(),
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
    // ext4d serves until it is stopped; the test is done with it.
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(IMAGE_PATH);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
