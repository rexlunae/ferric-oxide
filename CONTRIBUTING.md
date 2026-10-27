# Contributing to Ferric Oxide

Thank you for your interest in Ferric Oxide. This document covers how to get set up, where help is most useful, and what we expect from a contribution.

## Code of Conduct

We follow the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct).

## License and contribution terms

Contributions are released under the [MIT License](LICENSE), the same license as the upstream Redox OS code this project is built on. Keep existing copyright notices intact when you edit or move code.

By opening a pull request you agree that:

- You understand your changes in full and will respond to review comments.
- You have read the [Developer Certificate of Origin](https://developercertificate.org/) and certify your contribution under it. Sign off your commits with `git commit -s`.

## AI-assisted contributions

AI-assisted contributions are welcome. The short version: a human is accountable for every change, the change is tested, and it meets the same bar as anything else. Read the full [AI Contribution Policy](AI_POLICY.md) before your first pull request.

## Where we work

Ferric Oxide is developed on GitHub.

- **Issues** are for bugs, build failures, hardware reports and feature proposals. Search for duplicates first and fill in the issue template.
- **Pull requests** are for changes. Use the pull request template and keep each PR focused on one thing.
- **Larger design changes** (kernel interfaces, schemes, the file system format, the build system layout) should start as an issue labelled `proposal` so the approach can be agreed before the code is written.

## Getting a build working

Use the native Cargo workflow described in the
[build guide](docs/src/ferric-build.md):

```sh
git clone https://github.com/rexlunae/ferric-oxide
cd ferric-oxide
cargo xtask setup
cargo xtask build
cargo xtask run --headless
```

Commands use the host architecture by default; add `--arch x86_64` or
`--arch aarch64` to setup, build and run for a different guest architecture.
Setup requires rustup and a native compiler/SDK (plus NASM for x86-64), and installs only Rust
components and Cargo utilities. QEMU is needed for `run`. Existing
`ferric.toml` files are preserved. Podman is an explicit option via
`cargo xtask --podman build`; the inherited recipe toolchain remains available
for recipes that need GCC-specific tools.

The book in `docs/` covers the rest of the build. These pages are the most useful:

- [Configuration Settings](docs/src/configuration-settings.md), including how to keep local recipe changes from being overwritten
- [Build System Reference](docs/src/build-system-reference.md)
- [Coding and Building](docs/src/coding-and-building.md), including how to update initfs after changing system components or drivers
- [Porting Applications](docs/src/porting-applications.md)
- [Developer FAQ](docs/src/developer-faq.md)

To work on a single component without the full build system, use [Redoxer](tools/redoxer/README.md).

## What a good pull request looks like

- **Small and focused.** Small PRs get reviewed quickly. If a large change is waiting too long, split it into steps that each build and work.
- **Explained.** Say what problem the change solves, how it solves it, and how you tested it. List the targets you built and booted.
- **Measured, for performance work.** Performance is the reason this fork exists, so changes that claim a speedup include before-and-after numbers and how they were measured. Changes that might slow something down say so.
- **Formatted.** Run `cargo fmt` on Rust code, and keep `cargo clippy` clean in the code you touch.
- **Signed off.** Each commit carries a `Signed-off-by` line (`git commit -s`).

## Debugging tips

- Log everything: `your-command 2>&1 | tee build.log`.
- If a bug is random or hard to reproduce, try `make qemu kvm=no` to see if it becomes more frequent.
- If a program fails to build or run, the missing piece is often in relibc, such as an unimplemented POSIX or Linux function.
- Test on more than one host distribution when a build problem looks environment-specific.

## Where help is most useful

**Without writing code:**

- Test images on real hardware and add the results to [HARDWARE.md](HARDWARE.md).
- Report bugs, and check whether old reported bugs still happen.
- Improve documentation, including the start of a Ferric Oxide book.

**With Rust or another language:**

- Port programs and libraries using the recipe system (see `recipes/wip` for work in progress).
- Write tests and benchmarks.
- Implement missing POSIX and Linux functions in relibc.
- Improve the package manager and build tooling.

**With systems or OS experience:**

- Profile and optimize the kernel, scheduler, file system and network stack.
- Write and improve device drivers.
- Clean up warnings and resolve `TODO`, `FIXME` and `UNOPTIMIZED` markers.
- Improve automated testing and continuous integration.

## Relationship to Redox OS

Ferric Oxide was forked from Redox OS and its imported trees keep their history, but the connection is historical: nothing is synced from Redox automatically. If a Redox change is worth having, port it in an ordinary pull request that says where it came from.

Internal names such as `redox` in target triples, paths and crate names stay as they are, because the Rust targets, existing tools and imported code depend on them; please don't rename them in unrelated pull requests.

If you fix something that also affects Redox OS and wrote the fix yourself, consider sending it upstream under their [contribution guidelines](https://gitlab.redox-os.org/redox-os/redox/-/blob/master/CONTRIBUTING.md). Redox OS does not accept LLM-generated contributions, so please do not send AI-generated work there.
