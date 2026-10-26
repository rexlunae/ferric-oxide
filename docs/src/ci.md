# Continuous Integration

[Continuous integration](https://en.wikipedia.org/wiki/Continuous_integration)
builds and tests every change automatically, to catch build failures, bugs and
regressions before they land. This page covers Ferric Oxide's own CI on GitHub
Actions, and Redoxer, a tool for testing a single program in a virtual machine.

## Ferric Oxide's workflows

The workflows are in `.github/workflows/`. Every pull request runs the ones
that apply to it.

| Workflow | Runs on | What it does |
| --- | --- | --- |
| `host-tools.yml` | Pushes and pull requests | `cargo fmt --check`, the host workspace tests (including feature-enabled shared crates), strict Clippy and Rustdoc for the Ferric tools and libraries, and host RedoxFS tests |
| `kernel-lints.yml` | Pushes and pull requests | Keeps the kernel's Clippy warning counts and `unsafe` uses at or below `ci/kernel-lints.json`, and checks kernel relocations and IBT entry points |
| `image.yml` | Pushes to `main` and `ci/**`, pull requests, nightly | Builds the `ferric-smoke` image and boots it in QEMU several times, running the tests under `recipes/tests/`; also a crash-dump boot and the tracing overhead budget. The nightly run boots `ferric-fuzz` for a long system call fuzzing run |
| `cet-image.yml` | Pull requests, by hand | Builds and boots an image with supervisor CET enforcement |
| `profile-image.yml` | Pull requests that touch the profiler, by hand | Boots an image with the sampling profiler and draws a flamegraph |

[Validation and debugging](./ferric-testing.md) explains each check, the
image tests, and how to reproduce a CI boot locally. To try a CI change
without opening a pull request, push it to a branch named `ci/<something>`.

The image builds currently start from Redox's CI container and its prebuilt
packages, and build only the parts marked `source` in
`config/ferric-smoke.toml` from this repository. Replacing those prebuilts
with packages built by [`fox`](./ferric-packaging.md) is in progress.

## Testing a program with Redoxer

[Redoxer](https://github.com/rexlunae/ferric-oxide/tree/main/tools/redoxer)
builds a program for the `redox` target with Cargo and runs it inside a virtual
machine, without the full build system. Its source is in `tools/redoxer/`.

> ⚠️ **Warning:** Redoxer still downloads its toolchain, test image and
> libraries from the Redox project (`static.redox-os.org`) and its Docker
> image, so what it tests is a Redox system, not a Ferric Oxide one. Use it to
> check that a program builds and runs on the shared target; use the image
> tests above for anything that depends on Ferric Oxide's kernel. Moving
> Redoxer's downloads to `fox` builds is planned.

Refer to Redoxer's [README](https://github.com/rexlunae/ferric-oxide/tree/main/tools/redoxer#readme)
for its command line. This section assumes you have read it.

### Installing Redoxer

We provide two options to install Redoxer: Docker container or Cargo. To install via `redoxer` crate in `crates.io` :

```sh
cargo install redoxer
```

Then download the toolchain, QEMU image and test if installation works with `true` from `uutils` :

```sh
redoxer exec true
```

To install and run Redoxer via [the official Docker container](https://hub.docker.com/r/redoxos/redoxer):

```sh
docker run -ti --rm docker.io/redoxos/redoxer redoxer exec true
```

Compared to Redoxer via Cargo, the Docker variant is recommended for use in CI since it's **guaranteed to boot**, because the toolchain and QEMU image for testing is already inside it and tested before being published. The Docker variant is only available for `x86_64` Linux at this time.

All commands below will refer `redoxer` as the command to run Redoxer. For installations via Docker you need to prefix it using the `docker run` command or alias it in `.bashrc`:

```sh
alias redoxer='docker run -ti --rm -v ".:/mnt" -w /mnt -e REDOXER_SYSROOT --device /dev/kvm docker.io/redoxos/redoxer redoxer'
```

### Build and test a Rust project

Redoxer has a first-class integration with Cargo. Here's how to check and build a Rust project for the `redox` target:

```sh
redoxer check
```

```sh
redoxer build
```

Here's how to test a Rust project in a VM, which will run an actual test inside QEMU:

```sh
redoxer test
```

All Cargo commands are shorthanded; the complete list can be seen in the README or just run `redoxer`.

### Build and test a C or C++ project

Redoxer has a shorthand for `ar`, `cc`, `cxx` for the AR/GCC/G++ compiler. Here's how to build a single `main.c` file for the `redox` target:

```sh
redoxer cc main.c -o main
```

Here's how to test that single executable in a VM, which will run an actual test inside QEMU:

```sh
redoxer exec ./main
```

You can wrap Redoxer to the C and C++ compiler, which you can then use to compile larger C or C++ projects using GNU Make, CMake, Meson and so on:

```sh
export AR="redoxer ar"
export CC="redoxer cc"
export CXX="redoxer cxx"
```

### Building with native libraries

Sometimes a project needs native libraries such as OpenSSL or zlib. To have them included for compilation, run:

```sh
redoxer pkg openssl3 zlib
```

It will download and extract the compiled library to `target/$TARGET/sysroot` or configured from `REDOXER_SYSROOT`. After it does, all subsequent Redoxer calls will automatically be linked to them via `RUSTFLAGS`, `LDFLAGS`, etc.

To verify how the build works run the `redoxer env` command and examine how environment variables are produced. You can also debug it with a interactive GNU Bash using `redoxer env bash` :

```sh
[user@local ~]$ docker run -ti --rm docker.io/redoxos/redoxer redoxer env bash
root@20111589c5b4:/# $CC --version
x86_64-unknown-redox-gcc (GCC) 13.2.0
Copyright (C) 2023 Free Software Foundation, Inc.
This is free software; see the source for copying conditions.  There is NO
warranty; not even for MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
```

### Testing with additional files

To add files inside Redoxer use the `--folder` option, which will copy the contents of the specified directory path into `/root`:

- Add the contents of the current directory

```sh
redoxer exec --folder . true
```

You can also install more packages with `redoxer pkg`, then add those in a different folder other than the root:

```sh
REDOXER_SYSROOT=sysroot redoxer pkg curl
```

```sh
# note that the ending "/" in ./sysroot/usr/ matters (rsync behavior)
redoxer exec --folder ./sysroot/usr/:/usr curl --version
```

You can copy files from the QEMU image to the specified directory path with the `--artifact` option when execution is successful. It also can be combined with `--folder` :

- Add and copy the current directory back

```sh
redoxer exec --folder . --artifact . touch file
```

### CI templates for your own project

- GitHub CI template:

```yml
jobs:
  redox:
    runs-on: ubuntu-latest
    container: redoxos/redoxer
    env:
      REDOXER_QEMU_ARGS: "-smp 1"
    steps:
      - run: ln -sf /root/.redoxer ~/.redoxer
      - uses: actions/checkout@v6
      - run: redoxer test
```

- GitLab CI template:

```yml
redox_test:
  image: redoxos/redoxer
  variables:
    REDOXER_QEMU_ARGS: "-smp 1"
  script:
    - redoxer test
```

This solve two issues:

-  `REDOXER_QEMU_ARGS: "-smp 1"` ensures there's no flaky test caused by high contention in kernel multi-threading, which is an open issue to this day.
-  `ln -sf /root/.redoxer ~/.redoxer` fixes a missing toolchain and image problem because GitHub does not run as `/root`, but as `/github/home`. GitLab does not have this problem.
