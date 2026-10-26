# Quick Workflow

This page is a list of commands for common tasks, without much explanation.
Read [Building Ferric Oxide](./building-redox.md) and the
[Build System Reference](./build-system-reference.md) first; this page assumes
you know what each step does.

Run every command from the repository root.

## Setting up

| Task | Command |
| --- | --- |
| Get the source | `git clone https://github.com/rexlunae/ferric-oxide.git` |
| Check the host for required tools | `cargo xtask doctor` |
| Install the toolchain and write a default `ferric.toml` | `cargo xtask setup` |
| Update the source | `git pull` |
| Update Rust | `rustup update` |

## Configuring

| Task | Command |
| --- | --- |
| Change options in the terminal menu | `make menuconfig` |
| Set an option | `cargo run -q -p ferric-config -- set kernel.trace=true` |
| Explain an option | `cargo run -q -p ferric-config -- explain kernel.trace` |
| List what you changed from the defaults | `cargo run -q -p ferric-config -- list --changed` |
| Check the configuration | `cargo run -q -p ferric-config -- check` |

Package choices are in the image configuration (`config/<name>.toml`); to drop
a broken recipe from your image, comment out its line, for example
`#recipe-name = {}`.

## Building and running

| Task | Command |
| --- | --- |
| Build the image | `cargo xtask build` |
| Build and boot it in QEMU | `cargo xtask run` |
| Boot without rebuilding | `cargo xtask run --no-build` |
| Boot in the terminal, without a window | `cargo xtask run --headless` |
| Boot from an emulated NVMe disk, with 4 CPUs and 4 GiB | `cargo xtask run -- disk=nvme QEMU_SMP=4 QEMU_MEM=4096` |
| Build for ARM64 | `cargo xtask build --arch aarch64` |
| Build inside Podman | `cargo xtask build --podman` |
| Show the commands without running them | `cargo xtask build --dry-run` |
| Build a live ISO for real hardware | `cargo xtask build -- live=yes` |

## Working on recipes

| Task | Command |
| --- | --- |
| Rebuild one recipe and add it to the existing image | `make rp.<recipe>` |
| The same, after cleaning the recipe's build | `make crp.<recipe>` |
| Several recipes with their dependencies | `make rp.<recipe1>,<recipe2>,--with-package-deps` |
| Rebuild relibc | `make r.relibc` |
| Rebuild everything the image uses | `make rebuild` |

To put your own files in the image, place them in
`recipes/other/myfiles/source`, add `myfiles = {}` to the `[packages]` section
of your image configuration, and run `make rp.myfiles`.

## Cleaning up

| Task | Command |
| --- | --- |
| Remove build outputs and rebuild | `make clean` then `cargo xtask build` |
| Remove recipe sources and outputs too | `make distclean` then `cargo xtask build` |
| Remove the Podman container (Podman builds) | `make container_clean` |

## Saving a log

```sh
cargo xtask build 2>&1 | tee build.log
```

Attach the log when you [file an issue](./filing-issues.md).
