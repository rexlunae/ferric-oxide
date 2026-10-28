# redoxer

The tool used to build/run Rust programs (and C/C++ programs with zero dependencies) inside of a Redox VM, the Redox GitLab CI use a Docker image with `redoxer` pre-installed.

A pre-built Docker image can be found on [Docker Hub](https://hub.docker.com/r/redoxos/redoxer). Visit [the book for Continous Integration](https://doc.redox-os.org/book/ci.html) for more information.

## Options

```
redoxer env <command> [arguments]...
    Run as command with env configured to run with the toolchain
    The toolchain will be initialized by `redoxer toolchain`
    Environment flags:
        REDOXER_SYSROOT      Specify sysroot to link (default is target/$TARGET/sysroot on Cargo projects)
        REDOXER_USE_CLANG    Use clang as the default compiler
        REDOXER_USE_LTO      Add thin LTO to compiler flags

redoxer <bench | build | check | doc | fetch | install | run | rustc | test> [-g|--gui] [-o|--output file] [--] [arguments]
    Run as cargo passed by `redoxer env cargo`
    Additionally set `redoxer exec` as test runner

redoxer <ar | cc | cxx> [arguments]
    Run as GNU compiler passed by `redoxer env $GNU_TARGET-*`

redoxer exec [-f|--folder folder] [-f|--folder folder:/path/in/redox] [-g|--gui] [-h|--help] [-i|--install-config] [-o|--output file] [--image guest.img] [--] <command> [arguments]...
    Run a command inside QEMU, using a "base" or "gui" redox image, or provide custom one with --install-config
    The redox image will be initialized if not exist or different with the specified --install-config,
    and rebuilt when the cookbook or its built packages change
    Specify a folder to copy it into /root inside redox image, or more generic one with folder:path
    If folder for /root is not specified but <command> is a file, the file will be copied
    --image boots an image that already exists instead of installing one:
    the image is used as the disk as it is, with no package installation, no
    bootloader extraction and no re-archiving, and only the redoxerd
    configuration and the --folder copies are written. See "Running in an
    existing image" below
    Environment flags:
        REDOXER_COOKBOOK      Ferric Oxide checkout whose built repo/$TARGET the image is made from
                              (required unless the install config sets general.cookbook)
        REDOXER_QEMU_BINARY   Override qemu binary
        REDOXER_QEMU_ARGS     Override qemu args
        REDOXER_USE_FUSE      [true|false] Override use fuse (default is automatically detected)
    Notes:
    - Setting REDOXER_QEMU_BINARY will print qemu commands to terminal
    - Setting "-o -" will hide qemu serial output, only printing stdout/stderr from the command
    - x86_64 boots through UEFI when the cookbook's bootloader recipe produced no
      BIOS bootloader, which is what a Ferric Oxide build does (it is UEFI-only);
      a checkout that still builds `bootloader.bios` boots through BIOS as before
    - A UEFI guest needs its firmware installed on the host. When none of the
      usual OVMF/AAVMF/EDK2 paths exists, `exec` fails with an error that names
      the packages to install (for x86_64: `ovmf` on Debian/Ubuntu,
      `edk2-ovmf` on Fedora, `edk2-ovmf` on Arch).
    - `--image` and `--install-config` cannot be combined: one boots an image
      that exists, the other builds one. A missing `--image` file is an error.

redoxer pkg [install|remove|update] pkg-1 pkg-2 ...
    Install additional native packages for Cargo
    Environment flags:
        REDOXER_SYSROOT     Where to install sysroot (default is target/$TARGET/sysroot on Cargo projects)
        REDOXER_COOKBOOK    Ferric Oxide checkout whose repo/$TARGET has the built packages
                            (required to install or update; update also installs new dependencies)

redoxer toolchain [--update] [--url PATH]
    Install or manage toolchain
    Environment flags:
        REDOXER_TOOLCHAIN   Override toolchain path
```

## Running in an existing image

`redoxer exec --image <path>` boots a whole-disk Redox image that already
exists, rather than installing one from an install configuration. It is how the
cookbook runs a recipe's build tools when they were built for the Redox target
(`features.toml` `build.build_tools = "guest"`): `make` installs one guest
environment at `build/<arch>/<config>/build-tools/guest.img`, and
`repo guest-run` passes it here.

In this mode redoxer:

- treats the file as the disk: it does not install packages into it, does not
  add a bootloader and does not re-archive the result, so the kernel, the boot
  configuration and the installed packages are exactly what was installed;
- copies the image to a temporary file and works on the copy. The image itself
  is never written, so a build that runs several tools at once never races on
  one shared environment, and the next run still finds the environment it was
  given. The copy uses `cp --sparse=always`, so a largely empty 4 GiB image
  costs the space it occupies;
- writes only the files a run needs into the copy: `/etc/redoxerd` and the
  `30_redoxer` init unit, plus each `--folder` at its destination. Everything
  the environment installed — kernel, boot configuration, packages and their
  signed records — is left byte-for-byte as it was, so the guest's
  `fox verify --installed` keeps passing;
- does not support `--artifact`: reading files back out of a guest needs the
  FUSE mount, which this mode does not use; it fails and says so rather than
  returning an empty directory;
- validates the path first: a missing image is reported as an error naming
  `make build_tools`, never silently replaced by a freshly built image.

There is no execution cache: the image is the input, and a rebuilt environment
is picked up because nothing about a previous run is remembered.

## Commands

- Install the tool

```sh
cargo install redoxer --locked
```

- Install the Redox toolchain

```sh
redoxer toolchain
```

- Update the Redox toolchain using prebuilt toolchain from existing Redox OS Repo

```sh
make prefix
redoxer toolchain --update --url .
```

- Build the Rust program or library with Redoxer

```sh
redoxer build
```

- Build the Rust program or library with additional native packages

```sh
redoxer pkg install xz
redoxer build
```

`redoxer pkg` installs and records packages with fox. A sysroot an earlier,
`pkg`-based redoxer populated has no fox records yet: the first command that
names one of its packages adopts every package in `etc/pkg/packages.toml`
whose signed head verifies against the cookbook's key, so that command needs
`REDOXER_COOKBOOK` even for `remove`. Packages that cannot be adopted are
listed and left in place; remove the sysroot and install them again.


- Run the Rust program on Redox

```sh
redoxer run
```

- Test the Rust program or library with Redoxer

```sh
redoxer test
```

- Run arbitrary executable (`echo hello`) with Redoxer

```sh
redoxer exec echo hello
```

## Host specific customizations

`redoxer env` can be configured to compile host binaries by setting `TARGET` to the correct host target:

+ `*-unknown-redox`
+ `*-unknown-linux-gnu`
+ `*-unknown-linux-musl`
+ `*-unknown-freebsd`
+ `*-apple-darwin`

This feature is mainly used for [Redox build system](https://gitlab.redox-os.org/redox-os/redox). For other than Linux and Redox, you must supply additional environments to the correct paths or binary name for GCC and Binutils. You can also use it to specify other compiler or different version of GCC. Here's an example for using the system default, which also being the default values:

```sh
export REDOXER_HOST_AR=gcc-ar
export REDOXER_HOST_AS=as
export REDOXER_HOST_CC=cc
export REDOXER_HOST_CXX=c++
export REDOXER_HOST_LD=ld
export REDOXER_HOST_NM=gcc-nm
export REDOXER_HOST_OBJCOPY=objcopy
export REDOXER_HOST_OBJDUMP=objdump
export REDOXER_HOST_PKG_CONFIG=pkg-config
export REDOXER_HOST_RANLIB=gcc-ranlib
export REDOXER_HOST_READELF=readelf
export REDOXER_HOST_STRIP=strip
```
