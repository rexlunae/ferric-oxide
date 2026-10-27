# The Build Process

Ferric Oxide builds with Cargo. `cargo xtask` (the `tools/ferric-build`
crate) is the entry point: it checks and installs the Rust toolchain
components, resolves the feature configuration in `ferric.toml`, drives the
recipe engine that builds each package, writes the disk image and boots it in
QEMU. Under it, a Make layer and the cookbook recipe engine inherited from
Redox still order and build the packages; replacing more of that layer with
Cargo tooling is ongoing.

- [Building Ferric Oxide](./building-redox.md): the native Cargo build.
- [Building with Podman](./podman-build.md): the optional container build.
- [Configuration Settings](./configuration-settings.md): `ferric.toml`,
  image configurations and the Make settings underneath.
- [Build System Reference](./build-system-reference.md): the Make targets and
  recipe commands.
- [What the Build Does](./build-phases.md): what each step does.
- [From Nothing To Hello World](./nothing-to-hello-world.md): a short
  walkthrough from a fresh checkout to a program running in the guest.
- [Working with ARM64](./aarch64.md), [Self-hosted Development](./self-hosted.md),
  [Advanced Podman Build](./advanced-podman-build.md),
  [Advanced Native Build](./advanced-build.md) and
  [Troubleshooting the Build](./troubleshooting.md).

[Build and Configuration](./ferric-build.md), in the system chapters, is the
authoritative reference; the pages here are tutorials and background.
