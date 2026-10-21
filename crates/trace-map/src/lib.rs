//! Maps a CPU's kernel trace ring read-only into this process, on Redox.
//!
//! The `kernel.trace` scheme lets a reader `mmap` a `cpu/<n>` descriptor instead of reading
//! it ([`ferric_trace::abi`], "Mapping a ring"): [`CPU_MAP_BYTES`] from offset 0, `PROT_READ`
//! and `MAP_SHARED` only. `Mapping::ring` makes that mapping and `Mapping::words` hands
//! out its words, which [`ferric_trace::ring::View`] reads in place. Consuming what was read
//! is a `commit <end>` written to the same descriptor
//! ([`CpuCommand::Commit`](ferric_trace::abi::CpuCommand::Commit)), which this crate leaves
//! to the caller.
//!
//! The unsafe code of a mapping (making it, viewing it as atomics, unmapping it) is kept
//! here so that its users, the `ferric-trace` tool and the trace smoke test, stay safe Rust.
//! Off Redox the crate is empty.
//!
//! # Examples
//!
//! ```no_run
//! # #[cfg(target_os = "redox")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::os::fd::AsRawFd;
//!
//! let file = std::fs::File::open("/scheme/kernel.trace/cpu/0")?;
//! let fd = usize::try_from(file.as_raw_fd())?;
//! let map = ferric_trace_map::Mapping::ring(fd).map_err(|e| std::io::Error::from_raw_os_error(e.errno))?;
//! let view = ferric_trace::ring::View::new(map.words())?;
//! println!("{} words pending", view.head() - view.tail());
//! # Ok(())
//! # }
//! # #[cfg(not(target_os = "redox"))]
//! # fn main() {}
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "redox")]
mod redox;

#[cfg(target_os = "redox")]
pub use redox::Mapping;

pub use ferric_trace::abi::CPU_MAP_BYTES;
