//! A QEMU guest's RAM, read from the host while the guest runs.
//!
//! QEMU can back a guest's RAM with a file (`-object
//! memory-backend-file,id=ram,mem-path=<file>,size=<RAM>,share=on -machine
//! memory-backend=ram`). Every byte the guest stores lands in that file, so a host process
//! that maps it sees guest memory without stopping the guest or running anything in it. This
//! crate is the small piece such a reader needs:
//!
//! - [`RamLayout`] turns guest-physical addresses into file offsets and back. The file holds
//!   the RAM in order, but the machine places it in two pieces: on QEMU's q35 machine
//!   ([`RamLayout::q35`]) RAM below 4 GiB stops where the PCI hole begins and the rest
//!   continues at 4 GiB.
//! - [`GuestRam`] (Linux) maps the file and hands out its words as `AtomicU64`s, at file
//!   offsets or guest-physical addresses, plus the pages that hold data, for scanning
//!   without allocating the file's holes.
//!
//! The mapping never changes the guest: the file is opened read-only and mapped private, so
//! even a store through the words would only change this process's copy of a page. The
//! `ferric-trace follow` command reads a trace kernel's host-visible rings this way
//! (`ferric_trace::host`). All the `unsafe` code is in the mapping module.
//!
//! # Examples
//!
//! ```
//! use ferric_guest_ram::RamLayout;
//!
//! // 4 GiB on q35: 2 GiB below the PCI hole, 2 GiB from 4 GiB up.
//! let layout = RamLayout::q35(4 << 30);
//! assert_eq!(layout.offset_of(0x10_0000, 4096), Some(0x10_0000));
//! assert_eq!(layout.offset_of(0x1_0000_0000, 4096), Some(2 << 30));
//! assert_eq!(layout.offset_of(0x9000_0000, 8), None, "the PCI hole");
//! assert_eq!(layout.phys_of(2 << 30), Some(0x1_0000_0000));
//! ```

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod layout;
#[cfg(target_os = "linux")]
mod map;

pub use layout::{FOUR_GIB, LayoutError, Q35_LOW_SPLIT, Q35_SPLIT_THRESHOLD, RamLayout};
#[cfg(target_os = "linux")]
pub use map::{GuestRam, GuestRamError, PAGE_BYTES};
