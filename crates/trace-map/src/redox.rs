//! The mapping itself, through `redox_syscall`'s `fmap` and `funmap`.

use core::sync::atomic::AtomicU64;

use ferric_trace::abi::CPU_MAP_BYTES;
use ferric_trace::record::WORD_BYTES;
use syscall::{Error, Map, MapFlags};

/// A CPU's ring mapped read-only into this process, until dropped.
#[derive(Debug)]
pub struct Mapping {
    addr: usize,
}

/// Maps `size` bytes of `fd` from offset 0 with `flags`, at an address the kernel picks.
fn map(fd: usize, size: usize, flags: MapFlags) -> Result<usize, Error> {
    let map = Map {
        offset: 0,
        size,
        flags,
        address: 0,
    };
    // SAFETY: a new mapping at an address the kernel picks (no `MAP_FIXED`), so no memory
    // this process uses is replaced; nothing refers to it yet.
    unsafe { syscall::fmap(fd, &map) }
}

/// Unmaps `size` bytes at `addr`, a mapping no reference points into any more.
fn unmap(addr: usize, size: usize) {
    // SAFETY: the callers pass a mapping they made with `map` and whose borrows have ended.
    // A failure leaves the mapping in place, which is only a leak.
    let _ = unsafe { syscall::funmap(addr, size) };
}

impl Mapping {
    /// Maps the ring behind `fd`, an open `cpu/<n>` descriptor of the `kernel.trace` scheme:
    /// [`CPU_MAP_BYTES`] from offset 0, `PROT_READ | MAP_SHARED`.
    ///
    /// # Errors
    ///
    /// The kernel's error: `EOPNOTSUPP` or `ENODEV` from a kernel or file that cannot be
    /// mapped (kernels before T3a), `EBADF` for a bad descriptor.
    pub fn ring(fd: usize) -> Result<Self, Error> {
        let addr = map(
            fd,
            CPU_MAP_BYTES,
            MapFlags::PROT_READ | MapFlags::MAP_SHARED,
        )?;
        Ok(Self { addr })
    }

    /// Maps `size` bytes of `fd` with `flags` and unmaps them at once: for tests that the
    /// kernel refuses a mapping. The memory is never touched, so any flags are safe.
    ///
    /// # Errors
    ///
    /// The kernel's error, which such a test expects.
    pub fn probe(fd: usize, size: usize, flags: MapFlags) -> Result<(), Error> {
        let addr = map(fd, size, flags)?;
        unmap(addr, size);
        Ok(())
    }

    /// The address of the mapping, for a test that the kernel refuses to change it
    /// (`mprotect`).
    pub fn addr(&self) -> usize {
        self.addr
    }

    /// The ring's words, header included, for [`View::new`](ferric_trace::ring::View::new).
    pub fn words(&self) -> &[AtomicU64] {
        // SAFETY: `ring` mapped CPU_MAP_BYTES of page-aligned shared memory readable by this
        // process, which stays mapped until `drop`; `drop` needs `&mut self`, so the mapping
        // outlives this borrow. `AtomicU64` has the size and alignment of a `u64` and every
        // bit pattern is valid, and the kernel, the only writer, writes the words atomically.
        unsafe {
            core::slice::from_raw_parts(self.addr as *const AtomicU64, CPU_MAP_BYTES / WORD_BYTES)
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unmap(self.addr, CPU_MAP_BYTES);
    }
}
