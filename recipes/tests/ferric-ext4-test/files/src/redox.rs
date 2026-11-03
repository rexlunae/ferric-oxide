//! The steps, on Redox.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt, chown, fchown};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::ptr;
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
/// A volume that needs journal recovery (gzipped), built into the test: make-fixtures.sh
/// lists its transactions.
const JOURNAL_IMAGE: &[u8] =
    include_bytes!("../../../../../userspace/ext4d/tests/fixtures/journal-v3.ext4.gz");
/// Where it is written for `ext4d` to replay and read.
const JOURNAL_IMAGE_PATH: &str = "/tmp/ferric-ext4-journal.ext4";
/// Its scheme.
const JOURNAL_SCHEME: &str = "ferric-ext4-journal";
const JOURNAL_ROOT: &str = "/scheme/ferric-ext4-journal";
/// An independent image and mount for the compiled write capability.
const WRITE_IMAGE_PATH: &str = "/tmp/ferric-ext4-write.ext4";
const WRITE_SCHEME: &str = "ferric-ext4-write";
const WRITE_ROOT: &str = "/scheme/ferric-ext4-write";
/// The unprivileged user (and group) the access and owner checks run as; nothing in the
/// fixture belongs to it.
const USER: u32 = 2000;
/// The access and modification times the owner check's child sets on its own file.
const OWN_TIMES: [(i64, i64); 2] = [(1_700_000_000, 0), (1_700_000_100, 0)];
/// `s_feature_incompat` in the volume superblock, and its `INCOMPAT_RECOVER` bit.
const INCOMPAT_AT: u64 = 1024 + 0x60;
const INCOMPAT_RECOVER: u32 = 0x4;

/// Why a step failed.
#[derive(Debug, Error)]
enum Failure {
    #[error(transparent)]
    RemoveImage(#[from] super::image::RemoveError),
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
    #[error("{what} succeeded for a user who may not do it")]
    Allowed { what: &'static str },
    #[error("{what} failed with {source}, not errno {want}")]
    WrongErrno {
        what: &'static str,
        want: i32,
        #[source]
        source: io::Error,
    },
    #[error("{what} failed with {source}, not EROFS")]
    NotErofs {
        what: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("ext4d exited unexpectedly ({status})")]
    Exited { status: std::process::ExitStatus },
    #[error("ext4d did not mount within {MOUNT_TIMEOUT:?}")]
    Timeout,
    #[error("the volume still needs recovery after ext4d mounted it")]
    NotRecovered,
    #[error(
        "the read-only image changed at {offset:?}; size {actual_len}, expected {expected_len}"
    )]
    ImageChanged {
        offset: Option<usize>,
        actual_len: usize,
        expected_len: usize,
    },
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

/// Waits for `root` to answer, or for the daemon to give up.
fn mount(child: &mut Child, root: &str) -> Result<(), Failure> {
    let start = Instant::now();
    loop {
        if fs::metadata(root).is_ok() {
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

/// Runs a check against a live mount, then terminates and reaps that daemon.
fn mounted<T>(
    args: &[&str],
    root: &str,
    check: impl FnOnce() -> Result<T, Failure>,
) -> Result<T, Failure> {
    let mut child = io(
        "start ext4d",
        Command::new(EXT4D).args(args).stdin(Stdio::null()).spawn(),
    )?;
    let result = mount(&mut child, root).and_then(|()| check());
    let cleanup = (|| {
        if let Some(status) = io("check ext4d before stopping it", child.try_wait())? {
            return Err(Failure::Exited { status });
        }
        io("stop ext4d", child.kill())?;
        let status = io("reap ext4d", child.wait())?;
        if status.signal() != Some(libc::SIGKILL) {
            return Err(Failure::Exited { status });
        }
        Ok(())
    })();
    result.and_then(|value| cleanup.map(|()| value))
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
    same("hello.txt's size", meta("hello.txt")?.len(), 16)?;
    let link = io("lstat the link", fs::symlink_metadata(at("link")))?;
    same("link is a symlink", link.file_type().is_symlink(), true)?;
    same("link target length", link.len(), 9)?;
    match fs::read_link(at("hello.txt")) {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => access(),
        _ => Err(Failure::Changed {
            what: "read_link on a regular file",
        }),
    }
}

/// Exercise CallerCtx propagation using a process with neither file ownership nor group access.
fn access() -> Result<(), Failure> {
    let status = io(
        "run unprivileged access checks",
        Command::new("/usr/bin/ferric-ext4-test")
            .arg("--access-check")
            .uid(USER)
            .gid(USER)
            .status(),
    )?;
    same("unprivileged access checks", status.success(), true)
}

fn access_child() -> Result<(), Failure> {
    same(
        "public file is readable",
        io("read public file", fs::read(at("hello.txt")))?,
        b"hello from ext4\n".to_vec(),
    )?;
    for path in [
        "dir/private.txt",
        "dir/private-link",
        "dir/private-dir/secret",
        "dir/secret-link",
        "dir/private-dir/../nested.txt",
    ] {
        match File::open(at(path)) {
            Err(e) => same("protected open errno", e.raw_os_error(), Some(libc::EACCES))?,
            Ok(_) => same("protected open is denied", true, false)?,
        }
    }
    let meta = io(
        "lstat a protected target's link",
        fs::symlink_metadata(at("dir/private-link")),
    )?;
    same(
        "protected target's link is statted",
        meta.file_type().is_symlink(),
        true,
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

/// Whether the daemon advertises a compiled capability.
fn has_feature(feature: &str) -> Result<bool, Failure> {
    let output = io(
        "run ext4d --features",
        Command::new(EXT4D).arg("--features").output(),
    )?;
    same(
        "ext4d --features exits cleanly",
        output.status.success(),
        true,
    )?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == feature))
}

fn optional(
    step: &str,
    feature: &str,
    flag: &str,
    run: impl FnOnce() -> Result<(), Failure>,
) -> bool {
    match has_feature(feature) {
        Ok(true) => report(step, run()),
        Ok(false) => {
            println!("FERRIC_TEST_SKIP ext4-{step}: ext4d has no {feature} ({flag} is off)");
            true
        }
        Err(error) => report(step, Err(error)),
    }
}

/// Replays and mounts a volume that needs recovery: hello.txt must read as the journal left
/// it, and the image on disk must no longer need recovery. Any failure to do so, including
/// ext4d exiting before it mounts, fails the step; it is skipped only when ext4d says it has
/// no journal replay.
fn journal() -> Result<(), Failure> {
    let mut image = Vec::new();
    io(
        "unpack the journal image",
        flate2::read::GzDecoder::new(JOURNAL_IMAGE).read_to_end(&mut image),
    )?;
    io(
        "write the journal image",
        fs::write(JOURNAL_IMAGE_PATH, image),
    )?;
    let result = mounted(
        &[JOURNAL_IMAGE_PATH, JOURNAL_SCHEME],
        JOURNAL_ROOT,
        replayed,
    );
    super::image::finish(Path::new(JOURNAL_IMAGE_PATH), result)
}

fn writable() -> Result<(), Failure> {
    io(
        "write the writable image",
        fs::write(WRITE_IMAGE_PATH, IMAGE),
    )?;
    let result = mounted(&[WRITE_IMAGE_PATH, WRITE_SCHEME], WRITE_ROOT, || {
        let hello = format!("{WRITE_ROOT}/hello.txt");
        io("write hello.txt", fs::write(&hello, b"write enabled\n"))?;
        same(
            "written contents",
            io("read written hello.txt", fs::read(&hello))?,
            b"write enabled\n".to_vec(),
        )?;
        let file = io(
            "open hello.txt for truncation",
            OpenOptions::new().write(true).open(&hello),
        )?;
        io("truncate hello.txt", file.set_len(5))?;
        drop(file);
        same(
            "truncated contents",
            io("read truncated hello.txt", fs::read(&hello))?,
            b"write".to_vec(),
        )?;
        let created = format!("{WRITE_ROOT}/created.txt");
        io("create a regular file", fs::write(&created, b"created\n"))?;
        same(
            "created contents",
            io("read created file", fs::read(&created))?,
            b"created\n".to_vec(),
        )?;
        appends(&created)?;
        owner()
    });
    super::image::finish(Path::new(WRITE_IMAGE_PATH), result)
}

/// An `O_APPEND` write goes to the end of `path` (8 bytes long) and leaves the cursor there
/// (#244), whatever the cursor was.
fn appends(path: &str) -> Result<(), Failure> {
    let mut file = io(
        "open created.txt for appending",
        OpenOptions::new().read(true).append(true).open(path),
    )?;
    io("append to created.txt", file.write_all(b"more"))?;
    same(
        "cursor after the append",
        io("lseek created.txt", file.stream_position())?,
        12,
    )?;
    let mut rest = Vec::new();
    io("read after the append", file.read_to_end(&mut rest))?;
    same("what a read after the append finds", rest, Vec::new())?;
    io("seek created.txt to 0", file.seek(SeekFrom::Start(0)))?;
    io("append to created.txt again", file.write_all(b"!"))?;
    same(
        "cursor after the second append",
        io("lseek created.txt", file.stream_position())?,
        13,
    )?;
    drop(file);
    same(
        "appended contents",
        io("read appended created.txt", fs::read(path))?,
        b"created\nmore!".to_vec(),
    )
}

fn write_at(path: &str) -> String {
    format!("{WRITE_ROOT}/{path}")
}

/// `result` must have failed with `want`.
fn refused<T>(what: &'static str, want: i32, result: io::Result<T>) -> Result<(), Failure> {
    match result {
        Ok(_) => Err(Failure::Allowed { what }),
        Err(err) if err.raw_os_error() == Some(want) => Ok(()),
        Err(source) => Err(Failure::WrongErrno { what, want, source }),
    }
}

/// `futimens` on `file` with the access and modification `times`, or with a null pointer
/// (both the current time) for `None`. `std`'s `File::set_times` is unsupported on Redox.
#[expect(unsafe_code, reason = "futimens has no safe wrapper on Redox")]
fn futimens(file: &File, times: Option<[(i64, i64); 2]>) -> io::Result<()> {
    let times =
        times.map(|times| times.map(|(tv_sec, tv_nsec)| libc::timespec { tv_sec, tv_nsec }));
    let times_ptr = times.as_ref().map_or(ptr::null(), |times| times.as_ptr());
    // SAFETY: the descriptor is open for the call, and `times_ptr` is null or points at two
    // `timespec`s in `times`, which outlives the call.
    let status = unsafe { libc::futimens(file.as_raw_fd(), times_ptr) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What the owner check compares: a file's owner, group, permission bits and access and
/// modification times.
type Attributes = (u32, u32, u32, [(i64, i64); 2]);

fn attributes(path: &str) -> Result<Attributes, Failure> {
    let meta = io(format!("stat {path}"), fs::metadata(path))?;
    Ok((
        meta.uid(),
        meta.gid(),
        meta.mode() & 0o7777,
        [
            (meta.atime(), meta.atime_nsec()),
            (meta.mtime(), meta.mtime_nsec()),
        ],
    ))
}

/// `fchmod` and `futimens` reach ext4d as a `StdFsCall`, whose caller context carries no
/// IDs; ext4d must ask procmgr who the caller is (issue #399). An unprivileged child may
/// change its own file, but not root's: hello.txt, given to root first, must come back
/// unchanged.
fn owner() -> Result<(), Failure> {
    let hello = write_at("hello.txt");
    io("give hello.txt to root", chown(&hello, Some(0), Some(0)))?;
    io(
        "chmod hello.txt",
        fs::set_permissions(&hello, Permissions::from_mode(0o644)),
    )?;
    let own = write_at("own.txt");
    io("create own.txt", fs::write(&own, b"own\n"))?;
    io(
        "give own.txt to the user",
        chown(&own, Some(USER), Some(USER)),
    )?;
    io(
        "chmod own.txt",
        fs::set_permissions(&own, Permissions::from_mode(0o644)),
    )?;
    let before = attributes(&hello)?;
    let status = io(
        "run unprivileged owner checks",
        Command::new("/usr/bin/ferric-ext4-test")
            .arg("--owner-check")
            .uid(USER)
            .gid(USER)
            .status(),
    )?;
    same("unprivileged owner checks", status.success(), true)?;
    same("root's hello.txt afterwards", attributes(&hello)?, before)?;
    same(
        "the user's own.txt afterwards",
        attributes(&own)?,
        (USER, USER, 0o600, OWN_TIMES),
    )
}

/// The owner check's child, run as [`USER`] against the writable mount.
fn owner_child() -> Result<(), Failure> {
    let theirs = io("open root's hello.txt", File::open(write_at("hello.txt")))?;
    refused(
        "fchmod on root's file",
        libc::EPERM,
        theirs.set_permissions(Permissions::from_mode(0o666)),
    )?;
    refused(
        "futimens with explicit times on root's file",
        libc::EPERM,
        futimens(&theirs, Some(OWN_TIMES)),
    )?;
    refused(
        "futimens to now on root's read-only file",
        libc::EACCES,
        futimens(&theirs, None),
    )?;
    refused(
        "fchown of root's file to the user",
        libc::EPERM,
        fchown(&theirs, Some(USER), None),
    )?;
    let own = io("open own.txt", File::open(write_at("own.txt")))?;
    io(
        "fchmod on the user's own file",
        own.set_permissions(Permissions::from_mode(0o600)),
    )?;
    io(
        "futimens on the user's own file",
        futimens(&own, Some(OWN_TIMES)),
    )
}

fn unchanged_image() -> Result<(), Failure> {
    let bytes = io("read the read-only image", fs::read(IMAGE_PATH))?;
    let offset = bytes.iter().zip(IMAGE).position(|(a, b)| a != b);
    if bytes.len() != IMAGE.len() || offset.is_some() {
        return Err(Failure::ImageChanged {
            offset,
            actual_len: bytes.len(),
            expected_len: IMAGE.len(),
        });
    }
    Ok(())
}

fn replayed() -> Result<(), Failure> {
    same(
        "hello.txt after the replay",
        io(
            "read hello.txt",
            fs::read(format!("{JOURNAL_ROOT}/hello.txt")),
        )?,
        b"journaled hello\n".to_vec(),
    )?;
    let image = io("open the journal image", File::open(JOURNAL_IMAGE_PATH))?;
    let mut incompat = [0; 4];
    io(
        "read the superblock",
        image.read_exact_at(&mut incompat, INCOMPAT_AT),
    )?;
    if u32::from_le_bytes(incompat) & INCOMPAT_RECOVER != 0 {
        return Err(Failure::NotRecovered);
    }
    Ok(())
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

/// Runs the mount-policy and enabled replay/write checks, reporting each outcome.
pub fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--access-check") {
        return if report("access", access_child()) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if std::env::args().any(|arg| arg == "--owner-check") {
        return if report("owner", owner_child()) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if !Path::new(EXT4D).exists() {
        println!("FERRIC_TEST_SKIP ext4: no {EXT4D} (fs.ext4 is off)");
        return ExitCode::SUCCESS;
    }
    if !report("image", io("write the image", fs::write(IMAGE_PATH, IMAGE))) {
        return ExitCode::FAILURE;
    }
    let result = mounted(&["--read-only", IMAGE_PATH, SCHEME], ROOT, || {
        println!("FERRIC_TEST_PASS ext4-mount");
        let mut ok = report("list", list());
        ok &= report("read", read());
        ok &= report("stat", stat());
        ok &= report("erofs", read_only());
        Ok(ok)
    });
    let mut ok = match result {
        Ok(passed) => {
            let unchanged = report("image-unchanged", unchanged_image());
            if passed && unchanged {
                report(
                    "cleanup",
                    super::image::finish(Path::new(IMAGE_PATH), Ok(())),
                )
            } else {
                false
            }
        }
        Err(error) => report("mount", Err(error)),
    };
    ok &= optional(
        "journal",
        "journal-replay",
        "fs.ext4_journal_replay",
        journal,
    );
    ok &= optional("write", "write", "fs.ext4_write", writable);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
