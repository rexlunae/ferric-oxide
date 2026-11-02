//! Owned loader mappings and operation-specific I/O failures.
//!
//! A file guard is acquired immediately after open, so every later error closes
//! the descriptor. Successful mappings retain it until after they are unmapped.
//!
//! Ferric Oxide: only the start of a file that the caller needs is mapped, which it
//! decides from the file's first [`PROBE_LEN`] bytes. A scheme fills a file mapping when it
//! is made (RedoxFS reads the whole range, and keeps the buffer after the last unmap), so
//! mapping the debug info at the end of a debug build cost its whole size in memory.
use super::load_error::{ObjectIoError, ObjectIoOperation};
use crate::{
    c_str::CStr,
    error::Errno,
    fs::File,
    header::{
        errno::{EINTR, EOVERFLOW},
        sys_mman,
    },
    out::Out,
    platform::{
        Pal, Sys,
        types::{c_int, c_void, off_t},
    },
};
use core::ptr;

/// How many bytes from the start of a file [`MmapFile::open`] reads to decide how much of
/// it to map: enough for the ELF header and the program headers of an ordinary object.
pub(super) const PROBE_LEN: usize = 4096;

pub(super) struct MmapFile {
    _file: File,
    ptr: *mut c_void,
    size: usize,
}

impl MmapFile {
    /// Opens `path` and maps its first `needed(probe, size)` bytes read-only, where `probe`
    /// is the start of the file (its first [`PROBE_LEN`] bytes, or all of a shorter file) and
    /// `size` its length. An answer beyond `size` maps the whole file.
    pub(super) fn open(
        path: CStr,
        oflag: c_int,
        needed: impl FnOnce(&[u8], usize) -> usize,
    ) -> core::result::Result<Self, ObjectIoError> {
        let fd = File::new(Sys::open(path, oflag, 0).map_err(|source| ObjectIoError {
            operation: ObjectIoOperation::Open,
            source,
        })?);
        let mut stat = crate::header::sys_stat::stat::default();
        Sys::fstat(fd.fd, Out::from_mut(&mut stat)).map_err(|source| ObjectIoError {
            operation: ObjectIoOperation::Stat,
            source,
        })?;

        let size = usize::try_from(stat.st_size).map_err(|_| ObjectIoError {
            operation: ObjectIoOperation::Stat,
            source: Errno(EOVERFLOW),
        })?;
        let mut probe = [0; PROBE_LEN];
        let probed =
            read_start(&fd, &mut probe[..size.min(PROBE_LEN)]).map_err(|source| ObjectIoError {
                operation: ObjectIoOperation::Read,
                source,
            })?;
        let size = needed(&probe[..probed], size).min(size);
        Self::from_fd(fd, size).map_err(|source| ObjectIoError {
            operation: ObjectIoOperation::Map { size },
            source,
        })
    }

    fn from_fd(file: File, size: usize) -> core::result::Result<Self, Errno> {
        // SAFETY: a null hint requests a fresh mapping, and the owned descriptor stays
        // live until the mapping is dropped. No existing reference is invalidated.
        let ptr = unsafe {
            Sys::mmap(
                ptr::null_mut(),
                size,
                sys_mman::PROT_READ,
                sys_mman::MAP_PRIVATE,
                file.fd,
                0,
            )
        }?;

        Ok(Self {
            _file: file,
            ptr,
            size,
        })
    }

    pub(super) fn data(&self) -> &[u8] {
        // SAFETY: the mapping is readable for `size` bytes and lives as long as self.
        unsafe { core::slice::from_raw_parts(self.ptr.cast::<u8>(), self.size) }
    }
}

/// Reads the start of `file` into `buf`, as much of it as the file holds, and returns how many
/// bytes that was. A read of a scheme can return `EINTR` without a signal handler having run
/// (the kernel cancels a call whose context was woken), and short reads are allowed, so it
/// keeps reading until `buf` is full or the file ends, the way `read_to_end` does.
fn read_start(file: &File, buf: &mut [u8]) -> core::result::Result<usize, Errno> {
    let mut read = 0;
    while read < buf.len() {
        // `read` is at most `PROBE_LEN`, so it fits in `off_t`.
        match Sys::pread(file.fd, &mut buf[read..], read as off_t) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(Errno(EINTR)) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(read)
}

impl Drop for MmapFile {
    fn drop(&mut self) {
        // SAFETY: this object owns the live mapping, and no slices outlive this drop.
        unsafe {
            Sys::munmap(self.ptr, self.size)
                .expect("loader-owned mapping remains valid until drop");
        }
    }
}
