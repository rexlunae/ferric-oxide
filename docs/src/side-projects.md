# Related Projects

Ferric Oxide is a complete operating system, assembled from components in
this repository and from separately developed projects that the recipes build.

## In this repository

These were imported from Redox OS with their history, or written for Ferric
Oxide, and are maintained here:

| Component | Path | Origin |
| --- | --- | --- |
| Kernel | `crates/kernel/` | Redox |
| Bootloader (UEFI) | `crates/bootloader/` | Redox |
| relibc, the C library | `crates/relibc/` | Redox |
| Base services and drivers (`init`, `pcid`, `netstack`, storage, network, USB, input, audio and graphics drivers) | `userspace/base/` | Redox |
| RedoxFS, the default file system | `userspace/redoxfs/` | Redox |
| Cookbook, the recipe engine | `tools/cookbook/` | Redox |
| Installer and Redoxer | `tools/installer/`, `tools/redoxer/` | Redox |
| `fox`, `cargo fox` and `fox-registry`, the package tools | `tools/fox/`, `tools/cargo-fox/`, `tools/fox-registry/` | Ferric Oxide |
| `cargo xtask`, the native build entry point | `tools/ferric-build/` | Ferric Oxide |
| `ferric-config`, the feature configuration tool | `tools/ferric-config/` | Ferric Oxide |
| `ferric-trace`, the kernel trace tool | `tools/ferric-trace/` | Ferric Oxide |
| Shared libraries: IOMMU, registered buffers, rings, tracing, page cache, LUKS, LVM, JBD2, VMX/SVM formats, wasm ABI and more | `crates/` | Ferric Oxide |

[Architecture and Components](./ferric-architecture.md) describes how they fit
together.

## Built from recipes

Other components are still built from recipes that fetch their own source,
mostly from Redox's repositories. Among them:

- [Ion](https://gitlab.redox-os.org/redox-os/ion), the default shell.
- [Orbital](https://gitlab.redox-os.org/redox-os/orbital), the display server
  and window manager, with the [orbclient](https://gitlab.redox-os.org/redox-os/orbclient)
  client library.
- [coreutils](https://gitlab.redox-os.org/redox-os/coreutils),
  [extrautils](https://gitlab.redox-os.org/redox-os/extrautils) and
  [binutils](https://gitlab.redox-os.org/redox-os/binutils), small
  system-specific command-line tools such as `ps`, `free`, `dmesg` and `less`.
- [uutils/coreutils](https://github.com/uutils/coreutils), the cross-platform
  Rust rewrite of the GNU core utilities.
- [smoltcp](https://github.com/smoltcp-rs/smoltcp), the TCP/IP stack used by
  `netstack`.
- [winit](https://github.com/rust-windowing/winit), the window library used by
  the COSMIC applications.
- [pkgutils](https://gitlab.redox-os.org/redox-os/pkgutils), Redox's `pkg`
  package manager, which `fox` replaces on Ferric Oxide images.

Moving these sources away from live fetches from Redox, into this repository
or Ferric Oxide mirrors and builds distributed through `fox`, is in progress.
