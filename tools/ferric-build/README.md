# ferric-build

`cargo xtask` is the native entry point for the existing recipe and image build.
It keeps host, kernel, bootloader and userspace Cargo workspaces separate.

```sh
cargo xtask doctor
cargo xtask setup
cargo xtask build
cargo xtask run --headless
```

Setup adds `rust-src`, `llvm-tools-preview` and the Redox standard library to the
repository's pinned rustup toolchain, installs missing or outdated `cbindgen` and `just` with
Cargo, initializes the source submodules and creates `ferric.toml` if absent.
It never invokes Homebrew, another system package manager, or sudo. Install a
native C/C++ compiler and SDK (Xcode Command Line Tools on macOS), NASM and QEMU
using your preferred method. QEMU is required only for `run`.

The initial `native` image builds the local kernel, UEFI bootloader, relibc,
base services/drivers and RedoxFS; applications use existing binary packages.
The feature catalog also adds the selected package manager. A first build needs
network access. Existing `ferric.toml` files are preserved. Cargo commands
explicitly select x86-64, native LLVM and image writing without FUSE; Make
assignments supplied after `--` can override them:

```sh
cargo xtask build -- REPO_BINARY=0
cargo xtask run -- QEMU_MEM=4096 kvm=no
cargo xtask run --no-build --headless
cargo xtask --dry-run build
cargo xtask --podman build
```

`--podman` opts into the inherited Linux recipe toolchain and requires a working
Podman installation. Existing Make targets remain available for component work,
including `make r.kernel` and `make menuconfig`. Native LLVM is currently for
`x86_64-unknown-redox`; see the [build chapter](../../docs/src/ferric-build.md)
for its sysroot, driver adapter, ABI runtime requirements and limitations.

Native assembly uses Clang already on `PATH` and LLVM tools from rustup. It does
not build GCC, LLVM or Rust compilers. Existing target GCC runtime libraries and
C++ headers remain necessary for the current Rust and package ABI. They are
extracted from the upstream toolchain archive without installing host GCC
executables. The `recipe` toolchain remains available for GCC-specific recipes.

Run the offline toolchain regression tests with
`bash scripts/ferric-ci/test-native-toolchain.sh`, and the CLI tests with
`cargo test --locked -p ferric-build`. The installer has separate rootless-image
ownership tests. A full image build and a QEMU boot are still needed to validate
changes that affect the running OS.
