# Building Ferric Oxide

This page walks through building a Ferric Oxide image with the native Cargo
build and booting it in QEMU. It is the recommended way to build. The
reference for everything here, including its current limits, is
[Build and Configuration](./ferric-build.md).

## Prerequisites

Install these with your usual tools first. The build never runs a system
package manager, Homebrew or `sudo` for you.

- [rustup](https://rustup.rs/). The repository's `rust-toolchain.toml` pins
  the nightly toolchain, and rustup installs it on first use.
- Git, curl, tar and Make.
- A native C/C++ compiler and SDK: Clang on Linux, or the Xcode Command Line
  Tools on macOS. Clang must support `--gcc-install-dir`; recent versions do.
- [NASM](https://nasm.us/), for x86-64 guests only (the kernel has an assembly
  trampoline).
- [QEMU](https://www.qemu.org/), only to run the image:
  `qemu-system-x86_64` or `qemu-system-aarch64`, with its UEFI firmware.

The native build is developed on Linux and macOS, on both x86-64 and Apple
Silicon hosts. CI builds on Ubuntu 24.04.

## Get the source

```sh
git clone https://github.com/rexlunae/ferric-oxide
cd ferric-oxide
```

## Check and set up the toolchain

```sh
cargo xtask doctor
cargo xtask setup
```

`doctor` lists the tools it finds and stops at the first required one that is
missing. `setup` adds `rust-src` and `llvm-tools-preview` to the pinned
toolchain (and the prebuilt Redox-target standard library for x86-64),
installs `cbindgen` and `just` with Cargo if they are missing or outdated,
initializes the source submodules, and writes a starting `ferric.toml` if you
don't have one. `./native_bootstrap.sh` is an alias for `cargo xtask setup`.

Commands build for your host's architecture by default: ARM64 on Apple
Silicon and x86-64 on Intel and AMD. To build for the other one, pass
`--arch x86_64` or `--arch aarch64` to *every* command, starting with setup:

```sh
cargo xtask --arch x86_64 setup
cargo xtask --arch x86_64 build
cargo xtask --arch x86_64 run
```

## Build the image

```sh
cargo xtask build
```

The first build downloads Rust components, target runtime libraries, Cargo
dependencies and application packages, so it needs a network connection and
takes a while. The image appears under `build/<arch>/<config>/`:
`harddrive.img` (a persistent disk) for x86-64, and `redox-live.iso` (a
RAM-backed live image) for ARM64.

The starting configuration selects the `native` image, which builds the
kernel, UEFI bootloader, relibc, base services and drivers, RedoxFS and the
package manager from this repository, and installs other applications as
prebuilt packages. Images are written with Rust tools, without FUSE or `sudo`.

> 📝 **Note:** Prebuilt application packages and the target runtime libraries
> currently still come from Redox's servers. Replacing those downloads with
> packages built and distributed through [`fox`](./ferric-packaging.md) is in
> progress.

## Run it

```sh
cargo xtask run              # opens a QEMU window
cargo xtask run --headless   # serial console in your terminal
cargo xtask run --no-build   # boot the existing image without rebuilding
```

`run` builds the image first unless you pass `--no-build`. When the system
boots, log in as `user` with an empty password, or as `root` with the
password `password`. See [Running in a Virtual Machine](./running-vm.md) for
more options.

## Change what gets built

Your choices live in `ferric.toml`, which is not tracked by git. Edit them
with the menu or the CLI:

```sh
make menuconfig
cargo run -q -p ferric-config -- set image.config=desktop
cargo run -q -p ferric-config -- explain kernel.iommu
```

Every option, its default and its dependencies are in `features.toml`. See
[Configuration Settings](./configuration-settings.md) for an overview and
[Build and Configuration](./ferric-build.md) for the details.

Settings for a single command go after `--`, as Make assignments:

```sh
cargo xtask build -- REPO_BINARY=0        # build applications from source too
cargo xtask run -- QEMU_MEM=4096 kvm=no   # more memory, no acceleration
cargo xtask --dry-run build               # print what would run
```

## Updating

Pull the latest `main` and build again. The build regenerates its
configuration and rebuilds the recipes whose inputs changed.

```sh
git pull
cargo xtask build
```

If a build breaks after an update, see
[Troubleshooting the Build](./troubleshooting.md), and
[report it](./creating-proper-bug-reports.md) if the problem persists.
