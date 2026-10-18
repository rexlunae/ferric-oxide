# ferric-config

Feature configuration for the Ferric Oxide build, in the spirit of the Linux
kernel's `make menuconfig`, but stored as plain TOML so people, scripts and
agents can all edit it.

For the system-level build flow and component integration, see the book's
[Build and configuration](../../docs/src/ferric-build.md) chapter.

There are three files:

| File | Tracked | What it is |
| --- | --- | --- |
| `features.toml` | yes | The catalog: every menu and option, its type, default, dependencies, help text, and how it reaches the build. |
| `ferric.toml` | no (like `.config`) | Your choices. Only options you set are listed; everything else follows the defaults in the catalog. |
| `build/ferric/` | no | Generated from the two above: `config.mk`, `filesystem.toml`, `kernel-config.toml`, `state.toml`. |

Without a `ferric.toml` nothing changes and the build behaves like upstream
Redox.

## Using it

```sh
make menuconfig     # interactive editor; creates ferric.toml on first save
make config-check   # validate ferric.toml
make config-list    # every option, its value, and where the value came from
make all            # regenerates build/ferric/ first if ferric.toml changed
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
│  [ ] Page table isolation (KPTI) (EXPERIMENTAL)               │
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
the file. The next `make` regenerates `build/ferric/` and stops with a clear
message if a value is unknown or invalid.

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
```

## How options reach the build

| Option kind | Mechanism |
| --- | --- |
| Make variables (`ARCH`, `CONFIG_NAME`, `PODMAN_BUILD`, `REPO_*`, `QEMU_*`, ...) | `build/ferric/config.mk`, included near the top of `mk/config.mk`. Values use `?=`, so `.config` and the command line still win; `check` warns when `.config` shadows one. |
| Image contents | `build/ferric/filesystem.toml` includes the base `config/…/<name>.toml` and adds packages, `"ignore"` entries and `[general]` overrides. It becomes `FILESYSTEM_CONFIG`. |
| Processor baseline and extra flags | `FERRIC_TARGET_RUSTFLAGS` and `FERRIC_TARGET_CPPFLAGS`, appended by the cookbook prescript (`tools/cookbook/src/cook/script.rs`) for every package built for the target, except the recipes in `cpu.flags_exclude` (the kernel and bootloader). C, C++, CMake, Meson and the `cc` crate all read `CPPFLAGS`. |
| Kernel Cargo features | `FERRIC_KERNEL_FEATURES`, passed by `recipes/core/kernel/recipe.toml` as `KERNEL_CARGO_FEATURES` together with `--no-default-features`, so turning a default feature off works. |
| Kernel CPU feature policy | `build/ferric/kernel-config.toml`, copied into the kernel source as `config.toml`, which the kernel's `build.rs` reads (`always`/`auto`/`never` for SMAP, FSGSBASE, XSAVE, XSAVEOPT). |

If `ARCH`, `BOARD` or `CONFIG_NAME` is overridden on the command line, the
CPU, kernel and image settings are skipped for that run rather than applied to
the wrong target.

### Rebuilds

The cookbook rebuilds a recipe when its source or `recipe.toml` is newer than
its last build, and knows nothing about environment variables. So when you
change an option, `generate` compares per-recipe fingerprints with the
previous run (`build/ferric/state.toml`) and touches the `recipe.toml` of each
recipe the option names in `rebuild` (only the timestamp changes). Options
with `rebuild = ["*"]`, such as the processor baseline, mark every recipe, so
the next `make` rebuilds everything that is in the image. Kernel options only
rebuild the kernel. Pass `--no-touch` to `generate` to skip this.

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
