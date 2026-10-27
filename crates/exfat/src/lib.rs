//! A read-only exFAT reader, on any [`BlockDevice`].
//!
//! # Structure
//!
//! - [`boot`]: the boot sector's fields, checked, and the boot region's checksum.
//! - The FAT: one 32-bit entry per cluster, naming the next cluster of a chain (`FFFFFFFF`
//!   ends one, `FFFFFFF7` marks a bad cluster). Files marked `NoFatChain` are consecutive
//!   clusters the FAT does not describe.
//! - [`dir`]: directory entry sets (File, Stream Extension, File Name), checked by their
//!   checksums, and the root directory's system entries.
//! - [`upcase`]: the up-case table, for comparing names without regard to case.
//! - [`time`]: timestamps as Unix times (shared with FAT, whose format exFAT keeps).
//! - [`ExFat`] ties them together: [`ExFat::open`] checks the boot region and reads the
//!   root directory's system entries, then [`ExFat::list`], [`ExFat::find`] and
//!   [`ExFat::read`] serve directories and files. A [`Cursor`] remembers where a read left
//!   off in a cluster chain, so sequential reads do not walk the chain from its start.
//!
//! Nothing here writes. Reads beyond a file's valid data length (space allocated but not
//! yet written) return zeros, as the specification requires.
//!
//! # Specification
//!
//! Written from Microsoft's "exFAT file system specification" (published 2019 on
//! learn.microsoft.com, the version of the format with `FileSystemRevision` 1.00). Each
//! module names the sections it follows. No code comes from Linux or other exFAT
//! implementations.
//!
//! # Examples
//!
//! ```no_run
//! use ferric_blocklayer::Memory;
//! use ferric_exfat::{Cursor, ExFat};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let volume = ExFat::open(Memory::new(std::fs::read("usb.img")?))?;
//! let root = volume.root();
//! let entry = volume.find(&root, "README.TXT")?.ok_or("no README")?;
//! let mut buf = vec![0; entry.stream.length as usize];
//! let n = volume.read(&entry.stream, &mut Cursor::default(), 0, &mut buf)?;
//! println!("{}", String::from_utf8_lossy(&buf[..n]));
//! # Ok(())
//! # }
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;

pub mod boot;
pub mod dir;
pub use ferric_fat_types::time;
pub mod upcase;
mod wire;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use ferric_blocklayer::BlockDevice;
use simple_endian::{FromSlice, LittleEndian};
use thiserror::Error;

pub use boot::{BootSector, BootSectorError};
pub use dir::{DirError, Entry, Stream};
pub use upcase::{Upcase, UpcaseError};

/// The FAT entry that ends a chain.
const END_OF_CHAIN: u32 = 0xFFFF_FFFF;
/// The FAT entry of a bad cluster.
const BAD_CLUSTER: u32 = 0xFFFF_FFF7;
/// The size of the block of the FAT kept for entry reads.
const FAT_BLOCK: usize = 512;
/// The largest directory the specification allows (256 MiB).
const MAX_DIRECTORY: u64 = 256 << 20;

/// Everything that can go wrong reading an exFAT volume.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error<E> {
    /// The device failed.
    #[error("reading the device failed")]
    Device(#[source] E),
    /// The boot sector was refused.
    #[error("bad boot sector")]
    BootSector(#[from] BootSectorError),
    /// The boot region does not match the checksum in its twelfth sector.
    #[error(
        "the boot region's checksum is {computed:#010x}, its checksum sector says {stored:#010x}"
    )]
    BootChecksum {
        /// The checksum the checksum sector holds.
        stored: u32,
        /// The checksum of the boot region.
        computed: u32,
    },
    /// The boot sector describes a volume larger than the device.
    #[error("the volume is {volume} bytes, the device {device}")]
    Truncated {
        /// The volume's size from its boot sector.
        volume: u64,
        /// The device's size.
        device: u64,
    },
    /// The root directory has no Allocation Bitmap for the active FAT.
    #[error("the root directory has no allocation bitmap")]
    NoBitmap,
    /// The root directory has no Up-case Table.
    #[error("the root directory has no up-case table")]
    NoUpcase,
    /// The Up-case Table was refused.
    #[error("bad up-case table")]
    Upcase(#[from] UpcaseError),
    /// A directory's entries were refused.
    #[error("bad directory")]
    Directory(#[from] DirError),
    /// A cluster chain reaches a cluster the heap does not have, or a bad cluster.
    #[error("cluster {cluster} is not a usable cluster of the heap")]
    BadCluster {
        /// The cluster number.
        cluster: u32,
    },
    /// A cluster chain ends before or after its file's length, or never ends (loops back on
    /// itself).
    #[error("a cluster chain from cluster {first} does not match its length")]
    Chain {
        /// The chain's first cluster.
        first: u32,
    },
    /// The Allocation Bitmap is shorter than one bit per cluster of the heap, or has fewer
    /// bytes written than that.
    #[error("an allocation bitmap of {length} bytes ({valid} written) for {needed} needed")]
    Bitmap {
        /// The bitmap's length, from its directory entry.
        length: u64,
        /// The bytes of it written (its valid data length).
        valid: u64,
        /// The bytes the heap's cluster count needs.
        needed: u64,
    },
    /// A directory is larger than the 256 MiB the specification allows, or than memory can
    /// address.
    #[error("a directory of {length} bytes")]
    DirectoryTooLarge {
        /// Its length.
        length: u64,
    },
}

impl<E> Error<E> {
    /// The same error with the device's error, if it is one, turned into another type by
    /// `f`, for a caller that carries device errors as one type of its own.
    pub fn map_device<F>(self, f: impl FnOnce(E) -> F) -> Error<F> {
        match self {
            Self::Device(e) => Error::Device(f(e)),
            Self::BootSector(e) => Error::BootSector(e),
            Self::BootChecksum { stored, computed } => Error::BootChecksum { stored, computed },
            Self::Truncated { volume, device } => Error::Truncated { volume, device },
            Self::NoBitmap => Error::NoBitmap,
            Self::NoUpcase => Error::NoUpcase,
            Self::Upcase(e) => Error::Upcase(e),
            Self::Directory(e) => Error::Directory(e),
            Self::BadCluster { cluster } => Error::BadCluster { cluster },
            Self::Chain { first } => Error::Chain { first },
            Self::Bitmap {
                length,
                valid,
                needed,
            } => Error::Bitmap {
                length,
                valid,
                needed,
            },
            Self::DirectoryTooLarge { length } => Error::DirectoryTooLarge { length },
        }
    }

    /// Whether the error says the volume is not exFAT or not one this reader can mount, as
    /// opposed to a failure reading it (the device, or damage found past the boot region).
    pub const fn is_mount_error(&self) -> bool {
        matches!(
            self,
            Self::BootSector(_)
                | Self::BootChecksum { .. }
                | Self::Truncated { .. }
                | Self::NoBitmap
                | Self::NoUpcase
                | Self::Upcase(_)
                | Self::Bitmap { .. }
        )
    }
}

/// Where a read left off in a cluster chain: the `index`th cluster of the file is `cluster`.
/// Start with [`Cursor::default`]; [`ExFat::read`] keeps it up to date, and the first read
/// through a cursor checks the whole chain once (see [`ExFat::read`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    index: u64,
    cluster: u32,
    /// Whether the chain has been checked to end where the stream's length says.
    checked: bool,
}

/// A mounted exFAT volume.
#[derive(Debug)]
pub struct ExFat<D> {
    device: D,
    boot: BootSector,
    root: Stream,
    upcase: Upcase,
    bitmap: Stream,
    label: String,
    /// The block of the FAT last read, and where it starts.
    fat_block: RefCell<Option<(u64, [u8; FAT_BLOCK])>>,
}

impl<D: BlockDevice> ExFat<D> {
    /// Checks the boot region of `device`, and reads its root directory's system entries.
    ///
    /// # Errors
    ///
    /// [`Error::BootSector`] when the first sector is not an exFAT boot sector (so other
    /// file systems are told apart), [`Error::BootChecksum`], [`Error::Truncated`],
    /// [`Error::NoBitmap`], [`Error::NoUpcase`] and [`Error::Upcase`] for a volume that is
    /// damaged, and [`Error::Device`], [`Error::Chain`], [`Error::BadCluster`] and
    /// [`Error::Directory`] when the root directory cannot be read, [`Error::DirectoryTooLarge`]
    /// when its chain is over 256 MiB, and [`Error::Bitmap`] when the allocation bitmap
    /// cannot cover every cluster.
    pub fn open(device: D) -> Result<Self, Error<D::Error>> {
        let mut first = [0; boot::SECTOR_PREFIX];
        device.read_at(0, &mut first).map_err(Error::Device)?;
        let boot = BootSector::parse(&first)?;
        let volume = boot.volume_sectors.saturating_mul(boot.sector_size());
        if volume > device.size() {
            return Err(Error::Truncated {
                volume,
                device: device.size(),
            });
        }
        check_boot_region(&device, &boot)?;
        let mut fs = Self {
            device,
            boot,
            root: Stream::default(),
            upcase: Upcase::parse(&[], upcase::table_checksum(&[]))?,
            bitmap: Stream::default(),
            label: String::new(),
            fat_block: RefCell::new(None),
        };
        // root_stream stops counting past MAX_DIRECTORY, so this is all the root can be.
        fs.root = fs.root_stream()?;
        let listing = dir::parse(&fs.read_all(&fs.root)?)?;
        let system = listing.system;
        fs.bitmap = system.bitmaps[usize::from(fs.boot.active_fat)].ok_or(Error::NoBitmap)?;
        let needed = u64::from(fs.boot.cluster_count).div_ceil(8);
        if fs.bitmap.length < needed || fs.bitmap.valid_length < needed {
            return Err(Error::Bitmap {
                length: fs.bitmap.length,
                valid: fs.bitmap.valid_length,
                needed,
            });
        }
        let (table, checksum) = system.upcase.ok_or(Error::NoUpcase)?;
        // Validate the directory's untrusted length before read_all allocates it.
        Upcase::check_length(table.length)?;
        fs.upcase = Upcase::parse(&fs.read_all(&table)?, checksum)?;
        fs.label = String::from_utf16_lossy(&system.label);
        Ok(fs)
    }

    /// The boot sector's fields.
    pub fn boot_sector(&self) -> &BootSector {
        &self.boot
    }

    /// The volume label; empty when it has none.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The up-case table names are compared with.
    pub fn upcase(&self) -> &Upcase {
        &self.upcase
    }

    /// The root directory's data. Its length is that of its cluster chain.
    pub fn root(&self) -> Stream {
        self.root
    }

    /// How many clusters the allocation bitmap marks free.
    ///
    /// # Errors
    ///
    /// As [`Self::read`], for the bitmap.
    pub fn free_clusters(&self) -> Result<u64, Error<D::Error>> {
        let bits = u64::from(self.boot.cluster_count);
        // `open` checked that the bitmap has this many bytes, all of them written.
        let needed = usize::try_from(bits.div_ceil(8)).map_err(|_| Error::Bitmap {
            length: self.bitmap.length,
            valid: self.bitmap.valid_length,
            needed: bits.div_ceil(8),
        })?;
        let mut bytes = vec![0; needed];
        self.read(&self.bitmap, &mut Cursor::default(), 0, &mut bytes)?;
        let used: u64 = bytes
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                // Bits past the last cluster are not clusters.
                let first = i as u64 * 8;
                let mask = if bits - first < 8 {
                    (1u16 << (bits - first)) - 1
                } else {
                    0xFF
                };
                u64::from((u16::from(b) & mask).count_ones())
            })
            .sum();
        Ok(bits - used)
    }

    /// The files and directories of the directory whose data is `dir`.
    ///
    /// # Errors
    ///
    /// [`Error::DirectoryTooLarge`], [`Error::Directory`], and the errors of
    /// [`Self::read`].
    pub fn list(&self, dir: &Stream) -> Result<Vec<Entry>, Error<D::Error>> {
        if dir.length > MAX_DIRECTORY {
            return Err(Error::DirectoryTooLarge { length: dir.length });
        }
        Ok(dir::parse(&self.read_all(dir)?)?.entries)
    }

    /// The entry of the directory `dir` named `name`, compared through the up-case table.
    ///
    /// # Errors
    ///
    /// As [`Self::list`].
    pub fn find(&self, dir: &Stream, name: &str) -> Result<Option<Entry>, Error<D::Error>> {
        let name: Vec<u16> = name.encode_utf16().collect();
        Ok(self
            .list(dir)?
            .into_iter()
            .find(|entry| self.upcase.same(&entry.name, &name)))
    }

    /// Reads into `buf` from byte `offset` of the data `stream` describes, moving `cursor`
    /// to where the read ended. Returns how many bytes were read: fewer than `buf.len()` only
    /// at the end of the data. Bytes past the valid data length read as zero.
    ///
    /// The first read through a fresh `cursor` of a stream with a FAT chain follows the whole
    /// chain once and checks that it ends exactly at the last cluster the stream's length
    /// needs, so a chain that loops or ends early or late is an error before any of its data
    /// is returned. Later reads through the same cursor skip the check.
    ///
    /// # Errors
    ///
    /// [`Error::Device`], [`Error::BadCluster`] and [`Error::Chain`].
    pub fn read(
        &self,
        stream: &Stream,
        cursor: &mut Cursor,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error<D::Error>> {
        let cluster_size = self.boot.cluster_size();
        let left = stream.length.saturating_sub(offset);
        let len = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            let within = at % cluster_size;
            let n = (len - done).min((cluster_size - within) as usize);
            let out = &mut buf[done..done + n];
            if at >= stream.valid_length {
                out.fill(0);
            } else {
                let cluster = self.cluster_at(stream, cursor, at / cluster_size)?;
                let valid = ((stream.valid_length - at).min(n as u64)) as usize;
                let start = self.boot.cluster_start(cluster) + within;
                self.device
                    .read_at(start, &mut out[..valid])
                    .map_err(Error::Device)?;
                out[valid..].fill(0);
            }
            done += n;
        }
        Ok(done)
    }

    /// The whole of the data `stream` describes.
    fn read_all(&self, stream: &Stream) -> Result<Vec<u8>, Error<D::Error>> {
        let len = usize::try_from(stream.length).map_err(|_| Error::DirectoryTooLarge {
            length: stream.length,
        })?;
        let mut bytes = vec![0; len];
        let n = self.read(stream, &mut Cursor::default(), 0, &mut bytes)?;
        bytes.truncate(n);
        Ok(bytes)
    }

    /// The `index`th cluster of `stream`, walking its chain from `cursor` when that is at or
    /// before it, and from the start otherwise.
    fn cluster_at(
        &self,
        stream: &Stream,
        cursor: &mut Cursor,
        index: u64,
    ) -> Result<u32, Error<D::Error>> {
        let first = stream.first_cluster;
        let bad = |cluster| Error::BadCluster { cluster };
        if stream.contiguous {
            let cluster = u32::try_from(u64::from(first) + index).map_err(|_| bad(first))?;
            return if self.boot.is_cluster(first) && self.boot.is_cluster(cluster) {
                Ok(cluster)
            } else {
                Err(bad(cluster))
            };
        }
        if !self.boot.is_cluster(first) {
            return Err(bad(first));
        }
        if !cursor.checked {
            self.check_chain(stream)?;
            cursor.checked = true;
        }
        if cursor.cluster == 0 || cursor.index > index {
            *cursor = Cursor {
                index: 0,
                cluster: first,
                checked: true,
            };
        }
        while cursor.index < index {
            cursor.cluster = self
                .next_cluster(cursor.cluster)?
                .ok_or(Error::Chain { first })?;
            cursor.index += 1;
        }
        Ok(cursor.cluster)
    }

    /// Checks that the FAT chain of `stream` has exactly the clusters its length needs: a
    /// chain that loops never reaches its end marker, so it fails here too. A stream needing
    /// more clusters than the heap has is refused without walking.
    fn check_chain(&self, stream: &Stream) -> Result<(), Error<D::Error>> {
        let first = stream.first_cluster;
        let clusters = stream.length.div_ceil(self.boot.cluster_size()).max(1);
        if clusters > u64::from(self.boot.cluster_count) {
            return Err(Error::Chain { first });
        }
        let mut cluster = first;
        for _ in 1..clusters {
            cluster = self.next_cluster(cluster)?.ok_or(Error::Chain { first })?;
        }
        match self.next_cluster(cluster)? {
            None => Ok(()),
            Some(_) => Err(Error::Chain { first }),
        }
    }

    /// The cluster after `cluster` in its chain, or `None` at the chain's end.
    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, Error<D::Error>> {
        let at = self.boot.fat_start() + u64::from(cluster) * 4;
        let block_start = at - at % FAT_BLOCK as u64;
        let mut cached = self.fat_block.borrow_mut();
        let block = match &*cached {
            Some((start, block)) if *start == block_start => block,
            _ => {
                let mut block = [0; FAT_BLOCK];
                self.device
                    .read_at(block_start, &mut block)
                    .map_err(Error::Device)?;
                &cached.insert((block_start, block)).1
            }
        };
        let i = (at - block_start) as usize;
        let next = LittleEndian::<u32>::read_from_slice(&block[i..i + 4])
            .map_err(|_| Error::BadCluster { cluster })?
            .to_native();
        match next {
            END_OF_CHAIN => Ok(None),
            BAD_CLUSTER => Err(Error::BadCluster { cluster }),
            n if self.boot.is_cluster(n) => Ok(Some(n)),
            n => Err(Error::BadCluster { cluster: n }),
        }
    }

    /// The root directory: its chain, counted, as a stream. Counting stops with
    /// [`Error::DirectoryTooLarge`] as soon as the chain is longer than the 256 MiB a
    /// directory may be, so neither the walk nor the read after it is unbounded.
    fn root_stream(&self) -> Result<Stream, Error<D::Error>> {
        let first = self.boot.root_cluster;
        let mut clusters = 1u64;
        let mut cluster = first;
        while let Some(next) = self.next_cluster(cluster)? {
            clusters += 1;
            let length = clusters * self.boot.cluster_size();
            if length > MAX_DIRECTORY {
                return Err(Error::DirectoryTooLarge { length });
            }
            // A chain longer than the heap loops.
            if clusters > u64::from(self.boot.cluster_count) {
                return Err(Error::Chain { first });
            }
            cluster = next;
        }
        let length = clusters * self.boot.cluster_size();
        Ok(Stream {
            first_cluster: first,
            valid_length: length,
            length,
            contiguous: false,
        })
    }
}

/// Checks the 11 sectors of the main boot region against the checksum sector after them.
fn check_boot_region<D: BlockDevice>(device: &D, boot: &BootSector) -> Result<(), Error<D::Error>> {
    let size = boot.sector_size() as usize;
    let mut sector = vec![0; size];
    let mut computed = 0;
    for index in 0..boot::CHECKSUMMED_SECTORS {
        device
            .read_at(index * boot.sector_size(), &mut sector)
            .map_err(Error::Device)?;
        computed = boot::checksum(computed, index, &sector);
    }
    device
        .read_at(boot::CHECKSUMMED_SECTORS * boot.sector_size(), &mut sector)
        .map_err(Error::Device)?;
    for word in sector.chunks_exact(4) {
        let stored = LittleEndian::<u32>::read_from_slice(word)
            .map_err(|_| Error::BootSector(BootSectorError::Layout))?
            .to_native();
        if stored != computed {
            return Err(Error::BootChecksum { stored, computed });
        }
    }
    Ok(())
}
