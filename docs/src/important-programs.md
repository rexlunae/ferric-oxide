# Important Programs

> ⚠️ **Warning:** The status below was inherited from the Redox OS book (August 2026) and describes Redox images. It has not been re-tested on Ferric Oxide, whose x86-64-v3 baseline, kernel changes and package tooling can change the results. Report what you find, with the commit you tested, in an issue.

This page lists important programs and libraries known to work on the system and how well they were tested. The system is designed to be source-compatible with POSIX and Linux applications, usually needing only recompilation or small patches.

Programs are cross-compiled by the recipe engine (the cookbook, in `tools/cookbook/`), whose recipes in `recipes/` hold the patches and forks each program needs. The `native` build profile installs most applications as prebuilt packages from the cookbook's configured remote (see [Prebuilt packages](./ferric-packaging.md#prebuilt-packages-repo_binary)); without one they are built from source. CI still points that remote at Redox's package server, whose packages are built for Redox's baseline rather than x86-64-v3, until packages built and distributed through [`fox`](./ferric-packaging.md) replace them.

Everything on the following lists was tested on x86-64; other architectures may not have been tested yet.

## Compilers

The following compilers have been tested to build on Redox, but the runtime status varies:

| Name | Can cross-compile from Linux? | Can compile programs on Redox? | Compiled programs can run on Redox? | Works on Redox? |
|---|---|---|---|---|
| GCC 13 | Yes | Yes | Working Well | Yes |
| Rust 1.90 | Yes | Always Crashing | Working | Yes |
| Go 1.25 | Yes | Always Crashing | Always Crashing | No |
| Zig 0.15 | Not Known | Not Known | Not Known | No |

## Interpreters

The following interpreters have been tested to build on Redox, but the runtime status varies:

| Name | Can be compiled? | Works on Redox? |
|---|---|---|
| RustPython | Yes | Working Well |
| CPython 3.12 | Yes | Working Well |
| Lua 5.4 | Yes | Working |
| LuaJit 2.1 | Yes | Often Crashing |
| Node.js 24 | Yes | Working |
| PHP 8.4 | Yes | Mostly Well |

## GUI Libraries

Most GUI applications run on Orbital (the display server and window manager), which supports the following GUI libraries:

- Mesa3D (OpenGL and EGL, via liborbital)
- SDL1 and SDL2 (via Mesa3D)
- winit (via orbclient)
- X11 (via TWM, which is via liborbital)

## Applications

The following programs are well known to be working:

| Name | GUI Backend | Working? |
|---|--|---|
| COSMIC Editor | winit | Working Well |
| COSMIC Files | winit | Working Well |
| COSMIC Reader | winit | Working Well |
| COSMIC Store | winit | Not Tested |
| COSMIC Terminal | winit | Working Well |
| Netsurf | SDL1 | Working Well |
| FFplay | SDL2 | Working Well |
| Servo | winit | Often Crashes |
| xterm | X11 | Working Well |
| xeyes | X11 | Working Well |

### Emulators

| Name | GUI Backend | Working? |
|---|---|---|
| DOSBox | SDL1 | Working Well |
| Mednafen | SDL2 | Working Well |
| MGBA | SDL1 | Working Well |
| RVVM | SDL1 | Working Well |

### Games

| Name  | GUI Backend | Working? |
|---|---|---|
| Freeciv | SDL2 | Working Well |
| Gigalomania | SDL1 | Working Well |
| Neverball | SDL2 | Working Well |
| OpenTTD | SDL1 | Working Well |
| OpenTyrian | SDL2 | Working Well |
| Prboom | SDL2 | Working Well |
| Quakespam | SDL2 | Working Well |
| SpaceCadet Pinball | SDL2 | Working Well |

## Servers

| Name | Working? |
|---|---|
| OpenSSH Daemon | Working Well |
| Nginx | Working Well |
| Simple HTTP Server | Working Well |
| Python Static Server | Not Tested |
| PHP-FPM | Not Tested |

## CLI Tools

The following CLI tools are known to be working. Programs listed below may not include [core utilities](./side-projects.md):

### Terminal Shells

| Name | Working? |
|---|---|
| GNU Bash | Working Well |
| Ion | Working Well |
| Nushell | Working |
| Fish | Working |
| Zsh | Hangs |

### Text Editors

| Name | Working? |
|---|---|
| Kibi | Working Well |
| GNU Nano | Working Well |
| Vim | Working Well |
| Sodium | Working Well |
| Neovim | Crashes |

### System Monitors

| Name | Working? |
|---|---|
| Bottom | Working Well |
| Htop | Working Well |

### Development Tools

| Name | Working? |
|---|---|
| Git | Working Well |
| GNU Autotools | Not Tested |
| GNU Binutils | Working Well |
| GNU Grep | Working Well |
| GNU Make | Working Well |
| GNU Sed | Working Well |

### Media Tools

| Name | Working? |
|---|---|
| FFMPEG | Working Well |

### Archive Tools

| Name | Working? |
|---|---|
| Bzip2 | Working Well |
| Xz | Working Well |
| Xxhash | Working Well |
| Zstd | Working Well |

### Storage Tools

| Name | Working? |
|---|---|
| Ncdu | Not Tested |


### Network Tools

| Name | Working Status |
|---|---|
| OpenSSH Client | Working |
| Curl | Working Well |
| GoAccess | Working Well |
| Rsync | Working Well |
| SQLite | Not Tested |
| Wget | Working Well |

## Other Programs

The recipes under `recipes/` list every component and ported program the build knows about; `recipes/wip/` holds ports that are not finished.
