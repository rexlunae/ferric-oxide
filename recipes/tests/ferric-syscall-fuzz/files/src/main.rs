//! System call fuzzer for the Ferric Oxide kernel.
//!
//! The parent process starts child processes, each with its own seed, in a
//! scratch directory under `/tmp`. A child opens a set of file descriptors it
//! owns (pipes, an event queue, anonymous memory, kernel `sys:` and `time:`
//! files, a file and a directory of its own) and then makes thousands of raw
//! system calls with arguments drawn from those descriptors, from its own
//! buffers, and from boundary values: null, unaligned, non-canonical and
//! kernel addresses, zero and huge lengths, and random bits.
//!
//! Children are expected to get errors, and some die from what they did to
//! themselves (unmapping their own code, say). What must not happen is a
//! kernel panic or a hung system: the boot smoke test treats a panic as a
//! failure, and the parent only prints its result line if the system is still
//! working once the children are done.
//!
//! The fuzzer only touches descriptors and memory of the child itself, so it
//! cannot signal other processes, map physical memory, or write outside its
//! scratch directory. The arguments are reproducible from the seed:
//! `ferric-syscall-fuzz --child SEED CALLS` replays one child, and with
//! `FERRIC_FUZZ_VERBOSE=1` it prints each call to stderr before making it.
//!
//! Usage: `ferric-syscall-fuzz [--seed N] [--seconds S] [--calls C] [--procs P]`.
//! Prints `FERRIC_TEST_PASS fuzz ...` with the seed and what the children
//! did, and `FERRIC_TEST_FAIL fuzz ...` if a child could not be started or
//! reaped, or if a child hung and had to be killed.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

/// System call numbers and flags, copied from `redox_syscall` so the fuzzer
/// has no dependencies.
///
/// The copy is deliberate even though the kernel and user space share one ABI
/// crate: the fuzzer has to send the numbers and bits it was built against
/// without reading them back from the ABI it is meant to test. A drifted
/// constant then shows up as a call that no longer reaches the code under test,
/// which the checks below catch, instead of as a call that silently follows the
/// new value. The entries carry the same names as their `redox_syscall`
/// counterparts, so a change there is easy to trace.
mod sys {
    const CLASS_PATH: usize = 0x1000_0000;
    const CLASS_FILE: usize = 0x2000_0000;
    const ARG_SLICE: usize = 0x0100_0000;
    const ARG_MSLICE: usize = 0x0200_0000;
    const ARG_PATH: usize = 0x0300_0000;
    const RET_FILE: usize = 0x0010_0000;

    pub const OPENAT_INTO2: usize = CLASS_PATH | RET_FILE | 986;
    pub const UNLINKAT: usize = CLASS_PATH | 263;
    pub const CLOSE: usize = CLASS_FILE | 6;
    pub const DUP_INTO: usize = CLASS_FILE | RET_FILE | 988;
    pub const DUP2: usize = CLASS_FILE | RET_FILE | 63;
    pub const READ: usize = CLASS_FILE | ARG_MSLICE | 3;
    pub const READ2: usize = CLASS_FILE | ARG_MSLICE | 35;
    pub const WRITE: usize = CLASS_FILE | ARG_SLICE | 4;
    pub const WRITE2: usize = CLASS_FILE | ARG_SLICE | 45;
    pub const LSEEK: usize = CLASS_FILE | 19;
    pub const FCHOWN: usize = CLASS_FILE | 207;
    pub const FCNTL: usize = CLASS_FILE | 55;
    pub const FEVENT: usize = CLASS_FILE | 927;
    pub const CALL: usize = CLASS_FILE | ARG_SLICE | ARG_MSLICE | 0xCA11;
    pub const FMAP: usize = CLASS_FILE | ARG_SLICE | 900;
    pub const FUNMAP: usize = CLASS_FILE | 92;
    pub const MREMAP: usize = 155;
    pub const FLINK: usize = CLASS_FILE | ARG_PATH | 9;
    pub const FPATH: usize = CLASS_FILE | ARG_MSLICE | 928;
    pub const FRENAME: usize = CLASS_FILE | ARG_PATH | 38;
    pub const FSTAT: usize = CLASS_FILE | ARG_MSLICE | 28;
    pub const FSYNC: usize = CLASS_FILE | 118;
    pub const CLOCK_GETTIME: usize = 265;
    pub const FUTEX: usize = 240;
    pub const MPROTECT: usize = 125;
    pub const NANOSLEEP: usize = 162;
    pub const YIELD: usize = 158;

    /// `FUTEX_WAIT`: block while a 32-bit word holds the given value.
    pub const FUTEX_WAIT: usize = 0;
    /// `EINVAL`, returned negated in the result register.
    pub const EINVAL: usize = 22;

    pub const O_RDONLY: usize = 0x0001_0000;
    pub const O_RDWR: usize = 0x0003_0000;
    pub const O_NONBLOCK: usize = 0x0004_0000;
    pub const O_CREAT: usize = 0x0200_0000;
    pub const O_TRUNC: usize = 0x0400_0000;
    pub const O_EXCL: usize = 0x0800_0000;
    pub const O_DIRECTORY: usize = 0x1000_0000;
    pub const F_SETFL: usize = 4;

    pub const PROT_WRITE: usize = 0x0002_0000;
    pub const PROT_READ: usize = 0x0004_0000;
    pub const MAP_PRIVATE: usize = 0x0002;
    pub const MAP_FIXED: usize = 0x0004;

    /// `redox_syscall::UPPER_FDTBL_TAG`: descriptors with this bit live in the
    /// upper table, where relibc keeps its process and thread handles. The
    /// fuzzer never names one, so it cannot act on its own process.
    /// `CallFlags::CONSUME`: the call takes ownership of the descriptors it
    /// is passed, which leave the caller's table.
    pub const CALL_CONSUME: usize = 1 << 8;
    /// `CallFlags::MULTIPLE_FDS2`: the first argument points at a list of
    /// descriptors instead of naming one.
    pub const CALL_MULTIPLE_FDS2: usize = 1 << 19;

    pub const UPPER_FDTBL_TAG: usize = 1 << (usize::BITS - 2);

    /// Makes a raw system call and returns the kernel's result register.
    ///
    /// # Safety
    ///
    /// The kernel may write to, map over or unmap any memory the arguments
    /// name, so the caller must not rely on memory it passed staying intact.
    pub unsafe fn call(n: usize, args: [usize; 6]) -> usize {
        let mut ret = n;
        // SAFETY: the Redox x86_64 system call ABI; the kernel clobbers rcx,
        // r11 and the SSE argument registers, which are declared here.
        unsafe {
            core::arch::asm!(
                "syscall",
                inout("rax") ret,
                in("rdi") args[0],
                in("rsi") args[1],
                in("rdx") args[2],
                in("r10") args[3],
                in("r8") args[4],
                inout("r9") args[5] => _,
                out("rcx") _,
                out("r11") _,
                out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
                out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
                out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
                out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
                options(nostack),
            );
        }
        ret
    }

    /// Whether a raw result is an error (`-4095..=-1`).
    pub fn is_err(ret: usize) -> bool {
        ret > usize::MAX - 4095
    }
}

const PAGE_SIZE: usize = 4096;
/// Scratch buffer that pointer arguments usually point into.
const BUF_LEN: usize = 64 * 1024;
/// Anonymous mapping that the memory-management calls usually target.
const VICTIM_LEN: usize = 256 * 1024;
/// Largest mapping a fuzzed `fmap` or `mremap` may ask for, so the fuzzer
/// tests the kernel's argument handling rather than exhausting memory.
const MAX_MAP: usize = 1 << 20;
/// Successful `fmap` calls a child may make before it stops making more.
const MAX_MAPS: usize = 64;
/// Descriptors a child keeps track of; new ones beyond this are not used.
const MAX_FDS: usize = 48;
/// Time a child may run before the parent kills it.
const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
/// Exit code of a child whose fixed-bug check at start-up failed, so the
/// parent can fail the run even though children's stderr is discarded.
const REGRESSED_EXIT: i32 = 3;

/// xorshift64*: small, fast and good enough to pick arguments.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Never zero, which would make xorshift stick.
        Self((seed ^ 0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// True `percent`% of the time.
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// Addresses no valid buffer lives at: null, unaligned, the edges of the
/// user half, non-canonical and kernel addresses.
const BAD_ADDRS: [usize; 11] = [
    0,
    1,
    0xfff,
    0x1000,
    0x7fff_ffff_f000,
    0x7fff_ffff_ffff,
    0x8000_0000_0000,
    0xffff_8000_0000_0000,
    0xffff_ffff_8000_0000,
    usize::MAX - 0xfff,
    usize::MAX,
];

/// Lengths around page sizes, sign bits and overflow.
const EDGE_LENS: [usize; 12] = [
    0,
    1,
    7,
    8,
    4095,
    4096,
    4097,
    0x10000,
    0x7fff_ffff,
    isize::MAX as usize,
    isize::MAX as usize + 1,
    usize::MAX,
];

/// Integers worth trying for flags, offsets and ids.
const EDGE_INTS: [usize; 14] = [
    0,
    1,
    2,
    3,
    4,
    0xff,
    0xffff,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_ffff,
    1 << 32,
    isize::MAX as usize,
    isize::MIN as usize,
    usize::MAX,
];

/// Paths that may be opened. Absolute ones are kernel or harmless schemes;
/// relative ones resolve inside the child's scratch directory.
const OPEN_PATHS: [&[u8]; 17] = [
    b"a",
    b"b",
    b"a/b",
    b"",
    b".",
    b"..",
    b"/scheme/null",
    b"/scheme/zero",
    b"/scheme/rand",
    b"/scheme/event",
    b"/scheme/pipe",
    b"/scheme/memory",
    b"/scheme/memory/zeroed",
    b"/scheme/sys/uname",
    b"/scheme/sys/cpu",
    b"/scheme/sys/context",
    b"/scheme/time/1",
];

/// Paths that may be unlinked, renamed onto or linked: only names inside the
/// scratch directory, so nothing the rest of the system uses can go away.
const LOCAL_PATHS: [&[u8]; 5] = [b"a", b"b", b"a/b", b"c", b"d"];

/// What a child works with.
struct Fuzzer {
    rng: Rng,
    fds: Vec<usize>,
    buf: Vec<u8>,
    /// Path bytes, kept apart from `buf` so path arguments stay intact.
    paths: Vec<u8>,
    victim: usize,
    maps: usize,
    verbose: bool,
}

impl Fuzzer {
    fn fd(&mut self) -> usize {
        if !self.fds.is_empty() && self.rng.chance(85) {
            self.rng.pick(&self.fds)
        } else {
            // Not open, or not a valid index at all. `!0` makes fmap anonymous.
            self.rng
                .pick(&[usize::MAX, 9999, 0x7fff_ffff, 1 << 40, usize::MAX - 1])
        }
    }

    /// A descriptor number to install into: free or ours, never upper-table.
    fn target_fd(&mut self) -> usize {
        match self.rng.below(4) {
            0 if !self.fds.is_empty() => self.rng.pick(&self.fds),
            1 => self.rng.pick(&[4096, 65535, 1 << 20, 1 << 40]),
            _ => 64 + self.rng.below(64),
        }
    }

    fn len(&mut self) -> usize {
        match self.rng.below(4) {
            0 => self.rng.pick(&EDGE_LENS),
            _ => self.rng.below(512),
        }
    }

    fn int(&mut self) -> usize {
        match self.rng.below(4) {
            0 => self.rng.pick(&EDGE_INTS),
            1 => self.rng.next() as usize,
            _ => self.rng.below(64),
        }
    }

    /// Fills part of `buf` with random bytes and returns a pointer and length
    /// for it, or a bad pointer or length some of the time.
    fn slice(&mut self) -> (usize, usize) {
        let base = self.buf.as_mut_ptr() as usize;
        match self.rng.below(10) {
            0 => (self.rng.pick(&BAD_ADDRS), self.len()),
            // Starts inside the buffer, ends past it.
            1 => (base + BUF_LEN - 1 - self.rng.below(64), self.len()),
            // Our own code: readable, not writable.
            2 => (
                child_main as fn(u64, usize) -> ExitCode as usize,
                self.rng.below(4096),
            ),
            _ => {
                let len = self.rng.below(4097);
                let off = self.rng.below(BUF_LEN - len);
                for b in &mut self.buf[off..off + len.min(64)] {
                    *b = self.rng.next() as u8;
                }
                (base + off, len)
            }
        }
    }

    fn fill(&mut self) {
        for chunk in self.buf.chunks_mut(8) {
            let v = self.rng.next().to_ne_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    /// A path from `choices`, random bytes, or a bad pointer.
    fn path(&mut self, choices: &[&[u8]]) -> (usize, usize) {
        let base = self.paths.as_mut_ptr() as usize;
        let bytes: Vec<u8> = match self.rng.below(12) {
            0 => return (self.rng.pick(&BAD_ADDRS), self.len()),
            1 => (0..self.rng.below(16))
                .map(|_| self.rng.next() as u8)
                .collect(),
            2 => vec![b'a'; 4096],
            3 => b"a\0b".to_vec(),
            _ => self.rng.pick(choices).to_vec(),
        };
        self.paths[..bytes.len()].copy_from_slice(&bytes);
        let len = if self.rng.chance(10) {
            self.len().min(self.paths.len())
        } else {
            bytes.len()
        };
        (base, len)
    }

    /// An address for the memory-management calls, usually in the victim
    /// mapping.
    fn addr(&mut self) -> usize {
        match self.rng.below(5) {
            0 => self.rng.pick(&BAD_ADDRS),
            1 => self.victim + self.rng.below(VICTIM_LEN),
            _ => self.victim + self.rng.below(VICTIM_LEN / PAGE_SIZE) * PAGE_SIZE,
        }
    }

    /// `RwFlags` for `read2`/`write2`: `usize::MAX` keeps the descriptor's
    /// own flags. NONBLOCK (1) is always set otherwise, for the same reason
    /// descriptors are opened non-blocking.
    fn rw_flags(&mut self) -> usize {
        self.rng.pick(&[usize::MAX, 1, 3, 0xffff_ffff, 1 << 32 | 1])
    }

    fn map_len(&mut self) -> usize {
        match self.rng.below(4) {
            0 => self
                .rng
                .pick(&EDGE_LENS)
                .min(MAX_MAP)
                .max(self.rng.below(2)),
            _ => (1 + self.rng.below(8)) * PAGE_SIZE,
        }
    }

    /// Writes a `Map` (offset, size, flags, address) into `buf` and returns
    /// the pointer and length to pass to `fmap`.
    fn map_struct(&mut self) -> (usize, usize) {
        let flags = match self.rng.below(3) {
            0 => self.int(),
            _ => sys::PROT_READ | sys::PROT_WRITE | sys::MAP_PRIVATE | (self.rng.below(32)),
        };
        let addr = if self.rng.chance(50) { 0 } else { self.addr() };
        let fields = [self.int() & 0xffff_f000, self.map_len(), flags, addr];
        for (i, v) in fields.iter().enumerate() {
            self.buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
        }
        let len = if self.rng.chance(90) { 32 } else { self.len() };
        (self.buf.as_ptr() as usize, len)
    }

    /// A `TimeSpec` (`i64` seconds, `i32` nanoseconds) that is under a
    /// millisecond or invalid. Valid long times are left out: the kernel
    /// rightly sleeps for them, which would only stall the child.
    fn timespec(&mut self) -> usize {
        let (sec, nsec): (i64, i32) = match self.rng.below(4) {
            0 => (
                self.rng.pick(&[0, -1, i64::MIN]),
                self.rng.pick(&[-1, 1_000_000_000, i32::MAX, i32::MIN]),
            ),
            _ => (0, self.rng.below(1_000_000) as i32),
        };
        self.buf[..8].copy_from_slice(&sec.to_ne_bytes());
        self.buf[8..12].copy_from_slice(&nsec.to_ne_bytes());
        self.buf.as_ptr() as usize
    }

    /// Picks one call and its arguments.
    fn next_call(&mut self) -> (usize, [usize; 6]) {
        let fd = self.fd();
        match self.rng.below(27) {
            0 => {
                let (p, l) = self.slice();
                (sys::READ, [fd, p, l, 0, 0, 0])
            }
            1 => {
                let (p, l) = self.slice();
                let off = self.int();
                let flags = self.rw_flags();
                (sys::READ2, [fd, p, l, off, flags, 0])
            }
            2 => {
                let (p, l) = self.slice();
                (sys::WRITE, [fd, p, l, 0, 0, 0])
            }
            3 => {
                let (p, l) = self.slice();
                let off = self.rng.below(1 << 20);
                let flags = self.rw_flags();
                (sys::WRITE2, [fd, p, l, off, flags, 0])
            }
            4 => {
                let (p, l) = self.path(&OPEN_PATHS);
                let flags = self.rng.pick(&[
                    sys::O_RDONLY,
                    sys::O_RDWR,
                    sys::O_RDWR | sys::O_CREAT,
                    sys::O_RDWR | sys::O_CREAT | sys::O_EXCL,
                    sys::O_RDWR | sys::O_TRUNC,
                    sys::O_RDONLY | sys::O_DIRECTORY,
                ]) | if self.rng.chance(20) { self.int() } else { 0 };
                // Always non-blocking, so reads of empty pipes return at once.
                let fcntl = sys::O_NONBLOCK;
                let target = self.target_fd();
                (
                    sys::OPENAT_INTO2,
                    [fd, p, (l & 0xffff) | fcntl, flags, target, 0],
                )
            }
            5 => {
                let (p, l) = self.path(&LOCAL_PATHS);
                let flags = self.rng.pick(&[0, 0x200, usize::MAX]);
                (sys::UNLINKAT, [fd, p, l, flags, 0, 0])
            }
            6 => (sys::CLOSE, [fd, 0, 0, 0, 0, 0]),
            7 => {
                let (p, l) = self.path(&[b"", b"", b"read", b"write", b"x"]);
                let target = self.target_fd();
                (sys::DUP_INTO, [fd, p, l, target, 0, 0])
            }
            8 => {
                let target = self.target_fd();
                let (p, l) = self.path(&[b"", b"", b"read", b"write"]);
                (sys::DUP2, [fd, target, p, l, 0, 0])
            }
            9 => {
                let off = self
                    .rng
                    .pick(&[0, 1, 4096, 1 << 20, usize::MAX, isize::MIN as usize]);
                let whence = self.rng.pick(&[0, 1, 2, 3, usize::MAX]);
                (sys::LSEEK, [fd, off, whence, 0, 0, 0])
            }
            10 => (sys::FCHOWN, [fd, self.int(), self.int(), 0, 0, 0]),
            11 => {
                let cmd = self.rng.below(8);
                // Keep descriptors non-blocking whatever else F_SETFL sets.
                let arg = if cmd == sys::F_SETFL {
                    self.int() | sys::O_NONBLOCK
                } else {
                    self.int()
                };
                (sys::FCNTL, [fd, cmd, arg, 0, 0, 0])
            }
            12 => (sys::FEVENT, [fd, self.int(), 0, 0, 0, 0]),
            13 | 14 => self.call_args(fd),
            15 if self.maps < MAX_MAPS => {
                let (p, l) = self.map_struct();
                (sys::FMAP, [fd, p, l, 0, 0, 0])
            }
            15 | 16 => (sys::FUNMAP, [self.addr(), self.map_len(), 0, 0, 0, 0]),
            17 => {
                let flags = self
                    .rng
                    .pick(&[0, 1, 2, 3, 4, 0xc, sys::PROT_READ, usize::MAX]);
                (
                    sys::MREMAP,
                    [
                        self.addr(),
                        self.map_len(),
                        self.addr(),
                        self.map_len(),
                        flags,
                        0,
                    ],
                )
            }
            18 => {
                let (p, l) = self.path(&LOCAL_PATHS);
                (sys::FLINK, [fd, p, l, 0, 0, 0])
            }
            19 => {
                let (p, l) = self.slice();
                (sys::FPATH, [fd, p, l, 0, 0, 0])
            }
            20 => {
                let (p, l) = self.path(&LOCAL_PATHS);
                (sys::FRENAME, [fd, p, l, 0, 0, 0])
            }
            21 => {
                let (p, l) = self.slice();
                (sys::FSTAT, [fd, p, l, 0, 0, 0])
            }
            22 => (sys::FSYNC, [fd, 0, 0, 0, 0, 0]),
            23 => {
                let (p, _) = self.slice();
                (sys::CLOCK_GETTIME, [self.int(), p, 0, 0, 0, 0])
            }
            24 => {
                let addr = if self.rng.chance(70) {
                    self.buf.as_ptr() as usize + self.rng.below(BUF_LEN / 8) * 8
                } else {
                    self.addr()
                };
                // A wait rarely matches the random word, and has a short timeout.
                let op = self.rng.below(4);
                let ts = self.timespec();
                (sys::FUTEX, [addr, op, self.int(), ts, 0, 0])
            }
            25 => {
                let any = self.int();
                let prot =
                    self.rng
                        .pick(&[0, sys::PROT_READ, sys::PROT_READ | sys::PROT_WRITE, any]);
                (sys::MPROTECT, [self.addr(), self.map_len(), prot, 0, 0, 0])
            }
            26 => {
                let req = self.timespec();
                let rem = if self.rng.chance(50) {
                    0
                } else {
                    self.slice().0
                };
                (sys::NANOSLEEP, [req, rem, 0, 0, 0, 0])
            }
            _ => (sys::YIELD, [0; 6]),
        }
    }

    /// Arguments for `SYS_CALL`: our descriptors, a payload in `buf`, call
    /// flags with the payload length in the low byte, and metadata words.
    fn call_args(&mut self, fd: usize) -> (usize, [usize; 6]) {
        let (payload, payload_len) = self.slice();
        let mut flags = self
            .rng
            .pick(&[1 << 9, 1 << 10, 3 << 9, 1 << 16, 1 << 11 | 1 << 8])
            | if self.rng.chance(10) {
                self.int() & !0xff
            } else {
                0
            };
        let mut fd_arg = fd;
        if self.rng.chance(20) {
            // A list of our own descriptors, passed by pointer.
            let n = 1 + self.rng.below(4);
            let list: Vec<usize> = (0..n).map(|_| self.fd()).collect();
            // Eight-byte aligned, as the kernel rounds the pointer down.
            let at = BUF_LEN - 64 - (self.buf.as_ptr() as usize + BUF_LEN - 64) % 8;
            for (i, v) in list.iter().enumerate() {
                self.buf[at + i * 8..at + i * 8 + 8].copy_from_slice(&v.to_ne_bytes());
            }
            fd_arg = self.buf.as_ptr() as usize + at + n % 8;
            flags |= sys::CALL_MULTIPLE_FDS2;
        }
        let meta_words = self.rng.below(4);
        flags = (flags & !(0xff | (1 << 17))) | meta_words;
        let meta = self.buf.as_ptr() as usize + 128;
        (sys::CALL, [fd_arg, payload, payload_len, flags, meta, 0])
    }

    /// Opens the descriptors the child fuzzes with. Each is optional: one
    /// that fails to open is left out.
    fn open_fds(&mut self) {
        let opens: [(&str, usize); 8] = [
            ("/scheme/null", sys::O_RDWR),
            ("/scheme/zero", sys::O_RDONLY),
            ("/scheme/event", sys::O_RDWR),
            ("/scheme/memory/zeroed", sys::O_RDWR),
            ("/scheme/sys/uname", sys::O_RDONLY),
            ("/scheme/time/1", sys::O_RDONLY),
            ("file", sys::O_RDWR | sys::O_CREAT),
            (".", sys::O_RDONLY | sys::O_DIRECTORY),
        ];
        for (path, flags) in opens {
            let mut opts = OpenOptions::new();
            opts.read(true);
            if flags & sys::O_RDWR == sys::O_RDWR {
                opts.write(true);
            }
            opts.create(flags & sys::O_CREAT != 0)
                .custom_flags((sys::O_NONBLOCK | (flags & sys::O_DIRECTORY)) as i32);
            if let Ok(f) = opts.open(path) {
                self.fds.push(f.into_raw_fd() as usize);
            }
        }
        if let Ok((r, w)) = std::io::pipe() {
            for fd in [r.into_raw_fd() as usize, w.into_raw_fd() as usize] {
                // SAFETY: fcntl on a descriptor we own.
                unsafe { sys::call(sys::FCNTL, [fd, sys::F_SETFL, sys::O_NONBLOCK, 0, 0, 0]) };
                self.fds.push(fd);
            }
        }
        // Keep the console out of reach: descriptors 0-2 are /dev/null here.
        self.fds.retain(|&fd| fd > 2);
    }

    /// Maps the anonymous region the memory-management calls target.
    fn map_victim(&mut self) {
        let map = [
            0,
            VICTIM_LEN,
            sys::PROT_READ | sys::PROT_WRITE | sys::MAP_PRIVATE,
            0,
        ];
        // SAFETY: fmap of anonymous memory at an address the kernel picks.
        let ret = unsafe {
            sys::call(
                sys::FMAP,
                [
                    usize::MAX,
                    map.as_ptr() as usize,
                    size_of_val(&map),
                    0,
                    0,
                    0,
                ],
            )
        };
        if !sys::is_err(ret) {
            self.victim = ret;
        }
    }

    fn run(&mut self, calls: usize) {
        self.open_fds();
        self.map_victim();
        self.fill();
        self.regressions();
        for i in 0..calls {
            let (n, args) = self.next_call();
            if self.verbose {
                eprintln!("{i} {n:#x} {args:x?}");
            }
            // SAFETY: the arguments name only this process's descriptors and
            // memory; the kernel may break this process, which is allowed.
            let ret = unsafe { sys::call(n, args) };
            if n == sys::CALL {
                self.forget_consumed(&args);
            }
            if sys::is_err(ret) {
                continue;
            }
            match n {
                sys::FMAP => self.maps += 1,
                sys::OPENAT_INTO2 | sys::DUP_INTO | sys::DUP2
                    if self.fds.len() < MAX_FDS
                        && ret & sys::UPPER_FDTBL_TAG == 0
                        && ret > 2
                        && !self.fds.contains(&ret) =>
                {
                    self.fds.push(ret)
                }
                sys::CLOSE => self.fds.retain(|&fd| fd != args[0]),
                _ => {}
            }
        }
    }

    /// Calls that crashed the kernel before, made once at the start so a
    /// regression shows up on every run rather than only for the seed that
    /// first found it.
    fn regressions(&mut self) {
        // mremap onto the top of the kernel half panicked the kernel with
        // "Grant dropped while still mapped" (fixed by validating the range).
        let args = [
            self.victim,
            0x4000,
            0xFFFF_FFFF_FFFF_F000,
            0x10_0000,
            0x3,
            0,
        ];
        if self.verbose {
            eprintln!("regression {:#x} {args:x?}", sys::MREMAP);
        }
        // SAFETY: the kernel must refuse this; if it does not, only this
        // process's mapping is affected.
        let ret = unsafe { sys::call(sys::MREMAP, args) };
        if !sys::is_err(ret) {
            eprintln!("fuzz: mremap into the kernel half returned {ret:#x}");
            std::process::exit(REGRESSED_EXIT);
        }

        // fmap with MAP_FIXED at 0xffff_ffff_ffff_f000 and a length above one
        // page corrupted the free-space accounting of the address space, and a
        // later mapping then landed on memory a grant already owned, which
        // panicked the kernel with an overlap assertion (fixed by validating
        // the span). Anonymous memory is asked for with no descriptor at all,
        // so this half of the check cannot be skipped, and a file on the root
        // file system is asked for as well because that path reaches its
        // scheme's mapping request before the address space is touched.
        let kernel_half = [
            0xFFFF_FFFF_FFFF_F000,
            0x10_0000,
            sys::PROT_READ | sys::PROT_WRITE | sys::MAP_PRIVATE | sys::MAP_FIXED,
            0,
        ];
        if self.verbose {
            eprintln!("regression {:#x} {kernel_half:x?}", sys::FMAP);
        }
        // SAFETY: `!0` selects anonymous memory, and the mapping asks for an
        // address the kernel must refuse.
        let ret = unsafe {
            sys::call(
                sys::FMAP,
                [
                    usize::MAX,
                    kernel_half.as_ptr() as usize,
                    size_of_val(&kernel_half),
                    0,
                    0,
                    0,
                ],
            )
        };
        if !sys::is_err(ret) {
            eprintln!("fuzz: anonymous fmap MAP_FIXED in the kernel half returned {ret:#x}");
            std::process::exit(REGRESSED_EXIT);
        }

        // The same rule for a descriptor: a fixed span whose end leaves user
        // space has to be refused before the scheme is asked to map anything.
        // These addresses are below the kernel half, so even a kernel that
        // accepted one could not hand out pages there.
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open("fmap-fixed")
        {
            Ok(file) => file.into_raw_fd() as usize,
            Err(e) => {
                // The parent gives every child a writable scratch directory, so
                // a file that cannot be created there would silently skip the
                // check.
                eprintln!("fuzz: no scratch file for the fmap regression: {e}");
                std::process::exit(REGRESSED_EXIT);
            }
        };
        for (address, len) in [
            (0x7FFF_FFFF_F000usize, 0x10_0000usize),
            (0x8000_0000_0000, 0x1000),
        ] {
            let map = [
                address,
                len,
                sys::PROT_READ | sys::PROT_WRITE | sys::MAP_PRIVATE | sys::MAP_FIXED,
                0,
            ];
            if self.verbose {
                eprintln!("regression {:#x} fd {file} {map:x?}", sys::FMAP);
            }
            // SAFETY: the descriptor is this process's and the mapping asks for
            // a span the kernel must refuse.
            let ret = unsafe {
                sys::call(
                    sys::FMAP,
                    [file, map.as_ptr() as usize, size_of_val(&map), 0, 0, 0],
                )
            };
            if !sys::is_err(ret) {
                eprintln!("fuzz: fmap MAP_FIXED past user space returned {ret:#x}");
                std::process::exit(REGRESSED_EXIT);
            }
        }
        // SAFETY: closing a descriptor this process opened and owns.
        unsafe { sys::call(sys::CLOSE, [file, 0, 0, 0, 0, 0]) };

        // funmap with a length that has no aligned value rounded into a
        // different range; it now fails with EINVAL.
        let args = [self.victim, usize::MAX, 0, 0, 0, 0];
        if self.verbose {
            eprintln!("regression {:#x} {args:x?}", sys::FUNMAP);
        }
        // SAFETY: the range names this process's own mapping and a length the
        // kernel must refuse.
        let ret = unsafe { sys::call(sys::FUNMAP, args) };
        if ret != sys::EINVAL.wrapping_neg() {
            eprintln!("fuzz: funmap with a huge length returned {ret:#x}, not EINVAL");
            std::process::exit(REGRESSED_EXIT);
        }

        // FUTEX_WAIT on a matching word with a negative timeout slept forever,
        // because the deadline wrapped to a huge value (fixed by rejecting it
        // with EINVAL). If it comes back, this child hangs and the run fails.
        self.buf[..8].copy_from_slice(&(-1i64).to_ne_bytes());
        self.buf[8..16].fill(0);
        self.buf[64..68].fill(0);
        let base = self.buf.as_ptr() as usize;
        let args = [base + 64, sys::FUTEX_WAIT, 0, base, 0, 0];
        if self.verbose {
            eprintln!("regression {:#x} {args:x?}", sys::FUTEX);
        }
        // SAFETY: both pointers name this process's buffer, which the kernel
        // only reads.
        let ret = unsafe { sys::call(sys::FUTEX, args) };
        if ret != sys::EINVAL.wrapping_neg() {
            eprintln!("fuzz: negative futex timeout returned {ret:#x}, not EINVAL");
            std::process::exit(REGRESSED_EXIT);
        }

        // A timer write with a negative time registered a deadline that never
        // arrives; it now fails with EINVAL. Clock 4 is CLOCK_MONOTONIC.
        let mut time = [0u8; 16];
        time[..8].copy_from_slice(&(-1i64).to_ne_bytes());
        let result = OpenOptions::new()
            .write(true)
            .open("/scheme/time/4/timer")
            .and_then(|mut timer| timer.write(&time));
        if result.as_ref().err().and_then(std::io::Error::raw_os_error) != Some(sys::EINVAL as i32)
        {
            eprintln!("fuzz: negative timer write returned {result:?}, not EINVAL");
            std::process::exit(REGRESSED_EXIT);
        }
    }

    /// Drops the descriptors a `SYS_CALL` with `CONSUME` was given from the
    /// rotation, whether or not the call succeeded, so later calls do not
    /// keep failing with `EBADF` on descriptors that are gone.
    fn forget_consumed(&mut self, args: &[usize; 6]) {
        if args[3] & sys::CALL_CONSUME == 0 {
            return;
        }
        let gone: Vec<usize> = if args[3] & sys::CALL_MULTIPLE_FDS2 == 0 {
            vec![args[0]]
        } else {
            // `call_args` puts the list in `buf` and the count in the low bits
            // of the pointer; random flag bits can set the flag without a list.
            let base = self.buf.as_ptr() as usize;
            let at = (args[0] & !7).wrapping_sub(base);
            let n = args[0] & 7;
            (0..n)
                .filter_map(|i| self.buf.get(at + i * 8..at + i * 8 + 8))
                .map(|b| usize::from_ne_bytes(b.try_into().expect("8 bytes")))
                .collect()
        };
        self.fds.retain(|fd| !gone.contains(fd));
    }
}

fn child_main(seed: u64, calls: usize) -> ExitCode {
    let mut child = Fuzzer {
        rng: Rng::new(seed),
        fds: Vec::new(),
        buf: vec![0; BUF_LEN],
        paths: vec![0; 8192],
        victim: 0x7000_0000,
        maps: 0,
        verbose: std::env::var_os("FERRIC_FUZZ_VERBOSE").is_some(),
    };
    child.run(calls);
    ExitCode::SUCCESS
}

/// How the children ended.
#[derive(Default)]
struct Tally {
    children: usize,
    exited: usize,
    signaled: usize,
    regressed: usize,
    timed_out: usize,
    /// Seeds of the children that timed out, replayed with tracing at the end.
    hung: Vec<u64>,
}

/// A child that has not been reaped yet.
struct Running {
    child: Child,
    seed: u64,
    started: Instant,
    /// The child's scratch directory, removed once it is reaped.
    dir: String,
}

/// Options for the parent.
struct Options {
    seed: u64,
    seconds: u64,
    calls: usize,
    procs: usize,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        seed: 1,
        seconds: 20,
        calls: 5000,
        procs: 4,
    };
    let mut it = args.iter().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let parse_err = |_| format!("{flag}: bad number {value:?}");
        match flag.as_str() {
            "--seed" if value == "random" => {
                opts.seed = random_seed();
            }
            "--seed" => opts.seed = value.parse().map_err(parse_err)?,
            "--seconds" => opts.seconds = value.parse().map_err(parse_err)?,
            "--calls" => opts.calls = value.parse().map_err(parse_err)?,
            "--procs" => opts.procs = value.parse().map_err(parse_err)?,
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    opts.procs = opts.procs.max(1);
    Ok(opts)
}

fn random_seed() -> u64 {
    let mut bytes = [0; 8];
    if let Ok(mut f) = File::open("/scheme/rand") {
        let _ = std::io::Read::read_exact(&mut f, &mut bytes);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    u64::from_ne_bytes(bytes) ^ nanos
}

fn spawn(exe: &str, dir: &str, seed: u64, calls: usize) -> std::io::Result<Child> {
    Command::new(exe)
        .args(["--child", &seed.to_string(), &calls.to_string()])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// How many trailing trace lines a replay prints.
const REPLAY_LINES: usize = 12;

/// How many hung seeds are replayed, so many hangs cannot run past the boot
/// timeout before the fuzzer reports.
const MAX_REPLAYS: usize = 3;

/// Runs a hung child's seed again with tracing on and prints the last calls
/// it made, the final one being the call it was stuck in. The trace goes
/// through a pipe the parent drains, so writing it cannot block on a file
/// system the child may have wedged. Replays are close but not exact
/// (descriptor numbers and addresses can differ), so a replay that finishes
/// is reported as such.
fn replay(exe: &str, dir: &str, seed: u64, calls: usize) {
    let child_dir = format!("{dir}/replay-{seed}");
    let spawned = std::fs::create_dir_all(&child_dir).and_then(|()| {
        Command::new(exe)
            .args(["--child", &seed.to_string(), &calls.to_string()])
            .env("FERRIC_FUZZ_VERBOSE", "1")
            .current_dir(&child_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    });
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            println!("fuzz: replay of seed {seed} failed to start: {e}");
            return;
        }
    };
    let reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut tail = VecDeque::with_capacity(REPLAY_LINES);
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tail.len() == REPLAY_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            tail
        })
    });
    let started = Instant::now();
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if started.elapsed() > CHILD_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    // The pipe reaches end of file once the child is gone.
    let tail = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    let outcome = if finished { "finished" } else { "hung again" };
    println!("fuzz: replay of seed {seed} {outcome}; last calls:");
    for line in tail {
        println!("fuzz:   {line}");
    }
    let _ = std::fs::remove_dir_all(&child_dir);
}

/// Reaps finished children and kills ones past the timeout.
///
/// # Errors
///
/// Fails if waiting for a child fails, after which the parent cannot trust
/// its own state.
fn reap(running: &mut Vec<Running>, tally: &mut Tally) -> Result<(), String> {
    let mut i = 0;
    while i < running.len() {
        let r = &mut running[i];
        let status = r
            .child
            .try_wait()
            .map_err(|e| format!("wait failed: {e}"))?;
        let status = match status {
            Some(status) => status,
            None if r.started.elapsed() > CHILD_TIMEOUT => {
                println!("fuzz: child seed {} timed out, killing it", r.seed);
                let _ = r.child.kill();
                tally.timed_out += 1;
                tally.hung.push(r.seed);
                r.child.wait().map_err(|e| format!("wait failed: {e}"))?
            }
            None => {
                i += 1;
                continue;
            }
        };
        if status.signal().is_some() {
            tally.signaled += 1;
        } else if status.code() == Some(REGRESSED_EXIT) {
            tally.regressed += 1;
        } else {
            tally.exited += 1;
        }
        let done = running.swap_remove(i);
        // Children can leave files behind; a long nightly run starts tens of
        // thousands, which would fill the image if the directories stayed.
        let _ = std::fs::remove_dir_all(&done.dir);
    }
    Ok(())
}

/// Runs children until the deadline and returns how they ended. The scratch
/// directory is removed on every path out, including errors.
fn parent_main(opts: &Options) -> Result<Tally, String> {
    let dir = format!("/tmp/ferric-fuzz-{}", opts.seed);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let mut running = Vec::new();
    let result = run_children(opts, &dir, &mut running);
    for r in &mut running {
        let _ = r.child.kill();
        let _ = r.child.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// The parent's loop: keeps `opts.procs` children running until the deadline,
/// then waits for the last ones.
fn run_children(opts: &Options, dir: &str, running: &mut Vec<Running>) -> Result<Tally, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {e}"))?
        .to_string_lossy()
        .into_owned();
    let deadline = Instant::now() + Duration::from_secs(opts.seconds);
    let mut tally = Tally::default();
    let mut next = 0_u64;
    loop {
        reap(running, &mut tally)?;
        let more = Instant::now() < deadline || tally.children == 0;
        if !more && running.is_empty() {
            break;
        }
        if more && running.len() < opts.procs {
            let seed = opts.seed.wrapping_mul(1_000_003).wrapping_add(next);
            next += 1;
            let child_dir = format!("{dir}/{next}");
            std::fs::create_dir_all(&child_dir).map_err(|e| format!("mkdir {child_dir}: {e}"))?;
            let child =
                spawn(&exe, &child_dir, seed, opts.calls).map_err(|e| format!("spawn: {e}"))?;
            tally.children += 1;
            running.push(Running {
                child,
                seed,
                started: Instant::now(),
                dir: child_dir,
            });
            continue;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    for &seed in tally.hung.iter().take(MAX_REPLAYS) {
        replay(&exe, dir, seed, opts.calls);
    }
    Ok(tally)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "--child" {
        return match (args[2].parse(), args[3].parse()) {
            (Ok(seed), Ok(calls)) => child_main(seed, calls),
            _ => ExitCode::from(2),
        };
    }
    let opts = match parse_options(&args) {
        Ok(opts) => opts,
        Err(e) => {
            println!("FERRIC_TEST_FAIL fuzz {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "fuzz: seed {} for {}s, {} calls per child, {} at a time",
        opts.seed, opts.seconds, opts.calls, opts.procs
    );
    match parent_main(&opts) {
        Ok(t) if t.timed_out > 0 => {
            // A child stuck in the kernel past the timeout is a hang, which
            // the fuzzer exists to find; its seed replays it.
            println!(
                "FERRIC_TEST_FAIL fuzz seed={} {} of {} children hung (seeds printed above)",
                opts.seed, t.timed_out, t.children
            );
            ExitCode::FAILURE
        }
        Ok(t) if t.regressed > 0 => {
            // Every child runs the same checks, so one seed reproduces it with
            // FERRIC_FUZZ_VERBOSE=1 to see which check failed.
            println!(
                "FERRIC_TEST_FAIL fuzz seed={} {} of {} children failed a regression check",
                opts.seed, t.regressed, t.children
            );
            ExitCode::FAILURE
        }
        Ok(t) => {
            println!(
                "FERRIC_TEST_PASS fuzz seed={} children={} exited={} signaled={} timed_out={}",
                opts.seed, t.children, t.exited, t.signaled, t.timed_out
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("FERRIC_TEST_FAIL fuzz seed={} {e}", opts.seed);
            ExitCode::FAILURE
        }
    }
}
