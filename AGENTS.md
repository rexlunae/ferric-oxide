# AGENTS.md

Guidance for AI coding agents (and humans) working in the Ferric Oxide repository. Read this before making changes. It sits beside [CONTRIBUTING.md](CONTRIBUTING.md) and the [AI Contribution Policy](AI_POLICY.md); where they overlap, all three apply.

## What this project is

Ferric Oxide is a fork of [Redox OS](https://www.redox-os.org/): a microkernel operating system written in Rust, aimed at high performance on modern hardware. This repository started as the Redox build system and is becoming a monorepo that holds the kernel, bootloader, relibc, user space and build tools together.

## Repository layout

The repository is moving to the layout below. Parts of it do not exist yet; put new code where it will live in this layout, not where the old one would have put it.

| Path | What it is | Cargo workspace |
| --- | --- | --- |
| `Cargo.toml` (root) | Host workspace: build tools that run on the developer machine | host |
| `tools/cookbook/` | The recipe engine and `repo` binary | host |
| `tools/ferric-config/` | `make menuconfig` / TOML feature tool | host |
| `tools/installer/`, `tools/redoxfs-host/` | Image installer and host-side RedoxFS tools | host |
| `crates/kernel/` | Kernel and memory manager, `no_std`, custom target. Upstream subtree | kernel |
| `crates/bootloader/` | UEFI bootloader. Upstream subtree | bootloader |
| `crates/relibc/` | C library. Upstream subtree | relibc |
| `crates/` (others) | Shared libraries used by several workspaces (`ferric-nospec`, `ferric-hwrand`, later `ferric-abi`, …), `no_std` where possible | pulled in by path |
| `userspace/` | Daemons, drivers, file system, new Ferric Oxide services. `base/` and `redoxfs/` are upstream subtrees | userspace |
| `recipes/`, `config/`, `mk/` | Package recipes, image configs, Makefile fragments | — |
| `features.toml` | Catalog of build-time feature options | — |
| `docs/` | The book. Upstream subtree | — |

Each target (host, kernel, UEFI, `x86_64-unknown-redox`) has its own workspace because a Cargo workspace has one target, one lockfile and one set of profiles. Code shared between workspaces goes in `crates/` and is depended on by path (from `crates/kernel/` a shared crate is `../<name>`). Keep Rust crates out of the repository root: new crates go in `crates/`, `tools/` or `userspace/`. Do not merge the workspaces into one.

## Non-negotiable project rules

- **Licensing.** All code is MIT. Keep existing copyright notices, including `Copyright (c) 2016 Redox OS Developers` in [LICENSE](LICENSE). Only add dependencies under MIT-compatible licenses (MIT, Apache-2.0, BSD, Zlib, ISC, Unicode). No GPL, LGPL or AGPL code or crates.
- **Clean-room Linux compatibility.** Linux file systems (ext4, XFS, LVM, LUKS) and other Linux-compatible features are written from published format specifications and documentation, never ported or translated from GPL source. Do not read Linux kernel source while implementing them, and say which specification you used in the module docs.
- **Keep internal Redox names.** The `redox` target triple, `relibc`, `RedoxFS`, `redox_syscall`, scheme names and existing paths stay as they are so upstream changes keep merging. The rebrand is user-facing only. Do not use the Redox name or logo for the fork itself (see [TRADEMARK.md](TRADEMARK.md)).
- **Never send AI-generated work upstream.** Redox bans LLM-generated contributions. Do not open merge requests, issues or comments on gitlab.redox-os.org or the redox-os GitHub mirrors with AI-written content.
- **Respect upstream subtrees.** `crates/kernel/`, `crates/bootloader/`, `crates/relibc/`, `userspace/base/`, `userspace/redoxfs/` and `docs/` are synced from upstream with `git subtree`. Do not move or rename files inside them, and keep changes there small and local. Put fork-only functionality in new crates under `crates/` or `userspace/` and call into it from the upstream tree.
- **Optional behaviour is a feature toggle.** Anything a user might want on or off (a driver tier, tracing, a CPU level, a file system) is an option in `features.toml`, chosen in `ferric.toml` through `tools/ferric-config`. Prefer adding an entry there over custom Makefile or environment-variable logic. Without a `ferric.toml` the build must behave like upstream.
- **CPU baseline.** User space targets x86-64-v3 by default; x86-64-v4 is an optional build. Do not add code paths for older CPUs, i586 or BIOS boot. Runtime-detected faster paths (AVX-512 and the like) are welcome behind feature detection.

## Rust conventions

### Don't repeat yourself; build reusable modules

- Before writing a helper, search the repository (`crates/`, the workspace you are in, and the upstream trees) for one that already exists. Extend it rather than writing a second copy.
- When the same logic appears in a second place, extract it. If it is needed by more than one workspace, it belongs in a crate under `crates/`.
- Design modules around one responsibility with a small public surface. Expose types and functions that other crates can use without knowing the internals, and keep everything else private (`pub(crate)` by default, `pub` only when needed).
- Shared crates should be `no_std` (with an optional `std` or `alloc` feature) whenever the code does not need the operating system, so the kernel, drivers and host tools can all use them.
- Prefer generic code over copy-pasted variants. Use traits for real abstraction points, not one-implementation interfaces.
- Use one definition for each ABI type, constant or table. Kernel/user-space interfaces live in `redox_syscall` or `ferric-abi`, never duplicated in each consumer.

### Use existing crates

- Use a well-maintained crate from crates.io instead of hand-writing what it already does: parsing (`serde`, `toml`, `nom`), CLI (`clap`), errors (`thiserror`), bit flags (`bitflags`), zero-copy layouts (`zerocopy` or `bytemuck`), synchronization (`spin`, `parking_lot`), logging (`log`, `tracing`), and so on.
- Check that a crate is maintained, widely used, license-compatible and, for kernel or driver code, usable in `no_std` without pulling in `std`.
- Prefer crates.io releases to git dependencies. Cloud and CI environments often cannot reach gitlab.redox-os.org, so new git dependencies on it are a last resort.
- Declare versions shared across a workspace in `[workspace.dependencies]` and use `dep.workspace = true` in member crates.
- Turn off default features you do not need (`default-features = false`), especially in `no_std` code.
- Do not add a dependency for a few lines of trivial code, and do not add two crates that do the same job.

### Document all public APIs

- Every public item (crate, module, type, trait, function, method, constant, macro, feature flag) has a doc comment. New crates start with `#![deny(missing_docs)]`; existing crates use at least `#![warn(missing_docs)]` once they are cleaned up.
- Each crate and each non-trivial module begins with a `//!` overview: what it is for, how its pieces fit together, and for ABI or format code, which specification it implements.
- Function docs say what the function does and include these sections when they apply:
  - `# Errors`: which errors are returned and when.
  - `# Panics`: every way the function can panic.
  - `# Safety`: for every `unsafe fn` and `unsafe trait`, the exact invariants the caller must uphold.
- Add an example (`# Examples`) for the main entry points of a crate. Examples are doctests and must compile and pass.
- Document why, not what, in comments inside function bodies. Do not leave commented-out code.

### Errors stay typed

- Never convert an error into a `String` except to show it to a person. `map_err(|e| e.to_string())`, `format!` into an error, `Result<T, String>` and `Box<dyn Error>` built from strings are not allowed in library or daemon code.
- Each library crate or module defines its own error enum with one variant per failure the caller might handle differently. Use `thiserror` where it is available (it supports `no_std`); otherwise implement `core::fmt::Display` and `core::error::Error` by hand.
- Keep the underlying error as the source (`#[source]` or `#[from]` with `thiserror`) so the chain is never lost.
- Carry context as typed fields, not formatted text: `Io { path: PathBuf, #[source] source: io::Error }` rather than `format!("{}: {e}", path.display())`.
- Map an error into a different type only at a real boundary (for example, a scheme handler turning an internal error into `syscall::Error` with the right errno), and do the mapping in one place, with `From` or a dedicated function.
- Turning errors into text is the job of the outermost layer of a binary: `main`, a TUI status line, a log statement. Even there, print the full source chain.
- Do not use `unwrap()` or `expect()` on fallible operations in library, kernel or daemon code. They are fine in tests, and `expect("reason")` is acceptable for invariants that cannot fail, with the reason saying why.
- Existing code that returns string errors (for example `tools/ferric-config`) predates this rule. Convert it to typed errors when you touch it.

### Unsafe code

- Keep `unsafe` blocks as small as possible and put a `// SAFETY:` comment on each one explaining why the invariants hold.
- Wrap unsafe operations in safe abstractions inside a dedicated module, so the rest of the code stays safe.
- Enable `#![deny(unsafe_op_in_unsafe_fn)]` in new crates. Crates that need no unsafe code declare `#![forbid(unsafe_code)]`.
- Drivers that run in the kernel address space (the colocated driver tier) are safe Rust only: no `unsafe` in the driver crate itself.

### Style and structure

- Use the Rust 2024 edition for new crates, the toolchain in [rust-toolchain.toml](rust-toolchain.toml), and standard naming (`snake_case` functions and modules, `CamelCase` types, `SCREAMING_SNAKE_CASE` constants).
- Make invalid states unrepresentable: newtypes for IDs, addresses and units (`PhysAddr`, `Pid`), enums instead of boolean flags or magic integers, `#[non_exhaustive]` on public enums that may grow.
- Prefer borrowing and zero-copy over cloning on hot paths, and avoid allocation in interrupt handlers, the scheduler and I/O fast paths.
- Gate optional code with Cargo features that match the `features.toml` option, not with ad-hoc `cfg` flags or environment variables.
- Use `log` or `tracing` macros, not `println!`, for diagnostics outside of CLI output.
- Keep functions short enough to read in one screen. Split long `match` arms and deeply nested logic into named helpers.

### Tests

- Every change with behaviour comes with tests. Unit tests sit in a `#[cfg(test)] mod tests` beside the code; cross-crate behaviour goes in `tests/`.
- Put logic that can run on the host (parsers, allocators, format code, ring-buffer protocols) in host-testable crates so `cargo test` covers it without booting the OS.
- Tests for file system and format code include known-good images or byte fixtures.
- Performance changes include before-and-after benchmark numbers in the PR, as CONTRIBUTING.md asks.
- Never skip, ignore or delete a failing test to get CI green. Fix the cause, or explain in the PR why the test is wrong.

## Before you open a pull request

Run, in every workspace you changed:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo doc --no-deps   # must build without warnings
```

For the host workspace, CI (`.github/workflows/host-tools.yml`) runs `cargo fmt --all --check`, `cargo test --locked --workspace` and `cargo clippy --locked --all-targets -p ferric-config -- -D warnings` from the repository root. The cookbook is upstream code with many existing lint warnings, so clippy is not yet denied for it. If gitlab.redox-os.org is unreachable (as in cloud sessions), `git config --global url.https://github.com/redox-os/.insteadOf https://gitlab.redox-os.org/redox-os/` plus `CARGO_NET_GIT_FETCH_WITH_CLI=true` lets Cargo fetch the cookbook's git dependencies from the GitHub mirrors. For the kernel, CI (`.github/workflows/kernel-lints.yml`) runs `scripts/ferric-ci/kernel-lint-ratchet.sh`, which fails if any Clippy warning count or the number of `unsafe` uses goes above the baseline in `ci/kernel-lints.json`. Fix new warnings instead of raising the baseline, and when you remove some, run the script with `--update` and commit the lower baseline. Changes that affect the image (kernel, drivers, recipes, config) also need `make all` and a boot with `make qemu` where your environment allows it; say in the PR if you could not.

Then:

- Keep each PR focused on one change, and fill in the [pull request template](.github/pull_request_template.md), including the AI assistance section.
- Sign off each commit (`git commit -s`).
- Do not commit `ferric.toml`, `build/` or other generated output.
- Do not mix formatting-only changes in upstream subtrees with real changes; they make upstream merges conflict.
