# Ferric Oxide kernel

The Ferric Oxide microkernel, derived from Redox OS. Internal ABI, package and
target names retain their Redox identity for compatibility.

[![docs](https://img.shields.io/badge/docs-master-blue.svg)](https://docs.rs/redox_syscall/latest/syscall/)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

## Device trees

The kernel uses `ferric-devicetree` (`../devicetree`) for borrowed device-tree
access and MMIO translation. It shares the published crates.io parser with the
bootloader. See its README for host tests and `../../docs/src/ferric-testing.md`
for required image checks.

## Requirements

* [`nasm`](https://nasm.us/) needs to be available on the PATH at build time.

## Building The Documentation

From this directory, using the declared nightly toolchain:

```sh
RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps \
  --target targets/x86_64-unknown-kernel.json \
  -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem \
  -Z json-target-spec
```

## Debugging

### QEMU

Running [QEMU](https://www.qemu.org) with the `-s` flag will set up QEMU to listen on port `1234` for a GDB client to connect to it. To debug the Ferric Oxide kernel run.

```sh
make qemu gdb=yes
```

This will start a virtual machine with and listen on port `1234` for a GDB or LLDB client.

### GDB

If you are going to use [GDB](https://www.gnu.org/software/gdb/), run these commands to load debug symbols and connect to your running kernel:

```
(gdb) symbol-file build/kernel.sym
(gdb) target remote localhost:1234
```

### LLDB

If you are going to use [LLDB](https://lldb.llvm.org/), run these commands to start debugging:

```
(lldb) target create -s build/kernel.sym build/kernel
(lldb) gdb-remote localhost:1234
```

After connecting to your kernel you can set some interesting breakpoints and `continue`
the process. See your debuggers man page for more information on useful commands to run.

## Notes

- Always use `foo.get(n)` instead of `foo[n]` and try to cover for the possibility of `Option::None`. Doing the regular way may work fine for applications, but never in the kernel. No possible panics should ever exist in kernel space, because then the whole OS would just stop working.

- If you receive a kernel panic in QEMU, use `pkill qemu-system` to kill the frozen QEMU process.

## How To Contribute

To learn how to contribute to this system component you need to read the following document:

- [Contributing to Ferric Oxide](../../CONTRIBUTING.md)
- [AI contribution policy](../../AI_POLICY.md)

## Development

The kernel is part of the Ferric Oxide monorepo and keeps a separate workspace
and lockfile. Start with [build and configuration](../../docs/src/ferric-build.md)
and [validation and debugging](../../docs/src/ferric-testing.md). From the
repository root, `make all` builds an image and `make qemu` boots it. A direct
kernel build uses `make -C crates/kernel ARCH=x86_64` and the custom target.

Host tests of CPU masks, CPU publication and queue records run with:

```sh
cargo test --locked --manifest-path crates/kernel/tests/Cargo.toml --all-features
cargo test --locked --manifest-path crates/kernel/Cargo.toml -p rmm --features std --lib
```

These tests do not boot the kernel or execute interrupt/context-switch assembly.

## Funding - _Unix-style Signals and Process Management_

This project is funded through [NGI Zero Core](https://nlnet.nl/core), a fund established by [NLnet](https://nlnet.nl) with financial support from the European Commission's [Next Generation Internet](https://ngi.eu) program. Learn more at the [NLnet project page](https://nlnet.nl/project/RedoxOS-Signals).

[<img src="https://nlnet.nl/logo/banner.png" alt="NLnet foundation logo" width="20%" />](https://nlnet.nl)
[<img src="https://nlnet.nl/image/logos/NGI0_tag.svg" alt="NGI Zero Logo" width="20%" />](https://nlnet.nl/core)
