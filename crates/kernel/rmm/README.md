# Redox Memory Management

This is a Rust crate to provide abstractions for hardware memory management. It
also contains a mechanism for testing memory management with software emulation.

`BumpAllocator::new` is unsafe: the caller supplies exclusive ownership of
page-aligned, disjoint memory areas with writable physical mappings. Zero-frame
and overflowing requests fail without consuming frames. Skipping past the end
of all areas creates an exhausted allocator. Bump allocations cannot be freed
individually; the buddy allocator takes over after boot.

From the repository root, run the host frame-allocation and address tests with:

```sh
cargo test --locked --manifest-path crates/kernel/Cargo.toml -p rmm --features std --lib
```

The `std` feature enables the software memory model. These tests cover frame
allocation and accounting; they do not execute privileged instructions or test
hardware translation invalidation. This package retains its upstream name and
location in the separate kernel workspace.
