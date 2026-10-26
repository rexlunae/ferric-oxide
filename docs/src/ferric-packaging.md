# Package tooling: fox

`fox` operates on local `.pkgar` archives and an in-place install root. Its
command and libraries are host-workspace members, and `recipes/core/fox`
builds the same command for the target. Package authors use `cargo fox` to
build a git commit hermetically into a signed archive with a build SBOM and a
signed build attestation (see [Hermetic builds and
attestations](#hermetic-builds-and-attestations)), and `cargo fox release` to
make the SSH-signed release tag that `fox install <git-url>` checks before it
builds a source itself (see [Git sources](#git-sources)). Registry
maintainers use `fox-registry` to keep a signed package registry (see
[Registries](#registries)). With `ferric.toml`, images ship
`fox` instead of upstream `pkgutils` by default (see
[Target images](#target-images)). The image build itself still uses the
cookbook and the installer.

## Components and trust

| Component | Responsibility |
| --- | --- |
| `crates/fox-core/` | Names, manifests, BLAKE3 digests, trusted Ed25519 keys, signed-head and full archive verification, normalized git URLs and the trust store |
| `crates/fox-store/` | In-place file installation, ownership checks, installed database and re-verification |
| `crates/fox-sbom/` | Reproducible CycloneDX 1.5 JSON from verified package information, build SBOMs with the crate graph, and system SBOMs of a whole root |
| `crates/fox-git/` | Git through `gix`: resolving revisions, the `fox-tree/1` BLAKE3 tree hash, tree export, fetching into a bare cache, and SSH-signed release tags |
| `crates/fox-build/` | The hermetic Cargo build, staging, pkgar packing and attestation writing |
| `crates/fox-index/` | The registry layout, `fox-tuf/1` signed metadata, client verification, the maintainer-side editing, and RustSec-format advisories |
| `tools/fox/` | CLI and text/JSON reporting |
| `tools/cargo-fox/` | `cargo fox build`, `package`, `keygen` and `release` for package authors |
| `tools/fox-registry/` | `fox-registry` for registry maintainers, package owners and registry CI |

The archive format is upstream pkgar. A trusted Ed25519 signature covers the
header and its BLAKE3 entry-table hash; entries carry hashes for file content.
`Archive::open` verifies signer trust, signature, entry-table hash, safe paths,
every entry's data and framing, then package metadata. LZMA2-compressed archives
are supported. Absolute paths, traversal, duplicate entries and an entry
nested beneath another entry are rejected.

The signer key carried inside an archive is not trusted merely because it
verifies that archive. Accepted keys come from pkgar public-key files under
`<root>/etc/fox/keys/`, the root's local builder key
(`<root>/var/lib/fox/local-builder.pub.toml`, created the first time `fox`
builds a git source), plus explicit repeatable `--key <file>` arguments.
Packages cannot write `etc/fox/keys/`, `etc/fox/trust.toml` or
`var/lib/fox/`.

## Signed manifests and legacy metadata

A signed manifest is a normal archive entry at
`usr/share/fox/manifest/<name>.toml`:

```toml
format = "fox-manifest/1"
name = "hello"
version = "1.0.0"
target = "x86_64-unknown-redox"
summary = "Prints a greeting"
license = "MIT"
depends = ["relibc"]

[source]
git = "https://example.com/hello"
commit = "3f1c0e5a"
```

`name` and `version` are required along with the format. Version strings are
nonempty but need not be semantic versions. Dependency entries are package
names; they do not express version constraints. `fox install` fetches the
dependencies a package is missing (see [Dependencies](#dependencies)); one
that no repository or registry has is reported, and does not stop the
installation.

The cookbook packs this manifest into every target package when
`FERRIC_FOX_MANIFEST=1` (`packages.signed_manifests`, on by default with
`ferric.toml`; see [Target images](#target-images)). Archives built without
it, and upstream ones, do not embed it. For those, the reader
can load `<name>.toml` beside the archive and check that its `blake3` value
matches the signed entry-table hash. That binds the sidecar to the archive
but does **not** authenticate its name, version or other metadata. Verification
reports the metadata source and warns for sidecars; installation requires
`--allow-unsigned-metadata`. Such an archive cannot replace an installed
package whose identity came from a signed manifest.

## Commands

Build the host tool with `cargo build --locked -p fox`. The examples below
assume its binary is on `PATH`, `hello.pkgar` exists, and `signer.pub` is a
pkgar public key the operator has chosen to trust. They use a separate install
root rather than the CLI's default `/`.

```sh
mkdir -p ./image-root
fox --root ./image-root --key ./signer.pub verify ./hello.pkgar
fox --root ./image-root --key ./signer.pub sbom ./hello.pkgar --output ./hello.cdx.json
fox --root ./image-root --key ./signer.pub install ./hello.pkgar
fox --root ./image-root --json list
fox --root ./image-root --key ./signer.pub verify --installed
fox --root ./image-root remove hello
fox --root ./image-root adopt-pkg
fox --root ./image-root install https://example.com/hello.git   # see Git sources
fox --root ./image-root registry add ferric https://example.com/registry.git \
    --root-blake3 <hash>                                            # see Registries
fox --root ./image-root search hello
fox --root ./image-root install hello@^1
fox --root ./image-root sbom --system --name my-image --output ./image.cdx.json
fox --root ./image-root audit [--rustsec ./advisory-db]                # see Advisories
fox --root ./image-root sync --config ./packages.toml   # see Package sets
```

All commands accept `--root`, `--key` and `--json`. `verify` also checks
the build attestation beside an archive (`<stem>.attest.toml` and its
`.sig`) whenever one is present, and `verify --require-attestation` fails
archives without one; see [Attestations](#attestations). `verify --installed`
accepts optional package names; without them it checks all installed packages.
`install` and `remove` accept multiple inputs; `install` takes git sources
and `[registry/]name[@requirement]` package names as well as archives. Verification failures produce
a failing exit status. `install --force` permits replacement of otherwise
conflicting unowned files; `remove --force` bypasses dependent-package refusal.
Neither flag makes an untrusted signature acceptable. `adopt-pkg` is described
under [Adopting pkg's packages](#adopting-pkgs-packages).

### Dependencies

`install --repo <dir>` (repeatable) names directories of archives called
`<name>.pkgar`, such as the cookbook's `repo/<target>/`. A package named
without a registry or version requirement is taken from the first of them
that has it, before registries are tried:

```sh
fox --root ./image-root install --repo repo/x86_64-unknown-redox orbital
```

Once every named package is verified, `install` (`fox_store::deps::plan`)
looks up each dependency that is neither installed nor already named: in
the `--repo` directories in order, then in the configured registries (the
newest release, prebuilt or built as `install <name>` would). It repeats
for the dependencies of what it found, then installs everything with
dependencies first. Each archive must verify against the trusted keys like
a named one, and one that does not stops the command before anything is
installed. An installed dependency is not upgraded. Dependency cycles are
allowed. A `<name>.toml` in a repository directory with no `<name>.pkgar`
beside it is a meta-package, such as a cookbook group (`recipes/groups/`):
installing it installs the packages its `depends` lists, and it is not
recorded itself. `--no-deps` installs only what was named, and refuses a
meta-package. `fox sync` does not add
dependencies: a package set lists every package it installs, so its lock
pins all of them.

## Store behavior and persistence

| Path beneath the install root | Contents |
| --- | --- |
| `var/lib/fox/installed.toml` | `fox-installed/2` database: package metadata, signer/hash information, the registry each package was installed from (its name and pinned root), file records and removal state. A `fox-installed/1` database is still read, with no registry recorded for its packages, and is saved as format 2 on the next change |
| `var/lib/fox/heads/<name>.pkgar_head` | Retained signed header and entry table for installed verification |
| `var/lib/fox/lock` | Writer lock for database changes |
| `etc/pkg/packages.toml`, `var/lib/packages/` | `pkg`'s state, read by `adopt-pkg` and never written by `fox` |
| `etc/fox/keys/` | Trusted signer public keys |

Files are extracted into the root. Paths owned by another package conflict;
an unowned existing file is accepted if it matches or may be replaced with
`--force`. The store rejects symlink roots and writing through symlink path
components. Packages cannot replace or redirect store state or trusted-key
directories. Installed regular files receive their signed mode regardless of
umask.

Reinstallation/upgrades remove obsolete files unless changed locally. Removal
keeps locally changed files and normally refuses when another package depends
on the target. It records `removing` before touching files, so an interrupted
removal can be identified and retried. The database is replaced through a
temporary file and rename; this does not make an entire multi-file install or
multi-package command atomic.

Installed verification checks the retained signed head against currently
trusted keys and compares installed files with the signed entry records. The
database supplies management state, not a replacement trust root.

## Adopting pkg's packages

`fox adopt-pkg` (`Store::adopt_pkg`) takes over a root that upstream `pkg`
populated, such as an older system or an image built by upstream's
installer. Ferric's installer records packages with fox directly, so its
images need no adoption.
`pkg` lists its packages in `etc/pkg/packages.toml` and keeps each one's
signed head in `var/lib/packages/<name>.pkgar_head`. For every listed
package, adoption:

- verifies the head against the trusted keys, exactly as installed
  verification does, and records its signed entries as the package's files;
- takes the identity from the signed manifest when the head lists one and
  the installed copy still hashes to it. Otherwise only `pkg`'s state names
  the package. That is accepted only with `--allow-unsigned-metadata`, and
  the package is recorded with `legacy` metadata and version `unknown`;
- skips packages that are already recorded, would own a reserved path, or
  have files that already belong to another package, and lists why.

Adoption changes no installed file, and the first run does not hash them:
run `fox verify --installed` afterwards. It runs once per root. The database
records that it ran, so `pkg`'s stale state cannot bring back a package
removed with `fox` later; `--again` overrides this. The packages it skipped
are recorded as pending, and a later `fox adopt-pkg` retries those alone
(after their head is restored or their signer is trusted, say), so the
first-boot service picks them up on the next boot. Because the root may have
changed in between, a retry hashes the package's files as installed
verification does and leaves the package pending unless they all match. A
package installed with `fox` under a pending name is no longer pending, so
removing it later is not undone by a retry. A package that is skipped
makes the command exit with a failing status.

## Target images

| Setting (`features.toml`) | Default with `ferric.toml` | Effect |
| --- | --- | --- |
| `packages.manager` | `fox` | `fox`: the image gets the `fox` package, built from source even with `REPO_BINARY`, and no `pkgutils`. `pkgutils`: upstream `pkg` as before |
| `packages.signed_manifests` | on | Sets `FERRIC_FOX_MANIFEST=1`: the cookbook writes `usr/share/fox/manifest/<name>.toml` into each target package's stage before packing it, so the signature covers the name, version, target, dependencies and git source. Changing it repacks every package |
| `packages.set` | empty | With `fox`: a package set (see [Package sets and `fox sync`](#package-sets-and-fox-sync)) that the image build syncs into the image. Sets `FERRIC_FOX_PACKAGES` |
| `packages.locked` | off | With a set: build with `fox sync --locked`, so the image fails to build unless `fox.lock` pins every package. Sets `FERRIC_FOX_LOCKED=1` |

Without `ferric.toml` none of these apply, and the build matches upstream.

Remote (`REPO_BINARY`) packages are unpacked and signed again with the
cookbook's key, so they get manifests too. Packages already packed keep
what they had until they are repacked.

The installer places the configuration's packages, and every dependency
they name, from the cookbook's `repo/<target>/` with fox's store
(`fox_store::deps`), so they are recorded in `/var/lib/fox/` as they are
written; a package or dependency missing from the repository fails the
image build. It copies the cookbook's public key
(`build/id_ed25519.pub.toml`) to `/etc/fox/keys/cookbook.toml`, so the
target trusts the key that signed its packages. Archives without a signed
manifest (`packages.signed_manifests` off) are installed under the names
the cookbook's metadata gives them. The installer no longer downloads
packages: without `--cookbook` it installs only the configuration's files
and accounts. `redox_installer_tui`, which copies the running system to a
disk, takes the file list from fox's database.

With `packages.manager = "fox"` (`FERRIC_FOX_IMAGE=1`), the image build then
finishes the root with the host `fox` (`target/release/fox`, built with the
other host tools; `fox` itself on a self-hosted Redox build). Once the
packages, files and accounts are in place, the installer runs, against the
image root:

1. With `packages.set`, `fox sync --config <set> --target <image target>`
   (with `--level v3` or `v4` from `cpu.x86_64_level`, and `--locked` with
   `packages.locked`). The set's registries, policy and
   packages are installed into the image, and `fox.lock` is written beside
   the set. Commit it, and turn on `packages.locked` for release builds, so
   an image is built from exactly the pinned archives and commits. Changing
   the set rebuilds the image.
2. `fox sbom --system --name ferric-oxide-<config>`, writing the image SBOM
   to `build/<arch>/<config>/image.cdx.json`.

A failing step fails the image build. `fox`'s
`/usr/lib/init.d/05_fox-adopt.service` still runs
`fox adopt-pkg --allow-unsigned-metadata` on first boot. An image built
this way has no `pkg` state, so it only matters for roots `pkg` populated.
`fox list`, `fox verify --installed`, `fox install` and `fox remove` cover the
whole system.

The set's packages are installed for the image target, so they need
prebuilt artifacts for it from enough registry builders, or a git source
the host can cross-compile. The set's policy (`build = "never"`, say) can
forbid source builds. Registry checkouts and git source caches stay under
the image's `var/lib/fox/`, where the target's `fox` can use them. The
image SBOM is not signed yet.

On the target, `fox` installs local `.pkgar` files and their dependencies
from local repository directories. It does not fetch from package servers
over HTTPS yet, which `pkg` does. Choose `packages.manager = "pkgutils"` for
that.

The CI smoke image (`config/ferric-smoke.toml`) ships `fox`. Its boot test
lists the packages the installer recorded and runs `fox verify --installed`,
which checks each one against its signed head.

## Hermetic builds and attestations

`cargo fox` (`tools/cargo-fox`, on `crates/fox-build`) builds one commit of
a Cargo package. Install it with `cargo install --path tools/cargo-fox` or
run `cargo run -p cargo-fox -- fox ...`.

```sh
cargo fox keygen ./keys --name ci                 # ./keys/ci.toml, ./keys/ci.pub.toml
cargo fox build --manifest-path hello/Cargo.toml   # stage only
cargo fox package --key ./keys/ci.toml --label ferric-ci-1 --rev v1.2.0
fox --key ./keys/ci.pub.toml verify --require-attestation target/fox/hello-*.pkgar
cargo fox release 1.3.0 --signing-key ~/.ssh/id_ed25519   # commit and tag v1.3.0
```

Build options: `--rev` (default `HEAD`), `-p/--package`, `--target` (default
the toolchain's host), `--level v3|v4` (x86-64 only, default v3, the image
baseline), `--features`, `--no-default-features`, `--git-url` (default the
`origin` remote), `--work-dir`, `--cargo-home`, `--toolchain` and
`--auditable auto|require|off`. `package` writes into `--out` (default
`target/fox` in the repository) the files
`<name>-<version>-<target>[-<level>]-<features>.pkgar`, `.attest.toml`,
`.attest.toml.sig` and `.cdx.json`, where `<features>` is the first 12 hex
digits of the feature-set hash.

The pipeline:

1. The commit's tree is exported from git into an empty `src/` in the work
   directory. Uncommitted and untracked files are never built. Unsafe tree
   entries (`..`, `.git`, separators in names) are refused, and submodules
   become empty directories.
2. The workspace's `Cargo.lock` must be committed; builds fail without it.
3. Every command runs from an empty environment (only `PATH`, `HOME`,
   `RUSTUP_HOME` and `TMPDIR`) with a private `CARGO_HOME`. `RUSTFLAGS`,
   `RUSTC_WRAPPER`, `CARGO_BUILD_*` and `RUSTUP_TOOLCHAIN` do not pass
   through, so the source's `rust-toolchain.toml` picks the toolchain unless
   `--toolchain` names one. The build refuses to start if a
   `.cargo/config.toml` above the work directory, or one in `CARGO_HOME`,
   could change it; the default work directory is under the system
   temporary directory for that reason.
4. `cargo fetch --locked` is the only step given proxy and certificate
   settings. Metadata and the build run with `--frozen` and
   `CARGO_NET_OFFLINE`. Path dependencies outside the exported tree are
   refused.
5. `cargo build --frozen --release` runs with `SOURCE_DATE_EPOCH` set to the
   commit time, `--remap-path-prefix` for the source, target and Cargo
   directories, and `-C target-cpu` for the level. It uses
   `cargo auditable` when it is installed (`auto`), and the attestation
   records whether it did.
6. The stage holds `usr/bin/<binary>` for each binary target (or those in
   `binaries`), the `[package.metadata.fox.files]` entries (regular files
   only, mode 0644), the signed manifest and the build SBOM at
   `usr/share/fox/sbom/<name>.cdx.json`.
7. `package` packs the stage with `pkgar` (LZMA2), re-opens and verifies
   the archive, writes the attestation, signs it with the same key, and
   checks the result as a verifier would.

Two builds of the same commit, options and toolchain produce identical files,
SBOM and, with the same key, an identical archive; the tests check this
across different work directories. Isolation is by environment only: the
network is not blocked by a sandbox, and the build host's toolchain is
identified by its version output rather than a hash of its files.

`[package.metadata.fox]` is optional; without it a crate is an `app` that
installs every binary target, built natively. Unknown keys are refused.
`kind = "recipe"` packages and packages whose `targets` omit `native` are
refused; wasm targets are not built yet. `runtime-deps` become the
manifest's `depends`. `capabilities` and the `features` map to `ferric.toml`
option ids are read but not used yet.

### Build SBOMs

The build SBOM is CycloneDX 1.5 JSON. Besides the package and every staged
file with its BLAKE3, it lists every crate reached from the package through
normal and build dependencies for the build's target and features (from
`cargo metadata` and `Cargo.lock`): registry crates with their `pkg:cargo`
purl and the SHA-256 checksum `Cargo.lock` pins, git crates with their
commit, and path crates of the workspace. Crates used only by build scripts
or procedural macros have scope `excluded`. A `dependencies` section gives
the graph. Package properties record `fox:tree_blake3`,
`fox:cargo_lock_blake3`, the target, level, enabled features, feature-set
hash, toolchain and profile. It has no timestamp, and its serial number is
derived from the build identity, so it is reproducible. It cannot cite the
hash of the archive it is packed into; the attestation binds the two.

### Attestations

A `fox-attestation/1` file (`fox_core::attestation`) records:

| Table | Fields |
| --- | --- |
| `package` | `name`, `version` |
| `source` | `git`, `commit`, `tree_blake3`, `cargo_lock_blake3`, and `path` for a package below the repository root |
| `build` | `target`, `cpu_level`, the resolved `features`, `feature_set_hash`, `toolchain_id`, `profile`, `auditable`, `builder`, `builder_key`, `built_at` |
| `output` | `pkgar_blake3`, `pkgar_size`, `pkgar_header_signer`, `entries_blake3`, `sbom_blake3` |

`tree_blake3` is the `fox-tree/1` hash (`fox_git`): BLAKE3 over every file,
symlink and submodule of the commit in byte order of their paths, each with
a kind byte and length-prefixed path and contents, so git's SHA-1 ids are
not the only thing naming the source. `feature_set_hash` is BLAKE3 over the
sorted enabled features and the `Cargo.lock` hash. `entries_blake3` does not
depend on the signing key, so a rebuilder signing with its own key can still
show it produced the same files. `fox_build::reattest` does that: it
compares a rebuild's staged files with a published archive and, when they
match, writes the rebuilder's attestation of the published archive, whose
`output` names the original header signer.

The `.sig` file (`fox-signature/1`) holds the Ed25519 key and a signature
over `fox-attestation-signature/1\0` followed by the attestation's exact
bytes. Verification looks the key up among the trusted keys first, checks
the signature, requires `builder_key` to be that key, and then checks that
the attestation matches the archive: name, version, archive hash and size,
header signer, entry-table hash, and the BLAKE3 of the SBOM entry inside it.
Builder keys are the same trusted keys as archive signers
(`<root>/etc/fox/keys/` and `--key`); one CI key signs both for now.

## Git sources

`fox install` and `fox build` take a git URL (`https`, `http`, `ssh`, `git`,
`file`, or scp-like `user@host:path`) or a local repository directory, each
optionally followed by `#<tag or commit>`:

```sh
fox --root ./image-root install https://example.com/hello.git
fox --root ./image-root install https://example.com/hello.git#v1.2.0
fox --root ./image-root build --out ./out ../hello
fox --root ./image-root trust list
```

1. **Fetch.** The repository is fetched into a bare cache,
   `<root>/var/lib/fox/git/<id>`, keyed by the normalized URL (lowercase
   scheme and host, no trailing `/` or `.git`), which is also the source's
   identity in the trust store and in attestations. The cache mirrors the
   remote's branches and tags; a tag deleted upstream is deleted from the
   cache too, so a withdrawn release is not built. HTTPS uses `gix`'s reqwest transport with
   rustls on ring and is not built for Redox, where `fox` fetches over
   `file://`, `ssh://` (through `ssh`) and `git://` only.
2. **Choose.** Without `#`, the newest release tag is built: the highest
   semver-named tag (`1.2.3` or `v1.2.3`) that is not a pre-release. A name
   after `#` is a tag if one exists, otherwise any revision. Without tags,
   `HEAD` is built.
3. **Check the tag.** The tag must be annotated and SSH-signed (SSHSIG,
   namespace `git`, as `git tag -s` writes with `gpg.format = ssh`) by an
   Ed25519 key. The tag object must name the tag it is found under and point
   at a commit, so a signed tag cannot be replayed under another version.
   PGP and X.509 signatures are not verified. Unsigned tags, branches and
   commits are refused unless `--allow-unsigned` is given.
4. **Trust on first use.** The signing key is looked up in
   `<root>/etc/fox/trust.toml` for the repository. A pinned key must match;
   a different key is refused, with or without flags, until
   `fox trust remove-author <url>` drops the pin. A repository without a pin
   gets one after the user confirms the key's `SHA256:` fingerprint at the
   terminal, or with `--trust-new-key`; without either (for example in a
   script) the install fails and names the fingerprint.
5. **Build.** The commit is built as `cargo fox package` builds it (see
   above) in a work directory per source under the system temporary
   directory (`--work-dir` overrides it). The archive and attestation are
   signed with the root's local builder key,
   `<root>/var/lib/fox/local-builder.toml`, created (unencrypted, mode 0600)
   on first use and trusted by that root. A tag that names a version must
   name the package's version. Source options: `--target`, `-p/--package`,
   `--path <dir>` for a package below the repository root, `--features` and
   `--no-default-features`. `fox build --out <dir>` stops after packing.
6. **Or a prebuilt archive.** `fox install --prebuilt <file.pkgar> <source>`
   installs that archive instead of building, after the same fetch, choice
   and tag check. It is accepted only when the attestation beside it is
   signed by a `[[builder]]` in the trust store (or a key the root already
   trusts) and names the same repository, commit, `fox-tree/1` tree hash and
   package directory. There are no trusted builders by default, so
   third-party prebuilts are refused until the user adds one.

The trust store is `fox-trust/1` TOML (`fox_core::trust`):

```toml
format = "fox-trust/1"

[[author]]
git = "https://example.com/hello"
key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... alice@example.com"

[[builder]]
label = "example-ci"
key = "<pkgar public key, 64 hex digits>"
```

`fox trust list`, `add-author <url> <key or key file>`,
`remove-author <url>`, `add-builder <label> <pkgar public key file>` and
`remove-builder <label>` edit it.

`cargo fox release <version>` sets the version in `Cargo.toml` (or in the
workspace's `[workspace.package]` when the package inherits it), runs
`cargo update --workspace --offline`, commits with `git commit -a`, and
creates the annotated tag `v<version>`. With `--signing-key <file>` it signs
the tag itself with that unencrypted OpenSSH Ed25519 key; otherwise it runs
`git tag -s`, which must be configured for SSH signing. It then reads the tag
back, verifies its signature and prints the key's fingerprint, which users
see when they first install the source. It refuses a dirty work tree or an
existing tag, and pushes nothing.

## Registries

A registry is a git repository of TOML files whose integrity comes from
signed metadata, not from the git host. `crates/fox-index` implements both
sides, `fox-registry` is the maintainer and builder tool, and `fox registry`,
`fox search`, `fox info` and `fox install <name>` are the client.

```text
registry.toml                     name, artifact URLs, builder keys
packages/<xx>/<name>.toml         one file per package: git URL, owners, releases
artifacts/<xx>/<name>/<ver>.toml  prebuilt artifacts of one release
advisories/                       security advisories
vex/<id>.cdx.json                 CycloneDX VEX statements about its packages
metadata/root/<N>.json            every root version, kept for key rotation
metadata/targets.json             hashes of every file above, and the delegations
metadata/delegations/<name>.json  each package's file, signed by its owners
metadata/snapshot.json            versions and hashes of targets and delegations
metadata/timestamp.json           hash of the snapshot; expires after a day
```

`fox-registry` writes every file through a temporary file in
`.fox-staging/` at the top of the repository and a rename, so an
interrupted write leaves the old file whole and no stray file in a signed
directory (every file there is a target). Each write's temporary file has
its own name (the target path, the process id and a random value), so
concurrent writers never share one, and opening the registry removes only
staging files more than an hour old, the leftovers of killed writes. Git
does not track the empty directory.

`<xx>` is the name's first two characters (`_` pads one-character names).
Package files keep `[[owners]]` (an id and an OpenSSH Ed25519 key) and
`[[release]]` (version, tag, commit, `fox-tree/1` `tree_blake3`, `yanked`).
Artifact lists live apart from package files so CI can add them under the
targets key without the owners' signatures.

### Signed metadata: `fox-tuf/1`

The metadata follows the role model of The Update Framework (TUF
specification 1.0): root, targets, snapshot and timestamp roles, delegations
by exact path, version numbers and expiry dates. `fox-index` implements this
subset itself rather than using the `tough` crate, whose async runtime and
dependency set do not suit the target. Each file is
`{"signed": ..., "signatures": [{"keyid", "sig"}]}`; a signature is Ed25519
over `fox-tuf-metadata/1\0` followed by the OLPC canonical JSON of `signed`,
key ids are the raw public keys in hex, and file hashes are BLAKE3.

| Role | Signs | Default expiry |
| --- | --- | --- |
| root | the keys and thresholds of every role | 365 days |
| targets | `registry.toml`, `artifacts/`, `advisories/`, undelegated package files, and the delegations | 90 days |
| package delegation | that package's file only; keys are its owners' SSH keys, threshold 1 | 365 days |
| snapshot | the version, length and hash of `targets.json` and every delegation | 7 days |
| timestamp | the version, length and hash of `snapshot.json` | 1 day |

A client starts from a root it trusts (shipped with the image or pinned by
its BLAKE3) and keeps the versions it has seen. `fox_index::verify::update`
then checks, in order: each newer root against the old and its own root keys;
the timestamp; the snapshot against the timestamp's hash; that no version
went backwards (rollback); that nothing has expired (freeze); targets and
every delegation against the snapshot; and that a delegation lists only the
paths it was given. Files are read only through the verified registry, which
checks each against its signed length and hash; a file no metadata lists
cannot be read. Top-level targets win over a delegation for the same path.

The client also refuses a delegation whose name is not a single path
component, any read that would leave the checkout (including through a
symbolic link), a metadata length above 16 MiB whatever the signed
descriptor claims, and a `registry.toml` whose `name` differs from the
signed root's. It pins the BLAKE3 of the trusted timestamp's signed payload
in its saved state (`timestamp_payload`), so a timestamp with the trusted
version but different content is refused as an equivocation, while
signatures re-made over the same payload are accepted. A package file supplied by its
delegation must list as owners exactly the delegation's keys, so tag
authority cannot drift from update authority.

### Running a registry

```sh
fox-registry -C reg init --name ferric --keys ./keys
fox-registry -C reg admit https://example.com/hello.git --name hello \
    --owner alice="ssh-ed25519 AAAA... alice@example.com"
fox-registry -C reg sign --key keys/targets.key --key keys/snapshot.key \
    --key keys/timestamp.key
```

`init` generates a `pkgar-keys` pair per role (`--single-key` uses one key
for all four, for a personal registry) and prints the BLAKE3 of
`metadata/root/1.json` for clients to pin. Edits (`admit`, `release`,
`yank`, `set-role`) only change files; `sign` then recomputes every role in
order (root, delegations, targets, snapshot, timestamp), gives a role whose
content changed a new version and expiry, and signs with whichever `--key`
files the role lists. A version nobody has signed yet is updated in place.
A role that has used up half its lifetime is renewed when its key is given,
so a scheduled `fox-registry sign --key timestamp.key` keeps the timestamp
fresh. A role is only renewed when the given keys can sign the replacement
to its threshold, so a scheduled run holding one of two keys leaves the
still-valid metadata in place. A new root is complete only when its own
root keys meet their threshold and, separately, the previous root's root
keys meet theirs (`sign` prints both counts), and a root that is not yet complete is
amended by `set-role` rather than followed by another version. Amending it
drops every signature it already has, by old and new root keys alike, so
each key set has to approve the content it actually signs.
`sign --complete` fails if any file is short of its threshold, which
is a normal intermediate state: an owner signs a delegation without the
snapshot key, and CI adds snapshot and timestamp after the merge.

`--key` takes a `pkgar-keys` secret key file or an OpenSSH Ed25519 private
key (owners sign with the key their tags are signed with). Both must be
unencrypted; keep them on offline or encrypted storage and use a decrypted
copy only while signing.

| Command | What it does |
| --- | --- |
| `admit <git-url> --name N --owner ID=KEY` | Writes the package file and a delegation to the owners (`--kind`, `--summary`, `--path` optional) |
| `release <name> <tag>` | Fetches the repository, checks the tag is SSH-signed by an owner and names a version, and records its commit and tree hash |
| `check [name...]` | What registry CI runs on a pull request: re-checks every recorded release's tag signature, commit and tree hash, and that the owners match the delegation's keys; a package with no releases yet is only checked against its delegation, without fetching its repository |
| `import-recipes <dir> --git URL --owner ID=KEY...` | Admits every cookbook recipe under `dir` as a `kind = "recipe"` package whose source is its recipe directory (`path`) in the repository at `URL`, delegated to the owners; the summary is the recipe's `[package] description` or its upstream source. Recipes already in the registry, names that are not package names and recipes that are not valid TOML are skipped and reported. `targets.json` is written once for the whole import, after every package file has been checked, and a failed write removes the files already written, so the import can be retried. Imported recipes are listed and searchable, but `fox` does not build `kind = "recipe"` packages yet (see the `[package.metadata.fox]` notes), so releases recorded for them cannot be installed until recipe builds land |
| `advise <file>` | Validates a RustSec-format advisory and stores it as `advisories/<id>.toml`, replacing one with that id (to update or withdraw it); signed by the targets key |
| `vex <file> [--id ID]`, `vex --remove ID` | Validates a CycloneDX VEX document (see [VEX statements](#vex-statements)) and stores its bytes as `vex/<id>.cdx.json` (the id defaults to the file name without `.cdx.json`), replacing one with that id, or removes one; signed by the targets key |
| `yank <name> <version>` | Marks a release yanked (`--undo` reverses it) |
| `set-role <role> --key K... [--threshold N]` | Writes a new root that gives the role new keys; clients accept it once the old and the new root keys have signed |
| `sign [--key F...] [--bump ROLE...] [--complete]` | Updates and signs the metadata |
| `verify [--root FILE]` | Runs the client checks, trusting `metadata/root/1.json` or the given root |

| `configure [--builder LABEL=KEY...] [--remove-builder LABEL...] [--artifact-url URL...] [--min-builders N]` | Edits `registry.toml`: the builders whose attestations count, the artifact store and how many builders an artifact needs |
| `build [name@version...] --key F --label L --blobs DIR` | The registry builder: builds each release (by default every one with no artifact from `L` yet) from its checked tag with the hermetic pipeline (the Cargo package the registry names; the build fails if its name or version differs from the release), signs the archive and attestation, stores the three blobs as `DIR/blake3/<hash>` and records the attestation in the release's artifact list (`--target`, default this machine's, and `--level v3\|v4`, default v3 and only for x86-64, also decide which artifacts count as present, each level being its own artifact; `--work-dir` optional) |
| `rebuild [name@version...] --key F --label L --blobs DIR` | The independent rebuilder: builds each release the same way (by default every release with a default-feature artifact for `--target` and `--level` that `L` has not attested), downloads the published archive from the artifact store, and only if the rebuild staged exactly the archive's files (paths, kinds, modes, sizes and BLAKE3 hashes, the SBOM and manifest included) signs its own attestation of that archive and adds it to the same artifact. The archive is not repacked, so its header stays signed by the original builder. If the rebuild does not reproduce one or more of a release's archives, it names each one with its first difference, keeps the attestations it did record, moves on to the next release and exits unsuccessfully at the end. `L` must be listed in `registry.toml` |
| `init --github` | Also writes a README and GitHub Actions workflows: `check` on pull requests, `sign` after merges (build missing v3 and v4 artifacts, upload the blobs to a `blake3` release, sign and push) and `timestamp` every six hours |

`release`, `check` and `build` fetch into `~/.cache/fox-registry/git` (or
`$XDG_CACHE_HOME`; `--cache` overrides it). `build` refuses a key that
`registry.toml` does not list under that label, and artifact lists merge:
a second builder attesting to the same archive hash adds its attestation
to the same artifact.

When `build` and `rebuild` pick releases themselves, an attestation by `L`
counts only if its signature verifies with `L`'s current key in
`registry.toml`, as clients check it. After `configure --builder` replaces
a builder's key, its old attestations are stale, and the next run attests
again and replaces them. Attestation blobs are read from `DIR` when they are
there with the right hash, and otherwise downloaded from the artifact store;
a blob that cannot be read counts as missing, with a warning. Without
`--work-dir`, each builder label, mode, target, level and package gets its
own work directory under the system temporary directory, so builders that
share a machine do not overwrite each other's source and stage.

The `init --github` template runs only the registry's own builder. An
independent rebuilder runs `rebuild` on its own infrastructure, publishes its
blobs at its own URL (added with `--artifact-url`; clients try each URL in
turn) and proposes the `artifacts/` change as a pull request. The template
README describes this. No `rexlunae/ferric-registry` repository is
published yet; `init --github` is the template for one.

### Using registries

```sh
fox registry add ferric https://example.com/registry.git --root-blake3 <hash>
fox registry list
fox registry update [name...]
fox search <text>
fox info [registry/]name
fox install [registry/]name[@requirement]
fox registry remove ferric
```

`etc/fox/registries.toml` (`fox-registries/1`) lists each registry's name,
git URL and the BLAKE3 of its first root. `registry add` fetches the
repository, refuses it unless `metadata/root/1.json` has the pinned hash,
and verifies it. Each registry keeps `var/lib/fox/registries/<name>/`: a
bare mirror in `git/`, the verified checkout in `tree/`, and `state.json`
with the trusted root, the metadata versions seen and the time of the last
update. An update exports the head of the registry's default branch (the
mirror's `HEAD` follows the remote's, so a renamed default branch is picked
up) to `tree.new/`. Once that verifies, it writes the new state to
`state.json.new`, moves `tree/` aside to `tree.old/` and the new tree into
place, and then renames `state.json.new` over `state.json`, the commit
point. A failure, or an interruption found by the next open, before the
commit puts the old tree back and drops the new state; after it, only
`tree.old/` is removed, and failing to remove it is a warning, not a
failed update (the next open or update tries again). So the checkout and its state always stay a
verified pair, and a failed update keeps the last good checkout and the
rollback protection.

Installing by name looks the package up in every configured registry (or
the named one), updating any whose checkout is older than
`require-fresh-metadata`, and picks the highest release matching the
requirement (any version if none is given; yanked releases only with
`allow-yanked`). It then:

1. unless `build = "always"`, looks for an artifact of that release for the
   target, [CPU level](#cpu-levels) and feature set, downloads its archive, attestation and signature
   by hash from the registry's `artifact-base-urls`, and installs it when
   enough distinct builders listed in `registry.toml` signed attestations of
   that exact archive, naming the requested package and version and built
   from the release's commit and tree;
2. otherwise, unless `build = "never"`, fetches the package's repository,
   checks that the release tag is SSH-signed by one of the owners the
   registry lists and still names the recorded commit and tree, and builds
   it as a [git source](#git-sources). The Cargo package built is the one
   the registry names (so a workspace root can hold others), and the build
   fails if its manifest names another package. The owners come from the signed
   registry, so there is no trust-on-first-use prompt.

`fox verify --installed` accepts the registries' builder keys (labelled
`<registry>/<builder>`) for packages installed from them.

#### Install policy

`etc/fox/policy.toml` is optional; every key defaults as shown.

```toml
build = "auto"                # never | auto | always
min-builder-signatures = 1    # builders an artifact needs
allow-unsigned-git = false    # build unsigned tags and commits
allow-yanked = false          # pick yanked releases when resolving names
require-fresh-metadata = "24h"
# max-cpu-level = "v3"        # highest x86-64 level to install (unset: no cap)
```

The builder threshold is the larger of the policy's and the registry's
`min-builder-signatures`, and never less than one. Builders count by
distinct key, so one builder listed under two labels counts once. Once
independent rebuilders run `fox-registry rebuild`, a registry (`configure
--min-builders 2`) or a client (`min-builder-signatures = 2`) can require
two builders to have produced the same archive from the release's source;
`fox info` lists who attested each artifact. `allow-unsigned-git`
has the effect of `install --allow-unsigned` for git sources.

#### CPU levels

x86-64 artifacts come in levels: `x86_64-v3`, the baseline every Ferric
Oxide machine has, and `x86_64-v4`, which adds AVX-512. Other targets,
`aarch64` among them, have one variant each, chosen by target, and their
artifacts name no level. `fox install` and `fox sync` pick the level from,
in order:

1. `--level v3|v4`;
2. on the running system (`--root /`), the highest level the CPU supports
   with the state the kernel saves (CPUID and XCR0, so a kernel without
   AVX-512 state support, `kernel.cpu.avx512`, reads as v3), capped by
   `max-cpu-level`;
3. for any other root, such as an image being built, whose CPU `fox` cannot
   see, `max-cpu-level`, else v3.

From a registry, `fox` tries the artifacts of the highest level not above
that one first and falls back to lower ones, so a v4 machine installs a v3
build when no v4 build is attested, and a v3 machine never gets a v4 build.
An artifact listed without a level counts as v3, and an attestation whose
level differs from its artifact's listing is not counted. Source builds use
the level for x86-64 targets and ignore it for others. `fox build` builds
v3 unless given `--level`. The image build passes `cpu.x86_64_level` (v3
or v4) through `FERRIC_FOX_LEVEL`; v1 and v2 images have no matching
packages and get v3 ones. wasm32 and wasm64 variants, and the install-time
compilation of wasm modules, are not implemented.

### Package sets and `fox sync`

A package set (`fox-packages/1`) says what a root should have:

```toml
format = "fox-packages/1"

[[registry]]
name = "ferric"
git = "https://github.com/rexlunae/ferric-registry"
root-blake3 = "<BLAKE3 of its metadata/root/1.json>"

[policy]                      # optional; written to etc/fox/policy.toml
build = "auto"

[packages]
ripgrep = {}                                   # newest release, any registry
nginx = { version = "^1.27", registry = "ferric", features = ["http2"] }
mytool = { git = "https://github.com/me/mytool", rev = "v0.3.0" }
```

A package entry takes `version` and `registry`, or `git` with an optional
`rev` (a tag or commit; the newest release tag otherwise), plus `features`,
`default-features`, `target`, `path` and `package` as for `fox install`.

`fox sync --config FILE` adds the set's registries that are missing (and
refuses one configured with another URL or pin), writes `[policy]` if
present, installs every package and then writes the lock file, `fox.lock`
beside the set unless `--lock` names another. The lock (`fox-lock/1`)
records each package's version, its registry or its git URL and commit, a
digest of its build request, the target the archive was built for
(`"none"` for an archive that names no target; the field is required) and
the BLAKE3 of the archive installed:

```toml
format = "fox-lock/1"

[[package]]
name = "nginx"
version = "1.27.3"
registry = "ferric"
request-blake3 = "<BLAKE3 of the build request>"
target = "x86_64-unknown-redox"
pkgar-blake3 = "<BLAKE3 of the archive>"
```

The build request is what changes the archive built: the git source's
normalized URL (a local directory becomes the `file://` URL of its
canonical path, so the same checkout always has the same identity), the
revision (`rev`, or a `#` fragment of the URL; the two must agree if both
are given), `path`, `package`, the features (sorted, without repeats),
`default-features`, the target asked for (the entry's, else
`--target`'s, else none) and, unless that target is not x86-64, the
[CPU level](#cpu-levels), so moving a lock to a machine with another level
resolves its packages again. Without one, the target an archive gets depends
on the host: the toolchain the source's `rust-toolchain.toml` selects for a
build, `fox`'s own target for a prebuilt. So it cannot be known before the
build and is not part of the request. The lock instead records the
target the archive was actually built for, and when a locked package
has to be installed again, an archive for another target is refused
(name a target, or use `--update`), as a different commit or archive is.
A git
source that is neither a git URL nor an existing directory is an error,
not an unpinned entry. The lock's `format` is bumped (`fox-lock/2`, ...)
on any incompatible change to its fields.

On the next sync a locked package is kept while the lock still satisfies
the set (same source and build request, version still matching), so
changing a package's `rev`, features, target, `path` or `package` resolves
it again; a kept package is skipped if the
installed archive is the locked one, and otherwise installed again at the
locked version, which for a registry package must be the locked archive.
Git packages are pinned to their commit instead, since an archive built
locally is signed with that root's own builder key and so differs between
roots. Packages that the previous lock listed and the set no longer does
are removed if the installed archive is still the locked one; packages
installed by hand, or replaced by hand since the last sync, are left alone
(reported as `kept`). `--locked` fails on any package the lock does not pin
(for reproducible image builds), and checks every package before it adds a
registry or writes the policy, so a refused sync changes nothing;
`--update` resolves every package again, and replaces a lock file whose
contents do not parse instead of failing on it. The warning says why, and
that the packages the old lock installed are no longer known to sync, so
they are not removed automatically and need checking with `fox list`.
Without `--update` such a lock is an error, and a lock that cannot be read,
or a lock path that is not a regular file, is always an error before
anything in the root changes.

### Advisories and `fox audit`

A registry's `advisories/<id>.toml` files use the RustSec advisory format
(the `[advisory]` and `[versions]` tables of the RustSec advisory-db
`README.md`), with the title and description as fields:

```toml
[advisory]
id = "FOX-2026-0001"
package = "hello"          # a fox package; ecosystem = "crates.io" names a crate
date = "2026-10-01"
title = "hello prints its input unescaped"
aliases = ["CVE-2026-0001"]

[versions]
patched = [">= 1.2.1"]
unaffected = ["< 1.0.0"]
```

A version is affected when it matches none of the `patched` and
`unaffected` requirements (Cargo requirements); `withdrawn` and
`informational` advisories are ignored. The targets role signs the
directory like every other top-level file, and clients read advisories only
through the verified checkout.

`fox audit` checks what is installed under the root against the advisories
of every configured registry and, with `--rustsec <dir>`, a checkout of the
RustSec database (`crates/<crate>/*.md`, Markdown with TOML front matter).
Package advisories match installed package names and versions; crate
advisories match the crates listed in each package's build SBOM, so a
vulnerable crate is reported in every package that links it, marked
`build only` when it ran only at build time. Only crates whose
`fox:registry` is crates.io are matched against RustSec; crates from other
registries share the `pkg:cargo` purl type but not crates.io's names. The
SBOM is read from the path the package itself installed, never through a
symbolic link and no further than its recorded size (at most 64 MiB), and
only if it still has the hash recorded at install time. Packages whose
removal did not finish are left out. Package advisories match by name and
version alone: the installed-package database does not record which
registry a package came from, so with two registries publishing the same
name, both registries' advisories apply. `audit` fails when anything matches; `--json` gives the
findings, the packages whose versions are not semver (`unchecked`) and the
number of advisories checked. Installing a registry release that an
advisory affects prints a warning.

#### VEX statements

A registry can say that an advisory does not apply to one of its
packages, for example a crate advisory for code a package never compiles.
It publishes a CycloneDX VEX document (JSON, `specVersion` 1.4 or 1.5)
with `fox-registry vex`:

```json
{"bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
 "vulnerabilities": [{
   "id": "RUSTSEC-2026-0001",
   "analysis": {"state": "not_affected", "justification": "code_not_present",
                "detail": "built without the `http` feature"},
   "affects": [{"ref": "pkg:generic/hello@1.2.0"}]}]}
```

Each `affects[].ref` names a fox package by the purl its SBOM uses,
`pkg:generic/<name>` (every version) or `pkg:generic/<name>@<version>`,
directly or through the `bom-ref` of a component in the document with that
purl. `versions` entries list more exact versions; a version an entry marks
`affected` is never covered, even when the purl names it, and `vers:`
ranges are refused. A statement
matches a finding when its `id` or a `references[].id` is the advisory's id
or one of its aliases, and it covers the installed package and version.
`fox-registry vex` rejects a document that `fox` could not use, and the
targets role signs it like every other top-level file.

`fox audit` applies a registry's statements only to packages installed
from that registry. `fox install <name>` and `fox sync` record, in the
installed-package database, the registry a package came from (its name and
the root BLAKE3 its entry pins, whether a prebuilt was installed or the
release was built), and a statement applies only when both match the
configured registry. One registry therefore cannot clear a same-named
package from another, nor a package installed from an archive file or git
source, adopted from `pkg`, or recorded before the database kept origins
(`fox-installed/1`). Only reinstalling such a package from the registry
gives it an origin; a matching archive hash is not proof of where a copy
came from. A finding covered
by a `not_affected`, `false_positive`, `resolved` or
`resolved_with_pedigree` statement moves from `findings` to `suppressed`
(with the registry, document, state, justification and detail) and no
longer fails the audit. An `exploitable` or `in_triage` statement keeps the
finding and is shown with it, and it wins over a clearing statement from any
registry. `fox audit --no-vex` ignores VEX without reading it, so a
broken VEX document does not stop that audit.

### System and image SBOMs

`fox sbom --system [--name N]` writes one CycloneDX 1.5 document for the
whole root: an operating-system component named `N` (default
`ferric-oxide`), one application component per installed package with its
archive hash, signer, target and dependencies, and nested under each
package its files and the crates from its build SBOM (their `bom-ref`s
prefixed with `<package>/`). The dependency graph links the system to its
packages, packages to the installed packages they depend on and to their
root crates, and keeps each build's crate graph. Like the package SBOM it
is reproducible; its serial number derives from everything the document
lists, so a re-signed or changed package gives a new serial. Run against an image root (`--root`), it is that image's
SBOM; packages adopted from `pkg` have no build SBOM, so they list files but
no crates.

## SBOM scope and unfinished packaging work

`fox sbom` output describes the verified package, archive hash, signer,
source, declared runtime dependencies and individual files/hashes. It is
reproducible: there is no timestamp, and the serial number derives from the
archive hash. It is built from the archive's own metadata; the crate graph
is in the build SBOM that `cargo fox package` packs into archives it builds.
Reading embedded `cargo auditable` data, SPDX export, VEX documents other
than a registry's (and VEX inside SBOMs), and
listing the kernel, bootloader and initfs in the system SBOM are not
implemented. The image build does not yet write `image.cdx.json` itself;
that comes with images built through fox.

Git sources are built without a sandbox (see
[Hermetic builds and attestations](#hermetic-builds-and-attestations)), and
only release tags are signature-checked, not individual commits.
No official registry is published, and dependencies are found by name
only, without version constraints. The image SBOM is unsigned. Downloading artifacts over HTTPS is not
built for the target, where only `file://` artifact stores work. A dependency solver, hermetic recipe builds,
content-addressed generations, rollback and boot-time package verification
are not implemented by these crates. Some source comments refer to packaging
design phases and `packaging/packaging-design.md`; that design file is not
tracked in this checkout. Use the implemented APIs and tests, and do not treat
those phase references as delivered functionality.
