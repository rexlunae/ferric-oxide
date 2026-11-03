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

The installer writes an image (or a disk) from a filesystem configuration:
```bash
cargo run --release -- --config config/desktop.toml disk.img
```
An example configuration can be found in [config/desktop.toml](./config/desktop.toml).
Unsupplied configuration will use the default. You can use the `general.prompt`
setting to prompt when configuration is not set.

### Command line

`redox_installer --help` lists every option. The command line is parsed with
[clap](https://crates.io/crates/clap), so an option's value may follow it as a
separate argument or after `=`, and options with a short form also take it
attached: `--config file.toml`, `--config=file.toml`, `-c file.toml` and
`-cfile.toml` are the same. The short forms are `-c` (`--config`), `-b`
(`--cookbook`), `-o` (`--output-config`) and `-l` (`--list-packages`). A
repeated option takes its last value. Unknown options, a missing or malformed
value (such as `--size` that is not a number), more than one disk path, and
`--fox-*` options without the option they depend on (`--fox`, or
`--fox-packages` for `--fox-locked` and `--fox-level`) are rejected with a
usage error before anything is read or written.

Without a disk path, and without `--list-packages`, `--filesystem-size` or
`--output-config`, the installer prints its help to standard error and exits
with status 1.

## Image size

When the output is an image file, the installer sizes it before writing
anything. It adds up what the configured packages and their dependencies take
on RedoxFS (from the archives in `<cookbook>/repo/<target>/`), the
configuration's own files, and headroom (a quarter more, plus 64 MiB and the
partitions), and builds the image at `general.filesystem_size` or that size,
whichever is larger. Packages a fox package set adds later (`--fox-packages`)
are not counted; they use the headroom.

`--size=<MiB>`, or `filesystem_size_exact = true` in the configuration, makes
the size exact instead: the installer fails before writing if the contents do
not fit, and resizes an existing image file to that size. A block-device output
keeps its size and must hold what the image needs.
`--filesystem-size --cookbook=<dir>` prints the size an image would be built at
(and fails the same way); without `--cookbook` it prints the configured value.
The [`image_size`](src/size.rs) function is the library entry point.

## Images without FUSE or root privileges

`--no-mount` builds a staging tree and writes it through the Rust RedoxFS API.
The tree is staged in a new private directory (`.redox_installer.staging.*`,
mode 0700) beside an output image file, and removed afterwards, also when the
install fails. It is as large as the image's contents, so it goes on the
filesystem that holds the image rather than in `/tmp`, which is often a small
RAM-backed tmpfs. A block device, or an image in a directory that is not
writable, stages in `TMPDIR` instead.
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

## Installing build tools

A recipe's `build.dev-dependencies` are tools it runs while it builds; the
`host:` ones are built for the host by default, so a contributor needs them
installed. `general.build_tools` adds them to the package set this installation
places:

```toml
[general]
build_tools = { keep = true, list = "build/x86_64/desktop/build-tools/manifest.txt" }
```

`list` names a file with one tool recipe per line, as `repo build-tools-list`
writes it; `--build-tools-list <file>` sets the same path from the command line.
`keep` marks the tools as part of the image; the make build sets it from
`build.keep_build_tools`, and leaves it off for the scratch guest environment it
uses while building. An installation whose configuration has no
`general.build_tools` adds no tools.

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
