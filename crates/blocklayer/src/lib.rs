//! Stackable, read-only block layers: byte windows, LUKS decryption and LVM logical volumes,
//! each a [`BlockDevice`] built on other block devices.
//!
//! This is the block layer of Ferric Oxide's dm-crypt and LVM compatibility (roadmap Phase 3).
//! A layer takes the devices below it and is itself a device, so layers stack in any order the
//! disk was set up in: LUKS on a partition ([`Slice`]), LVM on LUKS, LUKS on an LVM logical
//! volume, and so on. The block daemon (`userspace/blockd`) serves the top of a stack as a
//! read-only scheme, for a file system daemon to mount.
//!
//! # Structure
//!
//! - [`BlockDevice`] is the one interface: a size and a positioned read. [`Memory`] serves a
//!   byte buffer, and references, `Box` and `Arc` of a device are devices.
//! - [`Slice`] is a byte window of a device (a partition, or a region of an image).
//! - [`Crypt`] (feature `luks`) opens a LUKS1 or LUKS2 volume with a passphrase, through
//!   `ferric-luks`, and decrypts its data sector by sector.
//! - [`Lvm`] (feature `lvm`) reads the LVM2 metadata of a group's physical volumes, through
//!   `ferric-lvm`, and maps a logical volume onto them.
//! - `file` (feature `std`) serves a disk, partition or image file as a device, widening
//!   reads to the whole blocks a Redox disk scheme requires.
//! - [`Stack`] (feature `alloc`) is a stack whose shape is chosen at run time, such as from a
//!   daemon's command line, with one typed error, [`StackError`].
//!
//! Every layer checks that a read lies inside it and fails with [`OutOfRange`] otherwise, and
//! keeps the error of the device below as the source of its own.
//!
//! # Specifications
//!
//! The formats are read by `ferric-luks` and `ferric-lvm`, which name their specifications.
//! What this crate adds is how LUKS data sectors are addressed (see [`Crypt`] and
//! `ferric_luks::data` for the IV of each sector) and the LVM extent mapping applied to reads
//! that cross segment and physical-volume boundaries.
//!
//! # Examples
//!
//! ```
//! use ferric_blocklayer::{BlockDevice, Memory, Slice};
//!
//! let disk = Memory::new((0u8..=255).collect::<Vec<u8>>());
//! let partition = Slice::new(disk, 16, 64)?;
//! let mut buf = [0; 4];
//! partition.read_at(60, &mut buf)?;
//! assert_eq!(buf, [76, 77, 78, 79]);
//! assert!(partition.read_at(62, &mut buf).is_err());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "luks")]
mod crypt;
#[cfg(feature = "std")]
pub mod file;
#[cfg(feature = "lvm")]
mod lvm;
mod memory;
mod slice;
#[cfg(feature = "alloc")]
mod stack;

use thiserror::Error;

#[cfg(feature = "luks")]
pub use crypt::{Crypt, CryptError, CryptOpenError};
#[cfg(feature = "luks")]
pub use ferric_luks::Limits;
#[cfg(feature = "lvm")]
pub use lvm::{Lvm, LvmError, LvmOpenError};
pub use memory::Memory;
pub use slice::{Slice, SliceError};
#[cfg(feature = "alloc")]
pub use stack::{Stack, StackError};

/// A read-only device of bytes that can be read at any offset.
pub trait BlockDevice {
    /// Why a read failed.
    type Error: core::error::Error + 'static;

    /// The size of the device, in bytes.
    fn size(&self) -> u64;

    /// Fills all of `buf` with the bytes from `offset`.
    ///
    /// # Errors
    ///
    /// The device's error: at least when the bytes are not all inside the device, and when
    /// the device below fails.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
}

impl<D: BlockDevice + ?Sized> BlockDevice for &D {
    type Error = D::Error;

    fn size(&self) -> u64 {
        (**self).size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        (**self).read_at(offset, buf)
    }
}

#[cfg(feature = "alloc")]
impl<D: BlockDevice + ?Sized> BlockDevice for alloc::boxed::Box<D> {
    type Error = D::Error;

    fn size(&self) -> u64 {
        (**self).size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        (**self).read_at(offset, buf)
    }
}

#[cfg(feature = "alloc")]
impl<D: BlockDevice + ?Sized> BlockDevice for alloc::sync::Arc<D> {
    type Error = D::Error;

    fn size(&self) -> u64 {
        (**self).size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        (**self).read_at(offset, buf)
    }
}

/// A read that does not lie inside a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Error)]
#[error("{len} bytes at {offset:#x} are not inside the {size}-byte device")]
pub struct OutOfRange {
    /// The first byte asked for.
    pub offset: u64,
    /// How many bytes were asked for.
    pub len: usize,
    /// The size of the device.
    pub size: u64,
}

impl OutOfRange {
    /// Checks that `len` bytes from `offset` lie inside a device of `size` bytes.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] when they do not.
    pub fn check(offset: u64, len: usize, size: u64) -> Result<(), Self> {
        let end = u64::try_from(len)
            .ok()
            .and_then(|len| offset.checked_add(len));
        match end {
            Some(end) if end <= size => Ok(()),
            _ => Err(Self { offset, len, size }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_checked_without_overflow() {
        assert_eq!(OutOfRange::check(0, 10, 10), Ok(()));
        assert_eq!(OutOfRange::check(10, 0, 10), Ok(()));
        assert_eq!(
            OutOfRange::check(5, 6, 10),
            Err(OutOfRange {
                offset: 5,
                len: 6,
                size: 10
            })
        );
        assert!(OutOfRange::check(u64::MAX, 2, u64::MAX).is_err());
        assert!(OutOfRange::check(11, 0, 10).is_err());
    }
}
