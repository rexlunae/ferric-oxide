//! `ext4d`: a read-only ext4 file system for Ferric Oxide, served as a scheme the way RedoxFS
//! serves its own (features.toml option `fs.ext4`, off by default).
//!
//! The pieces, from the disk up:
//!
//! - [`device`]: where the file system's bytes come from. A [`device::Window`] is a byte
//!   range of anything that reads at an offset ([`device::ReadAt`]): a whole disk, a
//!   partition inside one, or an image file. Later, LUKS (`crates/luks`) and LVM
//!   (`crates/lvm`) volumes plug in here by implementing [`device::ReadAt`].
//! - [`superblock`]: the few superblock fields the daemon needs and the `ext4-view` crate does
//!   not expose (block size and counts, for `statvfs`), read straight from the on-disk
//!   format.
//! - [`fs`]: the open-file table on top of the `ext4-view` crate, which does the real work
//!   of reading ext4 (extents, hashed directories, checksums, the journal). Every call
//!   that would change the file system fails with [`Error::ReadOnly`] (`EROFS`), except
//!   what `write` does on a volume mounted with it.
//! - `journal` (Cargo feature `journal-replay`, features.toml `fs.ext4_journal_replay`):
//!   replays a volume's journal onto the disk before it is served, with the `ferric-jbd2`
//!   crate. Without it, a volume that needs recovery is read as the replay would leave
//!   it, with the committed journal blocks laid over it in memory, and nothing is
//!   written.
//! - `write` (Cargo feature `write`, features.toml `fs.ext4_write`): changes the volume
//!   through its journal, one JBD2 transaction per change, with `ferric-jbd2`'s writer. So
//!   far it sets an inode's times, mode and owner, writes file data (allocating blocks),
//!   truncates files and creates regular ones, which the scheme does on a volume
//!   `Fs::new_writable` mounted. `lookup` finds the inode a path names, which
//!   `ext4-view` does not say.
//! - [`error`]: [`Error`], and the one place it becomes an errno.
//! - `scheme` (Redox only): the scheme handler, a thin layer from scheme calls to [`fs`].
//!
//! Everything but `scheme` runs on the host, so `cargo test` covers it against ext4 images
//! made by `mke2fs` (`tests/fixtures/`).
//!
//! # Specification
//!
//! The ext4 on-disk format is described in "Ext4 Disk Layout" on the ext4 wiki at kernel.org
//! (<https://ext4.wiki.kernel.org/index.php/Ext4_Disk_Layout>). [`superblock`] is written
//! from its "The Super Block" section. Everything else is read by the `ext4-view` crate
//! (MIT or Apache-2.0). No code here comes from the Linux kernel.
//!
//! # Examples
//!
//! ```no_run
//! use ext4d::device::Window;
//! use ext4d::fs::{Fs, OpenMode};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let image = std::fs::File::open("disk.ext4")?;
//! let mut fs = Fs::new(Window::whole(image)?)?;
//! let root = fs.root()?;
//! let file = fs.open_as(Some(root), "hello.txt", OpenMode::read(), 1000, 100)?;
//! let mut buf = [0; 64];
//! let n = fs.read(file, 0, &mut buf)?;
//! assert_eq!(&buf[..n], b"hello from ext4\n");
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod device;
pub mod error;
pub mod fs;
pub(crate) mod inode;
pub mod journal;
pub mod lookup;
#[cfg(target_os = "redox")]
pub mod scheme;
pub mod superblock;
#[cfg(feature = "write")]
pub mod write;

pub use error::Error;
