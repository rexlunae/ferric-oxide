# Avoiding Panics

A panic in the kernel stops the whole machine, so kernel code must not panic
on anything a user, a driver or a device can cause. Return an error instead,
and validate every index, length and address that comes from outside the
kernel. The kernel's index-hardening helpers (`crates/nospec/`) and the lint
ratchet on `indexing_slicing`, `arithmetic_side_effects` and `unwrap_used`
exist for this reason.

Drivers and other services should panic only when correct operation is no
longer possible, by calling `panic!()` deliberately. Even then, prefer
reporting the error and exiting: a user-space service can be restarted, but
only if it fails cleanly.

See the [kernel README](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/README.md)
and [Memory and CPU Protection](./ferric-memory.md) for kernel-specific
guidance.
