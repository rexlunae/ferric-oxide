# WebAssembly Interfaces

Ferric Oxide plans to run WebAssembly programs as ordinary user-space processes,
compiled ahead of time and cached system-wide. **Only the shared interface
crates exist so far.** There is no wasm runtime, compiler service, cache daemon,
`kernel.codecache` scheme or recipe yet, and nothing in an image uses these
crates. They are host-tested so that the runtime, the kernel and the tools that
come later share one definition of what they exchange.

## `ferric:sys@1` imports: `crates/wasm-abi`

`ferric-wasm-abi` defines the import module `ferric:sys@1`, the system interface
a Ferric wasm program links against (WASI preview 1 is to be layered on top of
it). The crate holds:

- the import names and wasm signatures (`imports::Import`), which a runtime
  checks at instantiation so a module built for a later ABI fails at load;
- the result convention: an `i64` that is either a value or a negated errno
  (`errno`), with the ABI's own errno and flag values rather than Redox's,
  which may change upstream (`errno`, `flags`);
- bounds checks for guest pointers, lengths and page-aligned mapping windows
  (`memory`), and the byte layouts of the structures imports write into guest
  memory (`layout`);
- with the `alloc` feature, the per-instance descriptor table (`handles`).

The Rustdoc of each module is the reference for its encoding. The ABI is
versioned by the module name; imports may be appended to version 1 but not
reordered, renamed or retyped.

## Code cache entries: `crates/codecache`

`ferric-codecache` defines what the planned compiler service, cache daemon and
kernel exchange about compiled code:

- `key`: the cache key (module BLAKE3, compiler identity, exact CPU features,
  codegen options, ABI and verifier versions) and its digest, which names the
  entry file;
- `entry`: the versioned entry format, a fixed header and four 16-byte-aligned
  sections (text, read-only data, runtime metadata, relocations), with a
  canonical encoding and a parser that checks every offset without allocating;
- `mac`: the keyed-BLAKE3 MAC over the key and every section, compared in
  constant time;
- `reloc`: relocation records and their application to a private copy of the
  text after the MAC is checked.

The crate is `no_std` and needs no allocator. Its tests include fixed byte
layouts, a recomputed MAC vector, single-bit-change coverage of a whole entry
and a deterministic mutation loop over the parser; there is no `cargo fuzz`
target.

## Checks

Both crates are host workspace members. CI runs their tests, Clippy with
warnings denied and strict Rustdoc; `ferric-wasm-abi` is also checked with
`--all-features` (see [Validation and debugging](./ferric-testing.md)).
