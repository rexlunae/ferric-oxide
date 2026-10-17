# Ferric Oxide

**Ferric Oxide** is a full-featured, Rust-based operating system focused on performance. It is a community fork of [Redox OS](https://www.redox-os.org), and this repository is its **build system**: the configuration, recipes and tooling that assemble the kernel, drivers, libraries and applications into a bootable image.

> Ferric Oxide is an independent project. It is not affiliated with, endorsed by, or sponsored by the Redox OS nonprofit. "Redox OS" is a trademark of the Redox OS nonprofit; see [NOTICE.md](NOTICE.md) and [TRADEMARK.md](TRADEMARK.md).

[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

## What it is

Ferric Oxide inherits Redox's design: a microkernel operating system written in Rust, built to be reliable, secure, usable and free, and drawing on ideas from seL4, MINIX, Plan 9, Linux and BSD. It is a complete operating system rather than just a kernel, with its own file system, display server, core utilities and C library, and source compatibility with many Rust, Linux and BSD programs.

What Ferric Oxide adds on top of that foundation:

- **Performance first.** Kernel, scheduler, file system and network stack changes are judged by measured results, and regressions are treated as bugs.
- **AI-assisted development is welcome.** Contributions written with the help of AI tools are accepted under the same review bar as any other. Read the [AI Contribution Policy](AI_POLICY.md).
- **Developed on GitHub.** Issues, pull requests and discussion happen in this repository.

## Getting started

Ferric Oxide currently builds the same way as upstream Redox. The bootstrap script sets up a Podman-based build environment:

```sh
git clone https://github.com/rexlunae/ferric-oxide
cd ferric-oxide
./podman_bootstrap.sh
make all
```

If your system does not use SELinux, pass `USE_SELINUX=0` to `make all`. To boot the result in a virtual machine, run `make qemu`.

### Choosing features

`make menuconfig` opens a menu, much like the Linux kernel's, for choosing the target, the processor baseline (x86-64-v3 by default), kernel features, image contents and build settings. Your choices are saved in `ferric.toml`, a short TOML file that you or an agent can also edit directly or change with `ferric-config set`; every option is described in [`features.toml`](features.toml). Without a `ferric.toml` the build behaves exactly like upstream. See [tools/ferric-config](tools/ferric-config/README.md).

Until Ferric Oxide has its own book, the upstream Redox documentation is the reference for building and running the system:

- [Building with Podman](https://doc.redox-os.org/book/podman-build.html)
- [Build System Reference](https://doc.redox-os.org/book/build-system-reference.html)
- [Running in a Virtual Machine](https://doc.redox-os.org/book/running-vm.html) and on [Real Hardware](https://doc.redox-os.org/book/real-hardware.html)
- [Developer FAQ](https://doc.redox-os.org/book/developer-faq.html)

Internal names such as the `x86_64-unknown-redox` target, `relibc`, `RedoxFS` and the `redox` paths in the build system are unchanged for now, so that the fork keeps building and can keep pulling fixes from upstream.

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) and the [AI Contribution Policy](AI_POLICY.md). Hardware test reports go in [HARDWARE.md](HARDWARE.md).

## Components

The system is assembled from many repositories. These are currently pulled from upstream Redox; Ferric Oxide forks of them will be listed here as they are created.

| Component | Upstream repository |
|-----------|---------------------|
| Kernel | [redox-os/kernel](https://gitlab.redox-os.org/redox-os/kernel) |
| Base (essential system components and drivers) | [redox-os/base](https://gitlab.redox-os.org/redox-os/base) |
| RedoxFS (default file system) | [redox-os/redoxfs](https://gitlab.redox-os.org/redox-os/redoxfs) |
| relibc (C POSIX library written in Rust) | [redox-os/relibc](https://gitlab.redox-os.org/redox-os/relibc) |
| Bootloader | [redox-os/bootloader](https://gitlab.redox-os.org/redox-os/bootloader) |
| Ion (default shell) | [redox-os/ion](https://gitlab.redox-os.org/redox-os/ion) |
| pkgutils (package manager) | [redox-os/pkgutils](https://gitlab.redox-os.org/redox-os/pkgutils) |
| Orbital (display server and window manager) | [redox-os/orbital](https://gitlab.redox-os.org/redox-os/orbital) |
| Redoxer (development tool for Linux hosts) | [redox-os/redoxer](https://gitlab.redox-os.org/redox-os/redoxer) |
| Book | [redox-os/book](https://gitlab.redox-os.org/redox-os/book) |

## Credits

Ferric Oxide exists because of the years of work by the Redox OS developers and community, led by Jeremy Soller (@jackpot51). All upstream code remains under its original MIT license and copyright. If you want to support the operating system this project is built on, consider [donating to Redox OS](https://www.redox-os.org/donate/).
