//! The mapping of a RAM file, and the only `unsafe` code of the crate.

use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;

use rustix::fs::SeekFrom;
use rustix::mm::{MapFlags, ProtFlags};

use crate::RamLayout;

/// The guest page size the scanning helpers work in.
pub const PAGE_BYTES: u64 = 4096;

/// The bytes of a word.
const WORD_BYTES: u64 = size_of::<AtomicU64>() as u64;

/// A guest's RAM file, mapped until dropped; see the [crate docs](crate).
#[derive(Debug)]
pub struct GuestRam {
    file: File,
    base: NonNull<AtomicU64>,
    bytes: usize,
    layout: RamLayout,
}

/// Why a RAM file could not be mapped or examined.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GuestRamError {
    /// The file could not be opened or its size read.
    #[error("cannot open {}", path.display())]
    Open {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is empty, or shorter than the RAM the layout puts in it.
    #[error("{}: {file} bytes cannot hold {ram} bytes of RAM", path.display())]
    Short {
        /// The file.
        path: PathBuf,
        /// The file's size.
        file: u64,
        /// The RAM's size.
        ram: u64,
    },
    /// The file is larger than this process can map.
    #[error("{}: {bytes} bytes do not fit in this process's address space", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
        /// The file's size.
        bytes: u64,
    },
    /// The mapping failed.
    #[error("cannot map {}", path.display())]
    Map {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl GuestRam {
    /// Opens the RAM file at `path` read-only and maps all of it. `layout` places its RAM in
    /// the guest's physical address space; `None` takes q35's layout of RAM the file's size
    /// ([`RamLayout::q35`]), which is what QEMU's `memory-backend-file` writes.
    ///
    /// The mapping is private: the guest's stores show through it until this process stores
    /// to a page, which nothing here does. A file truncated while mapped makes reads past its
    /// new end fault, as with any mapping; QEMU never truncates its memory backend.
    ///
    /// # Errors
    ///
    /// [`GuestRamError::Open`] when the file cannot be opened, [`GuestRamError::Short`] when it
    /// is empty or shorter than `layout`'s RAM, [`GuestRamError::TooLarge`] when it cannot be
    /// mapped whole, and [`GuestRamError::Map`] when the mapping fails.
    pub fn open(path: &Path, layout: Option<RamLayout>) -> Result<Self, GuestRamError> {
        let open_error = |source| GuestRamError::Open {
            path: path.to_owned(),
            source,
        };
        let file = File::open(path).map_err(open_error)?;
        let file_bytes = file.metadata().map_err(open_error)?.len();
        let layout = layout.unwrap_or(RamLayout::q35(file_bytes));
        if file_bytes == 0 || file_bytes < layout.ram_bytes() {
            return Err(GuestRamError::Short {
                path: path.to_owned(),
                file: file_bytes,
                ram: layout.ram_bytes(),
            });
        }
        let bytes = usize::try_from(file_bytes).map_err(|_| GuestRamError::TooLarge {
            path: path.to_owned(),
            bytes: file_bytes,
        })?;
        // SAFETY: a new mapping at an address the kernel picks (no fixed address), so it
        // replaces nothing this process uses, of a file opened read-only. It is private, so
        // nothing done through it can reach the file, and nothing refers to it yet.
        let base = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                bytes,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE | MapFlags::NORESERVE,
                &file,
                0,
            )
        }
        .map_err(|errno| GuestRamError::Map {
            path: path.to_owned(),
            source: errno.into(),
        })?;
        let base = NonNull::new(base.cast::<AtomicU64>()).ok_or_else(|| GuestRamError::Map {
            path: path.to_owned(),
            source: io::Error::from(io::ErrorKind::InvalidData),
        })?;
        Ok(Self {
            file,
            base,
            bytes,
            layout,
        })
    }

    /// Where the file's RAM sits in the guest's physical address space.
    pub fn layout(&self) -> RamLayout {
        self.layout
    }

    /// The file's size in bytes.
    pub fn file_bytes(&self) -> u64 {
        self.bytes as u64
    }

    /// The `count` words at file offset `offset`, or `None` when `offset` is not a multiple of
    /// 8 or the words pass the end of the file.
    ///
    /// The guest may store to them at any time: load them with relaxed ordering, and fence
    /// where ordering matters, as `ferric_trace::ring::Observer` does.
    pub fn words_at(&self, offset: u64, count: usize) -> Option<&[AtomicU64]> {
        if !offset.is_multiple_of(WORD_BYTES) {
            return None;
        }
        let start = usize::try_from(offset / WORD_BYTES).ok()?;
        let end = start.checked_add(count)?;
        if end > self.bytes / size_of::<AtomicU64>() {
            return None;
        }
        // SAFETY: the words lie inside the mapping (checked above), which stays mapped while
        // `self` is borrowed (only `drop` unmaps it). The mapping is page aligned, so the words
        // are aligned for `AtomicU64`, which has the size of a `u64` and takes any bits. It is
        // private and writable, so every atomic operation through the slice is allowed, and
        // the guest's own stores to the file are what the loads observe.
        Some(unsafe { std::slice::from_raw_parts(self.base.as_ptr().add(start), count) })
    }

    /// The `count` words at guest-physical `phys`, or `None` unless RAM holds them all in one
    /// piece of the layout (or `phys` is not a multiple of 8).
    pub fn phys_words(&self, phys: u64, count: usize) -> Option<&[AtomicU64]> {
        let len = u64::try_from(count).ok()?.checked_mul(WORD_BYTES)?;
        self.words_at(self.layout.offset_of(phys, len)?, count)
    }

    /// The parts of the RAM that hold data, as page-aligned file offset ranges, from the file
    /// system's record of its holes (`SEEK_DATA` and `SEEK_HOLE`). Reading a hole through the
    /// mapping would make the file system allocate it (tmpfs does, for `/dev/shm`), so a
    /// scan reads only these. A file system that keeps no holes reports all of it.
    ///
    /// # Errors
    ///
    /// The file system's error.
    pub fn data_ranges(&self) -> io::Result<Vec<Range<u64>>> {
        let end = self.layout.ram_bytes();
        let mut ranges: Vec<Range<u64>> = Vec::new();
        let mut pos = 0;
        while pos < end {
            let start = match rustix::fs::seek(&self.file, SeekFrom::Data(pos)) {
                Ok(start) => start,
                Err(rustix::io::Errno::NXIO) => break,
                Err(errno) => return Err(errno.into()),
            };
            let stop = rustix::fs::seek(&self.file, SeekFrom::Hole(start))?;
            let range = start / PAGE_BYTES * PAGE_BYTES..stop.div_ceil(PAGE_BYTES) * PAGE_BYTES;
            let range = range.start.min(end)..range.end.min(end);
            match ranges.last_mut() {
                Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
                _ if !range.is_empty() => ranges.push(range),
                _ => {}
            }
            pos = stop.max(start + 1);
        }
        Ok(ranges)
    }

    /// Whether the file was removed since it was opened, as a QEMU harness does with the
    /// memory backend once the guest is gone.
    ///
    /// # Errors
    ///
    /// The file system's error.
    pub fn is_unlinked(&self) -> io::Result<bool> {
        Ok(self.file.metadata()?.nlink() == 0)
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        // SAFETY: the mapping `open` made, of `bytes` bytes; `drop` has `&mut self`, so no
        // borrow of its words is left. A failure leaves the mapping in place, a leak only.
        let _ = unsafe { rustix::mm::munmap(self.base.as_ptr().cast(), self.bytes) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    use std::sync::atomic::Ordering;

    /// A file of `bytes` zero bytes (sparse), with `words` written at their offsets.
    fn ram_file(bytes: u64, words: &[(u64, u64)]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("a temporary file");
        file.as_file().set_len(bytes).expect("sized");
        for &(offset, word) in words {
            file.as_file()
                .write_all_at(&word.to_ne_bytes(), offset)
                .expect("written");
        }
        file
    }

    fn load(words: &[AtomicU64]) -> Vec<u64> {
        words.iter().map(|w| w.load(Ordering::Relaxed)).collect()
    }

    #[test]
    fn words_are_read_at_offsets_and_addresses() {
        let file = ram_file(1 << 20, &[(0x1000, 7), (0x1008, 8), (0xf_fff8, 9)]);
        let ram = GuestRam::open(file.path(), None).expect("maps");
        assert_eq!(ram.file_bytes(), 1 << 20);
        assert_eq!(ram.layout(), RamLayout::q35(1 << 20));
        assert_eq!(load(ram.words_at(0x1000, 2).expect("in the file")), [7, 8]);
        assert_eq!(load(ram.phys_words(0xf_fff8, 1).expect("in RAM")), [9]);
        assert!(ram.words_at(0x1004, 1).is_none(), "unaligned");
        assert!(ram.words_at(0xf_fff8, 2).is_none(), "past the end");
        assert!(ram.phys_words(0x10_0000, 1).is_none(), "past the RAM");
        assert!(ram.words_at(0, usize::MAX).is_none());
    }

    #[test]
    fn the_layout_places_the_ram() {
        let file = ram_file(3 << 20, &[(2 << 20, 42)]);
        let layout = RamLayout::split(3 << 20, 2 << 20).expect("valid");
        let ram = GuestRam::open(file.path(), Some(layout)).expect("maps");
        assert_eq!(
            load(ram.phys_words(crate::FOUR_GIB, 1).expect("above 4 GiB")),
            [42]
        );
        assert!(ram.phys_words(2 << 20, 1).is_none(), "the hole");
    }

    #[test]
    fn stores_to_the_file_show_through() {
        let mut file = ram_file(64 << 10, &[]);
        let ram = GuestRam::open(file.path(), None).expect("maps");
        let word = &ram.words_at(0x2000, 1).expect("in the file")[0];
        assert_eq!(word.load(Ordering::Relaxed), 0);
        file.as_file()
            .write_all_at(&0x1234_u64.to_ne_bytes(), 0x2000)
            .expect("written");
        file.flush().expect("flushed");
        assert_eq!(word.load(Ordering::Relaxed), 0x1234);
        file.as_file()
            .write_all_at(&0x5678_u64.to_ne_bytes(), 0x2000)
            .expect("written");
        assert_eq!(word.load(Ordering::Relaxed), 0x5678);
    }

    #[test]
    fn data_ranges_skip_holes() {
        let file = ram_file(16 << 20, &[(0x1008, 1), (9 << 20, 2)]);
        let ram = GuestRam::open(file.path(), None).expect("maps");
        let ranges = ram.data_ranges().expect("seeks");
        let holds = |offset: u64| ranges.iter().any(|r| r.contains(&offset));
        assert!(holds(0x1000) && holds(9 << 20), "{ranges:x?}");
        assert!(
            ranges
                .iter()
                .all(|r| r.start % PAGE_BYTES == 0 && r.end % PAGE_BYTES == 0),
            "{ranges:x?}"
        );
        // Every file system the tests run on (tmpfs, ext4, xfs, btrfs) keeps holes.
        assert!(!holds(4 << 20), "a hole: {ranges:x?}");
    }

    #[test]
    fn unlinking_is_noticed() {
        let file = ram_file(4096, &[]);
        let path = file.path().to_owned();
        let ram = GuestRam::open(&path, None).expect("maps");
        assert!(!ram.is_unlinked().expect("stat"));
        file.close().expect("removed");
        assert!(ram.is_unlinked().expect("stat"));
        assert_eq!(load(ram.words_at(0, 1).expect("still mapped")), [0]);
    }

    #[test]
    fn short_and_missing_files_are_refused() {
        let empty = ram_file(0, &[]);
        assert!(matches!(
            GuestRam::open(empty.path(), None),
            Err(GuestRamError::Short { file: 0, .. })
        ));
        let small = ram_file(4096, &[]);
        assert!(matches!(
            GuestRam::open(small.path(), Some(RamLayout::q35(8192))),
            Err(GuestRamError::Short {
                file: 4096,
                ram: 8192,
                ..
            })
        ));
        let missing = small.path().with_extension("missing");
        assert!(matches!(
            GuestRam::open(&missing, None),
            Err(GuestRamError::Open { .. })
        ));
    }
}
