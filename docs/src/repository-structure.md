# Repository Structure

Ferric Oxide is a monorepo: one repository holds the build system and the core
operating system. Applications are still built from recipes that fetch their
own source.

| Path | Contents |
| --- | --- |
| `crates/kernel/`, `crates/bootloader/`, `crates/relibc/` | The kernel, the UEFI bootloader and the C library, each with its own Cargo workspace |
| `crates/` (others) | Shared libraries, `no_std` where possible: IOMMU, registered buffers, rings, tracing, page cache, LUKS, LVM, `fox` and more |
| `userspace/base/` | Essential services and drivers (`init`, `pcid`, `netstack`, storage, network, USB, input, audio, graphics) |
| `userspace/redoxfs/` | RedoxFS, the default file system, and its host tools |
| `tools/` | Host tools: `cargo xtask` (`ferric-build`), `ferric-config`, the cookbook recipe engine, the installer, Redoxer, `fox`, `cargo fox`, `fox-registry` and `ferric-trace` |
| `recipes/` | Package recipes, one directory per package; `recipes/wip/` holds unfinished ports |
| `config/` | Image configurations: which packages and files go into each image |
| `features.toml` | The catalog of build-time feature options |
| `mk/`, `Makefile` | The Make layer that the recipe engine and image build still use |
| `docs/` | This book |
| `.github/` | CI workflows, issue and pull request templates |

[AGENTS.md](https://github.com/rexlunae/ferric-oxide/blob/main/AGENTS.md#repository-layout)
has the authoritative layout, and [Architecture and Components](./ferric-architecture.md)
explains how the pieces fit together.

## Separate workspaces

The host tools, kernel, bootloader, relibc and user space build for different
targets with different profiles, so they keep separate Cargo workspaces and
lockfiles. Code shared between them goes in `crates/` and is used by path.
Don't merge the workspaces into one.

## Origins

The kernel, bootloader, relibc, base services, RedoxFS and this book were
imported from Redox OS with their git history, and are now maintained here.
Nothing is synced from Redox; a Redox change that is worth having is ported by
hand in an ordinary pull request. Internal names such as the `redox` target
triple, `relibc` and `RedoxFS` are kept because the Rust targets and existing
tools depend on them.

## Recipes and their sources

A recipe (`recipes/<category>/<name>/recipe.toml`) says where a package's
source comes from and how to build it. Core components point at paths in this
repository. Many other recipes still fetch from Redox's GitLab, from other
upstream projects' repositories, or as tarballs; moving those away from live
Redox sources is in progress, as is distributing prebuilt packages through
[`fox`](./ferric-packaging.md) instead of Redox's package server.
