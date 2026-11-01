# ferric-timed

Host-testable, `no_std` clock and deadline mechanisms for Ferric's Phase S
migration. The default build provides allocation-free time arithmetic,
calibration, monotonic publication and hardware one-shot programming helpers.
The `alloc` feature adds ordered monotonic/realtime timeout storage with
globally distinct cancellation tokens and earliest monotonic wakeup projection.

Expiry uses binary heaps that retain capacity, including when their last entry
is removed. It does not enter the allocator. Individual cancellation rebuilds
the affected heap in O(n); owner-wide cancellation filters both heaps in O(n).
Neither leaves stale entries. Returned payloads and their destructors remain
the caller's responsibility. A latest-deadline diagnostic performs a linear
scan; hardware rearming uses the constant-time earliest-deadline lookup.

`Nanos` is the wrapping `u64` hardware/scheduler scale. The timeout module's
`Timestamp` retains the full `u128` absolute-date range of the native time
scheme. Callers synchronize queue mutation and hardware rearming, obtain clock
snapshots and dispatch events outside queue locks. This crate does not read
hardware, run callbacks or make an unbounded kernel path preemptible.

```sh
cargo test --locked -p ferric-timed
cargo test --locked -p ferric-timed --features alloc
cargo clippy --locked -p ferric-timed --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p ferric-timed --all-features
cargo run --release -p ferric-timed --features alloc --example timeout_bench
```

The microbenchmark compares registration, no-due-event checks and expiration
against the former vector scan. It reports insertion/removal costs as well as
lookup benefits; it is not a kernel performance or tickless-boot measurement.
See the [system architecture](../../docs/src/ferric-architecture.md#timers-and-timekeeping)
and [Phase S completion order](../../docs/src/ferric-roadmap.md#completion-priority-and-order).
