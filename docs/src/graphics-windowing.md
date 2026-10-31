# Windowing and Graphics

Orbital remains the inherited default desktop, but is deprecated for new
Ferric Oxide graphics development. The replacement direction is Wayland with
Smithay, Mesa and Xwayland, as described in [Phase 6 of the roadmap](./ferric-roadmap.md#phase-6-graphics-and-desktop).
Implementation is deferred until the current PR backlog is merged. Orbital is
not planned to be brought up to a working or supported state; existing recipes
and image defaults remain only as legacy artifacts and may not work.

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
