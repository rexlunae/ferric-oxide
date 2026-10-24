# Build and configuration

The Cargo entry point (`tools/ferric-build`, aliased as `cargo xtask`) runs the
existing Make/recipe engine for package ordering and image creation. Host,
kernel, bootloader and userspace retain their independent Cargo workspaces.
Plain `cargo build` at the root builds host tools and shared libraries;
`cargo xtask build` produces an OS image.

## Native build and QEMU

```sh
cargo xtask doctor
cargo xtask setup
cargo xtask build
cargo xtask run --headless
```

Prerequisites are rustup, Git, curl, tar, Make, a native C/C++ compiler and SDK,
and NASM. On macOS, Xcode Command Line Tools supplies Clang and the SDK. NASM is
still needed for the kernel's assembly trampoline. QEMU is needed only to boot.
Use existing installations or install missing tools using your preferred
method: setup never invokes Homebrew, a system package manager or sudo.

Setup installs missing or outdated `cbindgen` and `just` with Cargo, adds `rust-src`,
`llvm-tools-preview` and `x86_64-unknown-redox` to the pinned toolchain, initializes
submodules and creates `ferric.toml` only if absent. It selects `image.config =
"native"` and binary application packages. `config/native.toml` rebuilds the
local kernel, UEFI bootloader, relibc, base services/drivers and RedoxFS, even
with `build.repo_binary = true`. The selected package manager is added by the
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

Stock Clang has no complete Redox linker driver. The generated adapter uses
Clang's x86-64 ELF/Linux driver for linker/startup handling, removes Linux
predefines, defines `__redox__`, and selects the Redox sysroot, LLD and
`/usr/lib/ld64.so.1`. Rust's target remains `x86_64-unknown-redox`. A small linker
script exports the constructor-array symbols relibc needs in dynamic executables.
Static links and shared libraries keep their normal layouts. Clang must support
`--gcc-install-dir`; no separate Homebrew LLVM installation is needed when the
host's Clang supports it. Native artifacts live under
`prefix/llvm/x86_64-unknown-redox/`, apart from the inherited toolchain.

Configured builds default to native LLVM on x86-64, `build.podman = false`, and
`build.fuse = false`. Image creation records guest ownership and applies it
inside RedoxFS rather than calling host `chown`; sparse image allocation uses
Rust's filesystem API. This avoids FUSE, macFUSE, sudo and GNU coreutils for the
normal image path. Explicit `make mount` still requires FUSE.

`cargo xtask run` builds and boots `build/x86_64/<config>/harddrive.img` using the
existing QEMU configuration. `--headless` selects the serial console without a
display window. On Apple Silicon, the x86-64 guest uses software emulation;
hardware acceleration requires matching architectures. After building, use
`cargo xtask run --no-build --headless` to reuse the existing disk and preserve
guest changes. QEMU's UEFI firmware is
discovered in the usual Linux, Homebrew and MacPorts locations. If necessary,
pass `PFLASH0=/path/to/edk2-x86_64-code.fd` or `FIRMWARE=/path/to/OVMF.fd`.
Missing firmware produces an error rather than silently attempting BIOS boot.

```sh
cargo xtask run -- QEMU_MEM=4096 kvm=no
cargo xtask --dry-run build
cargo xtask build -- REPO_BINARY=0
make menuconfig
make config-check
make r.kernel
```

Make assignments after `--` override the Cargo command's native x86-64,
container and FUSE settings. Without a terminal, Cargo commands disable the
cookbook TUI so builds can be logged. The native path currently supports
x86-64 UEFI. Some third-party recipes still require GNU build utilities or
explicit GCC compilers; dependencies for those recipes are installed only when
needed. A full application rebuild has broader requirements than the initial
native profile. Local recipe snapshots skip root `target/`, `build/`, `sysroot/`
and Git metadata, as well as nested Cargo target directories marked as caches,
so existing component build caches are not copied into each recipe or used to
invalidate source fingerprints. A native sysroot stamp invalidates statically
linked source packages after relibc changes; kernel and UEFI builds do not link
that sysroot. The x86-64 relibc fork transition keeps the handoff buffer live
through address-space cloning and copies it above the child's Rust call stack,
so ordinary compiler stack layouts cannot overwrite its file-table handles.
Its floating-point control slots are separate from saved integer registers.

## Optional container and inherited toolchain

Use `cargo xtask --podman build` with an existing Podman installation to select
the Linux recipe toolchain. `./podman_bootstrap.sh` remains the explicit legacy
container setup, which can install host packages. On hosts without SELinux, the
Cargo wrapper disables SELinux volume labels. Container debugging and other
specialized workflows remain available independently.

`build.toolchain = "recipe"` retains the inherited prebuilt/source cross
compiler path. It is the default when selecting Podman or a non-x86-64 target.
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

The installer GUI recipe still fetches its own complete pinned upstream tree;
the local host installer import does not include that GUI.

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
  pinned revision so only the source moves.
- The relibc submodules (`openlibm`, `dlmalloc-rs`) use the same mirrors.
- The subtree-sync workflow still reads upstream Redox to open synced pull
  requests and never writes there. Recipes that fetch their own component
  trees from upstream Redox are outside this list and remain to be migrated.

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
