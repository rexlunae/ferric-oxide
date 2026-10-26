# Building with Podman

The [native Cargo build](./building-redox.md) is the recommended way to build
Ferric Oxide. A container build with [Podman](https://podman.io/) is
available as an option, for recipes that need the inherited GCC-based Linux
toolchain or a reproducible Debian build environment, and for some
specialized workflows such as the kernel debugger.

## Building

With a working rootless Podman installation:

```sh
cargo xtask --podman build
cargo xtask --podman run
```

`--podman` selects the inherited Linux recipe toolchain (`build.toolchain =
"recipe"`) and runs the recipe builds inside a Debian container that the build
creates on first use. On hosts without SELinux, the Cargo wrapper disables
SELinux volume labels.

`./podman_bootstrap.sh` remains as the older container setup script. Unlike
`cargo xtask setup`, it can install host packages, so read it before running
it.

## The Make interface

The container build can also be driven through Make directly, as in Redox:

```sh
make all PODMAN_BUILD=1
make qemu
```

`build.sh` is a shortcut that runs Make for a chosen architecture and image
configuration without saving the choice:

```sh
./build.sh -a x86_64 -c server qemu
./build.sh -A -c demo qemu
```

Settings saved in `ferric.toml` are the better way to make choices you want to
keep; see [Configuration Settings](./configuration-settings.md).

See [Advanced Podman Build](./advanced-podman-build.md) for how the container
is built, cleaned and debugged.
