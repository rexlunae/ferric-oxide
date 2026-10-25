# Aligned byte buffers

`ferric-aligned-buffer` provides fallible, owned, zeroed byte slices for `no_std`
consumers with `alloc`. The kernel uses it for aligned CPU-state storage; it has
no hardware or operating-system dependencies.

`AlignedBuffer::<ALIGN>::try_zeroed(len)` validates the layout and reports invalid
layouts or allocation exhaustion with `AllocError`. It retains the exact layout
for deallocation. Empty buffers have an aligned, non-null dangling pointer and do
not allocate. `try_clone` reports allocation failure; `Clone` uses the global
allocation-error handler on failure. All bytes are initialized and accessible
through slice operations.

From the repository root:

```sh
cargo test --locked -p ferric-aligned-buffer
cargo clippy --locked -p ferric-aligned-buffer --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --locked -p ferric-aligned-buffer --no-deps
cargo miri test --locked -p ferric-aligned-buffer
```

Miri runs the same allocation, clone and destruction paths as the kernel; it
checks the allocation layout and pointer invariants without booting the OS.
