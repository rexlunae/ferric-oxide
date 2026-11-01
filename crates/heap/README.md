# ferric-heap

`no_std` kernel heap-region ownership and growth around the crates.io
[`rlsf` TLSF allocator](https://docs.rs/rlsf/0.2.3/rlsf/struct.Tlsf.html), under its
MIT license option. The kernel uses this adapter for its global heap. The kernel
recipe installs the dependency notices from `crates/kernel/licenses/` into the
image's `/usr/share/licenses/kernel/`. Fixed
segregated free lists replace allocation/deallocation scans over a fragmented
address-ordered list. The old `linked_list_allocator` is a host-only development
dependency for the reproducible comparison, not a kernel runtime dependency.

`Heap` tracks mapped bytes separately from incorporated pool bytes. Contiguous
growth includes any unincorporated tail and allows one allocation to span multiple
mapping increments. The caller transfers exclusive mapped storage, preserves its
lifetime and pointer provenance, synchronizes access, and returns each allocation
with its exact original layout. APIs reject noncontiguous growth/reinitialization
without disturbing existing allocations.

The kernel retains its global spinlock and one-MiB mapping increments. TLSF bounds
local allocation/free bookkeeping; synchronization, physical-page allocation,
mapping and shootdowns remain unbounded. This does not make heap allocation safe
in an interrupt that might preempt the lock holder, nor establish a real-time
service bound. Pages are not unmapped or released by this adapter.

This shared crate also builds beside the cookbook's copied kernel tree. Its sole
runtime dependency version is local to its manifest, so the copy does not require
the host workspace's dependency inheritance.

```sh
cargo test --locked -p ferric-heap
cargo miri test --locked -p ferric-heap
cargo clippy --locked --all-targets -p ferric-heap -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p ferric-heap
cargo run --locked --release -p ferric-heap --example fragmentation_bench
```

Tests exercise alignment and live contents, unaligned initial regions, tiny
extensions/trailing bytes, coalescing across increments, failure ownership,
fragmentation and externally synchronized threads. The benchmark keeps small holes
separated by live barriers before a large usable tail and measures allocation/free
pairs at increasing hole counts. Setup is excluded. It measures backend bookkeeping
only, not SMP lock contention, kernel latency or native throughput.
