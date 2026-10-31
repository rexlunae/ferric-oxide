# ferric-config

Feature configuration for the Ferric Oxide build, in the spirit of the Linux
kernel's `make menuconfig`, but stored as plain TOML so people, scripts and
agents can all edit it.

For the system-level build flow and component integration, see the book's
[Build and configuration](../../docs/src/ferric-build.md) chapter.

Configured x86-64 builds now default to native Clang/rustup (`build.toolchain =
"llvm"`), with `build.podman = false` and image writing without FUSE
(`build.fuse = false`). `cargo xtask setup` creates an initial `native` image
profile. Choose `build.toolchain = "recipe"` for the inherited compiler path;
Podman and other architectures select it by default. See the build chapter
for the target ABI libraries and current native limitations.

There are three files:

| File | Tracked | What it is |
| --- | --- | --- |
| `features.toml` | yes | The catalog: every menu and option, its type, default, dependencies, help text, and how it reaches the build. |
| `ferric.toml` | no (like `.config`) | Your choices. Only options you set are listed; everything else follows the defaults in the catalog. |
| `build/ferric/` | no | Generated from the two above: `config.mk`, `filesystem.toml`, `kernel-config.toml`, `state.toml`. |

Without a `ferric.toml` nothing changes and the build behaves like upstream
Redox.

Older configurations containing `kernel.pti` must remove that obsolete boolean.
Run `cargo run -q -p ferric-config -- unset kernel.pti` to remove it from the
saved file. To require the implemented isolation path, then run
`cargo run -q -p ferric-config -- set kernel.cpu.kpti always`;
its default is `"auto"`. See
[KPTI and PCID](../../docs/src/ferric-memory.md#kpti-and-pcid).
Validation rejects the removed option before generating build files.

## Using it

```sh
make menuconfig     # interactive editor; creates ferric.toml on first save
make config-check   # validate ferric.toml
make config-list    # every option, its value, and where the value came from
make all            # resolves the current target and generates build/ferric/
```

The editor is a tree of menus:

```
 Ferric Oxide configuration > Kernel
┌ ferric.toml ──────────────────────────────────────────────────┐
│> -*- ACPI support                                             │
│  [*] Multi-core (SMP)                                         │
│  [ ] NUMA awareness                                           │
│  [*] Self-modifying code (CPU feature patching)               │
│  [*] KVM paravirtual clock                                    │
│  ...                                                          │
│      CPU feature use  --->                                    │
└───────────────────────────────────────────────────────────────┘
┌ Help ─────────────────────────────────────────────────────────┐
│ACPI support (kernel.acpi)                                     │
│type: bool   value: true   default: true                       │
│forced on by kernel.multi_core                                 │
│Parse ACPI tables (MADT, HPET, ...). Required for multi-core.  │
└───────────────────────────────────────────────────────────────┘
↑↓ move  ⏎ select  space toggle  y/n on/off  d default  / search  ? help  s save  esc back  q quit
```

`[*]` is on, `[ ]` off, `-*-` forced on by another option's `selects`, and
`(value)` shows choices, numbers and strings. A yellow dot marks options set
in `ferric.toml`. Grey entries have unmet dependencies and do nothing until
those are met. `d` resets an option to its default, which removes it from
`ferric.toml`.

## Editing ferric.toml directly

Option ids are dotted paths and the file uses the same nesting:

```toml
[cpu]
x86_64_level = "v3"

[kernel]
multi_core = false
debugger = true

[kernel.cpu]
smap = "always"

[image]
config = "server"
packages_add = ["vim", "htop"]
filesystem_size = 4096

[qemu]
kvm = "yes"
```

Comments and layout are kept when the editor or `ferric-config set` changes
the file. A build resolves the effective configuration before including
`build/ferric/config.mk` and stops with a clear message if it is invalid.
Configuration-only Make goals skip this automatic generation, so `make
menuconfig` remains available to repair conflicting choices.

The command line covers everything the editor does, which is the easiest
route for agents:

```sh
T="cargo run -q --release --manifest-path tools/ferric-config/Cargo.toml --"
$T list --json                 # all options with value, default, source, active
$T list --changed              # only what ferric.toml sets
$T get cpu.x86_64_level
$T set kernel.numa true
$T set kernel.debugger=true cpu.x86_64_level=v4
$T unset kernel.numa           # back to the default
$T explain kernel.cpu.smap     # help, choices, dependencies, conditional defaults
$T schema                      # the whole catalog as JSON
$T check                       # exit status 1 on errors
$T check --override target.arch=aarch64
$T generate --override target.arch=aarch64 --override image.config=native
$T variants --json             # enabled CPU variants and each CPU type's minimum
```

`unset` can remove a retired or misspelled leaf key that is present in the saved
file. It rejects unknown keys absent from the file and table names, and keeps
unrelated choices. `check` still rejects any unknown settings left in the file.

`check` and `generate` accept repeated `--override OPTION=VALUE` arguments.
They apply before dependency resolution and validation, and do not edit
`ferric.toml`. `menu` and `set` operate on saved choices.

The processor menu has independent x86-64 and ARM64 submenus. Both can be
configured from either host. Minimum baselines remain separate from the boolean
variant options (`cpu.x86_64_v3`, `cpu.x86_64_v4`, `cpu.aarch64_v8_0`,
`cpu.aarch64_v8_2`, `cpu.aarch64_v9`). Multiple boxes can be checked; variants
below a CPU type's minimum are inactive. `cargo xtask build --all-variants`
builds the checked variants into separate image directories. A normal build
uses the active target's baseline. See the build chapter for filtering, running
a variant and the remaining combined-image limitation.

## How options reach the build

On x86-64, select `image.config=ferric-smoke` for the smoke-test image or
`image.config=ferric-fuzz` for the extended syscall fuzzer, through the CLI or
menuconfig. Both profiles preserve saved CPU/kernel settings and generated
image overrides. They do not force CI's optional kernel test features. Other
architectures are rejected because the included tests use x86-64 instructions.
The `ferric-hardware` choice selects the x86-64 hardware inventory image;
`drivers.rtl8125=true` adds the experimental RTL8125B driver to that image.
Use these catalog choices instead of a mismatching command-line `CONFIG_NAME`;
see the book's [boot reproduction guide](../../docs/src/ferric-testing.md#reproducing-a-boot).

| Option kind | Mechanism |
| --- | --- |
| Make variables (`ARCH`, `CONFIG_NAME`, `PODMAN_BUILD`, `REPO_*`, `QEMU_*`, ...) | `build/ferric/config.mk`, included near the top of `mk/config.mk`. Values use `?=`, so `.config` and the command line still win; `check` warns when `.config` shadows one. |
| Image contents | `build/ferric/filesystem.toml` includes the base `config/…/<name>.toml` and adds packages, `"ignore"` entries and `[general]` overrides. It becomes `FILESYSTEM_CONFIG`. |
| Processor baseline and extra flags | `FERRIC_TARGET_RUSTFLAGS` and `FERRIC_TARGET_CPPFLAGS`, appended by the cookbook prescript (`tools/cookbook/src/cook/script.rs`) for every package built for the target, except the recipes in `cpu.flags_exclude` (the kernel and bootloader). C, C++, CMake, Meson and the `cc` crate all read `CPPFLAGS`. |
| Kernel Cargo features | `FERRIC_KERNEL_FEATURES`, passed by `recipes/core/kernel/recipe.toml` as `KERNEL_CARGO_FEATURES` together with `--no-default-features`, so turning a default feature off works. |
| Kernel CPU feature policy | `build/ferric/kernel-config.toml`, copied into the kernel source as `config.toml`, which the kernel's `build.rs` reads (`always`/`auto`/`never` for SMAP, FSGSBASE, XSAVE, XSAVEOPT). |

Make passes explicit `ARCH`, `BOARD` and `CONFIG_NAME` assignments (including
legacy `.config` assignments) to the resolver as temporary overrides. Feature
dependencies, CPU flags, kernel settings and image contents are then generated
for the effective target. Inapplicable options are inactive; shared choices
remain in effect. Every configured build resolves its inputs again, so switching
or removing an override cannot reuse settings from the preceding target.
Unchanged outputs retain their timestamps. Directly including an already
generated `config.mk` retains its target guard as a defensive check.

### Rebuilds

The cookbook rebuilds a recipe when its source or `recipe.toml` is newer than
its last build, and knows nothing about environment variables. So when you
change an option, `generate` compares per-recipe fingerprints with the
previous run (`build/ferric/state.toml`) and touches the `recipe.toml` of each
recipe the option names in `rebuild` (only the timestamp changes). Options
with `rebuild = ["*"]`, such as the processor baseline, mark every recipe, so
the next `make` rebuilds everything that is in the image. Kernel options only
rebuild the kernel. Pass `--no-touch` to `generate` to write outputs without
touching recipes or recording new rebuild fingerprints. Repeated uses preserve
pending rebuilds; a later ordinary generation marks them stale. Use a fresh
`--out` directory for previews that should also leave generated build settings
unchanged. After `--no-touch` in the normal output directory, run
`make config-generate` before building to apply pending rebuilds.

If opening a selected recipe or updating its timestamp fails, generation reports
the recipe path and filesystem error without recording new fingerprints. Fix
the reported problem and rerun `make config-generate`; all pending recipes are
retried, including any already touched before the failure.

## Processor baselines

`cpu.x86_64_level` defaults to `v3` (AVX2, BMI2, FMA; Haswell, Zen and
newer). `make qemu` picks a matching emulated CPU when KVM is off (`max` for
v3). Notes:

- `v4` (AVX-512) selects `kernel.cpu.avx512 = "always"` by default. The kernel
  enables opmask and ZMM state in XCR0 and saves/restores it on context switch;
  the boot CPU must provide the requested level's features.
- Prebuilt packages (`build.repo_binary`) and the prebuilt toolchain's
  libraries are compiled for baseline x86-64; only what you build locally uses
  the chosen level.
- With level v3 or v4, kernel XSAVE, XSAVEOPT and FSGSBASE default to
  `always`, and SMAP to `always` at v4 (Haswell lacks SMAP).

## Frame pointers

`cpu.frame_pointers = true` adds `-fno-omit-frame-pointer` to
`FERRIC_TARGET_CPPFLAGS` and `-C force-frame-pointers=yes` to
`FERRIC_TARGET_RUSTFLAGS`, so the kernel profiler (`kernel.profiling`) can walk
user stacks. Its scope and limits:

- Rust packages built by the cookbook keep frame pointers on 64-bit targets
  even with the option off, because redoxer adds the same flag; the option's
  real effect is on C and C++ code.
- Only locally built packages are covered. Prebuilt packages
  (`build.repo_binary`), prebuilt fox packages synced from `packages.set` and
  the prebuilt toolchain libraries keep their own flags, so a profiled call
  chain can stop inside them.
- The kernel and bootloader are excluded, as for every flag in
  `cpu.flags_exclude`.

## Debug info and unoptimized builds

`build.debug = true` sets `REPO_DEBUGINFO=1`: locally built packages stay optimized and
keep full debug info. `build.unoptimized = true` sets `REPO_DEBUG=1`, the inherited
unoptimized Cargo debug profile, which runs many times slower. See
[Debug info and unoptimized builds](../../docs/src/ferric-build.md#debug-info-and-unoptimized-builds).

## Adding an option

Add an `[[option]]` to `features.toml`; no code changes are needed for the
common cases. The header of that file documents the fields. For example, a
new kernel feature:

```toml
[[option]]
id = "kernel.my_feature"
menu = "kernel"
type = "bool"                 # bool, choice, int, string or list
title = "My feature"
default = false
depends = "kernel.multi_core" # expression over other options
selects = []                  # bools forced on when this is on
kernel_feature = "my_feature"
rebuild = ["kernel"]
help = "What it does and when to turn it on."
```

Expressions support option ids, `value` (inside an option's own effects),
string/number/bool literals, lists, `== != < <= > >=`, `in`, `!`, `&&` and
`||`. `[[rule]]` entries turn a condition into a warning or an error.

`cargo test` checks that the repository's `features.toml` is valid and covers
resolution, generation and the editor.
