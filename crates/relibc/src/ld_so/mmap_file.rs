//! Owned loader mappings and operation-specific I/O failures.
//!
//! A file guard is acquired immediately after open, so every later error closes
//! the descriptor. Successful mappings retain it until after they are unmapped.
use super::load_error::{ObjectIoError, ObjectIoOperation};
use crate::{
    c_str::CStr,
    error::Errno,
    fs::File,
    header::{errno::EOVERFLOW, sys_mman},
    out::Out,
    platform::{
        Pal, Sys,
        types::{c_int, c_void},
    },
};
use core::ptr;

pub(super) struct MmapFile {
    _file: File,
    ptr: *mut c_void,
    size: usize,
}

impl MmapFile {
    pub(super) fn open(path: CStr, oflag: c_int) -> core::result::Result<Self, ObjectIoError> {
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

impl Drop for MmapFile {
    fn drop(&mut self) {
        // SAFETY: this object owns the live mapping, and no slices outlive this drop.
        unsafe {
            Sys::munmap(self.ptr, self.size)
                .expect("loader-owned mapping remains valid until drop");
        }
    }
}
