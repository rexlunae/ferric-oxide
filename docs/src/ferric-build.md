# Build and configuration

The Cargo entry point (`tools/ferric-build`, aliased as `cargo xtask`) runs the
existing Make/recipe engine for package ordering and image creation. Host,
kernel, bootloader and userspace retain their independent Cargo workspaces.
Plain `cargo build` at the root builds host tools and shared libraries;
`cargo xtask build` produces an OS image.

## Native build and QEMU

```sh
./native_bootstrap.sh
cargo xtask build
cargo xtask run --headless
```

Prerequisites are rustup, Git, curl, tar, Make, a native C/C++ compiler and SDK.
On macOS, Xcode Command Line Tools supplies Clang and the SDK; `host:`
recipes link with its ld64, so the cookbook's `DYNAMIC_INIT` gives them only
`-L` for the dependency sysroot rather than the ELF `-rpath-link` and
`--export-dynamic` flags it uses for Redox. NASM is
needed only for the x86-64 kernel's assembly trampoline. QEMU is needed only to boot.
Without a package server every application recipe builds from source, so the
build machine also needs the programs recipes run: Autoconf, Automake,
`autopoint`, Bison, CMake, Flex, M4, `makeinfo`, Meson, Ninja, `patch`, Perl,
`pkg-config` and Python 3 (see [Building Ferric Oxide](./building-redox.md#prerequisites)). `cargo xtask doctor`, which `setup` runs first, fails
listing every missing one. `repo cook` also checks the Meson, CMake and Ninja
use of each recipe it is about to build from source before building any of
them, and again before each rebuild, naming each missing program with the
recipes that need it.
Recipes that need the build-time Python (`host:python312`, for example libxcb
through xcb-proto) also need OpenSSL 3 headers on the build machine; on macOS,
Homebrew's `openssl@3` is found even though it is keg-only. OpenSSL 4 does not
work with CPython 3.12.
`./native_bootstrap.sh` installs all of these with Homebrew, `apt-get` or `dnf`
and then runs setup; its package lists are the reference. Setup itself never
invokes Homebrew, a system package manager or sudo, so existing installations
work with `./native_bootstrap.sh --no-packages` or plain `cargo xtask setup`.

Setup installs missing or outdated `cbindgen` and `just` with Cargo, adds
`rust-src` and `llvm-tools-preview` to the pinned toolchain, initializes
submodules and creates `ferric.toml` only if absent. Commands default to the host
architecture: `aarch64` on Apple Silicon and `x86_64` on Intel/AMD. Setup adds
the prebuilt Redox standard library for x86-64. For ARM64, the generated Cargo
adapter builds std from rust-src; rustup does not distribute that target. Setup
selects `image.config = "native"`.
The cookbook always builds recipes whose
source is in this repository (a `path` source, or `same_as` one: the kernel,
UEFI bootloader, relibc, base services/drivers, RedoxFS, fox and the test
recipes) from source, even with `build.repo_binary = true` and in every image
configuration, because a package server cannot have this checkout's build. The selected package manager is added by the
catalog. Existing configurations are preserved, so their image choice and
package policy still apply. The first build downloads Rust components, target
runtime data, Cargo dependencies and application packages.

The native toolchain (`build.toolchain = "llvm"`, `mk/llvm.mk`) uses host Clang
and rustup's compiler, LLD and LLVM binary utilities. It extracts target headers,
libraries and startup objects from the existing upstream GCC toolchain archive
without installing or executing its Linux compiler binaries. Rust's current
Redox target still links `libgcc`; existing C++ packages require `libstdc++` and
its headers. Replacing these ABI runtimes with compiler-rt/libc++ is separate
work. Native builds do not compile GCC, LLVM or Rust itself.

The same archive supplies Redox's libtool: its m4 macros, `ltmain.sh` and
`libtoolize`, which know how to link shared libraries for Redox. They go into the
sysroot's `share/`, and `LIBTOOLIZE` points autoreconf at that `libtoolize`, so
recipes that regenerate their build files use one libtool version. Assembling the toolchain also
installs X.Org's `util-macros` (`xorg-macros.m4`, SHA-256 pinned in
`scripts/native-toolchain.sh`) into the sysroot for the X11 recipes, which
Podman images get from `xutils-dev`. Recipes that regenerate also need host
autoconf, automake and m4.

Stock Clang has no complete Redox linker driver. The generated adapter uses
Clang's corresponding ELF/Linux driver for linker/startup handling, removes Linux
predefines, defines `__redox__`, and selects the Redox sysroot, LLD and
`/usr/lib/ld64.so.1` on x86-64 or `/usr/lib/ld.so.1` on ARM64. Rust keeps
its `*-unknown-redox` target. A small linker
script exports the constructor-array symbols relibc needs in dynamic executables.
Static links and shared libraries keep their normal layouts. Clang must support
`--gcc-install-dir`; no separate Homebrew LLVM installation is needed when the
host's Clang supports it. The adapter is `<arch>-unknown-redox-clang` (and
`clang++`); the sysroot's plain `clang` and `clang++` use it only when called
with `--target=*-unknown-redox` and otherwise run host Clang, so `host:`
recipes build programs for the build machine. Native artifacts live under
`prefix/llvm/<arch>-unknown-redox/`, apart from the inherited toolchain.

Configured builds default to native LLVM on x86-64 and ARM64, `build.podman = false`, and
`build.fuse = false`. Image creation records guest ownership and applies it
inside RedoxFS rather than calling host `chown`. Later file declarations retain
previous guest IDs when those fields are omitted. Archiving currently makes a
second pass over the image tree to set guest owners, including default root IDs;
sparse image allocation uses
Rust's filesystem API. This avoids FUSE, macFUSE, sudo and GNU coreutils for the
normal image path. Explicit `make mount` still requires FUSE.

`cargo xtask build` builds the image selected by QEMU; `run` builds and boots
that same image, and `--headless` selects the serial console. Defaults are:

| Guest | Image under `build/<arch>/<config>/` | Execution |
| --- | --- | --- |
| x86-64 | `harddrive.img`, persistent | Hardware acceleration on matching hosts; software emulation across architectures |
| ARM64 | `redox-live.iso`, RAM-backed | Software emulation while storage and HVF bring-up remain incomplete |

ARM64's live root avoids the current NVMe initialization timeout and missing
virtio MSI-X support. HVF reached the kernel but trapped during early user-space
startup in local validation. `qemu.kvm = "yes"` (`kvm=yes` after `--`) remains
an explicit experimental acceleration option. Some prebuilt ARM64 applications
also fail at runtime; reaching login is not validation of every application.
`qemu.live` chooses live/persistent images; persistent ARM64 boots are not yet
validated. Live guest filesystem changes are lost on exit.

ARM64 UEFI boots initialize wall time from firmware `GetTime`, including
ACPI boots where the kernel cannot discover the PL031 RTC through a device
tree. The bootloader validates the Gregorian date, nanoseconds, timezone and
daylight flags using `ferric-boot-time`, then passes `BOOT_TIME_OFFSET_NS` as
a hexadecimal Unix-minus-physical-counter offset in the boot environment.
The counter continues across kernel entry, so time spent loading the kernel
or editing boot variables is counted once. `CLOCK_MONOTONIC` is unchanged.

UEFI timezone metadata is applied using its specified sign; an unspecified
timezone means the RTC is assumed to store UTC. Firmware with an incorrect
RTC still supplies an incorrect date. Failed or invalid firmware samples are
logged and omitted; the kernel retains its device-tree PL031 fallback where
available, anchoring the RTC it reads to the physical counter beside it so the
uptime before that read is not counted twice. Without either source, wall time
remains the Unix epoch plus the physical counter. This initializes time at
boot; it does not provide network time synchronization or write changes back to
the RTC. See the
[clock regression checks](ferric-testing.md#arm64-firmware-clock-regression).

After building, `cargo xtask run --no-build --headless` reuses the selected
image. Only persistent disk images retain guest changes between boots.
QEMU's UEFI firmware is
discovered in the usual Linux, Homebrew and MacPorts locations. If necessary,
pass `PFLASH0=/path/to/edk2-aarch64-code.fd` for ARM64, or
`PFLASH0=/path/to/edk2-x86_64-code.fd` for x86-64.
Missing firmware produces an error rather than silently attempting BIOS boot.
Boards that supply a boot ROM through `QEMU_KERNEL`, such as `raspi3bp`, use
that ROM instead of requiring generic ARM64 UEFI firmware.

Native LLVM x86-64 builds default to QEMU's `max` CPU when no model is selected,
including cross-builds whose saved CPU settings do not apply. Configured or
explicit CPU models take priority. The inherited unconfigured recipe workflow
retains its `core2duo` software CPU and original acceleration defaults.

```sh
cargo xtask run -- QEMU_MEM=4096 kvm=no
cargo xtask --dry-run build
cargo xtask build -- REPO_BINARY=0
make menuconfig
make config-check
make r.kernel
```

Make assignments after `--` override the Cargo command's architecture,
container and FUSE settings. Without a terminal, the cookbook does not start
its TUI, whether run through Cargo or Make, so builds can be logged. The native path currently supports
x86-64 and ARM64 UEFI. Some third-party recipes still require GNU build utilities or
explicit GCC compilers; dependencies for those recipes are installed only when
needed. A full application rebuild has broader requirements than the initial
native profile. Local recipe snapshots skip root `target/`, `build/`, `sysroot/`
and Git metadata, as well as nested Cargo target directories marked as caches,
so existing component build caches are not copied into each recipe or used to
invalidate source fingerprints. The native prefix tracks relibc's headers,
manifests, Make/build configuration, source submodules and shared path
dependencies, including directory changes for added or deleted inputs. Build
outputs and Git metadata are excluded. These inputs refresh the sysroot before
recipe/image cache decisions. Its stamp invalidates a source recipe when any
required package archive is missing or older than the sysroot; a newer archive
variant cannot hide a stale one. Kernel and UEFI builds do not link that sysroot. relibc builds its openlibm math library for Redox rather than for the build
machine's OS, so the library is the same whether the sysroot is assembled on
Linux or macOS (see `crates/relibc/README.md`).
The x86-64 relibc fork transition keeps the handoff buffer live
through address-space cloning and copies it above the child's Rust call stack,
so ordinary compiler stack layouts cannot overwrite its file-table handles.
Its floating-point control slots are separate from saved integer registers.

## Cross-compiling and booting another architecture

Select the guest with `--arch` on each command. For an x86-64 guest on Apple
Silicon:

```sh
cargo xtask --arch x86_64 setup
cargo xtask --arch x86_64 build
cargo xtask --arch x86_64 run --headless
```

For ARM64 on an Intel/AMD host, use `--arch aarch64` instead. `arm64` is an
alias for `aarch64`. The matching `qemu-system-aarch64` or
`qemu-system-x86_64` binary and its UEFI firmware must be available. Images
and toolchains are separate per architecture. `run --no-build` reuses the
selected architecture's image. `live=yes` or `live=no` after `--` explicitly
selects a live image or persistent disk, overriding `qemu.live`.

The Cargo architecture selection overrides a saved `target.arch` without
rewriting `ferric.toml`. Make resolves explicit `ARCH`, `BOARD` and `CONFIG_NAME`
assignments before validating and generating configuration. Saved feature
choices are resolved for that target: applicable settings remain active, while
x86-only tracing, profiling and PTI are inactive on ARM64. A saved x86 tracing/
profiling conflict therefore does not prevent an ARM64 build. Selecting x86-64
still diagnoses that conflict. Plain Make uses the saved architecture when
configured and the host architecture otherwise (`arm64` is normalized to
`aarch64`).

Configured builds resolve these inputs on every Make invocation, including
changes to or removal of command-line overrides without a TOML edit. This adds
a cached `ferric-config` invocation; unchanged generated files retain their
timestamps. Configuration-only goals (`menuconfig`, `config-check`,
`config-list`, `config-generate`) skip automatic generation so an invalid
configuration can be inspected and repaired. `make menuconfig` edits saved
choices; `make config-check ARCH=aarch64` checks the temporary ARM64 selection.

## CPU baselines and independent build variants

The processor menu contains x86-64 and ARM64 submenus. Each keeps a minimum
baseline and separate, nonexclusive variant checkboxes, editable even when the
other architecture is the active build target. Existing `cpu.x86_64_level` and
`cpu.aarch64_level` settings retain their baseline meaning. Normal build/run
commands use the selected architecture's baseline.

Variant selections are `cpu.x86_64_v3`, `cpu.x86_64_v4`, `cpu.aarch64_v8_0`,
`cpu.aarch64_v8_2` and `cpu.aarch64_v9`. Defaults select v3 and ARMv8.0-A;
raising a minimum makes lower variants inactive and defaults its matching
variant on. Explicit checkbox choices are preserved. Enabling v4 does not
raise the saved x86 minimum or affect ARM's minimum.

```sh
cargo run -p ferric-config -- variants --json
cargo xtask build --all-variants
cargo xtask --arch aarch64 build --all-variants
cargo xtask --arch x86_64 build --cpu-variant v4
cargo xtask --arch aarch64 run --cpu-variant armv8.0-a --no-build --headless
```

`--all-variants` builds each enabled target/level sequentially; explicit `--arch`
or `ARCH=` filters the list. Prepare each architecture with its `xtask setup`
command first. The resolver validates each effective build configuration, and
the build stops on failure. The variant listing describes CPU selections;
it does not validate all image profiles or kernel combinations for every target.

Each variant gets `build/<arch>/<image>/variants/<level>/`, including its live or
persistent image, instead of replacing another variant's output. Shared recipe
caches use the resolver's applied fingerprints, so changing the active level
marks affected source recipes stale. Variants use their own level for compiler
flags and kernel CPU-state/boot checks without rewriting saved baselines.
Prebuilt application packages keep their published baseline. Dry runs print the
variant query without executing it. These commands produce separate images;
putting multiple architectures in a single image is future work.

## Optional container and inherited toolchain

Use `cargo xtask --podman build` with an existing Podman installation to select
the Linux recipe toolchain. `./podman_bootstrap.sh` remains the explicit legacy
container setup, which can install host packages. On hosts without SELinux, the
Cargo wrapper disables SELinux volume labels. Container debugging and other
specialized workflows remain available independently.

`build.toolchain = "recipe"` retains the inherited prebuilt/source cross
compiler path. It is the default when selecting Podman or RISC-V.
Without `ferric.toml`, plain Make preserves the upstream-style configuration
and defaults. Saving `make menuconfig` opts into the Ferric catalog. Source
recipe inputs remain local fork sources in either mode. See the inherited
[Podman build](podman-build.md), [build-system reference](build-system-reference.md)
and [troubleshooting](troubleshooting.md) for additional Make workflows; verify
historical source paths against [Architecture](ferric-architecture.md).

## Configuration inputs and precedence

| Input or output | Meaning |
| --- | --- |
| `features.toml` | Tracked catalog: menus, typed options, defaults, dependencies, rules, build effects and rebuild scope |
| `ferric.toml` | Untracked local choices; omitted options take resolved catalog defaults |
| `.config` | Legacy Make assignments; can shadow generated defaults |
| `build/ferric/config.mk` | Generated Make values and exported recipe settings |
| `build/ferric/filesystem.toml` | Generated image configuration including the chosen base config and package/general overrides |
| `build/ferric/kernel-config.toml` | Generated architecture/CPU policy, copied to the recipe's kernel source as `config.toml` |
| `build/ferric/state.toml` | Prior per-recipe fingerprints used to mark affected recipes stale |

`tools/ferric-config/src/schema.rs` parses the catalog; `expr.rs` evaluates
conditions; `model.rs` resolves values and diagnostics; `generate.rs` writes
the outputs. The editor and `set` preserve comments and layout in the user's
file. Dependencies control whether options are active, `selects` force boolean
prerequisites, conditional defaults choose values, and catalog rules report
invalid combinations or warnings.

`mk/config.mk` reads `.config` before generated settings. Generated Make
defaults use `?=`, so existing assignments and command-line values win.
`ferric-config check` reports known `.config` shadowing. The generated Makefile
guards target-sensitive CPU, kernel and image settings when the actual
`ARCH`, `BOARD` or `CONFIG_NAME` differs from the generated target.

## CLI for scripts and agents

These commands run from the repository root:

```sh
cargo run -q -p ferric-config -- list --json
cargo run -q -p ferric-config -- explain kernel.iommu
cargo run -q -p ferric-config -- set cpu.x86_64_level=v3 kernel.iommu=auto
cargo run -q -p ferric-config -- get kernel.iommu
cargo run -q -p ferric-config -- unset kernel.iommu
cargo run -q -p ferric-config -- check
make config-generate
```

The CLI also accepts `--root`, `--schema`, `--config` and `--out` for explicit
inputs and destinations. `schema` emits the catalog as JSON; `list --changed`
shows explicitly chosen values. To preview generated settings, use a fresh
temporary output directory:

```sh
preview_dir=$(mktemp -d)
cargo run -q -p ferric-config -- --out "$preview_dir" generate --no-touch
```

Inspect the generated files under `$preview_dir`. `--no-touch` writes generated
outputs without changing recipe timestamps or committing `state.toml` rebuild
fingerprints. Repeated previews therefore leave pending rebuilds for the next
ordinary generation, including the first generation into a new output directory.
A temporary directory also keeps the real generated configuration unchanged.
If you use `--no-touch` in `build/ferric/`, run `make config-generate` before
building to apply pending rebuilds; Make may otherwise see the generated files
as already current.
The tool's own usage guide is `tools/ferric-config/README.md`.

A small configuration enabling the implemented buffer and DMA interfaces is:

```toml
[cpu]
x86_64_level = "v3"

[kernel]
iommu = "auto"
regbuf = true
```

All other active options resolve from the catalog. In particular, configured
x86-64 builds default to ASLR, physical-map KASLR, kernel-image KASLR and
`PROT_NONE` protection; IOMMU, registered buffers and kernel tracing
([Kernel tracing](ferric-tracing.md)) are opt-in. Consult
`explain` or the catalog rather than assuming a menu title means the feature
is fully enforced.

## How settings reach components

`image.config` includes `ferric-smoke` and `ferric-fuzz` for local reproduction
of the CI test profiles. Both require `target.arch = "x86_64"`; selecting an
unsupported architecture is a configuration error before generation. These
choices select image contents while preserving saved CPU/kernel policy and
image overrides. CI's forced test features remain separate. See
[Reproducing a boot](ferric-testing.md#reproducing-a-boot) for the commands.

1. `mk/ferric.mk` regenerates configuration when the user file, catalog or
   configuration tool sources change; Make includes the result and restarts.
2. `FERRIC_TARGET_RUSTFLAGS` and `FERRIC_TARGET_CPPFLAGS` reach target recipe
   builds through `tools/cookbook/src/cook/script.rs`. `cpu.flags_exclude`
   excludes the kernel and bootloader by default; their CPU policy is separate.
3. `recipes/core/kernel/recipe.toml` passes `FERRIC_KERNEL_FEATURES` with
   `--no-default-features`, copies CPU policy and forwards extra linker
   arguments. `kernel.kaslr_image` emits `--emit-relocs` and the bootloader's
   `kaslr` feature. `kernel.aslr` also selects relibc's `aslr` feature.
4. Local source recipes copy kernel, bootloader, relibc and base trees for
   building. They create links that preserve relative paths to shared crates.
   The target installer recipe uses `tools/installer/`; RedoxFS uses
   `userspace/redoxfs/`.
5. The installer consumes the filesystem configuration and staged packages to
   populate the image. `mk/fstools.mk` builds the local host installer and
   obtains the host RedoxFS tools through its recipe.
   It sizes the image from the packages it installs: the configuration's
   `filesystem_size` grown to what the packages, files and headroom need.
   `image.filesystem_size` in `ferric.toml` or `make FILESYSTEM_SIZE=<MiB>`
   fixes the size instead, and a fixed size that is too small stops the build
   before the image is written. See
   [Filesystem Size](./configuration-settings.md#filesystem-size).

The installer GUI recipe still fetches its own complete pinned upstream tree;
the local host installer import does not include that GUI.

## Build tools in the guest

A recipe names the tools it runs while building in `build.dev-dependencies`,
and a tool whose name carries the `host:` selector is built for the host by
default. Those tools are needed to build the image but not wanted in it, so
building them on the host asks every contributor to install software the image
never uses, and they are the one part of its software stack the build never
exercises the way the image runs it.

`build.build_tools` decides where they are built:

| Value | Behaviour |
| --- | --- |
| `guest` | The tools are built for the target and run inside a guest. Default when `host.arch == target.arch`. |
| `host` | The inherited behaviour: the tools are compiled for and run on the host. |

With `guest`, `mk/fstools.mk` installs one guest build environment per
configuration under `build/<arch>/<config>/build-tools/` and reuses it for the
whole image build instead of booting a guest per tool. It is keyed on the tool
list and the repository's `Cargo.lock`, so a later build reuses it until either
changes. That list comes from `repo build-tools-list`, which prints the
recursive `build.dev-dependencies` of the image's package set, one recipe per
line. `build.guest_env` enables the reuse and is active whenever
`build.build_tools = "guest"`.

The environment directory holds everything a later build compares: the tool list
it was built from (`manifest.txt`), the installer configuration generated from
that list (`build-tools.toml`) and the installed image (`guest.img`). The
installation is staged beside it and moved in, so a finished build leaves all
three newer than their own prerequisites and a second `make` reuses them instead
of reinstalling. The environment is installed before the image's packages are
cooked, because their build scripts are what run the shims.

`build.keep_build_tools` leaves the tools in the finished image as the
`build-tools` meta-package; without it they stay in the scratch environment and
are thrown away with it. It implies `build.build_tools = "guest"`, and setting
it together with `host` is a configuration error.

Every guest `redoxer exec` boots, the build-tools environment included, is given
4 GiB of RAM. The bootloader reads the whole disk image into RAM before the
kernel starts, so a working image needs room for the disk, the kernel and the
firmware at once; the previous 2 GiB was not enough, and a guest booted with too
little memory fails instead of starting. A host with less than about 4 GiB free,
plus QEMU's own overhead, cannot run a guest build: build with
`build.build_tools = "host"` there. `REDOXER_QEMU_ARGS` overrides the argument,
because a `-m` in it replaces the default `-m` that redoxer generates
(`REDOXER_QEMU_ARGS="-m 2048"`); that only works for an image small enough to
load in the smaller memory. See the
[Redoxer README](https://github.com/rexlunae/ferric-oxide/tree/main/tools/redoxer#readme)
for the `exec` options.

Cross-compiling is the exception. A tool built for a foreign architecture cannot
run inside a guest on this host, so a build whose `host.arch` differs from
`target.arch` uses the host tools whatever `build.build_tools` says, and
`cargo xtask build` warns about that rather than falling back silently.
`host.arch` is a catalog expression identifier, not an option: it resolves to
the architecture the build runs on, in `target.arch` names, so
`host.arch == target.arch` means "this is a native build".

Changing either option marks every recipe stale, because the tools a recipe
builds with change.

### How a recipe runs a guest tool

A `host:` recipe builds to the target in guest mode, so its result is a Redox
binary and the host cannot execute it. Two things keep recipe scripts unchanged:

1. The toolchain directory is not put on `PATH`, because everything in it is a
   Redox binary. Instead the cookbook writes a shim of the same name for each
   binary in `toolchain/bin`, and puts the shim directory there first.
2. A shim runs the tool through `repo guest-run --guest <image> -- <tool>
   <args>`, which boots the environment installed for this build and mirrors the
   tool's exit status. The command is the tool's name, not the path of the
   toolchain binary: the guest resolves it on its own `PATH`, where the
   installation put it. `repo guest-run` never rebuilds or reinstalls the
   environment — it only boots it — so repeated calls in one build reuse it.
   It passes the image to `redoxer exec --image`, which boots a private copy per
   invocation, so recipe builds running in parallel never write to the shared
   environment.

A recipe therefore keeps writing `gperf`, `mdbook build` and so on. The
`FERRIC_GUEST_TOOLS_DIR` variable names the environment image; it is empty when
the tools come from the host, which is what leaves a host build's `PATH` and
execution unchanged.

A `host:` name with no recipe (for example `gperf`, which is also a host system
package) has nothing to build for either target. Those entries are host-provided
binaries and guest mode neither builds nor runs them.

## Debug info and unoptimized builds

Two options control how locally built packages are compiled:

| Option | Make variable | Effect |
| --- | --- | --- |
| `build.debug` | `REPO_DEBUGINFO` | Optimized release build with full debug info (`CARGO_PROFILE_RELEASE_DEBUG=true`, `CARGO_PROFILE_RELEASE_STRIP=false`, `-g` for C), not stripped when packaged. |
| `build.unoptimized` | `REPO_DEBUG` | Cargo's debug profile: no optimization, debug assertions and debug info. |

Use `build.debug` for symbolized backtraces and profiles. `build.unoptimized` makes file
systems, drivers and the C library run many times slower: under QEMU emulation,
starting a large program from RedoxFS took seconds ([#286](https://github.com/rexlunae/ferric-oxide/issues/286)).
Keep it for bugs that need debug assertions. With both on, the debug profile wins.
The initfs is the exception: the bootloader loads it into memory whole and it
has a size limit, so its copies of the daemons and drivers have their debug info
stripped. For symbols, use the unstripped binaries in
`recipes/core/base/target/<target>/build/<target>/release/`.
Both mark every recipe for rebuild, and the relibc recipe's build stamp includes the
debug-info setting, so toggling `build.debug` rebuilds relibc too.

## Dependency sources

Ferric Oxide keeps its build inputs independent of upstream Redox services.
Redox components that it consumes through Cargo or git submodules resolve from
this repository or from Ferric-maintained mirrors under
[github.com/rexlunae](https://github.com/rexlunae); no lockfile or submodule
fetches from upstream Redox at build time.

- Components with an in-tree subtree (`crates/kernel/`, `crates/bootloader/`,
  `crates/relibc/`, `userspace/base/`, `userspace/redoxfs/`) are consumed by
  path. That includes relibc's `redox-ioctl` and `redox-rt` and the kernel's
  `redox_syscall`.
- Redox-maintained crates without an in-tree copy (`pkgar`, `pkgutils`,
  `redox-rings`, `redox-buffer-pool`, `redox-log`, `rehid`, `acpi`, `uefi`,
  `ring` and relibc's `object` fork) come from full-history mirror repositories.
  Each `Cargo.lock` pins the exact revision; when updating a mirror, keep the
  pinned revision so only the source moves. `redox-rings` is pinned with `rev`
  in `crates/relibc`, `userspace/base` and `userspace/redoxfs` to a Ferric
  commit that adds the memory fences its wait protocol needs (a waiter could
  otherwise sleep through a wake-up) and the stress tests and loom models
  that check them (see
  [rexlunae/redox-rings#1](https://github.com/rexlunae/redox-rings/pull/1));
  all three use the same revision.
- The relibc submodules (`openlibm`, `dlmalloc-rs`) use the same mirrors.
- Nothing in this repository syncs from upstream Redox. Recipes that fetch
  their own component trees from upstream Redox, and the prebuilt toolchain
  and package downloads from `static.redox-os.org`, are outside this list and
  remain to be migrated.

Device-tree parsing for the kernel and ARM64/RISC-V bootloader goes through
`crates/devicetree/` (`ferric-devicetree`), which uses crates.io's
`fdt = "=0.2.0-alpha3"` with default features disabled. This replaces the old git
revision. The exact prerelease is intentional: 0.1.5 lacks required interfaces,
and the 0.2 parser API is still evolving. Each consumer's lockfile records the
registry version and checksum. The MPL-2.0 parser remains an external dependency;
the MIT adapter does not vendor or copy its implementation.

The prebuilt toolchain, the package key and the Rust nightly components in
`mk/prefix.mk` are downloaded with `wget` through its `WGET` variable. It
prefers IPv4, because hosts without IPv6, such as GitHub's runners, cannot reach
an IPv6-only answer. It looks the host up again on each try. It tries up to 20
times (wget's default), waiting up to 5 seconds between tries, and also retries
refused connections and failed lookups.

## CPU baseline and rebuilds

Configured x86-64 user-space compilation defaults to x86-64-v3. v4 is optional
and selects AVX-512 kernel state support by default. The kernel checks the
requested level at boot and reports missing CPU features. With KVM disabled,
the generated QEMU model for v3/v4 is `max`. The catalog retains older choices
from upstream, but new Ferric development targets v3 or v4.

`cpu.frame_pointers` (off by default) keeps frame pointers in user space, so
the kernel profiler (`kernel.profiling`) can walk user stacks for `ferric-trace
flamegraph`. It adds
`-fno-omit-frame-pointer` to the target CPPFLAGS and `-C
force-frame-pointers=yes` to the target RUSTFLAGS. Rust packages built by the
cookbook already keep frame pointers on 64-bit targets, because redoxer adds the
same flag, so the option's real effect is on C and C++ code. Like the level, it
reaches only packages compiled locally: prebuilt packages, prebuilt fox packages
synced from `packages.set` and the prebuilt toolchain libraries (the Rust
standard library, libgcc) keep their own flags, and a profiled call chain may
stop inside them. The kernel and bootloader are left out, as for every CPU flag.

Flags affect locally compiled packages. `build.repo_binary` selects prebuilt
packages that ignore local compile-time choices, and the prebuilt toolchain's
libraries retain their original baseline. Selecting v4 is not evidence that
every object in an image was rebuilt for v4.

Cookbook detects recipe/source timestamps rather than arbitrary environment
changes. Configuration generation compares fingerprints in `state.toml` and
touches the `recipe.toml` files named by each changed option's `rebuild` scope.
`["*"]` marks all recipes; kernel-only options usually mark the kernel. The
fingerprints are saved only after all selected recipe timestamps are updated.
If a recipe cannot be opened or touched, generation fails with its path and
underlying error, retaining the prior rebuild state. Correct the filesystem
problem and rerun `make config-generate` to retry pending rebuilds. The kernel,
bootloader and relibc recipes also maintain feature stamps for their
Make-driven artifacts. Generated output, `ferric.toml` and image artifacts must
stay uncommitted.
