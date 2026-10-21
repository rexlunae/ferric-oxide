//! The Ferric Oxide wasm code cache's formats (roadmap Phase W, design in
//! `wasm/wasm-design.md` in the project files, section 2).
//!
//! Native code compiled from a wasm module is kept in a persistent, system-wide cache. The
//! compiler service (`wasmc`) produces an entry, the kernel seals it with a MAC under a key
//! only it holds, the cache daemon (`codecached`) stores it, and before mapping the code
//! executable the kernel copies the entry into its own memory and checks the MAC. This
//! crate is the one definition of what they exchange; the kernel, the daemon, the compiler
//! service and `fox` all use it.
//!
//! - [`key`]: the [`CacheKey`](key::CacheKey) an entry is filed under: the module's BLAKE3
//!   (the same value as its pkgar entry hash), the compiler identity, the exact CPU features,
//!   the codegen options and the ABI and verifier versions. Any change is a different key,
//!   so stale code is never loaded.
//! - [`entry`]: the entry format, a fixed header and four sections (text, read-only data,
//!   the runtime's metadata, relocations), parsed without copying and with every offset
//!   checked. The encoding is canonical, so one entry has one byte representation.
//! - [`mac`]: the MAC: keyed BLAKE3 over a domain string, the key and every section, each
//!   with its length.
//! - [`reloc`]: the relocation records, and how the kernel applies them to the text after
//!   checking the MAC (the MAC covers the unrelocated text).
//!
//! # Examples
//!
//! ```
//! use ferric_codecache::entry::{Entry, EntryBuilder};
//! use ferric_codecache::key::{Arch, CacheKey, CpuFeatures, x86};
//! use ferric_codecache::mac::MacKey;
//!
//! let mut key = CacheKey::example();
//! key.cpu = CpuFeatures::new(Arch::X86_64, x86::AVX2 | x86::BMI2);
//!
//! // The compiler service builds an unsealed entry...
//! let builder = EntryBuilder::new(key)
//!     .text(&[0xc3])
//!     .metadata(b"wasmtime metadata");
//! let mut buffer = [0; 256];
//! let len = builder.write(&mut buffer)?;
//! let bytes = &mut buffer[..len];
//!
//! // ...the kernel seals it...
//! let secret = MacKey::from_bytes([7; 32]);
//! ferric_codecache::entry::seal(bytes, &secret)?;
//!
//! // ...and checks it again before mapping the code.
//! let entry = Entry::parse(bytes)?;
//! entry.verify(&secret)?;
//! assert_eq!(entry.text(), &[0xc3]);
//! assert_eq!(entry.key(), &key);
//!
//! // A changed byte of code breaks the MAC.
//! let text_at = ferric_codecache::entry::HEADER_SIZE.next_multiple_of(16);
//! bytes[text_at] ^= 1;
//! assert!(Entry::parse(bytes)?.verify(&secret).is_err());
//! # Ok::<(), ferric_codecache::entry::EntryError>(())
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod entry;
pub mod key;
pub mod mac;
pub mod reloc;

/// Writes `bytes` into `out` at `*at` and advances `*at`; callers size `out` exactly.
pub(crate) fn put(out: &mut [u8], at: &mut usize, bytes: &[u8]) {
    out[*at..*at + bytes.len()].copy_from_slice(bytes);
    *at += bytes.len();
}

/// `N` bytes of `bytes` from `at`; callers have checked the length.
pub(crate) fn array<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}
