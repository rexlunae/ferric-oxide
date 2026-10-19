//! Typed kernel trace events, their compact binary record format, and the lock-free per-CPU
//! rings the Ferric Oxide kernel writes them to (roadmap Phase T).
//!
//! The pieces, and how they fit:
//!
//! - [`Level`] and [`Subsystem`] classify events, in the spirit of `tracing-core`'s level and
//!   target: the kernel compiles each subsystem's events in or out and sets a maximum level at
//!   build time.
//! - [`events!`](crate::events!) declares events as `const` [`Event`]s together with the [`Catalog`] that lists
//!   them. The catalog is checked when the crate using the macro is compiled: ids are dense and
//!   unique, names are unique, and no event has more than [`MAX_ARGS`] arguments. [`kernel`] is the
//!   kernel's catalog, shared by the kernel and every decoder.
//! - [`record`] is the binary format: a two-word header (length, argument count, event id;
//!   timestamp) followed by one 64-bit word per argument. The kernel never formats text.
//! - [`ring::Ring`] is a single-producer, single-consumer ring of 64-bit words: the CPU that owns
//!   it pushes records, one reader drains them. Its counters live either in the ring or, in
//!   the shared layout the kernel maps into readers, in a header ahead of the data, which a
//!   [`ring::View`] reads in place. It is built from atomics only, so this crate has no
//!   `unsafe` code.
//! - [`select()`] parses event selections (`all`, a subsystem name, an event name) into an
//!   [`EventSet`], for the boot environment and the trace scheme.
//! - [`clock`] names the clock record timestamps come from (an invariant TSC, the KVM clock or
//!   the HPET) and holds the rule the kernel picks it by.
//! - [`filter`] holds per-event argument filters (`nr == 3 && a < 0x1000`) and the lock-free
//!   slot the kernel checks them in.
//! - [`abi`] is the `kernel.trace` scheme's interface: its name, paths, commands and listing
//!   format.
//! - [`crash`] is the crash dump a panic writes to reserved memory for the next boot to
//!   read: its layout, a writer that copies the newest records of each ring, and a parser.
//! - `config` (feature `config`) is the initfs `trace.toml` that bootstrap applies before
//!   `init` starts.
//!
//! # Examples
//!
//! Push two records into a ring and decode them again:
//!
//! ```
//! use core::sync::atomic::AtomicU64;
//! use ferric_trace::{kernel, record::Records, ring::Ring};
//!
//! let words: [AtomicU64; 64] = core::array::from_fn(|_| AtomicU64::new(0));
//! let ring = Ring::new(&words[..]).expect("64 is a power of two");
//! ring.push(kernel::SYSCALL_ENTER.id(), 100, &[20, 0, 0, 0, 0]).expect("room");
//! ring.push(kernel::SYSCALL_EXIT.id(), 150, &[20, 7]).expect("room");
//!
//! let mut buf = [0u8; 512];
//! let len = ring.read(&mut buf).expect("512 bytes hold any record");
//! let names: Vec<&str> = Records::new(&buf[..len])
//!     .map(|r| kernel::CATALOG.get(r.expect("valid").event()).expect("known").name())
//!     .collect();
//! assert_eq!(names, ["syscall.enter", "syscall.exit"]);
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(any(feature = "alloc", test))]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod abi;
pub mod clock;
#[cfg(feature = "config")]
pub mod config;
pub mod crash;
mod event;
pub mod filter;
pub mod kernel;
mod level;
pub mod record;
pub mod ring;
mod select;

pub use event::{Catalog, Event, EventId, MAX_ARGS, MAX_EVENTS, Subsystem};
pub use level::{Level, ParseLevelError};
pub use select::{EventSet, SelectError, select};
