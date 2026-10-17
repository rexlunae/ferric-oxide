//! DMA domain test driver for Ferric Oxide.
//!
//! pcid-spawner starts this program for the host bridge (see `pcid.toml`) with its pcid
//! channel in `PCID_CLIENT_CHANNEL`, like any PCI driver. It then goes through the steps a
//! driver takes to get a DMA domain of its own (io/iommu-design.md in the project files,
//! section 1):
//!
//! 1. sends `GetDmaDomain` on the channel; pcid creates a domain in the kernel's `iommu`
//!    scheme and attaches the host bridge (and its requester-ID aliases) to it;
//! 2. reads the domain's descriptor from the channel (descriptor passing);
//! 3. binds the domain to itself, and checks that a second bind is refused (a process binds
//!    at most one domain).
//!
//! When it exits the descriptor closes and the kernel blocks the host bridge's DMA, which a
//! host bridge never issues. The kernel logs the attach, the bind and the close; the vtd boot
//! CI job checks those lines.
//!
//! It prints `FERRIC_TEST_PASS iommu-domain`, `FERRIC_TEST_SKIP iommu-domain` (pcid has no
//! domain to give: no IOMMU, or a kernel built with `kernel.iommu = "never"`) or
//! `FERRIC_TEST_FAIL iommu-domain`.
//!
//! The channel speaks bincode-encoded `pcid_interface` enums. This program has no
//! dependencies, so it writes the two messages it needs by hand: bincode encodes a unit enum
//! variant as its index, a little-endian `u32`. These indices follow the variant order in
//! `userspace/base/drivers/pcid/src/driver_interface/mod.rs`.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::process::ExitCode;

/// `PcidClientRequest::GetDmaDomain`.
const GET_DMA_DOMAIN: u32 = 9;
/// `PcidClientResponse::Error`.
const RESPONSE_ERROR: u32 = 6;
/// `PcidClientResponse::DmaDomain`.
const RESPONSE_DMA_DOMAIN: u32 = 11;

/// `ferric_iommu::abi::Verb::Bind`.
const VERB_BIND: u64 = 3;

/// `CallFlags::READ` and `CallFlags::FD` from `redox_syscall`.
const CALL_READ: usize = 1 << 10;
const CALL_FD: usize = 1 << 11;

/// `EBUSY`.
const EBUSY: usize = 16;

/// Why the test failed.
enum Failure {
    /// An environment variable is missing or not a descriptor number.
    Env(&'static str),
    /// Talking to pcid failed.
    Io(std::io::Error),
    /// pcid answered with something unexpected.
    Response(Vec<u8>),
    /// A system call failed with this errno.
    Call(&'static str, usize),
    /// A second bind succeeded.
    SecondBind,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(var) => write!(f, "{var} is not set to a descriptor"),
            Self::Io(err) => write!(f, "channel I/O failed: {err}"),
            Self::Response(bytes) => write!(f, "unexpected pcid response {bytes:02x?}"),
            Self::Call(what, errno) => write!(f, "{what} failed with errno {errno}"),
            Self::SecondBind => f.write_str("a second bind was not refused"),
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

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
}

/// `SYS_CALL` on `fd`, returning the errno on failure.
#[cfg(target_os = "redox")]
fn call(fd: usize, payload: &mut [u8], flags: usize, metadata: &[u64]) -> Result<usize, usize> {
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

#[cfg(not(target_os = "redox"))]
fn call(_fd: usize, _payload: &mut [u8], _flags: usize, _metadata: &[u64]) -> Result<usize, usize> {
    Err(38) // ENOSYS
}

/// The descriptor number in environment variable `var`.
fn env_fd(var: &'static str) -> Result<i32, Failure> {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .ok_or(Failure::Env(var))
}

/// Sends `GetDmaDomain` and returns the response's bytes.
fn request_domain(channel: &mut File) -> Result<Vec<u8>, Failure> {
    channel.write_all(&GET_DMA_DOMAIN.to_le_bytes())?;
    let mut len = [0_u8; 8];
    channel.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len);
    if len > 4096 {
        return Err(Failure::Response(len.to_le_bytes().to_vec()));
    }
    let mut response = vec![0_u8; len as usize];
    channel.read_exact(&mut response)?;
    Ok(response)
}

/// Runs the test; `Ok(false)` when pcid had no domain to give.
fn test(channel: &mut File, channel_fd: usize) -> Result<bool, Failure> {
    let response = request_domain(channel)?;
    match response.get(..4) {
        Some(tag) if tag == RESPONSE_DMA_DOMAIN.to_le_bytes() && response.len() == 4 => {}
        Some(tag) if tag == RESPONSE_ERROR.to_le_bytes() => return Ok(false),
        _ => return Err(Failure::Response(response)),
    }
    // usize::MAX asks the kernel to pick the descriptor number; any other value is the slot
    // to put it in, which must be free.
    let mut fd = usize::MAX.to_ne_bytes();
    call(channel_fd, &mut fd, CALL_READ | CALL_FD, &[])
        .map_err(|errno| Failure::Call("receiving the domain descriptor", errno))?;
    let domain = usize::from_ne_bytes(fd);
    call(domain, &mut [], 0, &[VERB_BIND]).map_err(|errno| Failure::Call("bind", errno))?;
    match call(domain, &mut [], 0, &[VERB_BIND]) {
        Err(EBUSY) => Ok(true),
        Err(errno) => Err(Failure::Call("the second bind", errno)),
        Ok(_) => Err(Failure::SecondBind),
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
    let mut channel = unsafe { File::from_raw_fd(channel_fd) };
    // SAFETY: as above.
    let mut notify = unsafe { File::from_raw_fd(notify_fd) };
    let result = test(&mut channel, channel_fd as usize);
    // pcid-spawner waits for this before it starts the next driver.
    let _ = notify.write_all(&[0]);
    match result {
        Ok(true) => {
            println!(
                "FERRIC_TEST_PASS iommu-domain: got and bound a DMA domain, a second bind was refused"
            );
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("FERRIC_TEST_SKIP iommu-domain: pcid has no DMA domain for the host bridge");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL iommu-domain: {err}");
            ExitCode::FAILURE
        }
    }
}
