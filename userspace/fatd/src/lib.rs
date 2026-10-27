//! `fatd`: a read-only FAT12, FAT16 and FAT32 file system for Ferric Oxide, served as a scheme
//! the way RedoxFS serves its own (features.toml option `fs.fat`, off by default). It is for
//! the EFI system partition and for removable media. With the Cargo feature `exfat`
//! (features.toml `fs.exfat`, off by default) it serves exFAT volumes too, read by
//! `ferric-exfat`.
//!
//! The pieces, from the disk up:
//!
//! - The device is any [`ferric_blocklayer::BlockDevice`]: a disk or image file
//!   ([`ferric_blocklayer::file::FileDevice`]), a partition of one
//!   ([`ferric_blocklayer::Slice`]), or a decrypted volume.
//! - [`disk`]: the device as the read-only, seekable stream `fatfs` reads through. It
//!   refuses every write, and keeps one chunk so that the allocation table and directories,
//!   which `fatfs` reads a few bytes at a time, do not cost a disk read each.
//! - [`fs`]: the open-file table on top of the `fatfs` crate, which reads the format (boot
//!   sector, allocation tables, cluster chains, long file names), or of `ferric-exfat`. Every call that would change
//!   the file system fails with [`Error::ReadOnly`] (`EROFS`).
//! - [`time`]: FAT dates and times as Unix times.
//! - [`error`]: [`Error`], and the one place it becomes an errno.
//! - `scheme` (Redox only): the scheme handler, a thin layer from scheme calls to [`fs`].
//!
//! Everything but `scheme` runs on the host, so `cargo test` covers it against FAT images
//! made by `mkfs.fat` and `mtools` (`tests/fixtures/`), and `cargo test --features exfat`
//! against the exFAT images of `crates/exfat/tests/fixtures/`.
//!
//! # Specification
//!
//! The FAT on-disk format is described in Microsoft's "Microsoft Extensible Firmware
//! Initiative FAT32 File System Specification: FAT: General Overview of On-Disk Format",
//! version 1.03 (December 2000). [`time`] is written from its "Date and Time Formats"
//! section, and the case-insensitive name lookup in [`fs`] from its rules for short and long
//! names. Everything else is read by the `fatfs` crate (MIT). exFAT is read by
//! `ferric-exfat`, written from Microsoft's "exFAT file system specification". No code here
//! comes from the Linux kernel.
//!
//! # Examples
//!
//! ```no_run
//! use ferric_blocklayer::file::FileDevice;
//! use fatd::fs::{Fs, OpenMode, mount};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let image = FileDevice::open("esp.img".as_ref())?;
//! let volume = mount(image)?;
//! let mut fs = Fs::new(&volume);
//! let root = fs.root()?;
//! let file = fs.open(Some(root), "efi/boot/bootx64.efi", OpenMode::read())?;
//! let mut buf = [0; 2];
//! let n = fs.read(file, 0, &mut buf)?;
//! assert_eq!(&buf[..n], b"MZ");
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod disk;
pub mod error;
pub mod fs;
#[cfg(target_os = "redox")]
pub mod scheme;
pub mod time;

pub use error::Error;
