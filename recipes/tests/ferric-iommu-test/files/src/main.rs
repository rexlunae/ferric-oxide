//! DMA domain and DMA remapping test driver for Ferric Oxide.
//!
//! pcid-spawner starts this program for QEMU's `edu` device (vendor 1234h, device 11E8h; see
//! `pcid.toml`), which CI adds to its QEMU command line, with the device's pcid channel in
//! `PCID_CLIENT_CHANNEL`, like any PCI driver. `edu` can copy between its own 4 KiB buffer and
//! memory by DMA, so this program can make a real device read and write memory and see what
//! the IOMMU lets through (io/iommu-design.md in the project files, sections 1 and 2):
//!
//! 1. it maps the device's registers and allocates a two-page DMA buffer and one *victim* page,
//!    and translates them while it has no domain: `memory:translation` gives physical
//!    addresses under `kernel.iommu = "auto"` and refuses with `EACCES` under `"enforce"`
//!    (`iommu-unbound`);
//! 2. it sends `GetDmaDomain` on the channel; pcid creates a domain in the kernel's `iommu`
//!    scheme and attaches the device to it. The program receives the domain's descriptor and
//!    binds it, and checks that a second bind is refused (`iommu-domain`);
//! 3. it translates the buffer again: now it gets IOVAs, the same IOVA each time, contiguous
//!    across the buffer's two pages, and different from the physical address;
//! 4. it has the device copy the buffer's first page into the device and back out into the
//!    second page, both at their IOVAs, and checks the copy (`iommu-dma`);
//! 5. it has the device write to the victim page's *physical* address, which it never
//!    translated in the domain, and checks the victim is unchanged (`iommu-fault`); the kernel
//!    logs a DMA fault for that address. Under `enforce` there is no physical address to
//!    use, so the device writes [`UNMAPPED_IOVA`], which the domain never maps, instead;
//! 6. it translates the victim, has the device write it at its IOVA (which works), then
//!    takes write access away with `mprotect` and has the device write it again: the kernel
//!    must have made the mapping read-only for the device and invalidated the IOTLB, so the
//!    write faults and the victim keeps what the CPU wrote (`iommu-readonly`);
//! 7. it forks; the child has an address space of its own, so it has no bound domain: its
//!    translation fails with `EACCES` under `enforce` (and gives a physical address under
//!    `auto`), and binding the domain descriptor it inherited is refused while the parent has
//!    it bound (`iommu-fork`);
//! 8. it unmaps the buffer and has the device read the old IOVA of its second page, which the
//!    kernel must have unmapped (strictly: entries cleared, IOTLB invalidated) when the memory
//!    went away; the kernel logs a release and a second DMA fault (`iommu-release`).
//!
//! For each fault it expects, it prints `FERRIC_IOMMU_EXPECT_FAULT <read|write> <address>`;
//! the vtd boot CI job checks the kernel logged exactly those faults, from this device. When
//! the program exits the domain closes and the kernel blocks the device.
//!
//! Each test prints `FERRIC_TEST_PASS <name>`, `FERRIC_TEST_FAIL <name>`, or
//! `FERRIC_TEST_SKIP <name>` when pcid has no domain to give (no IOMMU, or a kernel built with
//! `kernel.iommu = "never"`).
//!
//! The channel speaks bincode-encoded `pcid_interface` enums. This program has no
//! dependencies, so it writes the few messages it needs by hand: bincode encodes an enum
//! variant as its index, a little-endian `u32`, followed by its fields in little-endian order.
//! These indices follow the variant order in
//! `userspace/base/drivers/pcid/src/driver_interface/mod.rs`. The `edu` register layout is
//! QEMU's `docs/specs/edu.rst`.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::ExitCode;
use std::time::Duration;

/// `PcidClientRequest::ReadConfig(u16)`.
const READ_CONFIG: u32 = 7;
/// `PcidClientRequest::GetDmaDomain`.
const GET_DMA_DOMAIN: u32 = 9;
/// `PcidClientResponse::Error`.
const RESPONSE_ERROR: u32 = 6;
/// `PcidClientResponse::ReadConfig(u32)`.
const RESPONSE_READ_CONFIG: u32 = 9;
/// `PcidClientResponse::DmaDomain`.
const RESPONSE_DMA_DOMAIN: u32 = 11;

/// `ferric_iommu::abi::Verb::Bind`.
const VERB_BIND: u64 = 3;

/// `CallFlags::WRITE`, `CallFlags::READ` and `CallFlags::FD` from `redox_syscall`. relibc
/// passes the payload to the kernel only with READ, WRITE or both set.
const CALL_WRITE: usize = 1 << 9;
const CALL_READ: usize = 1 << 10;
const CALL_FD: usize = 1 << 11;

/// `EBUSY` and `EACCES`.
const EBUSY: usize = 16;
const EACCES: usize = 13;

/// An IOVA the test's domain never maps: the allocator hands out the lowest free block, and the
/// domain holds a few pages.
const UNMAPPED_IOVA: usize = 0x8000_0000;

/// The page size.
const PAGE: usize = 4096;

/// QEMU `edu`: vendor and device ID, as configuration dword 0 reads them.
const EDU_ID: u32 = 0x11e8_1234;
/// `edu` register 0x00: the identification value (major 1, minor 0, `0xed`).
const EDU_IDENT: u32 = 0x0100_00ed;
/// `edu` DMA registers: source address, destination address, count, command.
const EDU_DMA_SRC: usize = 0x80;
const EDU_DMA_DST: usize = 0x88;
const EDU_DMA_COUNT: usize = 0x90;
const EDU_DMA_CMD: usize = 0x98;
/// `edu` command bits: start the transfer; direction from the device to memory.
const EDU_CMD_RUN: u64 = 1;
const EDU_CMD_TO_RAM: u64 = 2;
/// Where the device's DMA buffer sits in its own address space.
const EDU_BUFFER: u64 = 0x4_0000;

/// The byte the victim page is filled with.
const CANARY: u8 = 0xa5;

/// Why a test failed.
enum Failure {
    /// An environment variable is missing or not a descriptor number.
    Env(&'static str),
    /// Talking to pcid or opening a scheme failed.
    Io(std::io::Error),
    /// pcid answered with something unexpected.
    Response(Vec<u8>),
    /// A system call failed with this errno.
    Call(&'static str, usize),
    /// A second bind succeeded.
    SecondBind,
    /// The device is not QEMU's `edu`.
    NotEdu(u32),
    /// The device's BAR 0 is not a 32-bit memory BAR.
    Bar(u32),
    /// A translation gave an unexpected answer.
    Translation(&'static str, usize, usize),
    /// A device transfer did not finish.
    DmaTimeout,
    /// The forked child reported this.
    Child(&'static str),
    /// Memory did not hold what the test expected after a transfer.
    Data(&'static str, usize, u8),
    /// The test did not run because an earlier step failed.
    NotRun,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(var) => write!(f, "{var} is not set to a descriptor"),
            Self::Io(err) => write!(f, "I/O failed: {err}"),
            Self::Response(bytes) => write!(f, "unexpected pcid response {bytes:02x?}"),
            Self::Call(what, errno) => write!(f, "{what} failed with errno {errno}"),
            Self::SecondBind => f.write_str("a second bind was not refused"),
            Self::NotEdu(id) => write!(f, "the device is {id:#010x}, not QEMU edu"),
            Self::Bar(bar) => write!(f, "BAR 0 is {bar:#x}, not a 32-bit memory BAR"),
            Self::Translation(what, got, other) => {
                write!(f, "{what}: got {got:#x}, compared with {other:#x}")
            }
            Self::DmaTimeout => f.write_str("the device did not finish a transfer"),
            Self::Child(what) => write!(f, "in the forked child: {what}"),
            Self::Data(what, offset, byte) => {
                write!(f, "{what}: byte {offset:#x} is {byte:#04x}")
            }
            Self::NotRun => f.write_str("not run: an earlier step failed"),
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// The few relibc calls this program makes, with stand-ins on other systems so it builds
/// there.
mod sys {
    #[cfg(target_os = "redox")]
    unsafe extern "C" {
        /// relibc's `SYS_CALL` wrapper. Returns the result, or the errno negated.
        fn redox_sys_call_v0(
            fd: usize,
            payload: *mut u8,
            payload_len: usize,
            flags: usize,
            metadata: *const u64,
            metadata_len: usize,
        ) -> usize;
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn fork() -> i32;
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
        fn _exit(status: i32) -> !;
    }

    /// relibc's `PROT_READ`, and `PROT_READ | PROT_WRITE`.
    const PROT_READ: i32 = 0x4;
    const PROT_RW: i32 = PROT_READ | 0x2;
    /// relibc's `MAP_SHARED` and `MAP_PRIVATE`.
    pub const MAP_SHARED: i32 = 1;
    pub const MAP_PRIVATE: i32 = 2;

    /// `SYS_CALL` on `fd`, returning the errno on failure.
    #[cfg(target_os = "redox")]
    pub fn call(
        fd: usize,
        payload: &mut [u8],
        flags: usize,
        metadata: &[u64],
    ) -> Result<usize, usize> {
        // SAFETY: the pointers and lengths describe live slices for the duration of the call.
        let ret = unsafe {
            redox_sys_call_v0(
                fd,
                payload.as_mut_ptr(),
                payload.len(),
                flags,
                metadata.as_ptr(),
                metadata.len(),
            )
        };
        if ret > usize::MAX - 4095 {
            Err(ret.wrapping_neg())
        } else {
            Ok(ret)
        }
    }

    /// Maps `len` bytes of `fd` at `offset`, readable and writable.
    #[cfg(target_os = "redox")]
    pub fn map(fd: i32, len: usize, flags: i32, offset: u64) -> std::io::Result<*mut u8> {
        let offset = i64::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        // SAFETY: a fresh mapping at an address the kernel chooses touches no existing memory.
        let ptr = unsafe { mmap(core::ptr::null_mut(), len, PROT_RW, flags, fd, offset) };
        if ptr as usize == usize::MAX {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ptr)
        }
    }

    /// Unmaps a mapping from [`map`]. Nothing may use it afterwards.
    #[cfg(target_os = "redox")]
    pub fn unmap(ptr: *mut u8, len: usize) -> std::io::Result<()> {
        // SAFETY: the caller gives a mapping from `map` and stops using it.
        if unsafe { munmap(ptr, len) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Makes a mapping from [`map`] read-only.
    #[cfg(target_os = "redox")]
    pub fn protect_read_only(ptr: *mut u8, len: usize) -> std::io::Result<()> {
        // SAFETY: the caller gives a mapping from `map`; taking write access away changes no
        // memory, and the caller stops writing it.
        if unsafe { mprotect(ptr, len, PROT_READ) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Runs `child` in a forked child, which exits with the code it returns, and returns that
    /// code.
    #[cfg(target_os = "redox")]
    pub fn in_child(child: impl FnOnce() -> i32) -> std::io::Result<i32> {
        // SAFETY: the program is single-threaded, so the child is a consistent copy; it only
        // makes system calls and exits without running the parent's destructors.
        let pid = unsafe { fork() };
        if pid < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid == 0 {
            let code = child();
            // SAFETY: ends the child at once, as `fork` requires.
            unsafe { _exit(code) }
        }
        let mut status = 0;
        // SAFETY: `status` is a live integer for the call to fill in.
        if unsafe { waitpid(pid, &mut status, 0) } != pid {
            return Err(std::io::Error::last_os_error());
        }
        // WIFEXITED and WEXITSTATUS.
        if status & 0x7f != 0 {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        Ok((status >> 8) & 0xff)
    }

    #[cfg(not(target_os = "redox"))]
    pub fn protect_read_only(_ptr: *mut u8, _len: usize) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(not(target_os = "redox"))]
    pub fn in_child(_child: impl FnOnce() -> i32) -> std::io::Result<i32> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(not(target_os = "redox"))]
    pub fn call(
        _fd: usize,
        _payload: &mut [u8],
        _flags: usize,
        _metadata: &[u64],
    ) -> Result<usize, usize> {
        Err(38) // ENOSYS
    }

    #[cfg(not(target_os = "redox"))]
    pub fn map(_fd: i32, _len: usize, _flags: i32, _offset: u64) -> std::io::Result<*mut u8> {
        let _ = (PROT_RW, PROT_READ);
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(not(target_os = "redox"))]
    pub fn unmap(_ptr: *mut u8, _len: usize) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

/// The descriptor number in environment variable `var`.
fn env_fd(var: &'static str) -> Result<i32, Failure> {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .ok_or(Failure::Env(var))
}

/// The pcid channel.
struct Channel {
    file: File,
}

impl Channel {
    /// Sends one request and returns the response's bytes.
    fn transact(&mut self, request: &[u8]) -> Result<Vec<u8>, Failure> {
        self.file.write_all(request)?;
        let mut len = [0_u8; 8];
        self.file.read_exact(&mut len)?;
        let len = u64::from_le_bytes(len);
        if len > 4096 {
            return Err(Failure::Response(len.to_le_bytes().to_vec()));
        }
        let mut response = vec![0_u8; len as usize];
        self.file.read_exact(&mut response)?;
        Ok(response)
    }

    /// Reads the configuration dword at `offset`.
    fn read_config(&mut self, offset: u16) -> Result<u32, Failure> {
        let mut request = READ_CONFIG.to_le_bytes().to_vec();
        request.extend_from_slice(&offset.to_le_bytes());
        let response = self.transact(&request)?;
        match response.as_slice() {
            [t0, t1, t2, t3, v0, v1, v2, v3]
                if [*t0, *t1, *t2, *t3] == RESPONSE_READ_CONFIG.to_le_bytes() =>
            {
                Ok(u32::from_le_bytes([*v0, *v1, *v2, *v3]))
            }
            _ => Err(Failure::Response(response)),
        }
    }

    /// Asks for a DMA domain and receives its descriptor; `None` when pcid has none to give.
    fn dma_domain(&mut self) -> Result<Option<usize>, Failure> {
        let response = self.transact(&GET_DMA_DOMAIN.to_le_bytes())?;
        match response.get(..4) {
            Some(tag) if tag == RESPONSE_DMA_DOMAIN.to_le_bytes() && response.len() == 4 => {}
            Some(tag) if tag == RESPONSE_ERROR.to_le_bytes() => return Ok(None),
            _ => return Err(Failure::Response(response)),
        }
        // usize::MAX asks the kernel to pick the descriptor number; any other value is the slot
        // to put it in, which must be free.
        let mut fd = usize::MAX.to_ne_bytes();
        sys::call(
            self.file.as_raw_fd() as usize,
            &mut fd,
            CALL_READ | CALL_FD,
            &[],
        )
        .map_err(|errno| Failure::Call("receiving the domain descriptor", errno))?;
        Ok(Some(usize::from_ne_bytes(fd)))
    }
}

/// A mapping of memory or registers.
struct Mapped {
    ptr: *mut u8,
    len: usize,
    /// Keeps the memory scheme handle open while mapped.
    _file: File,
}

impl Mapped {
    /// Maps `len` bytes of the memory scheme path `path` at `offset`.
    fn new(path: &str, len: usize, flags: i32, offset: u64) -> Result<Self, Failure> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let ptr = sys::map(file.as_raw_fd(), len, flags, offset)?;
        Ok(Self {
            ptr,
            len,
            _file: file,
        })
    }

    /// Zeroed, physically contiguous memory, as `common::dma` allocates DMA buffers.
    fn dma(len: usize) -> Result<Self, Failure> {
        Self::new(
            "/scheme/memory/zeroed@wb?phys_contiguous",
            len,
            sys::MAP_PRIVATE,
            0,
        )
    }

    /// The virtual address of byte `offset`.
    fn addr(&self, offset: usize) -> usize {
        self.ptr as usize + offset
    }

    /// The mapped bytes.
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` maps `len` readable and writable bytes of memory this value owns.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// The mapped bytes, to read only.
    fn view(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes of memory this value owns.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Takes write access away; only [`Mapped::view`] may be used afterwards.
    fn make_read_only(&mut self) -> Result<(), Failure> {
        sys::protect_read_only(self.ptr, self.len)?;
        Ok(())
    }

    /// Unmaps the memory.
    fn unmap(self) -> Result<(), Failure> {
        sys::unmap(self.ptr, self.len)?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        let _ = sys::unmap(self.ptr, self.len);
    }
}

/// QEMU's `edu` device, through its mapped registers.
struct Edu {
    regs: Mapped,
}

impl Edu {
    /// Reads the 32-bit register at `offset`.
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: `regs` maps the device's register page; `offset` is a register in it.
        unsafe { std::ptr::read_volatile(self.regs.ptr.add(offset).cast::<u32>()) }
    }

    /// Reads the 64-bit register at `offset`.
    fn read64(&self, offset: usize) -> u64 {
        // SAFETY: as in `read32`; the DMA registers take 64-bit accesses.
        unsafe { std::ptr::read_volatile(self.regs.ptr.add(offset).cast::<u64>()) }
    }

    /// Writes the 64-bit register at `offset`.
    fn write64(&self, offset: usize, value: u64) {
        // SAFETY: as in `read64`.
        unsafe { std::ptr::write_volatile(self.regs.ptr.add(offset).cast::<u64>(), value) }
    }

    /// Copies 4 KiB by DMA between the device's buffer and memory at `addr` (`to_ram` sets the
    /// direction), and waits until the device reports it done. The device finishes (and
    /// drops the data) even when the IOMMU refuses the access.
    fn dma(&self, addr: u64, to_ram: bool) -> Result<(), Failure> {
        let (src, dst) = if to_ram {
            (EDU_BUFFER, addr)
        } else {
            (addr, EDU_BUFFER)
        };
        self.write64(EDU_DMA_SRC, src);
        self.write64(EDU_DMA_DST, dst);
        self.write64(EDU_DMA_COUNT, PAGE as u64);
        let dir = if to_ram { EDU_CMD_TO_RAM } else { 0 };
        self.write64(EDU_DMA_CMD, EDU_CMD_RUN | dir);
        // QEMU runs the transfer from a 100 ms timer.
        for _ in 0..500 {
            if self.read64(EDU_DMA_CMD) & EDU_CMD_RUN == 0 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(Failure::DmaTimeout)
    }
}

/// `memory:translation`.
struct Translator {
    file: File,
}

impl Translator {
    /// The DMA address `memory:translation` gives the page at `virt`.
    fn translate(&self, virt: usize) -> Result<usize, Failure> {
        let mut buf = virt.to_ne_bytes();
        // The kernel reads the virtual address from the payload and writes the answer back.
        sys::call(
            self.file.as_raw_fd() as usize,
            &mut buf,
            CALL_READ | CALL_WRITE,
            &[],
        )
        .map_err(|errno| Failure::Call("translation", errno))?;
        Ok(usize::from_ne_bytes(buf))
    }
}

/// Fails with `what` unless every byte of `bytes` is `expected(offset)`.
fn check(what: &'static str, bytes: &[u8], expected: impl Fn(usize) -> u8) -> Result<(), Failure> {
    match bytes.iter().enumerate().find(|&(i, &b)| b != expected(i)) {
        Some((i, &b)) => Err(Failure::Data(what, i, b)),
        None => Ok(()),
    }
}

/// The pattern the device copies.
fn pattern(i: usize) -> u8 {
    (i.wrapping_mul(7) ^ (i >> 8)) as u8
}

/// The outcome of the tests that need a domain.
struct Outcome {
    unbound: Result<&'static str, Failure>,
    domain: Result<(), Failure>,
    dma: Result<(), Failure>,
    fault: Result<(), Failure>,
    readonly: Result<(), Failure>,
    fork: Result<&'static str, Failure>,
    release: Result<(), Failure>,
}

impl Outcome {
    /// Every test failed because of `err`, at a step before the first of them.
    fn all_failed(err: Failure) -> Self {
        Self {
            unbound: Err(Failure::NotRun),
            domain: Err(err),
            dma: Err(Failure::NotRun),
            fault: Err(Failure::NotRun),
            readonly: Err(Failure::NotRun),
            fork: Err(Failure::NotRun),
            release: Err(Failure::NotRun),
        }
    }
}

/// The test names, in the order they are reported.
const TESTS: [&str; 7] = [
    "iommu-unbound",
    "iommu-domain",
    "iommu-dma",
    "iommu-fault",
    "iommu-readonly",
    "iommu-fork",
    "iommu-release",
];

/// The device and memory the tests use, set up before the domain exists.
struct Setup {
    channel: Channel,
    edu: Edu,
    translator: Translator,
    buffer: Mapped,
    victim: Mapped,
    /// The physical addresses, when translation answers without a domain (not under
    /// `enforce`).
    buffer_phys: Option<usize>,
    victim_phys: Option<usize>,
    /// The domain's descriptor, once bound.
    domain: usize,
}

/// What translating without a domain gave: the physical address, or `None` when the kernel
/// refused (`EACCES`, `kernel.iommu = "enforce"`).
fn translate_unbound(t: &Translator, virt: usize) -> Result<Option<usize>, Failure> {
    match t.translate(virt) {
        Ok(phys) => Ok(Some(phys)),
        Err(Failure::Call(_, EACCES)) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Finds and maps the device, allocates the buffers and translates them without a domain.
fn setup(mut channel: Channel) -> Result<Setup, Failure> {
    let id = channel.read_config(0x00)?;
    if id != EDU_ID {
        return Err(Failure::NotEdu(id));
    }
    let bar = channel.read_config(0x10)?;
    // Memory space, 32-bit, so the low four bits hold flags and the rest the address.
    if bar & 0x7 != 0 || bar & !0xf == 0 {
        return Err(Failure::Bar(bar));
    }
    let regs = Mapped::new(
        "/scheme/memory/physical@uc",
        PAGE,
        sys::MAP_SHARED,
        u64::from(bar & !0xf),
    )?;
    let edu = Edu { regs };
    let ident = edu.read32(0x00);
    if ident != EDU_IDENT {
        return Err(Failure::NotEdu(ident));
    }
    let translator = Translator {
        file: OpenOptions::new()
            .read(true)
            .write(true)
            .open("/scheme/memory/translation")?,
    };
    let buffer = Mapped::dma(2 * PAGE)?;
    let mut victim = Mapped::dma(PAGE)?;
    victim.bytes().fill(CANARY);
    let buffer_phys = translate_unbound(&translator, buffer.addr(0))?;
    let victim_phys = translate_unbound(&translator, victim.addr(0))?;
    Ok(Setup {
        channel,
        edu,
        translator,
        buffer,
        victim,
        buffer_phys,
        victim_phys,
        domain: usize::MAX,
    })
}

/// Receives and binds the domain; `Ok(false)` when pcid has none.
fn bind(setup: &mut Setup) -> Result<bool, Failure> {
    let Some(domain) = setup.channel.dma_domain()? else {
        return Ok(false);
    };
    sys::call(domain, &mut [], 0, &[VERB_BIND]).map_err(|errno| Failure::Call("bind", errno))?;
    setup.domain = domain;
    match sys::call(domain, &mut [], 0, &[VERB_BIND]) {
        Err(EBUSY) => Ok(true),
        Err(errno) => Err(Failure::Call("the second bind", errno)),
        Ok(_) => Err(Failure::SecondBind),
    }
}

/// Translates the buffer in the domain and checks the IOVAs; returns the first page's.
fn translate_in_domain(setup: &Setup) -> Result<usize, Failure> {
    let t = &setup.translator;
    let iova = t.translate(setup.buffer.addr(0))?;
    if let Some(phys) = setup.buffer_phys.filter(|&phys| phys == iova) {
        return Err(Failure::Translation(
            "the IOVA equals the physical address",
            iova,
            phys,
        ));
    }
    let second = t.translate(setup.buffer.addr(PAGE))?;
    if second != iova + PAGE {
        return Err(Failure::Translation(
            "the second page is not contiguous",
            second,
            iova,
        ));
    }
    let again = t.translate(setup.buffer.addr(0))?;
    if again != iova {
        return Err(Failure::Translation(
            "a repeat translation moved",
            again,
            iova,
        ));
    }
    // The victim must lie outside what the domain maps for the buffer, or the fault test
    // would write the buffer instead.
    let target = setup.victim_phys.unwrap_or(UNMAPPED_IOVA);
    if (iova..iova + 2 * PAGE).contains(&target) {
        return Err(Failure::Translation(
            "the fault target overlaps the buffer's IOVAs",
            target,
            iova,
        ));
    }
    Ok(iova)
}

/// Copies the buffer's first page through the device into its second page, at their IOVAs.
fn dma_round_trip(setup: &mut Setup, iova: usize) -> Result<(), Failure> {
    let bytes = setup.buffer.bytes();
    let (src, dst) = bytes.split_at_mut(PAGE);
    for (i, b) in src.iter_mut().enumerate() {
        *b = pattern(i);
    }
    dst.fill(0);
    setup.edu.dma(iova as u64, false)?;
    setup.edu.dma((iova + PAGE) as u64, true)?;
    check(
        "the copy at the second IOVA",
        &setup.buffer.bytes()[PAGE..],
        pattern,
    )
}

/// Has the device write the victim's physical address (or, without one, an IOVA the domain
/// never maps), which the domain does not map.
fn dma_to_ungranted(setup: &mut Setup) -> Result<(), Failure> {
    let target = setup.victim_phys.unwrap_or(UNMAPPED_IOVA);
    println!("FERRIC_IOMMU_EXPECT_FAULT write {target:#x}");
    setup.edu.dma(target as u64, true)?;
    check("the victim page", setup.victim.view(), |_| CANARY)
}

/// Has the device write the victim at its IOVA, then again after `mprotect` took write access
/// away: the second write must fault. The device's buffer holds [`pattern`] from the round
/// trip.
fn dma_after_write_removed(setup: &mut Setup) -> Result<(), Failure> {
    let iova = setup.translator.translate(setup.victim.addr(0))?;
    setup.edu.dma(iova as u64, true)?;
    check(
        "the victim after a granted write",
        setup.victim.view(),
        pattern,
    )?;
    setup.victim.bytes().fill(CANARY);
    setup.victim.make_read_only()?;
    println!("FERRIC_IOMMU_EXPECT_FAULT write {iova:#x}");
    setup.edu.dma(iova as u64, true)?;
    check("the read-only victim", setup.victim.view(), |_| CANARY)
}

/// What the forked child reports, as its exit code.
const CHILD_REFUSED: i32 = 0;
const CHILD_PHYSICAL: i32 = 1;
const CHILD_TRANSLATE_FAILED: i32 = 2;
const CHILD_BIND_ALLOWED: i32 = 3;
const CHILD_BIND_FAILED: i32 = 4;

/// The forked child's checks: its translation of fresh DMA memory, and a bind of the domain
/// descriptor it inherited, which must be refused while the parent has the domain bound.
fn child_checks(translator: &Translator, domain: usize) -> i32 {
    match sys::call(domain, &mut [], 0, &[VERB_BIND]) {
        Err(EBUSY) => {}
        Err(_) => return CHILD_BIND_FAILED,
        Ok(_) => return CHILD_BIND_ALLOWED,
    }
    // Physically contiguous memory is not kept across a fork, so the child needs its own.
    let Ok(page) = Mapped::dma(PAGE) else {
        return CHILD_TRANSLATE_FAILED;
    };
    match translator.translate(page.addr(0)) {
        Err(Failure::Call(_, EACCES)) => CHILD_REFUSED,
        Ok(_) => CHILD_PHYSICAL,
        Err(_) => CHILD_TRANSLATE_FAILED,
    }
}

/// Forks, and checks that the child has no bound domain.
fn forked_child_unbound(setup: &Setup) -> Result<&'static str, Failure> {
    let code = sys::in_child(|| child_checks(&setup.translator, setup.domain))?;
    match code {
        CHILD_REFUSED => Ok(
            "the forked child is unbound: translation refused (EACCES), the inherited domain cannot be bound",
        ),
        CHILD_PHYSICAL => Ok(
            "the forked child is unbound: translation gives a physical address, the inherited domain cannot be bound",
        ),
        CHILD_BIND_ALLOWED => Err(Failure::Child("binding the parent's domain was allowed")),
        CHILD_BIND_FAILED => Err(Failure::Child(
            "binding the parent's domain failed, not with EBUSY",
        )),
        _ => Err(Failure::Child("translation failed, not with EACCES")),
    }
}

/// Unmaps the buffer, then has the device read its second page's old IOVA. A read, so a
/// kernel that failed to unmap it cannot corrupt the freed memory.
fn dma_after_release(setup: Setup, iova: usize) -> Result<(), Failure> {
    let Setup { buffer, edu, .. } = setup;
    buffer.unmap()?;
    println!("FERRIC_IOMMU_EXPECT_FAULT read {:#x}", iova + PAGE);
    edu.dma((iova + PAGE) as u64, false)
}

/// Runs the tests after setup; `None` when pcid had no domain to give.
fn run(mut setup: Setup) -> Option<Outcome> {
    let unbound = Ok(if setup.buffer_phys.is_some() {
        "translation without a domain gives a physical address"
    } else {
        "translation without a domain is refused (EACCES)"
    });
    match bind(&mut setup) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(err) => return Some(Outcome::all_failed(err)),
    }
    let iova = match translate_in_domain(&setup) {
        Ok(iova) => iova,
        Err(err) => {
            return Some(Outcome {
                unbound,
                domain: Ok(()),
                dma: Err(err),
                ..Outcome::all_failed(Failure::NotRun)
            });
        }
    };
    let dma = dma_round_trip(&mut setup, iova);
    let fault = dma_to_ungranted(&mut setup);
    let readonly = dma_after_write_removed(&mut setup);
    let fork = forked_child_unbound(&setup);
    let release = dma_after_release(setup, iova);
    Some(Outcome {
        unbound,
        domain: Ok(()),
        dma,
        fault,
        readonly,
        fork,
        release,
    })
}

/// Prints one test's result line, with the detail a passing test gave; returns whether it
/// passed.
fn report_with(name: &str, result: Result<&'static str, Failure>) -> bool {
    match result {
        Ok(detail) => report(name, Ok(()), detail),
        Err(err) => report(name, Err(err), ""),
    }
}

/// Prints one test's result line; returns whether it passed.
fn report(name: &str, result: Result<(), Failure>, pass: &str) -> bool {
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS {name}: {pass}");
            true
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL {name}: {err}");
            false
        }
    }
}

fn main() -> ExitCode {
    let fds = env_fd("PCID_CLIENT_CHANNEL").and_then(|c| Ok((c, env_fd("INIT_NOTIFY")?)));
    let (channel_fd, notify_fd) = match fds {
        Ok(fds) => fds,
        Err(err) => {
            println!("FERRIC_TEST_FAIL iommu-domain: {err}");
            return ExitCode::FAILURE;
        }
    };
    // SAFETY: pcid-spawner passes these descriptors to this process, which owns them.
    let channel = Channel {
        file: unsafe { File::from_raw_fd(channel_fd) },
    };
    // SAFETY: as above.
    let mut notify = unsafe { File::from_raw_fd(notify_fd) };
    let outcome = setup(channel).map(run);
    // pcid-spawner waits for this before it starts the next driver.
    let _ = notify.write_all(&[0]);
    let outcome = match outcome {
        Ok(Some(outcome)) => outcome,
        Ok(None) => {
            for name in TESTS {
                println!("FERRIC_TEST_SKIP {name}: pcid has no DMA domain for the device");
            }
            return ExitCode::SUCCESS;
        }
        Err(err) => Outcome::all_failed(err),
    };
    let results = [
        report_with(TESTS[0], outcome.unbound),
        report(
            "iommu-domain",
            outcome.domain,
            "got and bound a DMA domain, a second bind was refused",
        ),
        report(
            "iommu-dma",
            outcome.dma,
            "the device copied a buffer by DMA at its IOVAs",
        ),
        report(
            "iommu-fault",
            outcome.fault,
            "DMA to an ungranted physical address left the memory unchanged",
        ),
        report(
            "iommu-readonly",
            outcome.readonly,
            "after mprotect took write access away, the device's write faulted",
        ),
        report_with(TESTS[5], outcome.fork),
        report(
            "iommu-release",
            outcome.release,
            "the device read the buffer's old IOVA after the unmap (the kernel logs the fault)",
        ),
    ];
    if results.iter().all(|&ok| ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
