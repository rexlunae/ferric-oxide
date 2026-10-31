# Windowing and Graphics

Orbital remains the inherited default desktop, but is deprecated for new
Ferric Oxide graphics development. The replacement direction is Wayland with
Smithay, Mesa and Xwayland, as described in [Phase 6 of the roadmap](./ferric-roadmap.md#phase-6-graphics-and-desktop).
COSMIC remains an optional desktop choice to support. Its experimental image
configuration currently uses Orbital to host a nested compositor; this is a
build bring-up path, and the accelerated display stack remains future work.
Orbital is not planned to be brought up to a working or supported state as a
standalone desktop; its inherited recipes and defaults may not work.

## COSMIC build configuration

After the [native build setup](./ferric-build.md), select the existing image
configuration and build it:

```sh
cargo run -p ferric-config -- set image.config wayland
cargo xtask build
cargo xtask run --no-build
```

The `image.config` option in `features.toml` selects `config/wayland.toml`.
That configuration includes `cosmic-comp`, `cosmic-app-library`, the
`libcosmic-wayland` example, GTK and Mesa, together with the inherited desktop
packages. It does not install the complete COSMIC shell, panel and applet set.
`build.build_tools = "guest"` runs eligible build tools such as `gperf` in
the guest; tools that must run on the host include Python and LLVM. The build
needs a working host C++ standard library and enough disk space for both LLVM
builds described below.

On 31 October 2026, the build-fix combination
([#344](https://github.com/rexlunae/ferric-oxide/pull/344)–[#349](https://github.com/rexlunae/ferric-oxide/pull/349)),
with the NVMe/executor correction
[#352](https://github.com/rexlunae/ferric-oxide/pull/352), compiler-test fixture
[#354](https://github.com/rexlunae/ferric-oxide/pull/354), `dd` pipe correction
[#355](https://github.com/rexlunae/ferric-oxide/pull/355), obsolete kernel-option
removal [#356](https://github.com/rexlunae/ferric-oxide/pull/356), and LPSS startup
correction [#357](https://github.com/rexlunae/ferric-oxide/pull/357), completed
`COOKBOOK_MAKE_JOBS=6 cargo xtask build` on x86_64. This was an incremental
build using existing recipe outputs; the changed kernel, base drivers and
`uutils` were rebuilt during validation.

The COSMIC validation integration `d5b2d41c4b11` included the LPSS implementation
[`17ff802c6`](https://github.com/rexlunae/ferric-oxide/commit/17ff802c6a845c6a4f5d2f695a9cbf66852ea808), with
`kernel.lpss_debug = true` and `kernel.cpu.kpti = "always"`. Its headless KVM
snapshot boot, with four vCPUs and 4 GiB RAM, reported KPTI enabled and reached
`redox login:`. A 64 MiB regular-file write and piped transfer passed byte-count,
SHA256 and full file-comparison checks; the exact Ion `dd-pipe` smoke fragment
also passed. Snapshot disk writes preserved the built image. Its SHA256 was
`41110d0146ebd52bb31e2b5f57e55a4e11194b66721068585d0060a6b436c737`;
the generated CycloneDX SBOM contained 103 components.

A later completion-publication correction in #352 was validated at local
integration `b880ecc74737` by a native profiler-image source rebuild and full
KVM smoke boot, including storage stress, profile collection and package
verification. Both profile workflow postchecks passed, with 1,975 samples and
zero unreadable stacks. The COSMIC image hash above records the earlier build;
see [NVMe validation](./ferric-testing.md#nvme-command-identifiers) for the
publication regression.

The first-failure logs are retained for the preceding NVMe command-ID reuse
hang, truncated pipe transfer ([#353](https://github.com/rexlunae/ferric-oxide/issues/353))
and early LPSS MMIO panic. The successful check includes the corresponding
corrections above. See [validation](./ferric-testing.md) for their host and guest
regressions.
The graphical COSMIC session was not validated. The installed
`orbital-wayland` script selects
`COSMIC_BACKEND=winit`, starts `cosmic-comp` nested inside Orbital and then
runs the libcosmic example. Mesa uses software rendering; the compositor
recipe still records performance and keyboard-input limitations. This build
result does not establish a usable graphical session, hardware acceleration
or the Phase 6 exit criteria. ARM64 COSMIC image validation remains pending.

## Drivers

### VESA (vesad)

The `vesad` path writes to the framebuffer supplied by firmware. Ferric
Oxide targets UEFI; a boot framebuffer provides display output without
providing GPU acceleration.

### Intel GPUs

The inherited `ihdgd` driver has Intel-specific modesetting code. See
[Graphics and display](./ferric-hardware.md#graphics-and-display) for the
reference machines' current driver coverage and limitations; a driver recipe
or PCI match is not evidence of an accelerated desktop.

## GPUs

Mesa hardware drivers need GPU interfaces for memory management, command
submission and synchronization, in addition to a display server. A
DRM/KMS-compatible user-space interface, isolated GPU drivers and graphics
buffer sharing built on the registered-buffer grants are planned for
[Phase 6](./ferric-roadmap.md#phase-6-graphics-and-desktop). Orbital's presence
does not provide this planned driver stack.

## Software Rendering

The current `mesa` recipe selects software drivers (`swrast`), with optional
software Vulkan in dynamic builds, and links the inherited `liborbital`
backend. It does not enable hardware GPU drivers. This describes the build
configuration, not a verified working Orbital session.

### Mesa and LLVM

Mesa uses LLVM for its software rasterizer, so building the `mesa` recipe builds
LLVM twice:

- `llvm21`, the target's LLVM, which Mesa links;
- `host:llvm21`, a host build of the same recipe, which Mesa's build queries
  through `bin/<target>-llvm-config`. That wrapper runs the host build's
  `llvm-config`, keeps only the target architecture's libraries and components,
  asks for the target's backend where Mesa asks for `native`, and rewrites the
  paths it reports to the recipe's target sysroot.

Building the same recipe keeps the two at the same LLVM version. The host build
carries every backend a Redox target uses (X86, AArch64, RISC-V and
WebAssembly), so a build for another architecture than the build machine's
still finds its target's backend. It is a build tool that always runs on the
host (`tool-runs-on = "host"`), because build systems query `llvm-config` there,
even with `build.build_tools = "guest"`. Without a host build in the recipe's
toolchain, the wrapper uses the host sysroot's `llvm-config`, which the upstream
prefix toolchain ships. `scripts/ferric-ci/test-llvm-config-wrapper.sh` checks
the wrapper with stand-in `llvm-config` programs.

## Orbital (historical reference)

The sections below describe the inherited Redox design, feature list and old
porting paths. They have not been verified as working on current Ferric Oxide
and are not supported desktop instructions. New graphics work follows the
planned Wayland stack after the backlog is merged.

Orbital combined a display server, window manager, wallpaper daemon, effects
compositor, image viewer and basic terminal.

The original design favored a small software-rendering path and a dedicated
protocol. Ferric Oxide's replacement direction prioritizes existing Wayland
applications and toolkits, with Mesa and hardware acceleration.

### Historical features

The inherited feature list included the following; it is not a record of
current Ferric validation:

- Custom Resolutions
- App Launcher (bottom bar)
- File Manager
- Text Editor
- Calculator
- Terminal Emulator

The original desktop used the **Super** key to show its keyboard shortcuts.

### Historical library backends

The inherited porting notes listed Orbital backends for these libraries. Their
current applicability has not been verified:

- winit
- softbuffer
- Slint (through winit and softbuffer)
- Iced (through winit and softbuffer)
- egui (winit or SDL2 can be used)
- SDL1.2
- SDL2
- Mesa3D's OSMesa

### Historical security design

Orbital intended to isolate clients from other windows' input and content.
This describes the original design, not a current Ferric security-validation
result.

### Legacy client interfaces

Orbital clients used [orbclient](https://gitlab.redox-os.org/redox-os/orbclient)
to create and draw windows.

#### Historical examples

The inherited [orbclient examples](https://gitlab.redox-os.org/redox-os/orbclient/-/tree/master/examples)
illustrate that interface. They are references for understanding old ports,
not verified Ferric examples.

### Archived porting notes

Old Rust ports used Orbital backends in libraries such as `winit`; C and C++
ports could use [liborbital](https://gitlab.redox-os.org/redox-os/liborbital).
These notes explain the dependencies of inherited ports. They are not a
recommendation to adapt new applications to Orbital or a claim that those
ports work today. Future application integration targets Wayland and Xwayland
as part of the deferred Phase 6 work.
