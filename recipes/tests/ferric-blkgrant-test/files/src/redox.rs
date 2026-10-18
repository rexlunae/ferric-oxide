//! The test proper; see the crate documentation for the steps.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ferric_blkgrant::abi::{Request as DiskRequest, Slot, Transfer};
use ferric_regbuf::abi::{
    Access, BytesError, GrantId, GrantInfo, GrantSpec, REGION_PATH, Request, SCHEME_NAME,
};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_DIRECTORY, O_RDONLY, O_RDWR};
use syscall::CallFlags;
use syscall::error::{EACCES, ECANCELED, EINVAL, EIO, ENODEV, ENOENT, EOPNOTSUPP, EOVERFLOW};

/// The registered buffer's length.
const BUF_LEN: usize = 4 << 20;
/// The page size grants are aligned to.
const PAGE: usize = 4096;
/// Bytes compared in the read step.
const READ_LEN: usize = 1 << 20;
/// Bytes written (and put back) in the write step.
const WRITE_LEN: usize = 64 << 10;
/// Request sizes the benchmark times, with how many bytes it reads at each.
const BENCH: [(usize, usize); 2] = [(128 << 10, 32 << 20), (4 << 10, 8 << 20)];
/// The slot of the read-write grant over the whole buffer.
const SLOT_RW: u64 = 0;
/// The slot of a read-only grant of the first page.
const SLOT_RO: u64 = 1;
/// A slot the test never fills.
const SLOT_EMPTY: u64 = 9;
/// What the application puts in the buffer before revoking.
const FILL: u8 = 0xa5;

/// Why a step failed.
#[derive(Debug)]
enum Failure {
    /// A call failed with an errno.
    Errno { what: &'static str, errno: i32 },
    /// A call that must fail with `expected` returned `got` (an errno, or 0 for success).
    Expected {
        what: &'static str,
        expected: i32,
        got: i32,
    },
    /// Memory or the disk did not hold what it should.
    Data { what: &'static str },
    /// The kernel answered with a payload the ABI crate rejects.
    Payload(BytesError),
    /// The disk has no partition the test can use.
    NoPartition,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno { what, errno } => write!(f, "{what} failed with errno {errno}"),
            Self::Expected {
                what,
                expected,
                got,
            } => write!(f, "{what} should fail with errno {expected}, got {got}"),
            Self::Data { what } => write!(f, "{what}: wrong contents"),
            Self::Payload(err) => write!(f, "bad payload from the kernel: {err}"),
            Self::NoPartition => f.write_str("the NVMe disk has no partitions to test on"),
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Payload(err) => Some(err),
            _ => None,
        }
    }
}

/// Wraps a libredox result with what it was doing.
fn ctx<T>(what: &'static str, r: libredox::error::Result<T>) -> Result<T, Failure> {
    r.map_err(|e| Failure::Errno {
        what,
        errno: e.errno(),
    })
}

/// Wraps a `std::io` result with what it was doing.
fn io<T>(what: &'static str, r: std::io::Result<T>) -> Result<T, Failure> {
    r.map_err(|e| Failure::Errno {
        what,
        errno: e.raw_os_error().unwrap_or(EIO),
    })
}

/// Fails unless `r` failed with `expected`.
fn expect_errno<T>(
    what: &'static str,
    expected: i32,
    r: Result<T, Failure>,
) -> Result<(), Failure> {
    let got = match r {
        Ok(_) => 0,
        Err(Failure::Errno { errno, .. }) => errno,
        Err(other) => return Err(other),
    };
    if got == expected {
        Ok(())
    } else {
        Err(Failure::Expected {
            what,
            expected,
            got,
        })
    }
}

/// A page-aligned anonymous buffer of [`BUF_LEN`] bytes.
struct Buffer {
    ptr: *mut u8,
}

impl Buffer {
    fn new() -> Result<Self, Failure> {
        // SAFETY: a fresh anonymous mapping at an address the kernel chooses.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                BUF_LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Failure::Errno {
                what: "mmap the buffer",
                errno: std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(EIO),
            });
        }
        Ok(Self { ptr: ptr.cast() })
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the buffer is BUF_LEN bytes of this process's memory, mapped until drop. The
        // controller writes it by DMA only while a grant call is in progress, and no borrow
        // from here is held across one.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, BUF_LEN) }
    }

    fn addr(&self) -> u64 {
        self.ptr as u64
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: unmaps the buffer's own mapping, which nothing uses afterwards.
        unsafe { libc::munmap(self.ptr.cast(), BUF_LEN) };
    }
}

/// A call on a regbuf descriptor without payload.
fn regbuf_call(fd: &Fd, what: &'static str, request: Request) -> Result<usize, Failure> {
    let (words, n) = request.encode();
    ctx(what, fd.call_wo(&[], CallFlags::empty(), &words[..n]))
}

/// Opens a grant of `region` and returns it with its ID.
fn grant(region: &Fd, access: Access, offset: u64, len: u64) -> Result<(Fd, GrantId), Failure> {
    let spec = GrantSpec::new(access, offset, len).map_err(|_| Failure::Errno {
        what: "grant range",
        errno: EINVAL,
    })?;
    let mut path = [0; GrantSpec::PATH_MAX];
    let path = spec.encode_path(&mut path);
    let fd = ctx("open a grant", region.openat(path, O_RDWR | O_CLOEXEC, 0))?;
    let mut info = [0; GrantInfo::BYTES];
    let (words, n) = Request::Info.encode();
    ctx(
        "Info",
        fd.call_ro(&mut info, CallFlags::empty(), &words[..n]),
    )?;
    let info = GrantInfo::from_bytes(&info).map_err(Failure::Payload)?;
    Ok((fd, info.id))
}

/// A partition descriptor of the NVMe disk scheme.
struct Disk {
    file: File,
    /// Its name in the scheme (`1p2`, say).
    name: String,
    block: u64,
    len: u64,
}

impl Disk {
    fn open(scheme: &str, name: &str) -> Result<Self, Failure> {
        let file = io(
            "open a partition",
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/scheme/{scheme}/{name}")),
        )?;
        let meta = io("stat a partition", file.metadata())?;
        Ok(Self {
            file,
            name: name.to_owned(),
            block: std::os::unix::fs::MetadataExt::blksize(&meta).max(512),
            len: meta.len(),
        })
    }

    fn raw(&self) -> usize {
        self.file.as_raw_fd() as usize
    }

    /// Moves `grant` to the driver into `slot`.
    fn attach(&self, grant: Fd, slot: u64) -> Result<(), Failure> {
        // The descriptor moves to the driver, so it must not be closed here.
        let raw = grant.into_raw();
        ctx(
            "attach the grant",
            libredox::call::call_wo(self.raw(), &raw.to_ne_bytes(), CallFlags::FD, &[slot]),
        )?;
        Ok(())
    }

    /// A grant call.
    fn call(&self, what: &'static str, request: DiskRequest) -> Result<usize, Failure> {
        let (words, n) = request.encode();
        ctx(
            what,
            libredox::call::call_wo(self.raw(), &[], CallFlags::empty(), &words[..n]),
        )
    }

    /// Reads `len` bytes from byte `at` of the partition into `slot`'s grant at `offset`.
    fn read_grant(&self, slot: u64, offset: usize, at: u64, len: usize) -> Result<(), Failure> {
        let transfer = self.transfer(slot, offset, at, len)?;
        self.call("read through the grant", DiskRequest::Read(transfer))?;
        Ok(())
    }

    fn transfer(&self, slot: u64, offset: usize, at: u64, len: usize) -> Result<Transfer, Failure> {
        Ok(Transfer {
            slot: Slot::new(slot).ok_or(Failure::Errno {
                what: "slot",
                errno: EINVAL,
            })?,
            offset: offset as u64,
            lba: at / self.block,
            len: len as u64,
        })
    }
}

/// The NVMe disk scheme's name, if there is one.
fn nvme_scheme() -> Option<String> {
    std::fs::read_dir("/scheme")
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|name| name.starts_with("disk.") && name.ends_with("-nvme"))
}

/// The partitions the scheme lists (`NpM`).
fn partitions(scheme: &str) -> Result<Vec<String>, Failure> {
    let dir = ctx(
        "list the disk scheme",
        Fd::open(
            &format!("/scheme/{scheme}"),
            O_RDONLY | O_DIRECTORY | O_CLOEXEC,
            0,
        ),
    )?;
    let mut list = vec![0; 4096];
    let n = ctx("read the disk list", dir.read(&mut list))?;
    list.truncate(n);
    Ok(String::from_utf8_lossy(&list)
        .lines()
        .filter(|line| line.contains('p'))
        .map(str::to_owned)
        .collect())
}

/// Everything the steps share.
struct Test {
    /// The largest partition, which reads come from.
    read: Disk,
    /// The BIOS boot partition (`p0`), which the write step writes and restores.
    write: Disk,
    region: Fd,
    rw: GrantId,
    buffer: Buffer,
}

fn read_step(t: &mut Test) -> Result<(), Failure> {
    let mut expected = vec![0; READ_LEN];
    io("pread", t.read.file.read_at(&mut expected, 0))?;
    t.buffer.bytes()[..READ_LEN].fill(0);
    t.read.read_grant(SLOT_RW, 0, 0, READ_LEN)?;
    if t.buffer.bytes()[..READ_LEN] != expected[..] {
        return Err(Failure::Data {
            what: "blocks read through the grant",
        });
    }
    // The same blocks again, a megabyte into the grant.
    t.read.read_grant(SLOT_RW, READ_LEN, 0, READ_LEN)?;
    if t.buffer.bytes()[READ_LEN..2 * READ_LEN] != expected[..] {
        return Err(Failure::Data {
            what: "blocks read at an offset in the grant",
        });
    }
    Ok(())
}

fn write_step(t: &mut Test) -> Result<(), Failure> {
    let disk = &t.write;
    let mut original = vec![0; WRITE_LEN];
    io(
        "pread the original blocks",
        disk.file.read_at(&mut original, 0),
    )?;
    let pattern: Vec<u8> = (0..WRITE_LEN).map(|i| (i * 7 + 3) as u8).collect();
    t.buffer.bytes()[..WRITE_LEN].copy_from_slice(&pattern);
    let transfer = disk.transfer(SLOT_RW, 0, 0, WRITE_LEN)?;
    let written = disk.call("write through the grant", DiskRequest::Write(transfer));
    let mut back = vec![0; WRITE_LEN];
    let read_back = io("pread the written blocks", disk.file.read_at(&mut back, 0));
    // Put the blocks back whatever happened.
    io("restore the blocks", disk.file.write_at(&original, 0))?;
    written?;
    read_back?;
    if back != pattern {
        return Err(Failure::Data {
            what: "blocks written through the grant",
        });
    }
    io("pread the restored blocks", disk.file.read_at(&mut back, 0))?;
    if back != original {
        return Err(Failure::Data {
            what: "restored blocks",
        });
    }
    Ok(())
}

fn checks_step(t: &mut Test) -> Result<(), Failure> {
    let (ro, _) = grant(&t.region, Access::Read, 0, PAGE as u64)?;
    let disk = &t.read;
    disk.attach(ro, SLOT_RO)?;
    let page = PAGE;
    expect_errno(
        "a read into a read-only grant",
        EACCES,
        disk.read_grant(SLOT_RO, 0, 0, page),
    )?;
    expect_errno(
        "an unaligned offset in the grant",
        EINVAL,
        disk.read_grant(SLOT_RW, 512, 0, 512),
    )?;
    expect_errno(
        "a range past the grant",
        EINVAL,
        disk.read_grant(SLOT_RW, BUF_LEN - page, 0, 2 * page),
    )?;
    expect_errno(
        "an empty slot",
        ENOENT,
        disk.read_grant(SLOT_EMPTY, 0, 0, page),
    )?;
    let last = disk.len - disk.block;
    expect_errno(
        "a range past the partition",
        EOVERFLOW,
        disk.read_grant(SLOT_RW, 0, last, 2 * disk.block as usize),
    )?;
    let detach = |what| {
        let slot = Slot::new(SLOT_RO).expect("slot 1 exists");
        disk.call(what, DiskRequest::Detach { slot })
    };
    detach("detach the read-only grant")?;
    expect_errno("a second detach", ENOENT, detach("detach it again"))
}

fn revoke_step(t: &mut Test) -> Result<(), Failure> {
    t.buffer.bytes()[..READ_LEN].fill(FILL);
    regbuf_call(&t.region, "Revoke", Request::Revoke(t.rw))?;
    expect_errno(
        "a read through the revoked grant",
        ECANCELED,
        t.read.read_grant(SLOT_RW, 0, 0, READ_LEN),
    )?;
    if t.buffer.bytes()[..READ_LEN].iter().any(|&b| b != FILL) {
        return Err(Failure::Data {
            what: "the buffer after the revoke",
        });
    }
    Ok(())
}

fn detach_step(t: &mut Test) -> Result<(), Failure> {
    let slot = Slot::new(SLOT_RW).expect("slot 0 exists");
    t.read
        .call("detach the revoked grant", DiskRequest::Detach { slot })?;
    Ok(())
}

/// Bytes per second, as MiB/s.
fn mib_s(bytes: usize, time: Duration) -> f64 {
    bytes as f64 / time.as_secs_f64().max(1e-9) / (1024.0 * 1024.0)
}

/// Times `count` reads of `size` bytes, sequential from the partition's start (wrapping at
/// its end), each done by `read(i, at)`.
fn time_reads(
    disk: &Disk,
    size: usize,
    total: usize,
    mut read: impl FnMut(usize, u64) -> Result<(), Failure>,
) -> Result<Duration, Failure> {
    let span = (disk.len / size as u64).max(1) * size as u64;
    let start = Instant::now();
    for i in 0..total / size {
        read(i, (i as u64 * size as u64) % span)?;
    }
    Ok(start.elapsed())
}

/// Prints one `FERRIC_BENCH` line for reads of `size` bytes: with `pread` and, when `grant`
/// is set, through the grant in [`SLOT_RW`].
fn bench(read: &Disk, size: usize, total: usize, grant: bool) -> Result<(), Failure> {
    let mut copy = vec![0; size];
    let copied = time_reads(read, size, total, |_, at| {
        io("pread", read.file.read_at(&mut copy, at)).map(drop)
    })?;
    let label = if size >= 1 << 20 {
        format!("{}m", size >> 20)
    } else {
        format!("{}k", size >> 10)
    };
    let ops = (total / size) as f64;
    let mut line = format!(
        "FERRIC_BENCH nvme-read-{label} bytes={total} copy_mib_s={:.1} copy_iops={:.0}",
        mib_s(total, copied),
        ops / copied.as_secs_f64().max(1e-9)
    );
    if grant {
        let slots = BUF_LEN / size;
        let granted = time_reads(read, size, total, |i, at| {
            read.read_grant(SLOT_RW, (i % slots) * size, at, size)
        })?;
        line.push_str(&format!(
            " grant_mib_s={:.1} grant_iops={:.0} speedup={:.2}",
            mib_s(total, granted),
            ops / granted.as_secs_f64().max(1e-9),
            copied.as_secs_f64() / granted.as_secs_f64().max(1e-9)
        ));
    }
    println!("{line}");
    Ok(())
}

/// One step of the test.
type Step = fn(&mut Test) -> Result<(), Failure>;

/// Runs one step and reports it.
fn step(name: &str, t: &mut Test, f: Step) -> Result<(), ()> {
    match f(t) {
        Ok(()) => {
            println!("FERRIC_TEST_PASS {name}");
            Ok(())
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL {name}: {err}");
            Err(())
        }
    }
}

/// What setting up found.
enum Setup {
    /// Ready to test.
    Ready(Test),
    /// The grant steps cannot run; the reason, and the partition to benchmark `pread` on.
    Skip(&'static str, Option<Disk>),
}

fn setup() -> Result<Setup, Failure> {
    let Some(scheme) = nvme_scheme() else {
        return Ok(Setup::Skip("no NVMe disk scheme", None));
    };
    let names = partitions(&scheme)?;
    let mut disks = names
        .iter()
        .map(|name| Disk::open(&scheme, name))
        .collect::<Result<Vec<_>, _>>()?;
    let write_at = disks
        .iter()
        .position(|d| d.name.ends_with("p0") && d.len >= WRITE_LEN as u64)
        .ok_or(Failure::NoPartition)?;
    let write = disks.remove(write_at);
    let read = disks
        .into_iter()
        .max_by_key(|d| d.len)
        .filter(|d| d.len >= (2 * READ_LEN) as u64)
        .ok_or(Failure::NoPartition)?;
    println!(
        "nvme-grant: reading {scheme}/{} ({} MiB, {}-byte blocks), writing {scheme}/{}",
        read.name,
        read.len >> 20,
        read.block,
        write.name
    );
    let region_path = format!("/scheme/{SCHEME_NAME}/{REGION_PATH}");
    let region = match Fd::open(&region_path, O_RDWR | O_CLOEXEC, 0) {
        Ok(region) => region,
        Err(_) => return Ok(Setup::Skip("no kernel.regbuf scheme", Some(read))),
    };
    let buffer = Buffer::new()?;
    regbuf_call(
        &region,
        "Register",
        Request::Register {
            addr: buffer.addr(),
            len: BUF_LEN as u64,
        },
    )?;
    let (rw, id) = grant(&region, Access::ReadWrite, 0, BUF_LEN as u64)?;
    match read.attach(rw, SLOT_RW) {
        Err(Failure::Errno { errno, .. }) if errno == EOPNOTSUPP => {
            return Ok(Setup::Skip(
                "nvmed takes no grants (built without drivers.grant_dma)",
                Some(read),
            ));
        }
        Err(Failure::Errno { errno, .. }) if errno == ENODEV => {
            return Ok(Setup::Skip(
                "nvmed has no DMA domain (no IOMMU)",
                Some(read),
            ));
        }
        other => other?,
    }
    // The write partition is another descriptor: attach the same buffer there too.
    let (rw2, _) = grant(&region, Access::ReadWrite, 0, BUF_LEN as u64)?;
    write.attach(rw2, SLOT_RW)?;
    Ok(Setup::Ready(Test {
        read,
        write,
        region,
        rw: id,
        buffer,
    }))
}

pub fn main() -> ExitCode {
    let mut t = match setup() {
        Ok(Setup::Ready(t)) => t,
        Ok(Setup::Skip(why, read)) => {
            println!("FERRIC_TEST_SKIP nvme-grant: {why}");
            if let Some(read) = read {
                for (size, total) in BENCH {
                    if let Err(err) = bench(&read, size, total, false) {
                        println!("FERRIC_TEST_FAIL nvme-grant-bench: {err}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL nvme-grant-attach: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("FERRIC_TEST_PASS nvme-grant-attach");
    let steps: [(&str, Step); 3] = [
        ("nvme-grant-read", read_step),
        ("nvme-grant-write", write_step),
        ("nvme-grant-checks", checks_step),
    ];
    for (name, f) in steps {
        if step(name, &mut t, f).is_err() {
            return ExitCode::FAILURE;
        }
    }
    for (size, total) in BENCH {
        if let Err(err) = bench(&t.read, size, total, true) {
            println!("FERRIC_TEST_FAIL nvme-grant-bench: {err}");
            return ExitCode::FAILURE;
        }
    }
    println!("FERRIC_TEST_PASS nvme-grant-bench");
    let steps: [(&str, Step); 2] = [
        ("nvme-grant-revoke", revoke_step),
        ("nvme-grant-detach", detach_step),
    ];
    for (name, f) in steps {
        if step(name, &mut t, f).is_err() {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
