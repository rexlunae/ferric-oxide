//! Registered-buffer grants on disk schemes (io/registered-buffers-design.md in the project
//! files, PR 4): an application attaches a `kernel.regbuf` grant to a disk descriptor, and the
//! disk driver maps it into its DMA domain once and has the device move blocks straight
//! between the disk and the application's pages, with no copy and no per-request mapping.
//!
//! - [`abi`] is the one definition of the calls, shared by driver-block and its clients.
//! - `slots` (with the `alloc` feature) is the driver's table of a descriptor's attached
//!   grants and the checks a request passes before the device sees an address.
//! - [`prp`] lays out NVMe data pointers (PRP entries and lists) for a buffer that is
//!   contiguous in the device's address space, as a mapped grant is.
//!
//! # Examples
//!
//! ```
//! use ferric_blkgrant::abi::{Request, Slot, Transfer};
//!
//! let slot = Slot::new(0).expect("slot 0 exists");
//! let read = Request::Read(Transfer { slot, offset: 0, lba: 2048, len: 128 * 1024 });
//! let (words, len) = read.encode().expect("page-aligned offset");
//! assert_eq!(&words[..len], &[1, 2048, 128 * 1024]);
//! assert_eq!(Request::decode(&words[..len]), Ok(read));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod abi;
pub mod prp;
#[cfg(feature = "alloc")]
pub mod slots;
