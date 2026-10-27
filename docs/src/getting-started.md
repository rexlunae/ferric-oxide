# Getting started

Ferric Oxide is under heavy development and has no releases yet, so there are
no downloadable images. You build an image from source and run it in QEMU,
which takes a few commands:

```sh
git clone https://github.com/rexlunae/ferric-oxide
cd ferric-oxide
cargo xtask setup
cargo xtask build
cargo xtask run
```

The pages in this chapter cover:

- [Building Ferric Oxide](./building-redox.md): the prerequisites and the
  native Cargo build, which is the recommended path.
- [Running in a Virtual Machine](./running-vm.md): running the image in QEMU,
  with or without a display.
- [Running on Real Hardware](./real-hardware.md): writing a live image to a USB
  drive and booting it.
- [Installing](./installing.md): installing onto a disk.
- [Trying Out the System](./trying-out-redox.md) and [Tasks](./tasks.md):
  things to do once it boots.

[Build and Configuration](./ferric-build.md) is the full reference for the
build, including cross-architecture builds, CPU variants and feature
configuration.

> ⚠️ **Warning:** Ferric Oxide is experimental software. Back up your data
> before running it on real hardware, and expect bugs.
