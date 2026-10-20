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
//! 8. it maps another process's registered buffer into its domain with `MapGrant` and has the
//!    device DMA into it, then revokes the grant and checks the same DMA faults, for a grant
//!    and a sub-grant revoked with its parent (the `iommu-grant-*` tests, [`grant`]);
//! 9. it unmaps the buffer and has the device read the old IOVA of its second page, which the
//!    kernel must have unmapped (strictly: entries cleared, IOTLB invalidated) when the memory
//!    went away; the kernel logs a release and a second DMA fault (`iommu-release`).
//!
//! Between steps 2 and 3 it tests interrupts (section 3 of the design), with or without a
//! domain:
//!
//! - it reserves a vector on the BSP in the `irq` scheme, asks pcid for the MSI message that
//!   delivers it (`MsiMessage`), programs and enables the device's MSI capability through pcid,
//!   and has the device raise an interrupt, which must arrive (`iommu-msi`). With interrupt
//!   remapping the message is in remappable format and names an entry the kernel made for this
//!   device only; without it, it is the compatibility-format message;
//! - with a remappable message, it then programs the device with a forged one naming entry 0,
//!   which the kernel made for the I/O APIC's timer pin and which checks the I/O APIC's
//!   source-id, and has the device raise an interrupt again: the remapping unit must block it
//!   and log a source-id fault, and the vector must not fire (`iommu-irq-fault`).
//!
//! - with `kernel.irq_percpu` (the root-only `irq:cpus` file exists), it reserves one vector
//!   number on a CPU other than the BSP and the same number on the BSP, programs the device
//!   with the message for the other CPU, and has it raise an interrupt: the other CPU's handle
//!   must count it and the BSP's must not, so the kernel counts interrupts per (CPU, vector)
//!   and not by vector number alone. It then does the same with a vector below 48 on that
//!   CPU, which on an AP is an ordinary vector and not a legacy IRQ (`iommu-msi-percpu`);
//! - also with `kernel.irq_percpu`, it aims the device at a vector on the BSP and, while a
//!   second thread keeps raising interrupts, moves that vector to another CPU with the `irq:`
//!   handle's `Move` call. With interrupt remapping the kernel rewrites the entry and the
//!   device's message stays as it is; without it the test programs pcid's message for the new
//!   vector, as a driver would. Once the device is quiet, one interrupt must reach the same
//!   descriptor, the old vector on the BSP must be free again and count nothing, and pcid must
//!   give the old (CPU, vector) a message of its own that reaches it, not the moved entry
//!   (`iommu-msi-steer`).
//!
//! For each DMA fault it expects, it prints `FERRIC_IOMMU_EXPECT_FAULT <read|write> <address>`,
//! and for the interrupt fault `FERRIC_IOMMU_EXPECT_IRQ_FAULT <index>`; the vtd boot CI job
//! checks the kernel logged exactly those faults, from this device. With kernel tracing
//! (`kernel.trace_iommu`) it also turns the `iommu.fault` event on and checks the kernel
//! recorded those faults ([`trace`], `iommu-trace`). With `kernel.trace_irq` it turns the
//! `irq` events on around `iommu-msi`'s interrupt, which it acknowledges itself, and checks
//! the kernel recorded its delivery and the acknowledgement (`trace-irq`). When the program
//! exits the domain closes and the kernel blocks the device, and pcid frees its interrupt
//! remapping entries.
//!
//! Each test prints `FERRIC_TEST_PASS <name>`, `FERRIC_TEST_FAIL <name>`, or
//! `FERRIC_TEST_SKIP <name>` when pcid has no domain to give (no IOMMU, or a kernel built with
//! `kernel.iommu = "never"`), or, for `iommu-irq-fault`, when there is no interrupt remapping.
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

#[cfg(target_os = "redox")]
mod grant;
mod trace;

/// Without Redox there is no kernel to map grants into.
#[cfg(not(target_os = "redox"))]
mod grant {
    use super::{Edu, Failure, GRANT_TESTS, Translator};

    pub fn run(_: &Edu, _: &Translator, _: usize) -> Result<bool, Failure> {
        Err(Failure::NotRun)
    }

    pub fn report_not_run(err: &dyn std::fmt::Display) {
        for name in GRANT_TESTS {
            println!("FERRIC_TEST_FAIL {name}: not run: {err}");
        }
    }
}

/// The grant tests' result lines, in order ([`grant`]).
const GRANT_TESTS: [&str; 5] = [
    "iommu-grant-nodomain",
    "iommu-grant-dma",
    "iommu-grant-unmap",
    "iommu-grant-revoke",
    "iommu-subgrant-revoke",
];

/// `PcidClientRequest::EnableFeature(PciFeature)`.
const ENABLE_FEATURE: u32 = 4;
/// `PcidClientRequest::SetFeatureInfo(SetFeatureInfo)`.
const SET_FEATURE_INFO: u32 = 6;
/// `PcidClientRequest::ReadConfig(u16)`.
const READ_CONFIG: u32 = 7;
/// `PcidClientRequest::GetDmaDomain`.
const GET_DMA_DOMAIN: u32 = 9;
/// `PcidClientRequest::MsiMessage { cpu: u32, vector: u8 }`.
const MSI_MESSAGE: u32 = 10;
/// `PcidClientResponse::FeatureEnabled(PciFeature)`.
const RESPONSE_FEATURE_ENABLED: u32 = 4;
/// `PcidClientResponse::Error`.
const RESPONSE_ERROR: u32 = 6;
/// `PcidClientResponse::SetFeatureInfo(PciFeature)`.
const RESPONSE_SET_FEATURE_INFO: u32 = 8;
/// `PcidClientResponse::ReadConfig(u32)`.
const RESPONSE_READ_CONFIG: u32 = 9;
/// `PcidClientResponse::DmaDomain`.
const RESPONSE_DMA_DOMAIN: u32 = 11;
/// `PcidClientResponse::MsiMessage(MsiAddrAndData)`.
const RESPONSE_MSI_MESSAGE: u32 = 12;
/// `PciFeature::Msi`, and `SetFeatureInfo::Msi`.
const FEATURE_MSI: u32 = 0;

/// MSI address bit 4: the remappable format (VT-d, "Interrupt Requests in Remappable Format").
const MSI_REMAPPABLE: u64 = 1 << 4;
/// A remappable-format MSI address naming interrupt remapping entry 0, with no subhandle.
const MSI_ENTRY_0: u64 = 0xfee0_0000 | MSI_REMAPPABLE;

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
/// `edu` interrupt registers: raise (sets status bits, sends the MSI) and acknowledge.
const EDU_IRQ_RAISE: usize = 0x60;
const EDU_IRQ_ACK: usize = 0x64;
/// How long to wait for an interrupt.
const IRQ_WAIT: Duration = Duration::from_secs(2);
/// How long to wait before deciding a blocked interrupt did not arrive.
const IRQ_BLOCKED_WAIT: Duration = Duration::from_millis(500);

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
    /// No interrupt vector was free on the CPU.
    NoVector(u32),
    /// pcid had no MSI message for the vector.
    NoMsiMessage(u8),
    /// The device's interrupt did not arrive.
    NoInterrupt(u8),
    /// The forged interrupt arrived.
    ForgedDelivered(u8, usize),
    /// `irq:cpus` did not parse.
    Cpus(ferric_irq::ParseError),
    /// An interrupt for one CPU was counted by the same vector on another: the vector, the
    /// CPU it was held on, and the count there.
    CrossCpu(u8, u32, usize),
    /// The kernel's answer to a `Move` call did not decode.
    BadMove(usize),
    /// After a move, a vector is still reserved (or not reserved) where it should not be (or
    /// should be): the CPU, the vector, and what was expected.
    Reservation(u32, u8, &'static str),
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
            Self::NoVector(cpu) => write!(f, "no interrupt vector is free on CPU {cpu}"),
            Self::NoMsiMessage(vector) => {
                write!(f, "pcid has no MSI message for vector {vector:#x}")
            }
            Self::NoInterrupt(vector) => {
                write!(
                    f,
                    "the device's interrupt did not arrive on vector {vector:#x}"
                )
            }
            Self::ForgedDelivered(vector, count) => write!(
                f,
                "the forged interrupt was delivered: vector {vector:#x} fired {count} time(s)"
            ),
            Self::Cpus(err) => write!(f, "irq:cpus does not parse: {err}"),
            Self::CrossCpu(vector, cpu, count) => write!(
                f,
                "vector {vector:#x} held on CPU {cpu} counted {count} interrupt(s) sent to another CPU"
            ),
            Self::BadMove(word) => write!(f, "the kernel answered Move with {word:#x}"),
            Self::Reservation(cpu, vector, expected) => {
                write!(f, "vector {vector:#x} on CPU {cpu} should be {expected}")
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

    /// Sends `request` and checks the response is exactly `expected`.
    fn expect(&mut self, request: &[u8], expected: &[u8]) -> Result<(), Failure> {
        let response = self.transact(request)?;
        if response == expected {
            Ok(())
        } else {
            Err(Failure::Response(response))
        }
    }

    /// The MSI address and data that deliver `vector` to logical CPU `cpu`; `None` when pcid
    /// has none.
    fn msi_message(&mut self, cpu: u32, vector: u8) -> Result<Option<(u64, u32)>, Failure> {
        let mut request = MSI_MESSAGE.to_le_bytes().to_vec();
        request.extend_from_slice(&cpu.to_le_bytes());
        request.push(vector);
        let response = self.transact(&request)?;
        let (tag, rest) = response.split_at_checked(4).ok_or(Failure::NotRun)?;
        if tag == RESPONSE_ERROR.to_le_bytes() {
            return Ok(None);
        }
        match (
            tag == RESPONSE_MSI_MESSAGE.to_le_bytes(),
            rest.split_at_checked(8),
        ) {
            (true, Some((addr, data))) if data.len() == 4 => {
                let addr = u64::from_le_bytes(addr.try_into().map_err(|_| Failure::NotRun)?);
                let data = u32::from_le_bytes(data.try_into().map_err(|_| Failure::NotRun)?);
                Ok(Some((addr, data)))
            }
            _ => Err(Failure::Response(response)),
        }
    }

    /// Programs the MSI capability with one vector and message `(addr, data)`.
    fn set_msi(&mut self, (addr, data): (u64, u32)) -> Result<(), Failure> {
        let mut request = SET_FEATURE_INFO.to_le_bytes().to_vec();
        request.extend_from_slice(&FEATURE_MSI.to_le_bytes());
        // multi_message_enable: Some(0), one vector.
        request.extend_from_slice(&[1, 0]);
        // message_address_and_data: Some(MsiAddrAndData { addr, data }).
        request.push(1);
        request.extend_from_slice(&addr.to_le_bytes());
        request.extend_from_slice(&data.to_le_bytes());
        // mask_bits: None.
        request.push(0);
        let mut expected = RESPONSE_SET_FEATURE_INFO.to_le_bytes().to_vec();
        expected.extend_from_slice(&FEATURE_MSI.to_le_bytes());
        self.expect(&request, &expected)
    }

    /// Enables MSI.
    fn enable_msi(&mut self) -> Result<(), Failure> {
        let mut request = ENABLE_FEATURE.to_le_bytes().to_vec();
        request.extend_from_slice(&FEATURE_MSI.to_le_bytes());
        let mut expected = RESPONSE_FEATURE_ENABLED.to_le_bytes().to_vec();
        expected.extend_from_slice(&FEATURE_MSI.to_le_bytes());
        self.expect(&request, &expected)
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

// SAFETY: `Edu` only reads and writes the device's registers with volatile accesses, which
// any thread may do at once; `iommu-msi-steer` raises interrupts from a second thread.
unsafe impl Sync for Edu {}

impl Edu {
    /// Reads the 32-bit register at `offset`.
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: `regs` maps the device's register page; `offset` is a register in it.
        unsafe { std::ptr::read_volatile(self.regs.ptr.add(offset).cast::<u32>()) }
    }

    /// Writes the 32-bit register at `offset`.
    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as in `read32`.
        unsafe { std::ptr::write_volatile(self.regs.ptr.add(offset).cast::<u32>(), value) }
    }

    /// Has the device raise its interrupt (an MSI once MSI is enabled).
    fn raise_interrupt(&self) {
        self.write32(EDU_IRQ_RAISE, 1);
    }

    /// Clears the device's interrupt status.
    fn ack_interrupt(&self) {
        self.write32(EDU_IRQ_ACK, 1);
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

/// An interrupt vector reserved in the `irq` scheme.
struct Vector {
    file: File,
    /// The logical CPU.
    cpu: u32,
    vector: u8,
}

impl Vector {
    /// Reserves a free vector on logical CPU `cpu`, as `pcid_interface::irq_helpers` does.
    fn reserve(cpu: u32) -> Result<Self, Failure> {
        // IRQs 0 to 15 are the legacy ones, which are not reserved.
        Self::reserve_where(cpu, |irq| irq >= 16)
    }

    /// Reserves the first free vector on logical CPU `cpu` whose IRQ number `accept` takes.
    fn reserve_where(cpu: u32, accept: impl Fn(u8) -> bool) -> Result<Self, Failure> {
        let dir = format!("/scheme/irq/cpu-{cpu:02x}");
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let Some(irq) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u8>().ok())
            else {
                continue;
            };
            if !accept(irq) {
                continue;
            }
            match File::create(format!("{dir}/{irq}")) {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        cpu,
                        vector: irq + 32,
                    });
                }
                // Taken by another driver meanwhile.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }
        Err(Failure::NoVector(cpu))
    }

    /// Reserves one vector number on logical CPU `cpu` and the same number on `other`, for
    /// checking that an interrupt for one is not counted for the other.
    fn reserve_pair(cpu: u32, other: u32) -> Result<(Self, Self), Failure> {
        let dir = format!("/scheme/irq/cpu-{cpu:02x}");
        for entry in std::fs::read_dir(&dir)? {
            let Some(irq) = entry?
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u8>().ok())
            else {
                continue;
            };
            if irq < 16 {
                continue;
            }
            let open = |cpu: u32| File::create(format!("/scheme/irq/cpu-{cpu:02x}/{irq}"));
            let first = match open(cpu) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            };
            match open(other) {
                Ok(second) => {
                    let vector = irq + 32;
                    return Ok((
                        Self {
                            file: first,
                            cpu,
                            vector,
                        },
                        Self {
                            file: second,
                            cpu: other,
                            vector,
                        },
                    ));
                }
                // Taken on the other CPU: try the next number.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }
        Err(Failure::NoVector(cpu))
    }

    /// How many interrupts arrived and are not acknowledged yet (0 when none).
    fn pending(&mut self) -> Result<usize, Failure> {
        let mut count = [0_u8; 8];
        match self.file.read(&mut count)? {
            0 => Ok(0),
            _ => Ok(usize::from_ne_bytes(count)),
        }
    }

    /// Waits up to `wait` for an interrupt, acknowledges what arrived, and returns its count.
    fn wait(&mut self, wait: Duration) -> Result<usize, Failure> {
        let start = std::time::Instant::now();
        loop {
            let count = self.pending()?;
            if count != 0 {
                self.file.write_all(&count.to_ne_bytes())?;
                return Ok(count);
            }
            if start.elapsed() >= wait {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// The logical CPU number of the BSP.
fn bsp() -> Result<u32, Failure> {
    let mut buf = [0_u8; 8];
    let len = File::open("/scheme/irq/bsp")?.read(&mut buf)?;
    // The kernel writes a 32-bit number and may report a usize-sized read, as
    // `pcid_interface::irq_helpers::read_bsp_apic_id` allows.
    match buf.get(..len) {
        Some([a, b, c, d]) | Some([a, b, c, d, 0, 0, 0, 0]) => {
            Ok(u32::from_le_bytes([*a, *b, *c, *d]))
        }
        Some(bytes) => Err(Failure::Response(bytes.to_vec())),
        None => Err(Failure::NotRun),
    }
}

/// The interrupt remapping entry a remappable-format MSI address names.
fn entry_of(addr: u64) -> u64 {
    ((addr >> 5) & 0x7fff) | (((addr >> 2) & 1) << 15)
}

/// The device's MSI, set up from pcid's message, and the message.
struct Msi {
    vector: Vector,
    message: (u64, u32),
}

/// Sets up and enables the device's MSI with pcid's message for a fresh vector on the BSP,
/// raises an interrupt and checks it arrives.
fn msi_delivered(setup: &mut Setup) -> Result<Msi, Failure> {
    let mut vector = Vector::reserve(bsp()?)?;
    let message = setup
        .channel
        .msi_message(vector.cpu, vector.vector)?
        .ok_or(Failure::NoMsiMessage(vector.vector))?;
    setup.channel.set_msi(message)?;
    setup.channel.enable_msi()?;
    // The IRQ number of the vector: `irq:cpu-XX/N` names vector N + 32.
    trace::watch_interrupt(vector.cpu, vector.vector - 32);
    setup.edu.raise_interrupt();
    let count = vector.wait(IRQ_WAIT);
    setup.edu.ack_interrupt();
    trace::interrupt_handled(count.as_ref().is_ok_and(|&count| count != 0));
    if count? == 0 {
        return Err(Failure::NoInterrupt(vector.vector));
    }
    Ok(Msi { vector, message })
}

/// What [`msi_delivered`] reports when it passes.
fn msi_detail(msi: &Msi) -> String {
    let (addr, _) = msi.message;
    let (vector, cpu) = (msi.vector.vector, msi.vector.cpu);
    if addr & MSI_REMAPPABLE != 0 {
        format!(
            "the device's MSI reached vector {vector:#x} on CPU {cpu} through interrupt remapping entry {} (remappable format)",
            entry_of(addr)
        )
    } else {
        format!(
            "the device's MSI reached vector {vector:#x} on CPU {cpu} (compatibility format, no interrupt remapping)"
        )
    }
}

/// Programs the device with a remappable message naming entry 0, which the I/O APIC owns, and
/// checks the unit blocks the interrupt; then restores pcid's message.
fn forged_msi_blocked(setup: &mut Setup, msi: &mut Msi) -> Result<(), Failure> {
    setup.channel.set_msi((MSI_ENTRY_0, 0))?;
    trace::expect_irq_fault(u16::try_from(entry_of(MSI_ENTRY_0)).unwrap_or(u16::MAX));
    setup.edu.raise_interrupt();
    let count = msi.vector.wait(IRQ_BLOCKED_WAIT);
    setup.edu.ack_interrupt();
    setup.channel.set_msi(msi.message)?;
    match count? {
        0 => Ok(()),
        count => Err(Failure::ForgedDelivered(msi.vector.vector, count)),
    }
}

/// The BSP and another CPU, or `None` when the kernel has no `irq:cpus` (built without
/// `kernel.irq_percpu`) or runs one CPU.
fn bsp_and_other() -> Result<Option<(u32, u32)>, Failure> {
    let text = match std::fs::read_to_string("/scheme/irq/cpus") {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let cpus = ferric_irq::parse_cpus(&text)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Failure::Cpus)?;
    let bsp = bsp()?;
    Ok(cpus
        .iter()
        .map(|entry| entry.cpu.get())
        .find(|&cpu| cpu != bsp)
        .map(|other| (bsp, other)))
}

/// Runs `iommu-msi-percpu`: the device's interrupt goes to a CPU other than the BSP while the
/// same vector number is held on the BSP, and only the target's handle may count it. `Ok(None)`
/// when skipped ([`bsp_and_other`]).
fn msi_percpu(setup: &mut Setup, msi: &Msi) -> Result<Option<String>, Failure> {
    let Some((bsp, other)) = bsp_and_other()? else {
        return Ok(None);
    };
    let (mut target, mut decoy) = Vector::reserve_pair(other, bsp)?;
    let fired = fire(setup, &mut target);
    let stray = decoy.wait(IRQ_BLOCKED_WAIT);
    // On an AP, IRQs 0 to 15 are ordinary vectors: one reserved there must reach its handle,
    // not the BSP's legacy IRQ of the same number.
    let low = fired.and_then(|()| {
        let mut low = Vector::reserve_where(other, |irq| irq < 16)?;
        fire(setup, &mut low).map(|()| low.vector)
    });
    // Back to the message of the vector `iommu-msi` reserved, for whatever runs next.
    setup.channel.set_msi(msi.message)?;
    let low = low?;
    match stray? {
        0 => Ok(Some(format!(
            "vector {:#x} reached CPU {other}, and the same vector held on CPU {bsp} counted nothing; low vector {low:#x} reached CPU {other}",
            target.vector
        ))),
        count => Err(Failure::CrossCpu(decoy.vector, bsp, count)),
    }
}

/// How long the second thread of `iommu-msi-steer` raises interrupts before and after the move.
const STEER_FIRE: Duration = Duration::from_millis(50);

/// Runs `iommu-msi-steer`: moves the device's vector from the BSP to another CPU while the
/// device fires, and checks the descriptor follows it. `Ok(None)` when skipped
/// ([`bsp_and_other`]).
fn msi_steer(setup: &mut Setup, msi: &Msi) -> Result<Option<String>, Failure> {
    let Some((bsp, other)) = bsp_and_other()? else {
        return Ok(None);
    };
    let mut steered = Vector::reserve(bsp)?;
    let result = steer_while_firing(setup, &mut steered, other);
    // Back to the message of the vector `iommu-msi` reserved, for whatever runs next.
    setup.channel.set_msi(msi.message)?;
    let (from, remapped) = result?;
    let how = if remapped == 0 {
        "the test reprogrammed the device's message (no interrupt remapping)".to_owned()
    } else {
        format!("the kernel rewrote {remapped} interrupt remapping entry(s)")
    };
    Ok(Some(format!(
        "vector {from:#x} on CPU {bsp} moved to vector {:#x} on CPU {other} while the device fired; {how}; the same descriptor counted the next interrupt, and the old vector was free and got a message of its own",
        steered.vector
    )))
}

/// The body of [`msi_steer`]: aims the device at `steered`, moves it to CPU `to` while a
/// second thread raises interrupts, then checks where the next interrupts land. Returns the
/// old vector and how many remapping entries the kernel moved; `steered` is updated to its new
/// place.
fn steer_while_firing(
    setup: &mut Setup,
    steered: &mut Vector,
    to: u32,
) -> Result<(u8, u32), Failure> {
    fire(setup, steered)?;
    let (from_cpu, from) = (steered.cpu, steered.vector);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let edu = &setup.edu;
    let channel = &mut setup.channel;
    let moved = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                edu.raise_interrupt();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        std::thread::sleep(STEER_FIRE);
        let moved = move_vector(&steered.file, to).and_then(|moved| {
            if moved.remapped == 0 {
                // The message names the CPU and vector, so it is the driver's to rewrite.
                let vector = moved.irq.get() + 32;
                let message = channel
                    .msi_message(to, vector)?
                    .ok_or(Failure::NoMsiMessage(vector))?;
                channel.set_msi(message)?;
            }
            Ok(moved)
        });
        std::thread::sleep(STEER_FIRE);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        moved
    })?;
    setup.edu.ack_interrupt();
    steered.cpu = to;
    steered.vector = moved.irq.get() + 32;
    // What arrived during the move, and the one interrupt the kernel reports pending after it.
    steered.wait(IRQ_BLOCKED_WAIT)?;
    // The new vector is held, and the old one was given back.
    if !reserved(to, steered.vector)? {
        return Err(Failure::Reservation(to, steered.vector, "reserved"));
    }
    let mut old = Vector::reserve_where(from_cpu, |irq| irq + 32 == from)
        .map_err(|_| Failure::Reservation(from_cpu, from, "free"))?;
    setup.edu.raise_interrupt();
    let count = steered.wait(IRQ_WAIT);
    setup.edu.ack_interrupt();
    if count? == 0 {
        return Err(Failure::NoInterrupt(steered.vector));
    }
    match old.wait(IRQ_BLOCKED_WAIT)? {
        0 => {}
        count => return Err(Failure::CrossCpu(from, from_cpu, count)),
    }
    // pcid must not hand out the moved entry for the old (CPU, vector).
    fire(setup, &mut old)?;
    Ok((from, moved.remapped))
}

/// Whether vector `vector` on CPU `cpu` is reserved: creating it again fails with `EEXIST`.
fn reserved(cpu: u32, vector: u8) -> Result<bool, Failure> {
    match File::create(format!("/scheme/irq/cpu-{cpu:02x}/{}", vector - 32)) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(true),
        Err(err) => Err(err.into()),
    }
}

/// Steers the vector of `irq` handle `file` to CPU `cpu` with the handle's `Move` call.
fn move_vector(file: &File, cpu: u32) -> Result<ferric_irq::call::Moved, Failure> {
    let call = ferric_irq::call::Call::Move {
        cpu: ferric_irq::CpuId::new(cpu),
    };
    let (words, len) = call.encode();
    let fd = usize::try_from(file.as_raw_fd()).map_err(|_| Failure::NotRun)?;
    let word =
        sys::call(fd, &mut [], 0, &words[..len]).map_err(|errno| Failure::Call("Move", errno))?;
    ferric_irq::call::Moved::from_word(word as u64).ok_or(Failure::BadMove(word))
}

/// Aims the device's MSI at `target` with pcid's message for it, raises an interrupt, and
/// checks that `target`'s handle counted it.
fn fire(setup: &mut Setup, target: &mut Vector) -> Result<(), Failure> {
    let message = setup
        .channel
        .msi_message(target.cpu, target.vector)?
        .ok_or(Failure::NoMsiMessage(target.vector))?;
    setup.channel.set_msi(message)?;
    setup.edu.raise_interrupt();
    let count = target.wait(IRQ_WAIT);
    setup.edu.ack_interrupt();
    if count? == 0 {
        return Err(Failure::NoInterrupt(target.vector));
    }
    Ok(())
}

/// The outcome of the interrupt tests.
struct MsiOutcome {
    msi: Result<String, Failure>,
    /// `Ok(None)` when skipped: the message was in compatibility format.
    forged: Result<Option<()>, Failure>,
    /// `Ok(None)` when skipped ([`msi_percpu`]).
    percpu: Result<Option<String>, Failure>,
    /// `Ok(None)` when skipped ([`msi_steer`]).
    steer: Result<Option<String>, Failure>,
}

impl MsiOutcome {
    /// No test ran.
    fn not_run() -> Self {
        Self {
            msi: Err(Failure::NotRun),
            forged: Err(Failure::NotRun),
            percpu: Err(Failure::NotRun),
            steer: Err(Failure::NotRun),
        }
    }
}

/// Runs the interrupt tests.
fn msi_tests(setup: &mut Setup) -> MsiOutcome {
    let mut msi = match msi_delivered(setup) {
        Ok(msi) => msi,
        Err(err) => {
            return MsiOutcome {
                msi: Err(err),
                ..MsiOutcome::not_run()
            };
        }
    };
    let forged = if msi.message.0 & MSI_REMAPPABLE != 0 {
        forged_msi_blocked(setup, &mut msi).map(Some)
    } else {
        Ok(None)
    };
    let percpu = msi_percpu(setup, &msi);
    let steer = msi_steer(setup, &msi);
    MsiOutcome {
        msi: Ok(msi_detail(&msi)),
        forged,
        percpu,
        steer,
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
    /// Whether every grant test passed; they print their own result lines.
    grants: Result<bool, Failure>,
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
            grants: Err(Failure::NotRun),
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
    trace::expect_dma_fault(true, target as u64);
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
    trace::expect_dma_fault(true, iova as u64);
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
    trace::expect_dma_fault(false, (iova + PAGE) as u64);
    edu.dma((iova + PAGE) as u64, false)
}

/// Runs the tests after setup: the DMA tests, `None` when pcid had no domain to give, and the
/// interrupt tests.
fn run(mut setup: Setup) -> (Option<Outcome>, MsiOutcome) {
    let unbound = Ok(if setup.buffer_phys.is_some() {
        "translation without a domain gives a physical address"
    } else {
        "translation without a domain is refused (EACCES)"
    });
    let bound = bind(&mut setup);
    let msi = msi_tests(&mut setup);
    match bound {
        Ok(true) => {}
        Ok(false) => return (None, msi),
        Err(err) => return (Some(Outcome::all_failed(err)), msi),
    }
    let iova = match translate_in_domain(&setup) {
        Ok(iova) => iova,
        Err(err) => {
            let outcome = Outcome {
                unbound,
                domain: Ok(()),
                dma: Err(err),
                ..Outcome::all_failed(Failure::NotRun)
            };
            return (Some(outcome), msi);
        }
    };
    let dma = dma_round_trip(&mut setup, iova);
    let fault = dma_to_ungranted(&mut setup);
    let readonly = dma_after_write_removed(&mut setup);
    let fork = forked_child_unbound(&setup);
    let grants = grant::run(&setup.edu, &setup.translator, setup.domain);
    let release = dma_after_release(setup, iova);
    let outcome = Outcome {
        unbound,
        domain: Ok(()),
        dma,
        fault,
        readonly,
        fork,
        grants,
        release,
    };
    (Some(outcome), msi)
}

/// Prints one test's result line, with the detail a passing test gave; returns whether it
/// passed.
fn report_with(name: &str, result: Result<impl AsRef<str>, Failure>) -> bool {
    match result {
        Ok(detail) => report(name, Ok(()), detail.as_ref()),
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

/// Prints the interrupt tests' result lines; returns whether neither failed.
fn report_msi(msi: MsiOutcome) -> bool {
    let delivered = report_with("iommu-msi", msi.msi);
    let forged = match msi.forged {
        Ok(None) => {
            println!("FERRIC_TEST_SKIP iommu-irq-fault: no interrupt remapping for the device");
            true
        }
        Ok(Some(())) => report(
            "iommu-irq-fault",
            Ok(()),
            "a forged MSI naming the I/O APIC's interrupt remapping entry 0 was blocked (the kernel logs a source-id fault)",
        ),
        Err(err) => report("iommu-irq-fault", Err(err), ""),
    };
    let percpu = match msi.percpu {
        Ok(None) => {
            println!(
                "FERRIC_TEST_SKIP iommu-msi-percpu: no irq:cpus (kernel.irq_percpu off) or one CPU"
            );
            true
        }
        Ok(Some(detail)) => report("iommu-msi-percpu", Ok(()), &detail),
        Err(err) => report("iommu-msi-percpu", Err(err), ""),
    };
    let steer = match msi.steer {
        Ok(None) => {
            println!(
                "FERRIC_TEST_SKIP iommu-msi-steer: no irq:cpus (kernel.irq_percpu off) or one CPU"
            );
            true
        }
        Ok(Some(detail)) => report("iommu-msi-steer", Ok(()), &detail),
        Err(err) => report("iommu-msi-steer", Err(err), ""),
    };
    delivered && forged && percpu && steer
}

/// Prints the result of the tracing step `name`; returns whether it did not fail.
fn report_trace(name: &str, step: trace::Step) -> bool {
    match step {
        trace::Step::Pass(detail) => println!("FERRIC_TEST_PASS {name}: {detail}"),
        trace::Step::Skip(why) => println!("FERRIC_TEST_SKIP {name}: {why}"),
        trace::Step::Fail(err) => {
            println!("FERRIC_TEST_FAIL {name}: {err}");
            return false;
        }
    }
    true
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
    let tracing = trace::start();
    let (outcome, msi) = match setup(channel) {
        Ok(setup) => run(setup),
        Err(err) => (Some(Outcome::all_failed(err)), MsiOutcome::not_run()),
    };
    // Before the notify: the next drivers and the smoke script must find the rings as they
    // would without this program.
    let traced = tracing.and_then(|start| match start {
        trace::Start::Watching => trace::finish(),
        trace::Start::Skip(why) => Ok(trace::Report {
            iommu: trace::Step::Skip(why),
            irq: trace::Step::Skip(why),
        }),
    });
    // pcid-spawner waits for this before it starts the next driver.
    let _ = notify.write_all(&[0]);
    let trace_ok = match traced {
        Ok(report) => {
            report_trace("iommu-trace", report.iommu) & report_trace("trace-irq", report.irq)
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL iommu-trace: {err}");
            println!("FERRIC_TEST_FAIL trace-irq: {err}");
            false
        }
    };
    let msi_ok = report_msi(msi) && trace_ok;
    let Some(outcome) = outcome else {
        for name in TESTS.iter().chain(&GRANT_TESTS) {
            println!("FERRIC_TEST_SKIP {name}: pcid has no DMA domain for the device");
        }
        return if msi_ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
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
    let grants_ok = match outcome.grants {
        Ok(ok) => ok,
        Err(err) => {
            grant::report_not_run(&err);
            false
        }
    };
    if msi_ok && grants_ok && results.iter().all(|&ok| ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
