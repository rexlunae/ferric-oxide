# Windowing and Graphics

Orbital remains the inherited default desktop, but is deprecated for new
Ferric Oxide graphics development. The replacement direction is Wayland with
Smithay, Mesa and Xwayland, as described in [Phase 6 of the roadmap](./ferric-roadmap.md#phase-6-graphics-and-desktop).
Implementation is deferred until the current PR backlog is merged. Orbital is
not planned to be brought up to a working or supported state; existing recipes
and image defaults remain only as legacy artifacts and may not work.

## Drivers

### VESA (vesad)

vesad is not really a driver, it writes to a framebuffer given by firmware (via UEFI or BIOS software interrupts).

Because we don't have GPU drivers yet, we rely on what firmware gives to us.

### Intel GPUs

The `ihdgd` driver supports mode setting in Tiger Lake GPUs and later.

## GPUs

On Linux/BSDs, the kernel communication with GPU is done by the Linux DRM drivers (Direct Rendering Manager) and user-space applications use the `libdrm` library to access them, which Mesa3D drivers use to work (Mesa3D implements OpenGL/Vulkan drivers, DRM exposes the hardware interfaces).

That said, here a "Linux DRM driver" needs to be a user-space driver daemon which uses the system calls/schemes to communicate with the hardware.

Our windowing system (Orbital) support the user-space side of Linux DRM API (`libdrm` C library and Rust equivalent) to reduce user-space porting effort to use our GPU driver API.

For software rendering, the `mesa` recipe builds Mesa3D with a backend for this system (from the Redox project's [fork](https://gitlab.redox-os.org/redox-os/mesa)) to use LLVMPipe.

GPU drivers, with their buffers shared as capabilities through the registered-buffer grants, are planned for Phase 6 of the [roadmap](./ferric-roadmap.md#phase-6-graphics-and-desktop).

## Software Rendering

As we don't support 2D/3D hardware acceleration yet, we use [LLVMpipe](https://docs.mesa3d.org/drivers/llvmpipe.html) to support OpenGL and Vulkan applications for CPU rendering.

## Orbital

Orbital is the default desktop environment, inherited from Redox, which have a display server, window manager, wallpaper daemon, effects compositor, image viewer and basic terminal.

It was preferred over Wayland and X11 to avoid their requirements, dependencies, and limitations (EGL on Wayland and GLX on X11). Orbital was much more simple and easier for the young system API and had the fastest native software rendering performance.

### Comparison with X11/Wayland

This display server is more simple than X11 and Wayland making the porting task more quick and easy, it's not advanced like X11 and Wayland yet but enough to port most Linux/BSD programs.

Compared to Wayland, Orbital has one protocol server implementation (like X11 for a long time), while Wayland provides protocols for compositors and clients.

Also native software rendering on X11 and Wayland applications is rare, which would reduce performance by partially or fully using LLVMPipe on applications or user session.

### Features

- Custom Resolutions
- App Launcher (bottom bar)
- File Manager
- Text Editor
- Calculator
- Terminal Emulator

If you hold the **Super** key (generally the key with a Windows logo) it will show all keyboard shortcuts in a pop-up.

### Libraries

Programs using the following libraries can work on Orbital.

- winit
- softbuffer
- Slint (through winit and softbuffer)
- Iced (through winit and softbuffer)
- egui (winit or SDL2 can be used)
- SDL1.2
- SDL2
- Mesa3D's OSMesa

### Security

In Orbital a GUI program cannot read input events or the content (framebuffer) from windows of other GUI programs, like Wayland.

### Clients

Apps (or 'clients') create a window and draw to it by using the [orbclient](https://gitlab.redox-os.org/redox-os/orbclient)
client.

#### Client Examples

If you wish to see examples of client apps that use [orbclient](https://gitlab.redox-os.org/redox-os/orbclient)
to "talk" to Orbital and create windows and draw to them, then you can find some in [orbclient/examples](https://gitlab.redox-os.org/redox-os/orbclient/-/tree/master/examples)
folder.

### Porting

If you want to port a program to Orbital, see below:

- If the program is written in Rust it probably works on Orbital because the `winit` crate is used in most places, but there are programs that access X11 or Wayland directly. You need to port these programs to `winit` and merge to upstream.

- If the program is written in C or C++ and accesses X11 or Wayland directly, it must be ported to the [Orbital library](https://gitlab.redox-os.org/redox-os/liborbital).
