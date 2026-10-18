//! The `kernel.vm` scheme's paths and calls.
//!
//! | Operation | On | Encoding |
//! | --- | --- | --- |
//! | open [`VM_PATH`] | the scheme root (root only) | a new, empty VM |
//! | open a vCPU path | a VM | `openat(vm, VcpuId::encode_path(..))`; `mmap` of the fd gives its [run page](crate::run) |
//! | [`Request::SetMemory`] | VM | metadata `[verb, slot, offset, len, gpa, prot]`, the regbuf region fd as payload; `len` 0 removes the slot |
//! | [`Request::SetCpuid`] | VM | metadata `[verb]`, [`crate::cpuid::CpuidEntry`] records as payload |
//! | [`Request::PassMsr`] | VM | metadata `[verb, msr, access]` |
//! | [`Request::Msi`] | VM | metadata `[verb, address, data]` |
//! | [`Request::Notify`] | VM | metadata `[verb, gpa, len]`, an event fd as payload |
//! | [`Request::Unnotify`] | VM | metadata `[verb, gpa]` |
//! | [`Request::AttachDomain`] | VM | metadata `[verb]`, a `kernel.iommu` domain fd as payload |
//! | [`Request::Run`] | vCPU | metadata `[verb]`; returns when the run page holds an exit |
//! | [`Request::GetRegs`], [`Request::SetRegs`] | vCPU | metadata `[verb]`, a [`crate::regs::Regs`] record |
//! | [`Request::GetSregs`], [`Request::SetSregs`] | vCPU | metadata `[verb]`, a [`crate::regs::Sregs`] record |
//!
//! [`Request::encode`] and [`Request::decode`] are the one place the metadata layout is written
//! down.

use thiserror::Error;

/// The scheme's name: the enclave launcher opens `/scheme/kernel.vm`.
pub const SCHEME_NAME: &str = "kernel.vm";

/// The path that, opened from the scheme root, creates a VM.
pub const VM_PATH: &str = "vm";

/// The first component of a vCPU path, opened from a VM.
pub const VCPU_PREFIX: &str = "vcpu";

/// The most vCPUs a VM may have.
pub const MAX_VCPUS: u32 = 256;

/// The most memory slots a VM may have.
pub const MAX_SLOTS: u16 = 64;

/// The guest page size: slots, notifications and grants are in these units.
pub const PAGE_SIZE: u64 = 4096;

/// A vCPU's number in its VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VcpuId(u32);

impl VcpuId {
    /// vCPU `id`, or `None` at or above [`MAX_VCPUS`].
    pub const fn new(id: u32) -> Option<Self> {
        if id >= MAX_VCPUS {
            return None;
        }
        Some(Self(id))
    }

    /// The number.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The longest vCPU path: `vcpu/` and three digits.
    pub const PATH_MAX: usize = VCPU_PREFIX.len() + 1 + 3;

    /// The path that opens this vCPU, written into `buf`.
    pub fn encode_path(self, buf: &mut [u8; Self::PATH_MAX]) -> &str {
        let prefix = VCPU_PREFIX.as_bytes();
        buf[..prefix.len()].copy_from_slice(prefix);
        buf[prefix.len()] = b'/';
        let mut len = prefix.len() + 1;
        let digits = [self.0 / 100, (self.0 / 10) % 10, self.0 % 10];
        let skip = if self.0 >= 100 {
            0
        } else if self.0 >= 10 {
            1
        } else {
            2
        };
        for digit in &digits[skip..] {
            buf[len] = b'0' + *digit as u8;
            len += 1;
        }
        // Only ASCII was written.
        core::str::from_utf8(&buf[..len]).unwrap_or(VCPU_PREFIX)
    }

    /// The vCPU a path names.
    ///
    /// # Errors
    ///
    /// [`PathError`] when the path is not `vcpu/<n>` in canonical decimal with `n` below
    /// [`MAX_VCPUS`].
    pub fn parse_path(path: &[u8]) -> Result<Self, PathError> {
        let digits = path
            .strip_prefix(VCPU_PREFIX.as_bytes())
            .and_then(|rest| rest.strip_prefix(b"/"))
            .ok_or(PathError)?;
        if digits.is_empty() || digits.len() > 3 || (digits.len() > 1 && digits[0] == b'0') {
            return Err(PathError);
        }
        let mut id = 0u32;
        for &digit in digits {
            if !digit.is_ascii_digit() {
                return Err(PathError);
            }
            id = id * 10 + u32::from(digit - b'0');
        }
        Self::new(id).ok_or(PathError)
    }
}

/// A path that names no vCPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("not a vCPU path")]
pub struct PathError;

/// What a guest may do with a memory slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Protection {
    /// Writes (a read-only slot serves ROM images).
    pub write: bool,
    /// Instruction fetches.
    pub execute: bool,
}

impl Protection {
    const fn to_word(self) -> u64 {
        (self.write as u64) | ((self.execute as u64) << 1)
    }

    const fn from_word(word: u64) -> Option<Self> {
        if word & !3 != 0 {
            return None;
        }
        Some(Self {
            write: word & 1 != 0,
            execute: word & 2 != 0,
        })
    }
}

/// [`Request::SetMemory`]: map part of a regbuf region into guest-physical memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SetMemory {
    /// The slot, below [`MAX_SLOTS`].
    pub slot: u16,
    /// The offset in the region, page-aligned.
    pub offset: u64,
    /// The length, page-aligned; 0 removes the slot.
    pub len: u64,
    /// The guest-physical address, page-aligned.
    pub gpa: u64,
    /// What the guest may do.
    pub prot: Protection,
}

/// Which MSR accesses a [`Request::PassMsr`] lets through without an exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MsrAccess {
    /// Reads only.
    Read,
    /// Writes only.
    Write,
    /// Both.
    ReadWrite,
}

const VERB_SET_MEMORY: u64 = 1;
const VERB_SET_CPUID: u64 = 2;
const VERB_PASS_MSR: u64 = 3;
const VERB_MSI: u64 = 4;
const VERB_NOTIFY: u64 = 5;
const VERB_UNNOTIFY: u64 = 6;
const VERB_ATTACH_DOMAIN: u64 = 7;
const VERB_RUN: u64 = 8;
const VERB_GET_REGS: u64 = 9;
const VERB_SET_REGS: u64 = 10;
const VERB_GET_SREGS: u64 = 11;
const VERB_SET_SREGS: u64 = 12;

/// A call on a VM or vCPU fd.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Request {
    /// Map, remap or remove a memory slot.
    SetMemory(SetMemory),
    /// Set the CPUID table the kernel answers from.
    SetCpuid,
    /// Let accesses to an MSR through without an exit. Only MSRs on the kernel's own allow
    /// list are accepted; every other MSR access exits to the VMM.
    PassMsr {
        /// The MSR.
        msr: u32,
        /// Which accesses.
        access: MsrAccess,
    },
    /// Deliver an MSI to the guest.
    Msi {
        /// The MSI address.
        address: u64,
        /// The MSI data.
        data: u32,
    },
    /// Signal the payload's event fd on guest writes to `gpa`, without an exit to the VMM.
    Notify {
        /// The guest-physical address.
        gpa: u64,
        /// The write size to match: 1, 2, 4 or 8, or 0 for any.
        len: u8,
    },
    /// Remove a [`Request::Notify`] registration.
    Unnotify {
        /// The guest-physical address.
        gpa: u64,
    },
    /// Mirror the memory slots into a DMA domain at IOVA = GPA (device passthrough).
    AttachDomain,
    /// Run the vCPU.
    Run,
    /// Read the general registers.
    GetRegs,
    /// Write the general registers.
    SetRegs,
    /// Read the system registers.
    GetSregs,
    /// Write the system registers.
    SetSregs,
}

impl Request {
    /// The most metadata words a request takes.
    pub const MAX_WORDS: usize = 6;

    /// The metadata words, and how many are used.
    pub const fn encode(self) -> ([u64; Self::MAX_WORDS], usize) {
        let mut words = [0; Self::MAX_WORDS];
        let len = match self {
            Self::SetMemory(m) => {
                words = [
                    VERB_SET_MEMORY,
                    m.slot as u64,
                    m.offset,
                    m.len,
                    m.gpa,
                    m.prot.to_word(),
                ];
                6
            }
            Self::PassMsr { msr, access } => {
                words[0] = VERB_PASS_MSR;
                words[1] = msr as u64;
                words[2] = match access {
                    MsrAccess::Read => 1,
                    MsrAccess::Write => 2,
                    MsrAccess::ReadWrite => 3,
                };
                3
            }
            Self::Msi { address, data } => {
                words[0] = VERB_MSI;
                words[1] = address;
                words[2] = data as u64;
                3
            }
            Self::Notify { gpa, len } => {
                words[0] = VERB_NOTIFY;
                words[1] = gpa;
                words[2] = len as u64;
                3
            }
            Self::Unnotify { gpa } => {
                words[0] = VERB_UNNOTIFY;
                words[1] = gpa;
                2
            }
            Self::SetCpuid => Self::verb_only(&mut words, VERB_SET_CPUID),
            Self::AttachDomain => Self::verb_only(&mut words, VERB_ATTACH_DOMAIN),
            Self::Run => Self::verb_only(&mut words, VERB_RUN),
            Self::GetRegs => Self::verb_only(&mut words, VERB_GET_REGS),
            Self::SetRegs => Self::verb_only(&mut words, VERB_SET_REGS),
            Self::GetSregs => Self::verb_only(&mut words, VERB_GET_SREGS),
            Self::SetSregs => Self::verb_only(&mut words, VERB_SET_SREGS),
        };
        (words, len)
    }

    const fn verb_only(words: &mut [u64; Self::MAX_WORDS], verb: u64) -> usize {
        words[0] = verb;
        1
    }

    /// The request `words` encode.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// word count does not match it, and [`DecodeError::Value`] for an out-of-range value: an
    /// unaligned or overflowing slot, a slot number at or above [`MAX_SLOTS`], an unknown
    /// protection or access, an MSI data word above 32 bits, or a notify size other than 0, 1,
    /// 2, 4 or 8.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&verb, args) = words.split_first().ok_or(DecodeError::Verb)?;
        let expected = match verb {
            VERB_SET_MEMORY => 5,
            VERB_PASS_MSR | VERB_MSI | VERB_NOTIFY => 2,
            VERB_UNNOTIFY => 1,
            VERB_SET_CPUID | VERB_ATTACH_DOMAIN | VERB_RUN | VERB_GET_REGS | VERB_SET_REGS
            | VERB_GET_SREGS | VERB_SET_SREGS => 0,
            _ => return Err(DecodeError::Verb),
        };
        if args.len() != expected {
            return Err(DecodeError::Length);
        }
        Ok(match verb {
            VERB_SET_MEMORY => Self::SetMemory(decode_set_memory(args)?),
            VERB_PASS_MSR => Self::PassMsr {
                msr: u32::try_from(args[0]).map_err(|_| DecodeError::Value)?,
                access: match args[1] {
                    1 => MsrAccess::Read,
                    2 => MsrAccess::Write,
                    3 => MsrAccess::ReadWrite,
                    _ => return Err(DecodeError::Value),
                },
            },
            VERB_MSI => Self::Msi {
                address: args[0],
                data: u32::try_from(args[1]).map_err(|_| DecodeError::Value)?,
            },
            VERB_NOTIFY => Self::Notify {
                gpa: args[0],
                len: match args[1] {
                    0 | 1 | 2 | 4 | 8 => args[1] as u8,
                    _ => return Err(DecodeError::Value),
                },
            },
            VERB_UNNOTIFY => Self::Unnotify { gpa: args[0] },
            VERB_SET_CPUID => Self::SetCpuid,
            VERB_ATTACH_DOMAIN => Self::AttachDomain,
            VERB_RUN => Self::Run,
            VERB_GET_REGS => Self::GetRegs,
            VERB_SET_REGS => Self::SetRegs,
            VERB_GET_SREGS => Self::GetSregs,
            _ => Self::SetSregs,
        })
    }
}

fn decode_set_memory(args: &[u64]) -> Result<SetMemory, DecodeError> {
    let &[slot, offset, len, gpa, prot] = args else {
        return Err(DecodeError::Length);
    };
    let slot = u16::try_from(slot)
        .ok()
        .filter(|&slot| slot < MAX_SLOTS)
        .ok_or(DecodeError::Value)?;
    let aligned = |value: u64| value.is_multiple_of(PAGE_SIZE);
    if !aligned(offset) || !aligned(len) || !aligned(gpa) {
        return Err(DecodeError::Value);
    }
    if offset.checked_add(len).is_none() || gpa.checked_add(len).is_none() {
        return Err(DecodeError::Value);
    }
    Ok(SetMemory {
        slot,
        offset,
        len,
        gpa,
        prot: Protection::from_word(prot).ok_or(DecodeError::Value)?,
    })
}

/// Why metadata words are not a valid [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    #[error("missing or unknown kernel.vm verb")]
    Verb,
    /// The number of words does not match the verb.
    #[error("wrong number of metadata words for the kernel.vm verb")]
    Length,
    /// A word has a value the call does not allow.
    #[error("a kernel.vm call word is out of range")]
    Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> SetMemory {
        SetMemory {
            slot: 3,
            offset: 0x20_0000,
            len: 0x4000_0000,
            gpa: 0x1_0000_0000,
            prot: Protection {
                write: true,
                execute: true,
            },
        }
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::SetMemory(memory()),
            Request::SetMemory(SetMemory { len: 0, ..memory() }),
            Request::SetCpuid,
            Request::PassMsr {
                msr: 0xc000_0100,
                access: MsrAccess::ReadWrite,
            },
            Request::Msi {
                address: 0xfee0_1000,
                data: 0x4031,
            },
            Request::Notify {
                gpa: 0xfe00_3000,
                len: 2,
            },
            Request::Unnotify { gpa: 0xfe00_3000 },
            Request::AttachDomain,
            Request::Run,
            Request::GetRegs,
            Request::SetRegs,
            Request::GetSregs,
            Request::SetSregs,
        ] {
            let (words, len) = request.encode();
            assert_eq!(Request::decode(&words[..len]), Ok(request));
        }
    }

    #[test]
    fn bad_requests_are_refused() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[99]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[VERB_RUN, 0]), Err(DecodeError::Length));
        let (good, len) = Request::SetMemory(memory()).encode();
        for (at, value) in [
            (1, 64),
            (2, 0x1001),
            (3, 0x10),
            (4, 0x123),
            (5, 4),
            (3, !0xfff_u64),
        ] {
            let mut bad = good;
            bad[at] = value;
            assert_eq!(
                Request::decode(&bad[..len]),
                Err(DecodeError::Value),
                "word {at}"
            );
        }
        assert_eq!(
            Request::decode(&[VERB_PASS_MSR, 1 << 32, 1]),
            Err(DecodeError::Value)
        );
        assert_eq!(
            Request::decode(&[VERB_PASS_MSR, 0x10, 0]),
            Err(DecodeError::Value)
        );
        assert_eq!(
            Request::decode(&[VERB_MSI, 0xfee0_0000, 1 << 32]),
            Err(DecodeError::Value)
        );
        assert_eq!(
            Request::decode(&[VERB_NOTIFY, 0x1000, 3]),
            Err(DecodeError::Value)
        );
    }

    #[test]
    fn vcpu_paths_round_trip() {
        let mut buf = [0; VcpuId::PATH_MAX];
        for id in [0, 7, 10, 99, 100, 255] {
            let vcpu = VcpuId::new(id).expect("in range");
            let path = vcpu.encode_path(&mut buf);
            assert_eq!(VcpuId::parse_path(path.as_bytes()), Ok(vcpu), "{path}");
        }
        assert_eq!(
            VcpuId::new(0).expect("in range").encode_path(&mut buf),
            "vcpu/0"
        );
        assert_eq!(VcpuId::new(MAX_VCPUS), None);
        for bad in [
            "vcpu/",
            "vcpu/256",
            "vcpu/01",
            "vcpu/1a",
            "vcpu1",
            "cpu/1",
            "vcpu/1000",
        ] {
            assert_eq!(VcpuId::parse_path(bad.as_bytes()), Err(PathError), "{bad}");
        }
    }
}
