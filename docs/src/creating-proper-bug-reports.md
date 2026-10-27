# Creating Proper Bug Reports

If you find a problem that hasn't been reported, open a
[GitHub issue](https://github.com/rexlunae/ferric-oxide/issues/new/choose)
with the bug report template. A good report lets someone else reproduce the
problem without asking you anything.

## Before you file

- **Reproduce it on current `main`.** Pull the latest changes and rebuild; the
  bug may already be fixed.
- **Search** open and closed issues, and open pull requests, for the same
  problem.
- **Narrow it down.** Find the smallest set of steps that triggers it, and say
  how often it happens if it is intermittent.
- **Find the component.** A command usually comes from a recipe of the same
  name under `recipes/`. For Rust programs,
  `grep -rnw COMMAND --include Cargo.toml` in the source tree shows which crate
  provides a command. Problems in the kernel, drivers, base services, RedoxFS,
  the build or the package tools belong in this repository's issues. Problems
  in a ported program that also occur on other systems belong with that
  program's own project.

## What to include

- **The commit:** `git rev-parse HEAD`.
- **Your configuration:** your `ferric.toml` (or "none"), and the architecture
  and CPU variant you built (`cargo xtask --arch ...`, `--cpu-variant ...`).
- **The host:** operating system and version (`uname -a`), `rustc -V` and
  `rustup show` from the repository root, and your C compiler.
- **Where it runs:** QEMU (with `qemu-system-x86_64 --version` or the aarch64
  equivalent, and whether acceleration was on) or real hardware (the exact
  model).
- **Logs.** For a build failure, capture the whole build:

  ```sh
  cargo xtask build 2>&1 | tee build.log
  ```

  For a single recipe, `make r.RECIPE 2>&1 | tee RECIPE.log`.

  For a problem in the running system, boot with the serial console and keep
  its output:

  ```sh
  cargo xtask run --no-build --headless 2>&1 | tee qemu.log
  ```

  Log in, run the commands that show the bug, then shut down with `shutdown`
  (or quit QEMU with `Ctrl-a x` if shutdown hangs).

- **Traces, for kernel and performance problems.** If the image was built with
  kernel tracing, a `ferric-trace` capture or crash dump of the failure is
  often the fastest route to the cause. See [Kernel Tracing](./ferric-tracing.md).

Put logs in fenced code blocks, or attach them as files when they are long.

## After you file

Watch the issue and answer questions. If you find more information, such as a
simpler reproduction or the commit that introduced the bug (`git bisect`),
add it as a comment.
