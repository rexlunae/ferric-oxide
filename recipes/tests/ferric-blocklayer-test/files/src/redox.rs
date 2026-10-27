//! The steps, on Redox.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

/// LUKS2 (4096-byte sectors) holding PV innervg, LV "inner", pattern seed 6.
const LVM_ON_LUKS: &[u8] =
    include_bytes!("../../../../../crates/blocklayer/tests/fixtures/lvm-on-luks.img");
/// PV blkvg: LV "sealed", over two extent runs, holds LUKS2 (512-byte sectors), pattern seed 5.
const LVM: &[u8] = include_bytes!("../../../../../crates/blocklayer/tests/fixtures/lvm.img");
/// A LUKS1 header whose payload starts at sector 4096, and the first 8 KiB of that payload
/// (pattern seed 3).
const LUKS1_HEADER: &[u8] = include_bytes!("../../../../../crates/luks/tests/fixtures/luks1.img");
const LUKS1_DATA: &[u8] =
    include_bytes!("../../../../../crates/blocklayer/tests/fixtures/luks1-data.bin");
/// How many bytes of each encrypted volume the fixtures keep.
const KEPT: usize = 8192;
const BLOCKD: &str = "/usr/bin/ferric-blockd";
const KEY: &str = "/tmp/ferric-blocklayer-test.key";
const WRONG_KEY: &str = "/tmp/ferric-blocklayer-test.wrong";
/// ferric-unlock, which serves the recipe's /etc/ferric/crypttab at boot (fs.unlock_at_boot).
const UNLOCK: &str = "/usr/bin/ferric-unlock";
const EXT4D: &str = "/usr/bin/ext4d";
/// The volumes that table names: LVM on LUKS, and ext4 on a plain volume.
const BOOT_LOL: &str = "/scheme/ferric-boot-lol";
const BOOT_EXT4: &str = "/scheme/ferric-boot-ext4";
/// How long the daemon may take to serve.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);

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
        what: String,
        got: String,
        want: String,
    },
    #[error("{what} succeeded")]
    Allowed { what: &'static str },
    #[error("{what} failed with {source}, not {want}")]
    WrongErrno {
        what: &'static str,
        want: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("ferric-blockd exited before serving ({status})")]
    Exited { status: std::process::ExitStatus },
    #[error("ferric-blockd did not serve within {MOUNT_TIMEOUT:?}")]
    Timeout,
}

fn io<T>(what: impl Into<String>, result: io::Result<T>) -> Result<T, Failure> {
    result.map_err(|source| Failure::Io {
        what: what.into(),
        source,
    })
}

fn same<T: PartialEq + std::fmt::Debug>(
    what: impl Into<String>,
    got: T,
    want: T,
) -> Result<(), Failure> {
    if got == want {
        Ok(())
    } else {
        Err(Failure::Mismatch {
            what: what.into(),
            got: format!("{got:?}"),
            want: format!("{want:?}"),
        })
    }
}

fn pattern(seed: u64, start: u64, len: usize) -> Vec<u8> {
    (start..start + len as u64)
        .map(|i| ((i * 7 + i / 4096 + seed) % 251) as u8)
        .collect()
}

/// A running daemon serving `/scheme/<scheme>`.
struct Served {
    child: Child,
    root: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        // ferric-blockd serves until it is stopped; the test is done with it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(scheme: &str, steps: &[String]) -> Result<Child, Failure> {
    io(
        "start ferric-blockd",
        Command::new(BLOCKD)
            .arg(scheme)
            .args(steps)
            .stdin(Stdio::null())
            .spawn(),
    )
}

/// Starts the daemon and waits for `/scheme/<scheme>` to answer.
fn serve(scheme: &str, steps: &[String]) -> Result<Served, Failure> {
    let mut served = Served {
        child: spawn(scheme, steps)?,
        root: format!("/scheme/{scheme}"),
    };
    let start = Instant::now();
    loop {
        if fs::metadata(&served.root).is_ok() {
            return Ok(served);
        }
        if let Some(status) = io("wait for ferric-blockd", served.child.try_wait())? {
            return Err(Failure::Exited { status });
        }
        if start.elapsed() > MOUNT_TIMEOUT {
            return Err(Failure::Timeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Checks the volume's size and that its first `KEPT` bytes, and a run that crosses a
/// 4096-byte boundary, hold the pattern for `seed`.
fn check(root: &str, size: u64, seed: u64) -> Result<(), Failure> {
    let meta = io(format!("stat {root}"), fs::metadata(root))?;
    same(format!("{root}'s size"), meta.len(), size)?;
    let file = io(format!("open {root}"), File::open(root))?;
    let mut buf = vec![0; KEPT];
    io(format!("read {root}"), file.read_exact_at(&mut buf, 0))?;
    if buf != pattern(seed, 0, KEPT) {
        return Err(Failure::Mismatch {
            what: format!("{root}'s first {KEPT} bytes"),
            got: "other bytes".into(),
            want: "the pattern make-fixtures.sh writes".into(),
        });
    }
    let mut run = vec![0; 300];
    io(
        format!("read {root} at 4000"),
        file.read_exact_at(&mut run, 4000),
    )?;
    same(format!("{root} at 4000"), run, pattern(seed, 4000, 300))
}

fn write_images() -> io::Result<()> {
    fs::write("/tmp/ferric-blocklayer-lvm-on-luks.img", LVM_ON_LUKS)?;
    fs::write("/tmp/ferric-blocklayer-lvm.img", LVM)?;
    let mut luks1 = LUKS1_HEADER.to_vec();
    luks1.resize(4096 * 512, 0);
    luks1.extend_from_slice(LUKS1_DATA);
    fs::write("/tmp/ferric-blocklayer-luks1.img", luks1)?;
    // No trailing newline: the whole file is the passphrase.
    fs::write(KEY, b"correct horse")?;
    fs::write(WRONG_KEY, b"correct horse\n")
}

fn steps(steps: &[&str]) -> Vec<String> {
    steps.iter().map(|s| (*s).to_owned()).collect()
}

fn lvm_on_luks() -> Result<Served, Failure> {
    let served = serve(
        "ferric-blk-lol",
        &steps(&[
            "dev:/tmp/ferric-blocklayer-lvm-on-luks.img",
            &format!("luks:{KEY}"),
            "lvm:inner",
        ]),
    )?;
    check(&served.root, 64 * 1024, 6)?;
    Ok(served)
}

fn luks_on_lvm() -> Result<Served, Failure> {
    let served = serve(
        "ferric-blk-lul",
        &steps(&[
            "dev:/tmp/ferric-blocklayer-lvm.img",
            "lvm:sealed",
            &format!("luks:{KEY}"),
        ]),
    )?;
    check(&served.root, 128 * 1024, 5)?;
    // The LV jumps from PE 6 to PE 9 at 64 KiB into the decrypted data.
    let file = io("open /scheme/ferric-blk-lul", File::open(&served.root))?;
    let mut run = vec![0; 8192];
    io(
        "read across the extent jump",
        file.read_exact_at(&mut run, 60 * 1024),
    )?;
    same(
        "the bytes across the extent jump",
        run,
        pattern(5, 60 * 1024, 8192),
    )?;
    Ok(served)
}

fn luks1() -> Result<Served, Failure> {
    let served = serve(
        "ferric-blk-l1",
        &steps(&[
            "dev:/tmp/ferric-blocklayer-luks1.img",
            &format!("luks:{KEY}"),
        ]),
    )?;
    check(&served.root, KEPT as u64, 3)?;
    Ok(served)
}

/// The volumes ferric-unlock unlocked and mounted at boot, before this test started, must be
/// served: LVM on LUKS from a key file, and with ext4d an ext4 volume through ferric-blockd.
fn unlock_boot() -> Result<(), Failure> {
    check(BOOT_LOL, 64 * 1024, 6)?;
    if !Path::new(EXT4D).exists() {
        println!("ferric-blocklayer-test: no {EXT4D} (fs.ext4 is off), so no file system to read");
        return Ok(());
    }
    let hello = format!("{BOOT_EXT4}/hello.txt");
    same(
        format!("{hello} on the volume unlocked at boot"),
        io(format!("read {hello}"), fs::read(&hello))?,
        b"hello from ext4\n".to_vec(),
    )
}

/// `result` must have failed with `errno`.
fn fails_with<T>(
    what: &'static str,
    errno: i32,
    want: &'static str,
    result: io::Result<T>,
) -> Result<(), Failure> {
    match result {
        Ok(_) => Err(Failure::Allowed { what }),
        Err(err) if err.raw_os_error() == Some(errno) => Ok(()),
        Err(source) => Err(Failure::WrongErrno { what, want, source }),
    }
}

fn read_only(served: &Served) -> Result<(), Failure> {
    let root = &served.root;
    fails_with(
        "opening the volume for writing",
        libc::EROFS,
        "EROFS",
        OpenOptions::new().write(true).open(root),
    )?;
    fails_with(
        "opening the volume read-write",
        libc::EROFS,
        "EROFS",
        OpenOptions::new().read(true).write(true).open(root),
    )
}

/// Runs this test again as UID 2000 to check it cannot open `root`.
fn access(served: &Served) -> Result<(), Failure> {
    let status = io(
        "run the unprivileged open",
        Command::new("/usr/bin/ferric-blocklayer-test")
            .args(["--access-check", &served.root])
            .uid(2000)
            .gid(2000)
            .status(),
    )?;
    same("the unprivileged open is refused", status.success(), true)
}

fn access_child(root: &str) -> Result<(), Failure> {
    fails_with(
        "opening the volume as UID 2000",
        libc::EACCES,
        "EACCES",
        File::open(root),
    )
}

/// A wrong passphrase (the right one plus a newline) must make the daemon exit, unserved.
fn refuse() -> Result<(), Failure> {
    let mut child = spawn(
        "ferric-blk-bad",
        &steps(&[
            "dev:/tmp/ferric-blocklayer-lvm-on-luks.img",
            &format!("luks:{WRONG_KEY}"),
        ]),
    )?;
    let status = io("wait for ferric-blockd", child.wait())?;
    same(
        "ferric-blockd's exit with a wrong key",
        status.success(),
        false,
    )?;
    same(
        "/scheme/ferric-blk-bad exists",
        Path::new("/scheme/ferric-blk-bad").exists(),
        false,
    )
}

fn report<T>(step: &str, result: Result<T, Failure>) -> Option<T> {
    match result {
        Ok(value) => {
            println!("FERRIC_TEST_PASS blocklayer-{step}");
            Some(value)
        }
        Err(err) => {
            let mut line = format!("FERRIC_TEST_FAIL blocklayer-{step}: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                line.push_str(&format!(": {cause}"));
                source = cause.source();
            }
            println!("{line}");
            None
        }
    }
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, root] = args.as_slice()
        && flag == "--access-check"
    {
        return match report("access-child", access_child(root)) {
            Some(()) => ExitCode::SUCCESS,
            None => ExitCode::FAILURE,
        };
    }
    if !Path::new(BLOCKD).exists() {
        println!("FERRIC_TEST_SKIP blocklayer: no {BLOCKD} (fs.luks and fs.lvm are off)");
        return ExitCode::SUCCESS;
    }
    if report("image", io("write the images", write_images())).is_none() {
        return ExitCode::FAILURE;
    }
    let mut ok = true;
    let lol = report("lvm-on-luks", lvm_on_luks());
    ok &= lol.is_some();
    ok &= report("luks-on-lvm", luks_on_lvm()).is_some();
    ok &= report("luks1", luks1()).is_some();
    match &lol {
        Some(served) => {
            ok &= report("erofs", read_only(served)).is_some();
            ok &= report("access", access(served)).is_some();
        }
        None => ok = false,
    }
    ok &= report("refuse", refuse()).is_some();
    if Path::new(UNLOCK).exists() {
        ok &= report("unlock-boot", unlock_boot()).is_some();
    } else {
        println!("FERRIC_TEST_SKIP blocklayer-unlock-boot: no {UNLOCK} (fs.unlock_at_boot is off)");
    }
    drop(lol);
    for path in [
        "/tmp/ferric-blocklayer-lvm-on-luks.img",
        "/tmp/ferric-blocklayer-lvm.img",
        "/tmp/ferric-blocklayer-luks1.img",
        KEY,
        WRONG_KEY,
    ] {
        let _ = fs::remove_file(path);
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
