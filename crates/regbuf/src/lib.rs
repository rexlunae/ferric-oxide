//! The interface between the Ferric Oxide kernel's `regbuf` scheme and its user-space clients:
//! registered buffers and revocable zero-copy grants (roadmap Phase 2.2, design in
//! `io/registered-buffers-design.md` in the project files).
//!
//! An application opens a *region* ([`abi::REGION_PATH`]) and registers a page-aligned range of
//! its anonymous memory (private or `MAP_SHARED`) into it once ([`abi::Request::Register`]);
//! the kernel pins the pages. From the region it opens *grants* ([`abi::GrantSpec`]): a
//! read-only or read-write right to a byte range of the region, as a file descriptor of its
//! own. It passes a grant fd to
//! the scheme serving a request, which maps it with `mmap` and reads (and, for a read-write
//! grant, writes) the application's pages directly. The region fd is the owner's capability:
//! [`abi::Request::Revoke`] or closing it takes every mapping of the grant down, with a TLB
//! shootdown, before it returns.
//!
//! - [`abi`] has the scheme's name and number, its paths, and the encoding of each call. It is
//!   the one definition the kernel, bootstrap and clients share.
//! - `table` (with the `alloc` feature) is the kernel's per-region bookkeeping of grants and the
//!   mappings each one has made.
//! - [`bounce`] is the rule by which the kernel copies a writable bounce page back: only the
//!   bytes its holder changed.
//! - `budget` (with the `alloc` feature) is the kernel's accounting of pinned pages per user.
//!
//! # Examples
//!
//! ```
//! use ferric_regbuf::abi::{Access, GrantId, GrantSpec, Request};
//!
//! let spec = GrantSpec::new(Access::ReadWrite, 0x1000, 0x4000).expect("non-empty");
//! let mut buf = [0; GrantSpec::PATH_MAX];
//! let path = spec.encode_path(&mut buf);
//! assert_eq!(path, "grant/rw/1000/4000");
//! assert_eq!(GrantSpec::parse_path(path.as_bytes()), Ok(spec));
//!
//! let (words, len) = Request::Revoke(GrantId::new(7)).encode();
//! assert_eq!(Request::decode(&words[..len]), Ok(Request::Revoke(GrantId::new(7))));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod abi;
pub mod bounce;
#[cfg(feature = "alloc")]
pub mod budget;
#[cfg(feature = "alloc")]
pub mod table;
