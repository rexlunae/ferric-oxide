//! `ferric-blockd`: serves a stack of block layers (LUKS volumes, LVM logical volumes and byte
//! windows of disks) as a read-only scheme, for a file system daemon to mount (features.toml
//! options `fs.luks` and `fs.lvm`, off by default).
//!
//! The layers themselves are in `ferric-blocklayer` (`crates/blocklayer`), which reads the
//! formats through `ferric-luks` and `ferric-lvm`. This daemon adds, from the command line up:
//!
//! - [`spec`]: the command line, a scheme name and the steps that build the volume;
//! - [`assemble`]: running the steps over devices and key files ([`assemble::Sources`]);
//! - [`device`]: disks, partitions and image files as block devices;
//! - [`volume`]: the open handles of the served volume, and the one place an error becomes
//!   an errno;
//! - `scheme` (Redox only): the scheme handler, a thin layer from scheme calls to [`volume`];
//! - `crypttab` and `unlock` (Cargo feature `unlock`, features.toml `fs.unlock_at_boot`): the
//!   boot-time table of volumes to unlock and mount, and the `ferric-unlock` step that starts
//!   one `ferric-blockd` (and file system daemon) per line.
//!
//! Everything but `scheme` runs on the host, so `cargo test` covers it against the images in
//! `crates/blocklayer/tests/fixtures/`. A file system daemon reads the volume like a disk:
//! `ext4d /scheme/NAME ext4` mounts ext4 on LUKS, for example.
//!
//! # Examples
//!
//! ```no_run
//! use ferric_blockd::assemble::assemble;
//! use ferric_blockd::spec::Spec;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let spec = Spec::parse(["root", "dev:disk.img", "luks:key"].map(String::from))?;
//! let volume = assemble(&spec.steps, &mut ferric_blockd::Files)?;
//! println!("{} bytes", ferric_blocklayer::BlockDevice::size(&volume));
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(not(any(feature = "luks", feature = "lvm")))]
compile_error!("ferric-blockd needs the luks or lvm feature (features.toml fs.luks or fs.lvm)");

pub mod assemble;
#[cfg(feature = "unlock")]
pub mod crypttab;
pub mod device;
#[cfg(target_os = "redox")]
pub mod scheme;
pub mod spec;
#[cfg(feature = "unlock")]
pub mod unlock;
pub mod volume;

use std::io;
use std::path::Path;

use zeroize::Zeroizing;

use crate::assemble::Sources;
use crate::device::FileDevice;

/// Devices and key files from the file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct Files;

impl Sources for Files {
    type Device = FileDevice;

    fn device(&mut self, path: &Path) -> io::Result<FileDevice> {
        FileDevice::open(path)
    }

    fn passphrase(&mut self, path: &Path) -> io::Result<Zeroizing<Vec<u8>>> {
        std::fs::read(path).map(Zeroizing::new)
    }
}
