# Introduction

This is the **Ferric Oxide** book. It covers the system's design and
philosophy, how it is built and configured, how it works inside, and how to
contribute.

The book has two kinds of chapter:

- The **Ferric Oxide System** chapters at the start describe what this
  repository implements today, with source paths, feature options and
  validation status. When another chapter disagrees with them, they win.
- The remaining chapters started as the [Redox OS](https://www.redox-os.org)
  book. They are being revised for Ferric Oxide, and many still describe
  inherited Redox components in detail. A page that has not been revised yet
  says so at the top; its paths, commands and hardware coverage may not match
  this checkout.

If you want to try the system, start with [Getting started](./getting-started.md).
If you want to contribute, read [Contributing](./contributing.md), the
[CONTRIBUTING.md](https://github.com/rexlunae/ferric-oxide/blob/main/CONTRIBUTING.md)
file and the [AI Contribution Policy](https://github.com/rexlunae/ferric-oxide/blob/main/AI_POLICY.md).
Where the project is going is in the [Roadmap](./ferric-roadmap.md).

## Conventions

### Notices

The following notices are commonly used throughout the book to convey noteworthy information:

| Notice     | Meaning |
|:---------- |:------- |
| 🛈 **Info**    | Provides neutral information to deliver key facts. |
| 📝 **Note**    | Provides information to enhance understanding. |
| 💡 **Tip**     | Offers suggestions to optimize an experience. |
| ⚠️ **Warning** | Highlights potential risks or mistakes. |

### Implemented, prepared and planned

Ferric Oxide is changing quickly, so the book is careful about tense:

- **Implemented** means the code and its integration exist in this
  repository. It does not mean every machine has been tested or that the
  feature is on by default.
- **Prepared** means interfaces, formats or libraries exist and are tested on
  the host, but nothing in a running system uses them yet.
- **Planned** means the [Roadmap](./ferric-roadmap.md) describes it and no
  code exists yet.

Many implemented features are optional. They are listed in `features.toml`
and chosen in `ferric.toml` (see [Build and configuration](./ferric-build.md)).

## What is Ferric Oxide?

Ferric Oxide is a general-purpose operating system written in
[Rust](https://www.rust-lang.org), built for performance on modern hardware.
It is a microkernel system: drivers, file systems and network stacks run as
isolated user-space services, and the kernel manages memory, scheduling and
communication between them.

It is a complete operating system rather than a kernel alone. It has its own
bootloader, file system (RedoxFS), C library (relibc), display server
(Orbital), shell and core utilities, and it runs many Rust, C and C++ programs
with little or no porting.

### Origin

Ferric Oxide is a fork of Redox OS. Redox was started by Jeremy Soller in 2015
to find out whether an advanced operating system could be written in Rust. It
became a microkernel system with a unified resource API ("schemes") drawing on
MINIX, seL4 and Plan 9, and years of work by its developers and community
produced the kernel, file system, C library and user space that Ferric Oxide
builds on. All of that code remains under its original MIT license and
copyright.

Ferric Oxide was forked in October 2026 for two reasons. The first is
direction: it targets modern x86-64 and ARM64 machines only, and is
willing to make large structural changes to the kernel and I/O path in pursuit
of measured performance. The second is process: Redox does not accept
contributions written with AI tools, and Ferric Oxide does, under the same
review bar as any other change. [Why Ferric Oxide?](./why-a-new-os.md)
explains both in more detail.

Ferric Oxide is an independent project. It is not affiliated with, endorsed
by or sponsored by the Redox OS nonprofit, and "Redox OS" is a trademark of
that nonprofit. The fork's connection to Redox is historical: nothing is
synced from Redox. Internal names such as the `x86_64-unknown-redox` target,
`relibc`, `RedoxFS` and the scheme names are kept because the Rust targets and
existing tools depend on them.

This book is written so that you don't need prior knowledge of Rust or OS
development to follow it.
