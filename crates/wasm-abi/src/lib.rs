//! The `ferric:sys` WebAssembly import ABI (roadmap Phase W, design in `wasm/wasm-design.md`
//! in the project files, section 3).
//!
//! A wasm program on Ferric Oxide reaches the system only through imports. `ferric:sys@1` is
//! a thin, versioned mirror of the Redox system-call surface: files and schemes through
//! `openat` on directory fds, scheme verbs through `call`, file mappings into windows of linear
//! memory, process and signal calls, and brokered rings. The runtime (`wasmrun`) implements
//! these imports, and WASI preview 1 is built on top of them. This crate is the one
//! definition the runtime and guest-side bindings share; it holds no system calls itself.
//!
//! - [`imports`] names every import, with its parameter and result types, so the runtime can
//!   check a module's imports before instantiating it and register one host function each.
//! - [`errno`] is how results are returned: a non-negative value, or `-errno`.
//! - [`memory`] checks guest `(pointer, length)` pairs against linear memory, page-aligned
//!   windows and packed string lists, without overflow. Every pointer a guest passes is an
//!   offset into its memory 0, never a host address.
//! - [`flags`] holds the guest-facing open, mapping and wait flags. They are this ABI's own
//!   stable values; the runtime converts them to Redox's.
//! - [`layout`] is the byte layout of the structures imports write into linear memory.
//! - `handles` (with the `alloc` feature) is the per-instance table that maps guest fd numbers
//!   to the runtime's own handles, so a guest never names the runtime's descriptors.
//!
//! **Types.** Pointers and lengths are always `i64`, for wasm32 and wasm64 modules alike (a
//! wasm32 guest zero-extends), so one signature table serves both. Fds and small flags are
//! `i32`. Every import that can fail returns `i64`.
//!
//! # Examples
//!
//! ```
//! use ferric_wasm_abi::errno::{Errno, decode_result, encode_result};
//! use ferric_wasm_abi::imports::{Import, ValType};
//! use ferric_wasm_abi::memory::GuestRange;
//!
//! let read = Import::from_name("read").expect("a ferric:sys import");
//! assert_eq!(read.params(), &[ValType::I32, ValType::I64, ValType::I64]);
//! assert_eq!(read.results(), &[ValType::I64]);
//!
//! // A guest buffer must lie inside its linear memory.
//! let memory_len = 64 * 1024;
//! assert!(GuestRange::new(0xff00, 0x100).check(memory_len).is_ok());
//! assert_eq!(GuestRange::new(0xff00, 0x101).check(memory_len), Err(Errno::EFAULT));
//!
//! assert_eq!(decode_result(encode_result(Err(Errno::EBADF))), Err(Errno::EBADF));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod errno;
pub mod flags;
#[cfg(feature = "alloc")]
pub mod handles;
pub mod imports;
pub mod layout;
pub mod memory;

/// The import module name every `ferric:sys` function is imported from.
pub const MODULE: &str = "ferric:sys@1";

/// The ABI version, returned by the `abi_version` import and part of the code cache key.
pub const ABI_VERSION: u32 = 1;
