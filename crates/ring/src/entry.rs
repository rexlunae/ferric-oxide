//! Submission and completion queue entries, and how an SQE's fields are checked.
//!
//! An [`Sqe`] is 64 bytes, the size of the kernel's scheme request (`schemev2::Sqe`), and its
//! [`Opcode`]s are that request's numbers wherever the operation is a scheme operation, so a
//! ring request and a scheme request name the same thing. A [`Cqe`] is 32 bytes.
//!
//! | SQE word | Fields |
//! | --- | --- |
//! | 0 | `opcode: u8`, `flags: u8`, `ioprio: u16`, `fd: u32` |
//! | 1 | `offset` ([`CURRENT_POSITION`]: the descriptor's own position) |
//! | 2 | `addr`: the buffer's address, or with [`SqeFlags::BUF_GRANT`] a grant slot and offset |
//! | 3 | `len: u32`, `op_flags: u32` |
//! | 4 | `user_data`, returned in the CQE |
//! | 5..7 | reserved, zero |
//!
//! | CQE word | Fields |
//! | --- | --- |
//! | 0 | `user_data` |
//! | 1 | `result: i64`: the byte count or other result, or minus the errno |
//! | 2 | `flags: u32`, 4 reserved bytes |
//! | 3 | reserved |

use core::fmt;

use bitflags::bitflags;

/// An entry of one of the queues: a fixed number of 64-bit words.
pub trait Entry: Sized + Copy {
    /// The entry's words.
    type Words: AsRef<[u64]> + AsMut<[u64]> + Default;

    /// How many words an entry has.
    const WORDS: usize;

    /// The entry's encoding.
    fn to_words(&self) -> Self::Words;

    /// The entry encoded in `words`. Every bit pattern decodes; fields are checked where they
    /// are used.
    fn from_words(words: &Self::Words) -> Self;
}

/// The `offset` that means "the descriptor's own position", as `read` and `write` use it.
pub const CURRENT_POSITION: u64 = u64::MAX;

/// What an SQE asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u8)]
pub enum Opcode {
    /// Nothing; completes with 0.
    Nop = 0,
    /// Read `len` bytes from `fd` at `offset` into the buffer (`schemev2::Opcode::Read`).
    Read = 5,
    /// Write `len` bytes of the buffer to `fd` at `offset` (`schemev2::Opcode::Write`).
    Write = 6,
    /// Complete with the size of `fd` in bytes (`schemev2::Opcode::Fsize`).
    Fsize = 7,
    /// Write `fd`'s `Stat` into the buffer (`schemev2::Opcode::Fstat`); the buffer must hold a
    /// whole `Stat`.
    Fstat = 15,
    /// Flush `fd` to its device (`schemev2::Opcode::Fsync`).
    Fsync = 17,
}

impl Opcode {
    /// The opcode numbered `raw`, if this crate knows it.
    pub const fn from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            0 => Self::Nop,
            5 => Self::Read,
            6 => Self::Write,
            7 => Self::Fsize,
            15 => Self::Fstat,
            17 => Self::Fsync,
            _ => return None,
        })
    }

    /// Whether the operation moves `len` bytes, so that moving fewer breaks a chain.
    pub const fn transfers(self) -> bool {
        matches!(self, Self::Read | Self::Write)
    }
}

bitflags! {
    /// An SQE's flags.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct SqeFlags: u8 {
        /// The next SQE runs only if this one succeeds ([`chain`](crate::chain)).
        const LINK = 1 << 0;
        /// `fd` is a slot of the ring's file table, not a descriptor number.
        const FIXED_FILE = 1 << 1;
        /// `addr` names a grant slot and an offset in it ([`Buffer::Grant`]).
        const BUF_GRANT = 1 << 2;
        /// On the first SQE of a chain: the chain runs on the ring's worker, after `Enter`
        /// returns ([`abi`](crate::abi#asynchronous-chains)). Elsewhere it has no effect.
        const ASYNC = 1 << 3;
    }
}

bitflags! {
    /// A CQE's flags.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CqeFlags: u32 {
        /// More CQEs for the same SQE follow (multi-shot requests, later).
        const MORE = 1 << 0;
    }
}

/// A submission queue entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sqe {
    /// The raw [`Opcode`].
    pub opcode: u8,
    /// The flags; unknown bits are kept so they can be refused.
    pub flags: SqeFlags,
    /// The priority; must be 0 for now.
    pub ioprio: u16,
    /// The descriptor, or with [`SqeFlags::FIXED_FILE`] the file table slot.
    pub fd: u32,
    /// Where in the file, or [`CURRENT_POSITION`].
    pub offset: u64,
    /// The buffer ([`Buffer`]).
    pub addr: u64,
    /// The buffer's length in bytes.
    pub len: u32,
    /// Operation flags; must be 0 for now.
    pub op_flags: u32,
    /// The caller's tag for the request, returned in its CQE.
    pub user_data: u64,
    /// Reserved words; must be zero.
    pub reserved: [u64; 3],
}

/// The file an SQE names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileRef {
    /// A descriptor of the submitting process.
    Fd(u32),
    /// A slot of the ring's file table.
    Fixed(u32),
}

/// The buffer an SQE names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Buffer {
    /// `len` bytes at `addr` in the submitting process.
    Addr {
        /// The address.
        addr: u64,
        /// The length.
        len: u32,
    },
    /// `len` bytes at `offset` in the grant in `slot` of the ring's grant table.
    Grant {
        /// The grant table slot: `addr`'s top 16 bits.
        slot: u16,
        /// The byte offset in the grant: `addr`'s low 48 bits.
        offset: u64,
        /// The length.
        len: u32,
    },
}

impl Buffer {
    /// The length in bytes.
    pub const fn len(&self) -> u32 {
        match *self {
            Self::Addr { len, .. } | Self::Grant { len, .. } => len,
        }
    }

    /// Whether the buffer is empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An SQE's request, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// [`Opcode::Nop`].
    Nop,
    /// [`Opcode::Read`] into `buf`.
    Read {
        /// The file.
        file: FileRef,
        /// The offset, or [`CURRENT_POSITION`].
        offset: u64,
        /// The buffer.
        buf: Buffer,
    },
    /// [`Opcode::Write`] from `buf`.
    Write {
        /// The file.
        file: FileRef,
        /// The offset, or [`CURRENT_POSITION`].
        offset: u64,
        /// The buffer.
        buf: Buffer,
    },
    /// [`Opcode::Fsize`].
    Fsize {
        /// The file.
        file: FileRef,
    },
    /// [`Opcode::Fstat`] into `buf`.
    Fstat {
        /// The file.
        file: FileRef,
        /// The buffer for the `Stat`.
        buf: Buffer,
    },
    /// [`Opcode::Fsync`].
    Fsync {
        /// The file.
        file: FileRef,
    },
}

impl Op {
    /// The file the operation runs on, if it has one.
    pub const fn file(&self) -> Option<FileRef> {
        match *self {
            Self::Nop => None,
            Self::Read { file, .. }
            | Self::Write { file, .. }
            | Self::Fsize { file }
            | Self::Fstat { file, .. }
            | Self::Fsync { file } => Some(file),
        }
    }
}

/// Why an SQE is not a valid request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SqeError {
    /// No operation has this opcode (`EOPNOTSUPP`).
    Opcode(u8),
    /// Flags this crate does not know (`EINVAL`).
    Flags(u8),
    /// `ioprio`, `op_flags`, a reserved word, or a field the operation does not use is not zero
    /// (`EINVAL`).
    Reserved,
}

impl fmt::Display for SqeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Opcode(raw) => write!(f, "no ring operation has opcode {raw}"),
            Self::Flags(raw) => write!(f, "unknown SQE flags {raw:#x}"),
            Self::Reserved => f.write_str("a reserved or unused SQE field is not zero"),
        }
    }
}

impl core::error::Error for SqeError {}

/// The bits of `addr` that hold a grant offset with [`SqeFlags::BUF_GRANT`].
const GRANT_OFFSET_BITS: u32 = 48;

impl Sqe {
    /// An SQE's size in bytes.
    pub const BYTES: usize = 64;

    /// A [`Opcode::Nop`] tagged `user_data`.
    pub const fn nop(user_data: u64) -> Self {
        Self::new(Opcode::Nop, 0, 0, 0, 0, user_data)
    }

    /// A [`Opcode::Read`] of `len` bytes from descriptor `fd` at `offset` into `addr`.
    pub const fn read(fd: u32, offset: u64, addr: u64, len: u32, user_data: u64) -> Self {
        Self::new(Opcode::Read, fd, offset, addr, len, user_data)
    }

    /// A [`Opcode::Write`] of `len` bytes from `addr` to descriptor `fd` at `offset`.
    pub const fn write(fd: u32, offset: u64, addr: u64, len: u32, user_data: u64) -> Self {
        Self::new(Opcode::Write, fd, offset, addr, len, user_data)
    }

    /// A [`Opcode::Fsync`] of descriptor `fd`.
    pub const fn fsync(fd: u32, user_data: u64) -> Self {
        Self::new(Opcode::Fsync, fd, 0, 0, 0, user_data)
    }

    /// A [`Opcode::Fsize`] of descriptor `fd`.
    pub const fn fsize(fd: u32, user_data: u64) -> Self {
        Self::new(Opcode::Fsize, fd, 0, 0, 0, user_data)
    }

    /// A [`Opcode::Fstat`] of descriptor `fd` into the `len` bytes at `addr`.
    pub const fn fstat(fd: u32, addr: u64, len: u32, user_data: u64) -> Self {
        Self::new(Opcode::Fstat, fd, 0, addr, len, user_data)
    }

    /// An SQE with no flags.
    const fn new(op: Opcode, fd: u32, offset: u64, addr: u64, len: u32, user_data: u64) -> Self {
        Self {
            opcode: op as u8,
            flags: SqeFlags::empty(),
            ioprio: 0,
            fd,
            offset,
            addr,
            len,
            op_flags: 0,
            user_data,
            reserved: [0; 3],
        }
    }

    /// This SQE with [`SqeFlags::LINK`]: the next one runs only if this one succeeds.
    pub const fn linked(mut self) -> Self {
        self.flags = self.flags.union(SqeFlags::LINK);
        self
    }

    /// This SQE with [`SqeFlags::FIXED_FILE`]: `fd` is a slot of the ring's file table.
    pub const fn fixed(mut self) -> Self {
        self.flags = self.flags.union(SqeFlags::FIXED_FILE);
        self
    }

    /// This SQE with [`SqeFlags::ASYNC`]: on a chain's first SQE, the chain runs on the ring's
    /// worker.
    pub const fn run_async(mut self) -> Self {
        self.flags = self.flags.union(SqeFlags::ASYNC);
        self
    }

    /// Whether the next SQE belongs to the same chain.
    pub const fn is_linked(&self) -> bool {
        self.flags.contains(SqeFlags::LINK)
    }

    /// The request, after checking every field.
    ///
    /// # Errors
    ///
    /// [`SqeError::Opcode`] for an unknown opcode, [`SqeError::Flags`] for unknown flags, and
    /// [`SqeError::Reserved`] when a field that must be zero is not.
    pub fn op(&self) -> Result<Op, SqeError> {
        let opcode = Opcode::from_raw(self.opcode).ok_or(SqeError::Opcode(self.opcode))?;
        if !SqeFlags::all().contains(self.flags) {
            return Err(SqeError::Flags(self.flags.bits()));
        }
        if self.ioprio != 0 || self.op_flags != 0 || self.reserved != [0; 3] {
            return Err(SqeError::Reserved);
        }
        let file = if self.flags.contains(SqeFlags::FIXED_FILE) {
            FileRef::Fixed(self.fd)
        } else {
            FileRef::Fd(self.fd)
        };
        let buf = if self.flags.contains(SqeFlags::BUF_GRANT) {
            Buffer::Grant {
                slot: (self.addr >> GRANT_OFFSET_BITS) as u16,
                offset: self.addr & ((1 << GRANT_OFFSET_BITS) - 1),
                len: self.len,
            }
        } else {
            Buffer::Addr {
                addr: self.addr,
                len: self.len,
            }
        };
        let no_file = self.fd == 0 && !self.flags.contains(SqeFlags::FIXED_FILE);
        let no_buf = self.addr == 0 && self.len == 0 && !self.flags.contains(SqeFlags::BUF_GRANT);
        Ok(match opcode {
            Opcode::Nop if no_file && no_buf && self.offset == 0 => Op::Nop,
            Opcode::Read => Op::Read {
                file,
                offset: self.offset,
                buf,
            },
            Opcode::Write => Op::Write {
                file,
                offset: self.offset,
                buf,
            },
            Opcode::Fsize if no_buf && self.offset == 0 => Op::Fsize { file },
            Opcode::Fstat if self.offset == 0 => Op::Fstat { file, buf },
            Opcode::Fsync if no_buf && self.offset == 0 => Op::Fsync { file },
            Opcode::Nop | Opcode::Fsize | Opcode::Fstat | Opcode::Fsync => {
                return Err(SqeError::Reserved);
            }
        })
    }

    /// Encodes a grant buffer into an `addr` for [`SqeFlags::BUF_GRANT`], or `None` when the
    /// offset needs more than 48 bits.
    pub const fn grant_addr(slot: u16, offset: u64) -> Option<u64> {
        if offset >> GRANT_OFFSET_BITS != 0 {
            None
        } else {
            Some((slot as u64) << GRANT_OFFSET_BITS | offset)
        }
    }
}

impl Entry for Sqe {
    type Words = [u64; 8];
    const WORDS: usize = 8;

    fn to_words(&self) -> [u64; 8] {
        [
            u64::from(self.opcode)
                | u64::from(self.flags.bits()) << 8
                | u64::from(self.ioprio) << 16
                | u64::from(self.fd) << 32,
            self.offset,
            self.addr,
            u64::from(self.len) | u64::from(self.op_flags) << 32,
            self.user_data,
            self.reserved[0],
            self.reserved[1],
            self.reserved[2],
        ]
    }

    fn from_words(words: &[u64; 8]) -> Self {
        let [w0, offset, addr, w3, user_data, r0, r1, r2] = *words;
        Self {
            opcode: w0 as u8,
            flags: SqeFlags::from_bits_retain((w0 >> 8) as u8),
            ioprio: (w0 >> 16) as u16,
            fd: (w0 >> 32) as u32,
            offset,
            addr,
            len: w3 as u32,
            op_flags: (w3 >> 32) as u32,
            user_data,
            reserved: [r0, r1, r2],
        }
    }
}

/// A completion queue entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cqe {
    /// The SQE's `user_data`.
    pub user_data: u64,
    /// The result: zero or more on success, minus the errno on failure.
    pub result: i64,
    /// The flags.
    pub flags: CqeFlags,
}

impl Cqe {
    /// A CQE's size in bytes.
    pub const BYTES: usize = 32;

    /// The completion of `user_data` with `result` ([`Cqe::encode_result`]).
    pub const fn new(user_data: u64, result: i64) -> Self {
        Self {
            user_data,
            result,
            flags: CqeFlags::empty(),
        }
    }

    /// The `result` word for a success with `value`, or a failure with `errno`. Values above
    /// `i64::MAX` cannot be told apart from errors and are clamped to it.
    pub const fn encode_result(result: Result<u64, u32>) -> i64 {
        match result {
            Ok(value) if value > i64::MAX as u64 => i64::MAX,
            Ok(value) => value as i64,
            Err(errno) => -(errno as i64),
        }
    }

    /// The result: `Ok` with the value, or `Err` with the errno.
    pub const fn status(&self) -> Result<u64, u32> {
        if self.result >= 0 {
            Ok(self.result as u64)
        } else {
            Err(self.result.unsigned_abs() as u32)
        }
    }
}

impl Entry for Cqe {
    type Words = [u64; 4];
    const WORDS: usize = 4;

    fn to_words(&self) -> [u64; 4] {
        [
            self.user_data,
            self.result as u64,
            u64::from(self.flags.bits()),
            0,
        ]
    }

    fn from_words(words: &[u64; 4]) -> Self {
        let [user_data, result, flags, _] = *words;
        Self {
            user_data,
            result: result as i64,
            flags: CqeFlags::from_bits_retain(flags as u32),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_sizes_match_their_words() {
        assert_eq!(Sqe::WORDS * 8, Sqe::BYTES);
        assert_eq!(Cqe::WORDS * 8, Cqe::BYTES);
        assert_eq!(<Sqe as Entry>::Words::default().len(), Sqe::WORDS);
        assert_eq!(<Cqe as Entry>::Words::default().len(), Cqe::WORDS);
    }

    #[test]
    fn sqes_round_trip() {
        let mut sqe = Sqe::write(7, 0x1234, 0xdead_b000, 4096, 99).linked();
        sqe.reserved = [1, 2, 3];
        sqe.ioprio = 0xabcd;
        sqe.op_flags = 0x5555_aaaa;
        sqe.flags |= SqeFlags::from_bits_retain(0x80);
        assert_eq!(Sqe::from_words(&sqe.to_words()), sqe);
        assert_eq!(sqe.to_words()[0], 6 | 0x81 << 8 | 0xabcd << 16 | 7 << 32);
    }

    #[test]
    fn cqes_round_trip() {
        for cqe in [
            Cqe::new(1, 0),
            Cqe::new(u64::MAX, -125),
            Cqe::new(5, i64::MAX),
        ] {
            assert_eq!(Cqe::from_words(&cqe.to_words()), cqe);
        }
    }

    #[test]
    fn results_encode_errors_as_negative_errnos() {
        assert_eq!(Cqe::encode_result(Ok(4096)), 4096);
        assert_eq!(Cqe::encode_result(Err(22)), -22);
        assert_eq!(Cqe::encode_result(Ok(u64::MAX)), i64::MAX);
        assert_eq!(Cqe::new(0, -22).status(), Err(22));
        assert_eq!(Cqe::new(0, 8).status(), Ok(8));
    }

    #[test]
    fn ops_are_checked() {
        assert_eq!(Sqe::nop(1).op(), Ok(Op::Nop));
        assert_eq!(
            Sqe::read(3, 0, 0x1000, 16, 1).op(),
            Ok(Op::Read {
                file: FileRef::Fd(3),
                offset: 0,
                buf: Buffer::Addr {
                    addr: 0x1000,
                    len: 16
                }
            })
        );
        assert_eq!(
            Sqe::fsync(4, 1).op(),
            Ok(Op::Fsync {
                file: FileRef::Fd(4)
            })
        );

        let mut sqe = Sqe::nop(1);
        sqe.opcode = 99;
        assert_eq!(sqe.op(), Err(SqeError::Opcode(99)));

        let mut sqe = Sqe::nop(1);
        sqe.flags = SqeFlags::from_bits_retain(0x40);
        assert_eq!(sqe.op(), Err(SqeError::Flags(0x40)));

        for field in 0..4 {
            let mut sqe = Sqe::read(3, 0, 0x1000, 16, 1);
            match field {
                0 => sqe.ioprio = 1,
                1 => sqe.op_flags = 1,
                2 => sqe.reserved[0] = 1,
                _ => sqe.reserved[2] = 1,
            }
            assert_eq!(sqe.op(), Err(SqeError::Reserved));
        }

        // Fields an operation does not use must be zero, so they stay usable later.
        let mut nop = Sqe::nop(1);
        nop.len = 1;
        assert_eq!(nop.op(), Err(SqeError::Reserved));
        let mut fsync = Sqe::fsync(4, 1);
        fsync.offset = 8;
        assert_eq!(fsync.op(), Err(SqeError::Reserved));
    }

    #[test]
    fn metadata_ops_are_checked() {
        assert_eq!(
            Sqe::fsize(3, 1).fixed().op(),
            Ok(Op::Fsize {
                file: FileRef::Fixed(3)
            })
        );
        assert_eq!(
            Sqe::fstat(4, 0x2000, 128, 1).op(),
            Ok(Op::Fstat {
                file: FileRef::Fd(4),
                buf: Buffer::Addr {
                    addr: 0x2000,
                    len: 128
                }
            })
        );
        let mut fsize = Sqe::fsize(3, 1);
        fsize.addr = 0x1000;
        assert_eq!(fsize.op(), Err(SqeError::Reserved));
        let mut fstat = Sqe::fstat(4, 0x2000, 128, 1);
        fstat.offset = 1;
        assert_eq!(fstat.op(), Err(SqeError::Reserved));
        assert!(!Opcode::Fstat.transfers());
        assert!(Sqe::nop(1).fixed().flags.contains(SqeFlags::FIXED_FILE));
        assert_eq!(Sqe::nop(1).run_async().op(), Ok(Op::Nop));
        assert_eq!(Op::Nop.file(), None);
        assert_eq!(
            Sqe::fsize(3, 1).fixed().op().map(|op| op.file()),
            Ok(Some(FileRef::Fixed(3)))
        );
        assert_eq!(
            Sqe::write(4, 0, 0x1000, 8, 1).op().map(|op| op.file()),
            Ok(Some(FileRef::Fd(4)))
        );
    }

    #[test]
    fn fixed_files_and_grant_buffers_decode() {
        let mut sqe = Sqe::read(2, 0, Sqe::grant_addr(5, 0x3000).expect("fits"), 512, 1);
        sqe.flags = SqeFlags::FIXED_FILE | SqeFlags::BUF_GRANT;
        assert_eq!(
            sqe.op(),
            Ok(Op::Read {
                file: FileRef::Fixed(2),
                offset: 0,
                buf: Buffer::Grant {
                    slot: 5,
                    offset: 0x3000,
                    len: 512
                }
            })
        );
        assert_eq!(Sqe::grant_addr(1, 1 << 48), None);
    }

    #[test]
    fn opcodes_match_the_scheme_request_numbers() {
        // The numbers of `redox_syscall::schemev2::Opcode`, so a scheme decodes both alike.
        assert_eq!(Opcode::Read as u8, 5);
        assert_eq!(Opcode::Write as u8, 6);
        assert_eq!(Opcode::Fsize as u8, 7);
        assert_eq!(Opcode::Fstat as u8, 15);
        assert_eq!(Opcode::Fsync as u8, 17);
        for raw in 0..=u8::MAX {
            if let Some(op) = Opcode::from_raw(raw) {
                assert_eq!(op as u8, raw);
            }
        }
    }
}
