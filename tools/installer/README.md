# Redox OS installer

The Redox installer will allow you to produce a Redox OS image. You will
be able to specify:
- Output device (raw image, ISO, QEMU, VirtualBox, drive)
- Filesystem
- Included packages
- Method of installation (from source, from binary)
- User accounts

You will be prompted to install dependencies, based on your OS and method of
installation. The easiest method is to install from binaries.

## Usage

It is recommended to compile with `cargo`, in release mode:
```bash
cargo build --release
```

By default, you will be prompted to supply configuration options. You can
use the scripted mode by supplying a configuration file:
```bash
cargo run --release -- config/desktop.toml
```
An example configuration can be found in [config/desktop.toml](./config/desktop.toml).
Unsuplied configuration will use the default. You can use the `general.prompt`
setting to prompt when configuration is not set. Multiple configurations can
be specified, they will be built in order.

## Images without FUSE or root privileges

`--no-mount` builds a staging tree and writes it through the Rust RedoxFS API.
Guest ownership is recorded during installation and applied inside the image;
it does not require changing host file ownership. Package files default to
guest root, while explicit file declarations and account homes retain their
configured IDs. Repeated declarations update only explicitly supplied UID/GID
fields; omitted fields retain the recorded owner. Permissions, including private
user homes and `/etc/shadow`,
are preserved. This path needs neither sudo nor macFUSE on macOS.

Library callers can use `install_dir_unprivileged` and the returned
`ImageOwnership::archive`. Ordinary directory installation keeps its existing
host ownership behavior. `with_redoxfs_ar` also retains its existing behavior
of archiving host metadata.

## Finishing an image with fox (Ferric Oxide)

The installer places packages with fox's store (`crates/fox-store`): each
configured package and its dependencies come from the cookbook's
`repo/<target>/`, verified against `build/id_ed25519.pub.toml`, and are
recorded in fox's database in the image. That key is installed as
`/etc/fox/keys/cookbook.toml`. Without `--cookbook` no packages can be
installed.

`--fox=<host fox binary>` runs `fox` against the finished image root, after
the packages, files and accounts are in place:

```bash
redox_installer image.img -c config/desktop.toml --cookbook=. \
    --fox=target/release/fox --fox-sbom=build/image.cdx.json \
    --fox-packages=config/packages.toml --fox-locked
```

With `--fox-packages` it runs `fox sync --config <set> --target <image target>`
(`--fox-locked` adds `--locked`), then, with `--fox-sbom`,
`fox sbom --system`. A failing step fails the installation. The make
build passes these options from `ferric.toml`; see the package tooling
chapter of the book (`docs/src/ferric-packaging.md`, "Target images").
Library users set `Config::fox` to a `FoxImage`.

## Embedding

The installer can also be used inside of other crates, as a library:

```toml
# Cargo.toml
[dependencies]
redox_installer = "0.1"
```

```rust
// src/main.rs
extern crate redox_installer;

fn main() {
    let mut config = redox_installer::Config::default();
    ...
    redox_installer::install(config);
}
```
