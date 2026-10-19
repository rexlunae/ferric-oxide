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
| CET | Optional supervisor indirect branch tracking (IBT), plus shadow-stack allocation and bookkeeping | Shadow-stack enforcement and user-mode CET are not enabled |
| DMA isolation | Intel VT-d and AMD-Vi DMA domains, driver binding, IOVA translation, revocation and an enforce policy | IOMMU defaults to `never`; AMD interrupt remapping is not implemented |
| Registered buffers | Anonymous-memory registration, byte grants, sub-grants, CPU mappings, page-aligned DMA grants with IOVA retirement on revoke, and `nvmed` DMA into an attached grant (`drivers.grant_dma`) | Off by default; partial pages use bounce buffers; disk grants are direct calls on `nvmed` descriptors, with no RedoxFS or ring support; other I/O is not converted |
| Rings | `kernel.ring` scheme (`kernel.rings`): shared SQ/CQ memory, brokered `Nop`, `Read`, `Write` and `Fsync` on any scheme in the caller's context, linked chains whose files are held from submission to completion, CQ back-pressure and `EVENT_READ` notification ([I/O chapter](ferric-io.md)) | Off by default; no fixed files, grant buffers, waiting for completions in `Enter`, asynchronous chains, `SqPoll` or `Umwait` notification, or rings served natively by user schemes |
| Kernel tracing | Typed tracepoints as static keys (`syscall.enter`/`exit`, `sched.switch`, `scheme.request`/`response`/`cancel` with SQE tags), per-CPU rings, boot-time and live enable, a root-only `kernel.trace` scheme | Off by default; x86_64 only; no fault, IRQ or IOMMU tracepoints, ring mapping, export or overhead budget yet |
| `fox` | Local signed archives, installed-file verification, in-place install/remove, CycloneDX SBOMs; replaces `pkg` on the target by default (`packages.manager`), with signed manifests packed by the cookbook; the image build adopts the installed packages with the host `fox`, syncs an optional package set (`packages.set`, `fox.lock`, `packages.locked`) and writes `image.cdx.json`, and a first-boot service adopts any packages still unadopted. `cargo fox` builds a git commit hermetically into a signed archive with a build SBOM and a signed attestation, which `fox verify` checks, and `cargo fox release` makes SSH-signed release tags; `fox install <git-url>` builds a signed release from source, pinning its author key on first use, or installs a prebuilt from an explicitly trusted builder. `fox-registry` creates and signs a git-hosted package registry (`fox-tuf/1`, a TUF subset), checks owners' release tags, builds attested artifacts in registry CI, lets independent rebuilders add matching attestations (`fox-registry rebuild`) and imports cookbook recipes as `recipe` packages; `fox registry`, `fox search`, `fox info` and `fox install <name>` use registries, installing prebuilt artifacts that enough registry builders attest to (the highest x86-64 level the CPU and `max-cpu-level` allow, v3 or v4) and otherwise building the owner-signed tag, under `etc/fox/policy.toml`; `fox audit` matches installed packages and the crates in their build SBOMs against registry and RustSec advisories, setting aside findings a registry's signed CycloneDX VEX statements clear, `fox sbom --system` describes a whole root, and `fox sync` installs a package set and pins it in `fox.lock` | no official registry is published; dependencies are not resolved; dependency solver or generation/rollback store; HTTPS fetching is not built for the target; builds are isolated by environment, not a network sandbox; the installer still places the configuration's packages with `pkg`'s library before `fox` adopts them; the image SBOM is not signed; no wasm variants |
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
