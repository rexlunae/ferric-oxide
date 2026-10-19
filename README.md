# Ferric Oxide

**Ferric Oxide** is a full-featured, Rust-based operating system focused on performance. It is a fork of [Redox OS](https://www.redox-os.org), and this repository brings its build tools and core OS components together in a monorepo. Separate Cargo workspaces keep host tools, kernel, bootloader and user space on their own targets.

> Ferric Oxide is an independent project. It is not affiliated with, endorsed by, or sponsored by the Redox OS nonprofit. "Redox OS" is a trademark of the Redox OS nonprofit; see [NOTICE.md](NOTICE.md) and [TRADEMARK.md](TRADEMARK.md).

[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

## What it is

Ferric Oxide inherits Redox's design: a microkernel operating system written in Rust, built to be reliable, secure, usable and free, and drawing on ideas from seL4, MINIX, Plan 9, Linux and BSD. It is a complete operating system rather than just a kernel, with its own file system, display server, core utilities and C library, and source compatibility with many Rust, Linux and BSD programs.

What Ferric Oxide adds on top of that foundation:

- **Performance first.** Kernel, scheduler, file system and network stack changes are judged by measured results, and regressions are treated as bugs.
- **AI-assisted development is welcome.** Contributions written with the help of AI tools are accepted under the same review bar as any other. Read the [AI Contribution Policy](AI_POLICY.md).
- **Developed on GitHub.** Issues, pull requests and discussion happen in this repository.

## Statement on the Use of AI in this Project

The use of AI in FOSS projects is controversial and produces some very heated arguments, and since the Redox OS project's stance on AI is quite hardline against it, I felt that it was important to provide some context to the creation of this fork and my reasons for deciding to make it. I have no doubt that there will be many people who still hate it, and I am under no illusions that my comments will settle anything. But because narratives about AI are often so pervasively negative, and because so many of us are none-the-less somewhere in the middle on the issue, I think it's important to express my views openly without hiding behind a relatively obscure online identity.

I have been a Rust enthusiast for many years, and also interested in operating systems since I was a teenager, which was a very long time ago. I've experimented with OSes and other system software such as EFI, in C, and even in Python (sorta). I kinda like to dabble in a lot of different things, so I've written web frameworks, network tools, compilers, nothing important or that you would have heard about. Actually, probably the most successful, if you can call it that, thing I've ever written for public consumption was a byte-order crate from Rust called simple-endian, which was at one point forked for use in Redox, mostly because I was being a bad maintainer and not responding to issues. I actually wrote it because I was dissatisfied with the byte-order crates that exist for Rust, because they are a pain to work with and lead to code that's hard to read, and I thought using the Rust type system to handle endianness was more elegant. I guess in short a both work as a professional programmer, mostly for the public sector until recently, and also hobbyist coding just to try things out, without it ever being too serious. Although I have never been a serious contributor to an open source project, I have always tried to advance open source where I can.

### The Harms of AI should be Acknowledged and Contextualized, not Dismissed or Overhyped

Critics of AI often focus on the environmental impact, the use of electricity, the use of scarce water resources, ruining of the environment around hyper-scaler data centers, destruction of rare books, the proliferation of low-quality "slop" code. All of these concerns are legitimate to some degree, but a lot of them are a little bit overblown. The environmental impacts are real, but often exaggerated, and don't ever seem to ask if the value returned is worth the trade. AI isn't even the majority of data center resource use, and it isn't close. It may someday be, but one would assume that would come along with at least some return of value to justify it. It's true that some data centers have been built in a way that harms the local environment, although some of the damage (such as contaminated well water) is inherent to large development projects, and most people don't take a general anti-growth attitude, and a lot of it is temporary. We can and should criticize the destruction of rare books, and a lot of bad practices, like training on pirated materials, but it doesn't seem to me that these critiques get to the heart of the issue, and the line that AI is just a "stochastic parrot" has been undermined by the fact that it has now contributed significantly to the expansion of knowledge in science and mathematics, including the development of new medications.

Rust came about in an era when there was already a large and established software ecosystem worldwide. Redox is an impressive project, that exhibits a lot of skill in its creation, but one thing it is not is a production operating system. Maybe with enough time it will become that, but as AI has become more capable and more ubiquitous, it's become more and more difficult for a new project to catch up to the state of the art. I basically just want to have a Rust-based operating system that I can actually use. And I want to try out large ideas about operating systems bigger than I can manually code. I am hoping this project will serve as a place for people to build something that can actually take Rust to new places, using whatever tools can do that the best. And I'm hoping to build it in a way that facilitates a high-quality product even with AI.

### Our Hobby/Industry is Changing

Sometimes it seems like people who reject AI think that anyone who isn't aligned with them simply doesn't care about what they're producing. While it is true that there is a real problem with low-quality contributions, it is not true that that anyone using an AI simply doesn't care, or that all of what they are doing is low-effort. Speaking for myself, I do care quite a bit, and a lot of my effort comes from finding ways to make the agents produce higher quality code, better tested, with less input effort. It's easy to deride a concept like "context engineering", but the definition of engineering is something like "systematic and creative application of scientific principles, mathematics, and empirical evidence to design, build, and improve structures, machines, systems, and processes for practical human use". It does take skill and knowledge to be able to make an AI into a useful system, and to analyze the output, and adjust as you go to get better result.

Whether we like it or not, AI is not going away. I originally had to adopt AI because my work insisted on it. But as the models and tooling have improved, and as I've used them more, I've realized that it would be dishonest to deny that they are pretty good at writing better code faster, and reviewing code faster now. And I'm not alone. The Rust project itself uses it. Linus Torvalds has used it in the Linux kernel, Microsoft uses it extensively in Windows, Apple uses it to write their operating systems. Whether you're in an open source environment or not, if you are using a mainstream OS, you are probably using the products of AI, and you certainly are if you are using Rust. And meanwhile, frontier models like Mythos have been uncovering serious bugs in popular, security-focused operating systems such as OpenBSD, where it found a 27-year-old but in the TCP SACK implementation.

However you feel about AI, engaging with it in the current threat environment is becoming non-optional. Even operating systems that are reluctant to use it have allowed some AI contributions in, often with increased review requirements relative to other contributions. Outright, absolute rejection of AI is becoming a thing only hobbyist OSes can reasonably do. However, it is absolutely right, in my view, that the decisions of other developers and other projects about how they want to use or not use AI be respected. And that is why a fork is both necessary and appropriate.

## Getting started

Ferric Oxide currently builds the same way as upstream Redox. The bootstrap script sets up a Podman-based build environment:

```sh
git clone https://github.com/rexlunae/ferric-oxide
cd ferric-oxide
./podman_bootstrap.sh
make all
```

If your system does not use SELinux, pass `USE_SELINUX=0` to `make all`. To boot the result in a virtual machine, run `make qemu`.

### Choosing features

`make menuconfig` opens a menu, much like the Linux kernel's, for choosing the target, the processor baseline (x86-64-v3 by default), kernel features, image contents and build settings. Your choices are saved in `ferric.toml`, a short TOML file that you or an agent can also edit directly or change with `ferric-config set`; every option is described in [`features.toml`](features.toml). Without a `ferric.toml` the build behaves exactly like upstream. See [tools/ferric-config](tools/ferric-config/README.md).

Where the project is heading, phase by phase, is in the [roadmap](ROADMAP.md).

The in-tree [system documentation](docs/src/ferric-oxide.md) describes the implemented architecture, configuration, memory and CPU protection, registered buffers and DMA isolation, package tooling, and validation. See [docs/README.md](docs/README.md) to build or serve the book. The inherited Redox chapters remain useful background, alongside the upstream build guides:

- [Building with Podman](https://doc.redox-os.org/book/podman-build.html)
- [Build System Reference](https://doc.redox-os.org/book/build-system-reference.html)
- [Running in a Virtual Machine](https://doc.redox-os.org/book/running-vm.html) and on [Real Hardware](https://doc.redox-os.org/book/real-hardware.html)
- [Developer FAQ](https://doc.redox-os.org/book/developer-faq.html)

Internal names such as the `x86_64-unknown-redox` target, `relibc`, `RedoxFS` and the `redox` paths in the build system are unchanged for now, so that the fork keeps building and can keep pulling fixes from upstream.

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) and the [AI Contribution Policy](AI_POLICY.md). Update the relevant system documentation alongside implementation changes; [AGENTS.md](AGENTS.md) maps tasks to the useful documents. Hardware test reports go in [HARDWARE.md](HARDWARE.md).

## Components

The kernel, bootloader, relibc, base, RedoxFS and book are imported upstream trees. Host tools live under `tools/` and share the root Cargo workspace. Other packages still come from upstream recipes.

| Component | Upstream repository |
|-----------|---------------------|
| Kernel ([local](crates/kernel/)) | [redox-os/kernel](https://gitlab.redox-os.org/redox-os/kernel) |
| Base (essential system components and drivers) | [redox-os/base](https://gitlab.redox-os.org/redox-os/base) |
| RedoxFS (default file system) | [redox-os/redoxfs](https://gitlab.redox-os.org/redox-os/redoxfs) |
| relibc (C POSIX library written in Rust, [local](crates/relibc/)) | [redox-os/relibc](https://gitlab.redox-os.org/redox-os/relibc) |
| Bootloader ([local](crates/bootloader/)) | [redox-os/bootloader](https://gitlab.redox-os.org/redox-os/bootloader) |
| Ion (default shell) | [redox-os/ion](https://gitlab.redox-os.org/redox-os/ion) |
| pkgutils (package manager) | [redox-os/pkgutils](https://gitlab.redox-os.org/redox-os/pkgutils) |
| Orbital (display server and window manager) | [redox-os/orbital](https://gitlab.redox-os.org/redox-os/orbital) |
| Redoxer ([local host tool](tools/redoxer/)) | [redox-os/redoxer](https://gitlab.redox-os.org/redox-os/redoxer) |
| Installer ([local host and target source](tools/installer/)) | [redox-os/installer](https://gitlab.redox-os.org/redox-os/installer) |
| Book | [redox-os/book](https://gitlab.redox-os.org/redox-os/book) |

### Host workspace imports

The root workspace contains cookbook, ferric-config, installer, Redoxer, fox
and the host-testable shared crates listed in the [architecture guide](docs/src/ferric-architecture.md).
Cookbook uses the local installer and Redoxer libraries; `make fstools` installs
from `tools/installer`, and the installer recipe copies that same source for
Redox target builds. RedoxFS stays under `userspace/redoxfs` and builds separately
for each target.

The new sources are MIT-licensed snapshots of the revisions already pinned by
cookbook, preserving their copyright notices:

| Directory | Upstream revision |
| --- | --- |
| `tools/installer` | `b5a11310e1ac915494efea6d18e242966da74f3f` |
| `tools/redoxer` | `f50e0d7f490b60110ee6494a06d9b2f9981f8a5a` |

These two snapshots are not yet registered with the subtree-sync workflow.
Keep fork changes local so their upstream history can be attached when the
imports are committed. The root lockfile controls host builds; the installer's
upstream lockfile and ring patch remain for the separate target recipe build.
Its dependency versions also remain standalone for that recipe. The imported
installer uses Rust 2024, and both host libraries enforce public API documentation.
Redoxer's nested toolchain file is removed so host members use the root toolchain.
The installer GUI recipe independently fetches the complete pinned upstream
installer tree; the GUI is omitted from this host-tool import; Redoxer's example
remains standalone and is excluded from the host workspace.

Validate host changes with `cargo test --locked --workspace`. A build of the
installer recipe and a QEMU boot additionally validate the target path.

## Credits

Ferric Oxide exists because of the years of work by the Redox OS developers and community, led by Jeremy Soller (@jackpot51). All upstream code remains under its original MIT license and copyright. If you want to support the operating system this project is built on, consider [donating to Redox OS](https://www.redox-os.org/donate/).
