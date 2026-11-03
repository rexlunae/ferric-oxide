# Windowing and Graphics

Orbital remains the inherited default desktop, but is deprecated for new
Ferric Oxide graphics development. The replacement direction is Wayland with
Smithay, Mesa and Xwayland, as described in [Phase 6 of the roadmap](./ferric-roadmap.md#phase-6-graphics-and-desktop).
COSMIC remains an optional desktop choice to support. Its experimental image
configuration currently uses Orbital to host a nested compositor; this is a
build bring-up path, and the accelerated display stack remains future work.
Orbital is not planned to be brought up to a working or supported state as a
standalone desktop; its inherited recipes and defaults may not work.

## Planned native graphics stack

The direction agreed on 31 October 2026 is a small native Wayland compositor
built with Smithay, backed by Ferric's Rust display and input services. The
requirements are existing protocols and toolkit support, Rust-oriented APIs,
reuse of license-compatible graphics drivers, and a graphical boot path with
few dependencies. Both Wayland and X11 application support are required.
This section describes planned work; the nested COSMIC build below is the
current integration path.

### Compositor and application protocols

Use [Smithay](https://github.com/Smithay/smithay) as the MIT-licensed Rust
compositor foundation, with Ferric-specific platform integration and a minimal
window-management policy. Rust applications can use existing Wayland crates
and toolkits; applications in other languages use the same protocols. A new
Ferric-specific application protocol is not part of this design.

Provide native Wayland and integrated rootless
[Xwayland](https://wayland.freedesktop.org/docs/book/Xwayland.html), so X11
windows can share the desktop with Wayland windows. Validate input, focus,
clipboard, drag-and-drop, resizing and fullscreen behavior across the two
protocols. Start Xwayland on demand in profiles that enable it, keeping it
outside the early-boot dependency chain.

Keep COSMIC as an optional complete desktop session. It uses its own
compositor; share the Ferric platform integration where practical and arrange
an explicit display/input ownership handoff for that session. The current
COSMIC-inside-Orbital nesting remains build scaffolding. A complete COSMIC
shell, panel and applet set is separate integration work.

Useful license-compatible Orbital code may supply reusable platform or drawing
pieces. Evolving that code into the new minimal layer is acceptable, but the
inherited Orbital protocol and desktop are not compatibility requirements.

### From graphical boot to a session

Aim for one small compositor tool and codebase covering early userspace boot,
recovery, a greeter and a minimal desktop. Start it from initfs when display
and input services are available, using the firmware framebuffer and
Smithay's [Pixman renderer](https://smithay.github.io/smithay/smithay/backend/renderer/pixman/index.html).
The boot path should not require Mesa, LLVM, a desktop D-Bus session or the
Linux compatibility runtime. Smithay's optional Linux backends can be replaced
by [native display, input and session adapters](https://docs.rs/smithay/latest/smithay/backend/).
Drivers remain separate services in the microkernel architecture.

The UEFI bootloader still handles the pre-kernel graphical environment. Define
framebuffer ownership and presentation handoff from firmware/bootloader to
userspace, from the boot framebuffer to an accelerated driver, and between
the greeter and user sessions. Preserve display continuity where possible;
allow explicit restarts or ownership transfers at security boundaries. A
single process spanning firmware and every desktop session is not required.

Use software composition for boot, recovery and initial validation. Hardware
acceleration remains a separate required desktop milestone. Optional desktop,
Xwayland and driver choices must enter `features.toml` through `ferric-config`
as they are implemented; this design adds no configuration keys or defaults.

### Rendering and interchangeable driver backends

Use Mesa for EGL, OpenGL/OpenGL ES and Vulkan. Bring up native virtio-gpu in
QEMU/KVM, with [VirGL](https://docs.mesa3d.org/drivers/virgl.html) for OpenGL
followed by [Venus](https://docs.mesa3d.org/drivers/venus.html) for Vulkan.
Check host and guest requirements and verify actual host GPU use separately
from software rendering. Bare-metal enablement proceeds one GPU family at a
time.

Native Rust drivers, compatibility-hosted drivers and eventual driver VMs
should expose common native display, buffer and synchronization contracts.
Extend the existing DRM-style display/dumb-buffer plumbing in `driver-graphics`
and `graphics-ipc`; it is not a complete accelerated GPU interface. Mesa also
needs the selected driver's memory management, command submission, buffer
sharing and fence interfaces. Grant-based sharing needs graphics-specific
formats, ownership and synchronization semantics.

A LinuxKPI-style runtime is an option for recompiling selected driver sources
against versioned APIs. It must reproduce the driver's execution assumptions,
including locking, interrupt context, work queues, memory ordering, DMA
lifetimes and recovery. A Linux driver VM is another option. Neither exists
today; selection and licensing remain open. Reuse must meet the repository's
license policy: [Mesa is mostly MIT-licensed](https://docs.mesa3d.org/license.html),
but individual components and driver dependencies need review. Hosting code
behind a shim does not change its license.

The planned [Linux userspace ABI](./ferric-linux-compat.md) will let supported
unmodified Linux applications reach this desktop through native Wayland or
Xwayland. Its syscall, filesystem and device compatibility must share Rust
mechanisms with relibc. GPU acceleration for those binaries also requires the
interfaces used by their userspace drivers. Linux application compatibility
and kernel-driver compatibility have separate validation requirements.

### Bring-up and completion criteria

1. Establish native framebuffer presentation and keyboard/pointer delivery in a
   fullscreen Wayland session. Validate focus and resize handling, damage-based
   redraw and frame pacing; measure idle CPU use and input-to-display latency.
2. Exercise boot, recovery, greeter and session ownership transfers, including
   repeated teardown and restart. Integrate and validate Xwayland clients.
3. Add virtio-gpu acceleration and verify buffer sharing, synchronization and
   GPU use. Repeat application and latency checks with acceleration enabled.
4. Enable a reference bare-metal GPU backend, integrate the optional COSMIC
   session, and test the declared Linux ABI profile with graphical applications.

Track results and gaps against [Phase 6](./ferric-roadmap.md#phase-6-graphics-and-desktop).
These are validation requirements, not claims that the current image passes.

## COSMIC build configuration

On x86_64, after the [native build setup](./ferric-build.md), select the existing image
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

On 31 October 2026, merged main
[`f91836459d4d`](https://github.com/rexlunae/ferric-oxide/commit/f91836459d4d4e504cfddddce83d9adce5f42fa0)
completed `COOKBOOK_MAKE_JOBS=6 cargo xtask build` for the x86_64 `wayland`
image. The run retained recipe caches and rebuilt both target and host LLVM,
Mesa, COSMIC applications and the selected Wayland examples. The target LLVM
build selected X86; the host build selected X86, AArch64, RISC-V and WebAssembly,
as required by the Mesa query wrapper.

The resulting image was 9,600,761,856 bytes, with SHA256
`6efa4ba34be3765896127421ee3154fbbe28b8c3ab99c0aa938b8afe6db6feb8`.
Its `image.cdx.json` CycloneDX SBOM contained 103 components and every package
selected explicitly by `config/wayland.toml`.

That image booted to login under headless KVM with four vCPUs and 4 GiB RAM.
The saved configuration kept `kernel.lpss_debug = true` and
`kernel.cpu.kpti = "always"`; the runtime banner confirmed KPTI enabled.
A 64 MiB regular-file write and piped transfer passed byte-count, SHA256 and
full file-comparison checks, and the exact Ion `dd-pipe` smoke fragment passed.
Snapshot disk writes preserved the image, whose checksum was verified again
after the guest check. The saved `ferric.toml` was unchanged.

This main commit includes the merged build, NVMe completion-publication, pipe
and LPSS startup corrections. First-failure logs are retained for the earlier
NVMe command-ID hang, truncated pipe transfer and early LPSS MMIO panic; see
[validation](./ferric-testing.md) for their host and guest regressions. The
separate profiler and AMD-Vi smoke checks remain evidence for their respective
configurations, rather than measurements of desktop performance.

The build validation above was headless. A subsequent manual graphical test
on 31 October 2026 displayed the libcosmic example's "Page 4" window inside
the nested compositor, but the user reported severe slowness and unresponsive
keyboard and pointer input. This is a partial rendering observation, not a
successful usability test or a measured performance result; the causes remain
unresolved.

With the default 2 GiB of `cargo xtask run`, the example then started only some
of the time. Loading it and `cosmic-comp`, debug builds of 436 and 392 MiB,
mapped both files whole, RedoxFS read and kept them (it now frees a file's
buffer once the file is unused,
[#373](https://github.com/rexlunae/ferric-oxide/issues/373)), and the guest
ran out of memory, which panics the kernel
([#372](https://github.com/rexlunae/ferric-oxide/issues/372)). `ld.so` now maps
only the part of an object that loading reads, 18 and 26 MiB for these two (see
[memory](./ferric-memory.md#loading-dynamic-objects)). On 1 November
2026 that image, booted three times under KVM with four vCPUs and 2 GiB from a
snapshot, panicked before the example appeared each time with main's `libc.so`.
With only `libc.so` replaced by this loader, built with the same features, all
three runs showed the example within 30 seconds and none panicked.

The installed `orbital-wayland` script selects `COSMIC_BACKEND=winit`, starts `cosmic-comp`
nested inside Orbital and then runs the libcosmic example. Mesa uses software
rendering; the compositor recipe still records performance and keyboard-input
limitations. This build result does not establish a usable graphical session,
hardware acceleration or the Phase 6 exit criteria. ARM64 COSMIC image
validation remains pending.

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
the wrapper with stand-in `llvm-config` programs. The host-tools workflow runs
it for changes to the wrapper, LLVM and Mesa recipes, or the fixture itself.

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
