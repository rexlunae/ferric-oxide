# Developing for Ferric Oxide

Ferric Oxide does not yet have a complete set of development tools that run
on it. You develop on Linux (or, with some limits, macOS), build with
`cargo xtask`, and run the result in QEMU or copy it to a machine. This
chapter covers writing, porting, testing and measuring programs.

Before reading it, read [Building Ferric Oxide](./building-redox.md) and the
[Build System Reference](./build-system-reference.md). For work on the kernel
and the base system, also read [Implemented System](./ferric-oxide.md) and
[Validation and debugging](./ferric-testing.md).

Programs built for Ferric Oxide use the `x86_64-unknown-redox` (or
`aarch64-unknown-redox`) Rust target and relibc, the same target and C library
as Redox. A program that runs on Redox will usually build here too, but
system-level behavior differs: see [System Design](./system-design.md).
