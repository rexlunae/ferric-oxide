# What the Build Does

This page explains what each build command does, from `cargo xtask` down to
the Make targets it runs. (Read the [Build System Reference](./build-system-reference.md)
for the context of each Make command.)

- [cargo xtask](#cargo-xtask)
- [Configuration](#configuration)
- [Toolchain](#toolchain)
- [Build System](#build-system)
- [Recipes](#recipes)
- [QEMU](#qemu)

## cargo xtask

### cargo xtask setup

- Check for rustup, Cargo, Clang, Git, curl, tar, Make (and NASM for x86-64).
- Add `rust-src` and `llvm-tools-preview` to the pinned toolchain, and the
  prebuilt Redox-target standard library for x86-64.
- Install or update `cbindgen` and `just` with Cargo.
- Initialize the source submodules.
- Write a starting `ferric.toml` (the `native` image, native LLVM toolchain,
  no Podman, no FUSE) if there isn't one.

`./native_bootstrap.sh` first installs the build machine's packages with
Homebrew, `apt-get` or `dnf`, then runs the same thing. The older
`./podman_bootstrap.sh` installs Podman and other host packages instead; see
[Building with Podman](./podman-build.md).

### cargo xtask build

- Select the guest architecture (`--arch`, or the host's) and run
  `make qemu-image` for it, with the native toolchain and rootless image
  writing unless `--podman` is given.
- Make then does everything below: generates the configuration, prepares the
  toolchain, builds the recipes and writes the image.

### cargo xtask run

- The same as `build`, then boot the image in QEMU (`make qemu`).
  `--no-build` skips straight to booting (`make qemu-run`).

## Configuration

On every Make invocation, `mk/ferric.mk` runs `ferric-config` to resolve
`ferric.toml` against the catalog in `features.toml` and any overrides on the
command line. It writes `build/ferric/config.mk` (Make settings),
`build/ferric/filesystem.toml` (image contents) and
`build/ferric/kernel-config.toml` (kernel CPU policy), and touches the recipes
whose settings changed so they rebuild. See [Build and Configuration](./ferric-build.md#how-settings-reach-components).

## Toolchain

### make prefix

- With the native toolchain (the default), assemble the target sysroot under
  `prefix/llvm/<arch>-unknown-redox/`: target headers, libraries and startup
  objects extracted from the prebuilt GCC toolchain archive (its Linux compiler
  binaries are never run), a Clang driver adapter for the Redox target, and
  relibc built from `crates/relibc/`.
- With the inherited recipe toolchain (`build.toolchain = "recipe"`, the
  default with Podman), download the prebuilt Rust and GCC cross compilers.
- The toolchain archive is still downloaded from Redox's server; replacing it
  is in progress.

### make prefix (after relibc changes)

- The sysroot tracks relibc's sources, so changes to relibc are rebuilt into
  it automatically before recipes that depend on it.

## Build System

### make pull

- Run `git pull` and remove the filesystem-tools tag so the host tools are
  rebuilt. It doesn't update submodules; run
  `git submodule update --init --recursive` for that.

### make all (first execution)

- Prepare the toolchain (if `make prefix` was not executed before).
- Fetch the sources of the recipes in your image configuration. Core components (kernel, bootloader, relibc, base, RedoxFS, installer, `fox`) come from this repository.
- Cross-compile recipes for the target, or download prebuilt packages when `build.repo_binary` is on.
- Package each recipe as a signed `pkgar` archive, with a signed `fox` manifest when `packages.signed_manifests` is on.
- Install the packages into a RedoxFS disk image, without FUSE in the native build.
- With `packages.manager = "fox"` (the default), adopt the installed packages with the host `fox`, sync the optional package set, and write the image SBOM to `image.cdx.json`.

### make all (second execution and next)

If the `build/$ARCH/$CONFIG/repo.tag` file is up to date, it won't do anything. If the `repo.tag` file is missing it will work like the `make rebuild` command.

### make all (Podman environment, first execution)

- Build the Debian-based build container.
- Install the Rust and cross toolchains (inside the container).
- Install the recipe build tools (inside the container).
- Download the sources of the recipes specified on your filesystem configuration.
- Cross-compile recipes for the target (inside the container).
- Package recipe binaries as `pkgar` files.
- Install recipe packages in the QEMU virtual disk formatted with RedoxFS.

### make rebuild

- Verify source changes on recipes (if available, download them) or if a new recipe was added to the filesystem configuration.
- Cross-compile recipes with changes.
- Package recipe binaries with changes as `pkgar` files.
- Install recipe packages with changes in the QEMU virtual disk formatted with RedoxFS.

### make image

- Verify source changes on recipes (if the build system was updated or has changes)
- Create a new image with new recipe changes or recipes

## Recipes

### make r.recipe

- Search the recipe location.
- Verify if the `source` folder is present, if not, download the source from the method specified inside the `recipe.toml` (this step will be ignored if the `[source]` section and its data types aren't present or were commented out).
- Build the recipe library dependencies as shared or static objects.
- Start the compilation based on the template of the `recipe.toml`
- If the recipe is using Cargo, it will download the crates, build them, link them/relibc to the program binary.
- If the recipe is using GNU Autotools, CMake or Meson, they will check the build environment and dependencies presence/versions for compatibility, build the libraries or the program and link them/relibc to the final binary.
- Package the recipe binaries.

## QEMU

### make qemu

- It checks for pending changes, if found, it will trigger `make rebuild`.
- It checks the existence of the QEMU image, if not available, it will works like `make image`.
- QEMU starts with the UEFI firmware, the image and the options from `ferric.toml` and the command line.
- The QEMU window is shown with a menu to choose the resolution.
- The bootloader does a bootstrap of the kernel, the kernel starts the init, the init starts the user-space daemons and Orbital.
- The Orbital login screen appears.
