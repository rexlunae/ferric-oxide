# Building Ferric Oxide

This page walks through building a Ferric Oxide image with the native Cargo
build and booting it in QEMU. It is the recommended way to build. The
reference for everything here, including its current limits, is
[Build and Configuration](./ferric-build.md).

## Prerequisites

The build machine needs the following. `./native_bootstrap.sh` (below)
installs everything except rustup; `cargo xtask` itself never runs a package
manager or `sudo`.

- [rustup](https://rustup.rs/). The repository's `rust-toolchain.toml` pins
  the nightly toolchain, and rustup installs it on first use.
- Git, curl, tar and Make.
- A native C/C++ compiler and SDK: Clang on Linux, or the Xcode Command Line
  Tools on macOS. Clang must support `--gcc-install-dir`; recent versions do.
- [NASM](https://nasm.us/), for x86-64 guests only (the kernel has an assembly
  trampoline).
- The programs recipes run while building packages from source: Autoconf,
  Automake, gettext's `autopoint`, Bison, CMake, Flex, M4, Texinfo's
  `makeinfo`, Meson, Ninja, `patch`, Perl, `pkg-config` and Python 3. There is
  no package server yet, so every application package is built from source.
- OpenSSL 3 headers, for the build-time Python some recipes use (for example
  libxcb through xcb-proto). OpenSSL 4 does not work with CPython 3.12.
- [QEMU](https://www.qemu.org/), only to run the image:
  `qemu-system-x86_64` or `qemu-system-aarch64`, with its UEFI firmware.

`./native_bootstrap.sh` installs all of these with Homebrew on macOS (after
`xcode-select --install`), or with `apt-get` or `dnf` (through `sudo`) on
Debian, Ubuntu and Fedora, and then runs `cargo xtask setup`. On macOS it also
prints the `PATH` line that puts Homebrew's keg-only Bison, Flex, Texinfo and
gettext ahead of the system's older copies. If you'd rather install them yourself, use the
package lists in the script and run `./native_bootstrap.sh --no-packages` or
`cargo xtask setup`. The [Podman build](./podman-build.md) is an optional
alternative that has these programs in its container image; the native build
does not need Podman.

The native build is developed on Linux and macOS, on both x86-64 and Apple
Silicon hosts. CI builds on Ubuntu 24.04.

## Get the source

```sh
git clone https://github.com/rexlunae/ferric-oxide
cd ferric-oxide
```

## Check and set up the toolchain

```sh
./native_bootstrap.sh
```

The script installs the build machine's packages (see
[Prerequisites](#prerequisites)) and then runs `cargo xtask setup`, which
first runs `cargo xtask doctor`. `doctor` lists the tools it finds; if any
required ones are missing it names all of them and stops. `setup` adds `rust-src` and `llvm-tools-preview` to the pinned
toolchain (and the prebuilt Redox-target standard library for x86-64),
installs `cbindgen` and `just` with Cargo if they are missing or outdated,
initializes the source submodules, and writes a starting `ferric.toml` if you
don't have one.

Commands build for your host's architecture by default: ARM64 on Apple
Silicon and x86-64 on Intel and AMD. To build for the other one, pass
`--arch x86_64` or `--arch aarch64` to *every* command, starting with setup:

```sh
./native_bootstrap.sh --arch x86_64
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
package manager from this repository, and builds the other applications from
their upstream sources. Images are written with Rust tools, without FUSE or `sudo`.

> 📝 **Note:** There is no Ferric package server yet, so application packages
> are built locally. Prebuilt packages distributed through
> [`fox`](./ferric-packaging.md) are in progress.

## Run it

```sh
cargo xtask run              # opens a QEMU window
cargo xtask run --headless   # serial console in your terminal
cargo xtask run --no-build   # boot the existing image without rebuilding
```

`run` builds the image first unless you pass `--no-build`. When the system
boots, log in as `user` with an empty password, or as `root` with the
password `password`. These are development credentials set in
`config/base.toml`; change them before exposing an image to anyone else. See
[Running in a Virtual Machine](./running-vm.md) for more options.

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
