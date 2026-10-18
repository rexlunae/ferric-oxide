//! The run page: how `Run` hands an exit to the VMM, and how the VMM answers it.
//!
//! Each vCPU fd maps one page shared by the kernel and the VMM. Only its first [`AREA_BYTES`]
//! bytes are used:
//!
//! | Offset | Size | Written by | Field |
//! | --- | --- | --- | --- |
//! | 0 | 4 | kernel | [`VERSION`] |
//! | 4 | 4 | kernel | exit kind: 1 I/O, 2 MMIO, 3 MSR, 4 shutdown, 5 entry failed, 6 interrupted |
//! | 8 | 8 | kernel | reserved, 0 |
//! | 16 | 48 | kernel | the exit's fields (see [`Exit`]) |
//! | 64 | 8 | VMM | completion data, for IN, an MMIO read or RDMSR |
//! | 72 | 4 | VMM | completion flags: bit 0 raises #GP in the guest (an MSR the VMM refuses) |
//! | 76 | 52 | — | reserved, 0 |
//!
//! Integers are little-endian. Neither side trusts the other: the VMM decodes the exit with
//! [`Exit::read`] and the kernel checks the completion against the exit it reported with
//! [`Completion::read`], each from its own copy of the bytes.

use thiserror::Error;

/// The layout version the kernel writes.
pub const VERSION: u32 = 1;

/// The bytes of the page the layout uses.
pub const AREA_BYTES: usize = 128;

const KIND_IO: u32 = 1;
const KIND_MMIO: u32 = 2;
const KIND_MSR: u32 = 3;
const KIND_SHUTDOWN: u32 = 4;
const KIND_ENTRY_FAILED: u32 = 5;
const KIND_INTERRUPTED: u32 = 6;

/// An exit the VMM must handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Exit {
    /// Port I/O. For OUT, `data` holds the value written; IN needs a [`Completion`].
    Io {
        /// The port.
        port: u16,
        /// The access size: 1, 2 or 4.
        size: u8,
        /// IN (true) or OUT (false).
        input: bool,
        /// The value written by OUT.
        data: u32,
    },
    /// An access to guest-physical memory no slot maps. For writes, `data` holds the value;
    /// reads need a [`Completion`].
    Mmio {
        /// The guest-physical address.
        gpa: u64,
        /// The access size: 1, 2, 4 or 8.
        size: u8,
        /// A write (true) or read (false).
        write: bool,
        /// The value written.
        data: u64,
    },
    /// An MSR access the kernel does not handle. WRMSR carries the value; RDMSR needs a
    /// [`Completion`]. Either may be refused with [`Completion::fault`].
    Msr {
        /// The MSR.
        index: u32,
        /// WRMSR (true) or RDMSR (false).
        write: bool,
        /// The value written.
        value: u64,
    },
    /// The guest shut down (for example a triple fault).
    Shutdown,
    /// VM entry failed; `code` is the backend's reason (the VMX instruction error or exit
    /// reason, or the SVM exit code).
    EntryFailed {
        /// The reason.
        code: u64,
    },
    /// A signal interrupted `Run` before an exit.
    Interrupted,
}

impl Exit {
    /// Whether the VMM must write completion data before the next `Run`.
    pub const fn needs_data(&self) -> bool {
        matches!(
            self,
            Self::Io { input: true, .. }
                | Self::Mmio { write: false, .. }
                | Self::Msr { write: false, .. }
        )
    }

    /// Writes the exit into `area` (the first [`AREA_BYTES`] of the run page), clearing the
    /// completion fields.
    pub fn write(&self, area: &mut [u8; AREA_BYTES]) {
        area.fill(0);
        put(area, 0, &VERSION.to_le_bytes());
        let kind = match *self {
            Self::Io {
                port,
                size,
                input,
                data,
            } => {
                put(area, 16, &port.to_le_bytes());
                area[18] = size;
                area[19] = u8::from(input);
                put(area, 24, &data.to_le_bytes());
                KIND_IO
            }
            Self::Mmio {
                gpa,
                size,
                write,
                data,
            } => {
                put(area, 16, &gpa.to_le_bytes());
                area[24] = size;
                area[25] = u8::from(write);
                put(area, 32, &data.to_le_bytes());
                KIND_MMIO
            }
            Self::Msr {
                index,
                write,
                value,
            } => {
                put(area, 16, &index.to_le_bytes());
                area[20] = u8::from(write);
                put(area, 24, &value.to_le_bytes());
                KIND_MSR
            }
            Self::Shutdown => KIND_SHUTDOWN,
            Self::EntryFailed { code } => {
                put(area, 16, &code.to_le_bytes());
                KIND_ENTRY_FAILED
            }
            Self::Interrupted => KIND_INTERRUPTED,
        };
        put(area, 4, &kind.to_le_bytes());
    }

    /// The exit an area holds.
    ///
    /// # Errors
    ///
    /// [`RunError::Version`] for another layout version, [`RunError::Kind`] for an unknown
    /// kind, [`RunError::Size`] for an access size the kind does not allow, and
    /// [`RunError::Reserved`] when a field the kind does not use is not zero.
    pub fn read(area: &[u8; AREA_BYTES]) -> Result<Self, RunError> {
        if u32_at(area, 0) != VERSION {
            return Err(RunError::Version);
        }
        let exit = match u32_at(area, 4) {
            KIND_IO => Self::Io {
                port: u16::from_le_bytes([area[16], area[17]]),
                size: io_size(area[18])?,
                input: flag(area[19])?,
                data: u32_at(area, 24),
            },
            KIND_MMIO => Self::Mmio {
                gpa: u64_at(area, 16),
                size: mmio_size(area[24])?,
                write: flag(area[25])?,
                data: u64_at(area, 32),
            },
            KIND_MSR => Self::Msr {
                index: u32_at(area, 16),
                write: flag(area[20])?,
                value: u64_at(area, 24),
            },
            KIND_SHUTDOWN => Self::Shutdown,
            KIND_ENTRY_FAILED => Self::EntryFailed {
                code: u64_at(area, 16),
            },
            KIND_INTERRUPTED => Self::Interrupted,
            _ => return Err(RunError::Kind),
        };
        // Everything but the completion fields must be exactly what `write` produces.
        let mut canonical = [0; AREA_BYTES];
        exit.write(&mut canonical);
        if canonical[..64] != area[..64] {
            return Err(RunError::Reserved);
        }
        Ok(exit)
    }
}

/// The VMM's answer to an exit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Completion {
    /// The value for IN, an MMIO read or RDMSR.
    pub data: u64,
    /// Raise #GP in the guest instead (MSR exits only).
    pub fault: bool,
}

impl Completion {
    /// Writes the completion fields of `area`.
    pub fn write(&self, area: &mut [u8; AREA_BYTES]) {
        put(area, 64, &self.data.to_le_bytes());
        put(area, 72, &u32::from(self.fault).to_le_bytes());
        area[76..].fill(0);
    }

    /// The completion for `exit` in `area`.
    ///
    /// # Errors
    ///
    /// [`RunError::Reserved`] when an unknown flag or reserved byte is set,
    /// [`RunError::Fault`] when a fault answers an exit other than an MSR access, and
    /// [`RunError::DataTooWide`] when the data does not fit the access size.
    pub fn read(area: &[u8; AREA_BYTES], exit: &Exit) -> Result<Self, RunError> {
        let flags = u32_at(area, 72);
        if flags & !1 != 0 || area[76..].iter().any(|&b| b != 0) {
            return Err(RunError::Reserved);
        }
        let completion = Self {
            data: u64_at(area, 64),
            fault: flags == 1,
        };
        if completion.fault && !matches!(exit, Exit::Msr { .. }) {
            return Err(RunError::Fault);
        }
        let size = match *exit {
            Exit::Io { size, .. } | Exit::Mmio { size, .. } => size,
            _ => 8,
        };
        if size < 8 && completion.data >> (8 * u32::from(size)) != 0 {
            return Err(RunError::DataTooWide);
        }
        Ok(completion)
    }
}

fn put(area: &mut [u8; AREA_BYTES], at: usize, bytes: &[u8]) {
    area[at..at + bytes.len()].copy_from_slice(bytes);
}

fn u32_at(area: &[u8; AREA_BYTES], at: usize) -> u32 {
    u32::from_le_bytes([area[at], area[at + 1], area[at + 2], area[at + 3]])
}

fn u64_at(area: &[u8; AREA_BYTES], at: usize) -> u64 {
    let mut le = [0; 8];
    le.copy_from_slice(&area[at..at + 8]);
    u64::from_le_bytes(le)
}

fn flag(byte: u8) -> Result<bool, RunError> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(RunError::Reserved),
    }
}

fn io_size(size: u8) -> Result<u8, RunError> {
    match size {
        1 | 2 | 4 => Ok(size),
        _ => Err(RunError::Size),
    }
}

fn mmio_size(size: u8) -> Result<u8, RunError> {
    match size {
        1 | 2 | 4 | 8 => Ok(size),
        _ => Err(RunError::Size),
    }
}

/// Why a run-page area was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RunError {
    /// The layout version is not [`VERSION`].
    #[error("unknown run page version")]
    Version,
    /// The exit kind is unknown.
    #[error("unknown exit kind")]
    Kind,
    /// The access size is not one the exit kind allows.
    #[error("invalid access size in the run page")]
    Size,
    /// A reserved field or flag is set.
    #[error("a reserved run page field is set")]
    Reserved,
    /// A fault answers an exit that cannot fault.
    #[error("only MSR exits can be answered with a fault")]
    Fault,
    /// The completion data does not fit the access size.
    #[error("completion data is wider than the access")]
    DataTooWide,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exits() -> [Exit; 8] {
        [
            Exit::Io {
                port: 0x3f8,
                size: 1,
                input: false,
                data: 0x41,
            },
            Exit::Io {
                port: 0xcfc,
                size: 4,
                input: true,
                data: 0,
            },
            Exit::Mmio {
                gpa: 0xfe00_0000,
                size: 8,
                write: true,
                data: u64::MAX,
            },
            Exit::Mmio {
                gpa: 0xfe00_1000,
                size: 2,
                write: false,
                data: 0,
            },
            Exit::Msr {
                index: 0x8b,
                write: false,
                value: 0,
            },
            Exit::Shutdown,
            Exit::EntryFailed { code: 0x8000_0021 },
            Exit::Interrupted,
        ]
    }

    #[test]
    fn exits_round_trip() {
        for exit in exits() {
            let mut area = [0; AREA_BYTES];
            exit.write(&mut area);
            assert_eq!(Exit::read(&area), Ok(exit));
        }
        assert!(exits()[1].needs_data() && exits()[3].needs_data() && exits()[4].needs_data());
        assert!(!exits()[0].needs_data() && !exits()[2].needs_data());
    }

    #[test]
    fn bad_areas_are_refused() {
        let mut area = [0; AREA_BYTES];
        exits()[0].write(&mut area);
        let mut bad = area;
        bad[0] = 2;
        assert_eq!(Exit::read(&bad), Err(RunError::Version));
        let mut bad = area;
        bad[4] = 9;
        assert_eq!(Exit::read(&bad), Err(RunError::Kind));
        let mut bad = area;
        bad[18] = 3;
        assert_eq!(Exit::read(&bad), Err(RunError::Size));
        let mut bad = area;
        bad[19] = 2;
        assert_eq!(Exit::read(&bad), Err(RunError::Reserved));
        let mut bad = area;
        bad[40] = 1;
        assert_eq!(Exit::read(&bad), Err(RunError::Reserved));
        // The completion half is the VMM's and is not checked here.
        let mut answered = area;
        answered[64] = 0xff;
        assert_eq!(Exit::read(&answered), Ok(exits()[0]));
    }

    #[test]
    fn completions_are_checked_against_the_exit() {
        let mut area = [0; AREA_BYTES];
        let read16 = exits()[3];
        read16.write(&mut area);
        Completion {
            data: 0xbeef,
            fault: false,
        }
        .write(&mut area);
        assert_eq!(
            Completion::read(&area, &read16),
            Ok(Completion {
                data: 0xbeef,
                fault: false
            })
        );
        Completion {
            data: 0x1_0000,
            fault: false,
        }
        .write(&mut area);
        assert_eq!(Completion::read(&area, &read16), Err(RunError::DataTooWide));
        Completion {
            data: 0,
            fault: true,
        }
        .write(&mut area);
        assert_eq!(Completion::read(&area, &read16), Err(RunError::Fault));
        let rdmsr = exits()[4];
        assert_eq!(
            Completion::read(&area, &rdmsr),
            Ok(Completion {
                data: 0,
                fault: true
            })
        );
        area[100] = 1;
        assert_eq!(Completion::read(&area, &rdmsr), Err(RunError::Reserved));
    }
}
