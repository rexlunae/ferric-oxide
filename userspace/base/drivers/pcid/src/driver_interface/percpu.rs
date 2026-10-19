//! Ferric Oxide: interrupt vectors on a chosen CPU (roadmap Phase 2.4, `kernel.irq_percpu`).
//!
//! A kernel built with `kernel.irq_percpu` counts and delivers interrupts per (logical CPU,
//! vector), and lists its CPUs in the root-only `irq:cpus` file. With it, a driver can give each
//! MSI-X table entry to a different CPU ([`crate::irq_helpers::Msix::alloc_on`] and
//! [`crate::irq_helpers::Msix::alloc_spread`]), so a per-core queue interrupts the core that
//! serves it. Without it, [`online_cpus`] is `None` and those calls put every vector on the BSP,
//! as upstream's [`crate::irq_helpers::Msix::alloc`] does.
//!
//! CPUs are logical ids ([`CpuId`]), the numbers `irq:cpu-XX` takes. pcid turns them into APIC
//! ids or interrupt remapping entries when it builds the MSI message.
//!
//! Root can steer a vector to another CPU with [`move_vector`] (the `irq:` handle's
//! [`ferric_irq::call::Call::Move`]); [`crate::irq_helpers::Msix::retarget`] does it for an
//! MSI-X table entry and reprograms the entry when the kernel does not remap its interrupts.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;

use ferric_irq::call::{Call, Moved};
pub use ferric_irq::{CpuEntry, CpuId};

/// The kernel's CPU map.
const CPUS_PATH: &str = "/scheme/irq/cpus";

/// Why the kernel's CPU map could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CpusError {
    /// Reading `irq:cpus` failed.
    #[error("cannot read {CPUS_PATH}")]
    Read(#[source] io::Error),
    /// `irq:cpus` has a malformed line.
    #[error("{CPUS_PATH} is malformed")]
    Parse(#[source] ferric_irq::ParseError),
}

/// Why an interrupt vector could not be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VectorError {
    /// Reserving a vector in the `irq` scheme failed.
    #[error("cannot reserve an interrupt vector on CPU {}", cpu.get())]
    Reserve {
        /// The CPU asked for.
        cpu: CpuId,
        /// What the `irq` scheme answered.
        #[source]
        source: io::Error,
    },
    /// Every vector of the CPU is taken.
    #[error("no interrupt vector is free on CPU {}", cpu.get())]
    NoneFree {
        /// The CPU asked for.
        cpu: CpuId,
    },
    /// pcid has no MSI message for the vector (for example, no interrupt remapping entry, or an
    /// APIC id a compatibility-format message cannot name).
    #[error("pcid has no MSI message for vector {vector:#x} on CPU {}", cpu.get())]
    NoMessage {
        /// The CPU asked for.
        cpu: CpuId,
        /// The vector reserved on it.
        vector: u8,
    },
    /// The CPU is not in the kernel's CPU map.
    #[error("CPU {} is not online", cpu.get())]
    UnknownCpu {
        /// The CPU asked for.
        cpu: CpuId,
    },
    /// Duplicating the vector's `irq` handle failed (out of file descriptors, for example).
    #[error("cannot duplicate the interrupt handle on CPU {}", cpu.get())]
    Clone {
        /// The CPU of the vector.
        cpu: CpuId,
        /// Why `dup` failed.
        #[source]
        source: io::Error,
    },
    /// Every entry of the MSI-X table already has a vector.
    #[error("all {size} MSI-X table entries are in use")]
    TableFull {
        /// The number of entries in the table.
        size: u16,
    },
    /// The kernel refused to move the vector (see [`ferric_irq::call`] for its errors).
    #[error("cannot move the interrupt vector to CPU {}", cpu.get())]
    Move {
        /// The CPU asked for.
        cpu: CpuId,
        /// What the `irq` scheme answered.
        #[source]
        source: io::Error,
    },
    /// The MSI-X table has no entry with this index, or it has no vector yet.
    #[error("MSI-X table entry {index} has no vector")]
    NoEntry {
        /// The entry asked for.
        index: u16,
    },
}

/// The logical id of the bootstrap processor, which `irq:bsp` holds.
///
/// # Errors
///
/// When `irq:bsp` cannot be read or holds a number above 32 bits.
pub fn bsp_cpu() -> io::Result<CpuId> {
    let cpu = crate::irq_helpers::read_bsp_apic_id()?;
    u32::try_from(cpu)
        .map(CpuId::new)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "BSP id above 32 bits"))
}

/// The kernel's online CPUs, from `irq:cpus`, or `None` when the kernel has no per-CPU interrupt
/// delivery (built without `kernel.irq_percpu`).
///
/// # Errors
///
/// [`CpusError`] when the file exists but cannot be read or parsed.
pub fn online_cpus() -> Result<Option<Vec<CpuEntry>>, CpusError> {
    let text = match fs::read_to_string(CPUS_PATH) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(CpusError::Read(err)),
    };
    ferric_irq::parse_cpus(&text)
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
        .map_err(CpusError::Parse)
}

/// Reserves a vector on `cpu` and asks pcid over `channel` for the MSI message that delivers it.
///
/// # Errors
///
/// [`VectorError::Reserve`], [`VectorError::NoneFree`] or [`VectorError::NoMessage`].
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) fn allocate_msi(
    channel: &crate::PcidChannel,
    cpu: CpuId,
) -> Result<(crate::msi::MsiAddrAndData, File), VectorError> {
    let index = cpu.get() as usize;
    let (vector, handle) = crate::irq_helpers::allocate_single_interrupt_vector(index)
        .map_err(|source| VectorError::Reserve { cpu, source })?
        .ok_or(VectorError::NoneFree { cpu })?;
    let message = channel
        .msi_message(index, vector)
        .ok_or(VectorError::NoMessage { cpu, vector })?;
    Ok((message, handle))
}

/// Steers the vector of `irq` handle `handle` to logical CPU `cpu` (root only). The kernel picks
/// the new vector and re-keys `handle` (and its duplicates) to it.
///
/// When [`Moved::remapped`] is 0 the device's interrupts are not remapped, and the caller must
/// program the device's message for the new vector on `cpu`.
///
/// # Errors
///
/// [`VectorError::Move`] with the kernel's answer, or `EIO` in it when the kernel's reply is
/// malformed.
pub fn move_vector(handle: &File, cpu: CpuId) -> Result<Moved, VectorError> {
    let move_error = |source| VectorError::Move { cpu, source };
    let (words, len) = Call::Move { cpu }.encode();
    let fd = usize::try_from(handle.as_raw_fd())
        .map_err(|_| move_error(io::Error::from_raw_os_error(libredox::errno::EBADF)))?;
    let word = libredox::call::call_ro(fd, &mut [], syscall::CallFlags::empty(), &words[..len])
        .map_err(|err| move_error(io::Error::from_raw_os_error(err.errno())))?;
    Moved::from_word(word as u64)
        .ok_or_else(|| move_error(io::Error::from_raw_os_error(libredox::errno::EIO)))
}

/// `err` and its sources, for a log line or a panic message.
pub(crate) fn report(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}
