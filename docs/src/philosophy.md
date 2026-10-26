# Our Philosophy

## Free software, MIT licensed

Ferric Oxide is MIT licensed, like the Redox OS code it is built on, and keeps
Redox's copyright notices. The MIT license gives every user complete access to
the software: anyone may *inspect* it for vulnerabilities, *modify* it to fix
them, and *redistribute* the fixes.

The project is stricter than Redox about what goes into its own tree. All
code in this repository is MIT licensed, and new dependencies must use
MIT-compatible licenses (MIT, Apache-2.0, BSD, Zlib, ISC, Unicode). GPL, LGPL
and AGPL code and crates are not accepted. For the same reason, Linux
compatibility features such as the ext4, XFS, LVM and LUKS support are written
clean-room from published format specifications, without reading or
translating Linux kernel source.

This applies to the operating system itself, not to what runs on it. Programs
installed on Ferric Oxide may use any license, and the microkernel design means
even drivers can be distributed separately under their own terms.

## Measure, don't assume

Every performance claim comes with a benchmark and an account of how it was
measured. A change that might slow something down says so. Designs that are
attractive on paper, such as colocating drivers in the kernel's address space
or making WebAssembly the default userland, are gated on numbers: if they
don't pay off, they stay optional or get dropped.

The same rule applies to correctness. Documentation distinguishes what is
implemented from what is prepared or planned, and nobody reports a test,
boot or hardware result they didn't observe.

## Isolation first, then remove its cost

The microkernel's isolation is the point, not an obstacle. Rather than moving
services back into the kernel to make them fast, Ferric Oxide changes how data
moves: file descriptors stay the permission handles, while the bytes travel
through shared-memory rings, registered buffers and DMA fenced by an IOMMU.
Where code does move closer to the kernel (the planned colocated driver tier),
it stays isolated by safe Rust, protection keys and the IOMMU, and it is a
toggle that can be turned off.

## Optional means configurable

Anything a user might reasonably want on or off is a feature option in
`features.toml`, with its dependencies and defaults declared, chosen in
`ferric.toml` through `make menuconfig` or the `ferric-config` CLI. That
keeps experimental work in the main tree without forcing it on every image,
and lets one tree build server, workstation and test images. Without a
`ferric.toml`, the build behaves like upstream Redox.

## Standard tools over custom ones

The build uses Cargo and rustup wherever possible (`cargo xtask` is the entry
point), with as little custom Make and shell as the remaining recipe engine
allows. Shared code goes in reusable, documented `no_std`-first crates, and
crates.io dependencies are preferred over home-grown equivalents. Packages are
Cargo projects at signed git tags. See [Build and Configuration](./ferric-build.md).

## Independent, with credit where it's due

Ferric Oxide's connection to Redox is historical. The kernel, relibc,
bootloader, base services, RedoxFS and this book were imported with their
history, and are now maintained here; nothing is synced from Redox
automatically. When a Redox change is worth having, it is ported by hand like
any other change.

Internal names (the `redox` target triple, `relibc`, `RedoxFS`,
`redox_syscall`, scheme names and paths) stay unchanged, because the Rust
targets, existing tools and imported code depend on them. The rebrand is
user-facing: Ferric Oxide does not call itself Redox OS or use the Redox logo,
and it credits Redox wherever it builds on Redox's work.

## AI-assisted development, with a human accountable

Ferric Oxide accepts contributions written with the help of AI tools, under
the same review bar as any other change. A human understands and answers for
every change, every change is tested, and the documentation is updated with
the code. The [AI Contribution Policy](https://github.com/rexlunae/ferric-oxide/blob/main/AI_POLICY.md)
has the details, and the
[README](https://github.com/rexlunae/ferric-oxide/blob/main/README.md#statement-on-the-use-of-ai-in-this-project)
explains why the fork takes this position.

Redox does not accept AI-generated contributions, and that decision is
respected: AI-assisted work from this project is never sent upstream.
