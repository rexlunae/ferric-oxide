# Ferric Oxide: implemented system

Ferric Oxide is a Rust microkernel operating system built on Redox OS. This
section describes the implementation in this repository, including the fork's
build tools and kernel changes. The rest of the book supplies the inherited
Redox background; its historical paths, hardware coverage and implementation
descriptions may differ from this checkout.

## Reading the system

| Task | Start here |
| --- | --- |
| Understand the components, workspaces and boot sequence | [Architecture](ferric-architecture.md) |
| Build an image, choose features or change a recipe | [Build and configuration](ferric-build.md) |
| Change CPU state, address spaces or kernel hardening | [Memory and CPU protection](ferric-memory.md) |
| Work on drivers, DMA, shared buffers or rings | [Registered buffers, IOMMUs and rings](ferric-io.md) |
| Trace system calls and scheduling, or add a tracepoint | [Kernel tracing](ferric-tracing.md) |
| Verify or install a local package, install one from a signed git release, generate its SBOM, build, attest and release a package from git with `cargo fox`, run a signed package registry with `fox-registry`, search and install from registries by name, audit installed packages against advisories, write a system SBOM, sync a root to a package set with a lock file, import cookbook recipes into a registry, or see how images are finished with `fox` (adoption, a package set and the image SBOM) | [Package tooling](ferric-packaging.md) |
| Find tests, reproduce a boot failure or understand CI | [Validation and debugging](ferric-testing.md) |
| See what is planned next and why | [Roadmap](ferric-roadmap.md) |

Source paths in these chapters are relative to the repository root. Rustdoc
beside the implementation defines the public APIs, error types and format
invariants. `features.toml` defines the available configuration options and
their dependencies; the chapters explain how those options fit together.

## Implementation status

"Implemented" below means that the code and its integration exist. It does not
claim that every machine has been tested, that every configured CI run has
passed, or that a feature is enabled in every image.

| Area | Implemented behavior | Boundary or remaining work |
| --- | --- | --- |
| Monorepo | Local kernel, bootloader, relibc, base services, RedoxFS, installer and Redoxer sources; shared host workspace | Other applications still come from recipes; target builds remain separate |
| Feature configuration | TOML catalog, interactive editor, CLI, validation and generation of Make, image and kernel settings | Catalog defaults apply through `ferric.toml`; no file preserves the upstream-style build path |
| CPU state | x86-64-v3 baseline for configured user-space builds; optional v4, AVX and AVX-512 state saving, boot checks | Prebuilt packages and toolchain libraries do not acquire local CPU flags |
| Memory protection | Physical-map and kernel-image KASLR, user ASLR, `PROT_NONE` guards, KPTI and PCID support | Address randomization uses RDRAND with a fixed-layout fallback; fixed user mappings are not relocated |
| CET | Optional supervisor IBT and shadow-stack enforcement with private frame pools | Shadow stacks require KPTI off; fresh CET hardware validation is pending (#78); user-mode CET remains off |
| DMA isolation | Intel VT-d and AMD-Vi DMA domains, driver binding, IOVA translation, revocation and an enforce policy | IOMMU defaults to `never`; AMD interrupt remapping is not implemented |
| Registered buffers | Anonymous-memory registration, byte grants, sub-grants, CPU mappings, page-aligned DMA grants with IOVA retirement on revoke, and `nvmed` DMA into an attached grant (`drivers.grant_dma`) | Off by default; partial pages use bounce buffers; disk grants are direct calls on `nvmed` descriptors, with no RedoxFS or ring support; other I/O is not converted |
| Interrupt delivery | Per-(CPU, vector) interrupt counts and events, and a root-only `irq:cpus` map of logical CPU, APIC id and NUMA node (`kernel.irq_percpu`) | Off by default; x86_64 only; `nvmed` can run one I/O queue pair and pinned worker per CPU (`drivers.nvme_percore`, off by default), but the disk scheme spreads requests round-robin, and serves reads and writes concurrently only under the `Workers` queue policy (on QEMU, spilling to per-CPU queues does not beat one queue); root can steer a vector to another CPU (`irq:` `Move` call; VT-d rewrites its remapping entry), but no balancer uses it yet |
| Rings | `kernel.ring` scheme (`kernel.rings`): shared SQ/CQ memory, brokered `Nop`, `Read`, `Write`, `Fsize`, `Fstat` and `Fsync` on any scheme in the caller's context, a fixed-file table, linked chains whose files are held from submission to completion, asynchronous chains on a per-ring kernel worker, `Enter` waiting for `min_complete` completions, CQ back-pressure and `EVENT_READ` notification ([I/O chapter](ferric-io.md)) | Off by default; one worker per ring runs asynchronous chains in order; no grant buffers, `SqPoll` or `Umwait` notification, or rings served natively by user schemes |
| Kernel tracing | Typed tracepoints as static keys (`syscall.enter`/`exit`, `sched.switch`, `scheme.request`/`response`/`cancel` with SQE tags), per-CPU rings, boot-time and live enable (also from an initfs `trace.toml` bootstrap applies before init), per-event argument filters (`filter <event> <arg> <op> <value> && ...` on `events`, listed in `filters`), a `kernel.trace` scheme, open by path to root and to others through `cap/read` or `cap/control` capabilities root hands over, with read-only mapping of the rings (`mmap` of `cpu/<n>`, `commit <end>`), a host text view and Perfetto JSON export (`tools/ferric-trace/`), captures that read the rings through that mapping where the kernel offers it and fall back to reads (`ferric-trace capture --via auto`), an optional crash dump that keeps a panic's message and newest records in reserved RAM across a warm reboot, with flight-recorder rings that overwrite their oldest records when full (`kernel.trace_crash`, read with `ferric-trace crash`), and a persistent kernel log whose end the next boot serves as `/scheme/sys/log_previous` (`kernel.log_persist`) | Off by default; x86_64 only; CI holds tracing to a 1% overhead budget compiled in but off (null system calls) and 5% with every event on (RedoxFS requests) |
| `fox` | Local signed archives, installed-file verification, in-place install/remove, CycloneDX SBOMs; replaces `pkg` on the target by default (`packages.manager`), with signed manifests packed by the cookbook; the image build adopts the installed packages with the host `fox`, syncs an optional package set (`packages.set`, `fox.lock`, `packages.locked`) and writes `image.cdx.json`, and a first-boot service adopts any packages still unadopted. `cargo fox` builds a git commit hermetically into a signed archive with a build SBOM and a signed attestation, which `fox verify` checks, and `cargo fox release` makes SSH-signed release tags; `fox install <git-url>` builds a signed release from source, pinning its author key on first use, or installs a prebuilt from an explicitly trusted builder. `fox-registry` creates and signs a git-hosted package registry (`fox-tuf/1`, a TUF subset), checks owners' release tags, builds attested artifacts in registry CI, lets independent rebuilders add matching attestations (`fox-registry rebuild`) and imports cookbook recipes as `recipe` packages; `fox registry`, `fox search`, `fox info` and `fox install <name>` use registries, installing prebuilt artifacts that enough registry builders attest to (the highest x86-64 level the CPU and `max-cpu-level` allow, v3 or v4) and otherwise building the owner-signed tag, under `etc/fox/policy.toml`; `fox audit` matches installed packages and the crates in their build SBOMs against registry and RustSec advisories, setting aside findings a registry's signed CycloneDX VEX statements clear, `fox sbom --system` describes a whole root, and `fox sync` installs a package set and pins it in `fox.lock` | no official registry is published; dependencies are not resolved; dependency solver or generation/rollback store; HTTPS fetching is not built for the target; builds are isolated by environment, not a network sandbox; the installer still places the configuration's packages with `pkg`'s library before `fox` adopts them; the image SBOM is not signed; no wasm variants |
| Page cache (prepared) | `ferric-pcache`: the planned `kernel.pcache` scheme's paths, calls and fill-queue records, the page state machine, per-description readahead, two-list CLOCK reclaim and the per-object reverse map, as a `no_std` crate ([I/O chapter](ferric-io.md#page-cache-prepared)) | Crate only, host-tested: the kernel has no `kernel.pcache` scheme yet and no file system fills from it, so reads still go through the file system's scheme; no `features.toml` option until that integration lands |
| LVM2 (prepared) | `ferric-lvm`: a `no_std`, read-only library that reads PV labels and metadata areas (checksums, wrapped text, the newest of two copies), parses volume-group metadata, and maps linear and striped logical-volume offsets to PV offsets | Library only, host-tested: no daemon, bootloader or image uses it yet, so LVM volumes cannot be mounted on the target; mirror, RAID, thin, cache and snapshot segments are refused; no writing; no `features.toml` option until that integration lands |
| Linux disk formats (prepared) | `ferric-luks`: a `no_std` library that reads LUKS1 and LUKS2 headers (both copies, checksums, JSON metadata) and recovers the volume key from a passphrase with PBKDF2 or Argon2 and aes-xts-plain64 | Library only, host-tested: no daemon, bootloader or image uses it yet, so encrypted volumes cannot be opened on the target; no `features.toml` option until that integration lands |
| Disk benchmark | `ferric-iobench`: fio-style read jobs (`--rw`, `--bs`, `--iodepth`, `--numjobs`, `--io_size`, `--randseed`) run on the largest NVMe partition with one positioned read per thread, IOPS, bandwidth and fio-layout latency percentiles, a same-boot A/B of the queue policies, and the matching Linux fio command (`ferric_iobench`, host-tested; [I/O chapter](ferric-io.md#disk-benchmark-ferric-iobench)) | Reads only, `psync` and `--direct=1` only; runs in the smoke image's five CI boots on emulated NVMe, so its numbers are same-boot comparisons; the comparison with fio on Linux on reference hardware has not been run yet |
| Validation | Host unit/integration tests, kernel lint ratchet, binary checks, image boot and syscall fuzz workflows | Configured checks are not evidence of a successful run; hardware coverage depends on reports |

The focus of new Ferric development is modern x86-64 hardware and UEFI. Older
architectures and BIOS code remain in imported sources and recipes for upstream
compatibility; their presence is not a commitment to extend them.

## Keeping this section current

Update the relevant chapter in the same change as system behavior, an
interface, a feature default, a build command or a validation requirement.
Keep incomplete work explicit, and move an item out of that category only
when its implementation and integration support the claim. Add new chapters
to `docs/src/SUMMARY.md`. Contributor and agent obligations are in the root
`CONTRIBUTING.md`, `AI_POLICY.md` and `AGENTS.md`.
