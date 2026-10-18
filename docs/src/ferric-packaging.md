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
| `crates/fox-sbom/` | Reproducible CycloneDX 1.5 JSON from verified package information, and build SBOMs with the crate graph |
| `crates/fox-git/` | Git through `gix`: resolving revisions, the `fox-tree/1` BLAKE3 tree hash, tree export, fetching into a bare cache, and SSH-signed release tags |
| `crates/fox-build/` | The hermetic Cargo build, staging, pkgar packing and attestation writing |
| `crates/fox-index/` | The registry layout, `fox-tuf/1` signed metadata, client verification and the maintainer-side editing |
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
names; they do not express version constraints or implement resolution. Install
reports missing dependencies but does not fetch them or refuse installation
solely because they are absent.

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

## Store behavior and persistence

| Path beneath the install root | Contents |
| --- | --- |
| `var/lib/fox/installed.toml` | `fox-installed/1` database: package metadata, signer/hash information, file records and removal state |
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
populated: the installer writes such roots, and older systems used `pkg`.
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

Without `ferric.toml` neither applies, and the build matches upstream.

Remote (`REPO_BINARY`) packages are unpacked and signed again with the
cookbook's key, so they get manifests too. Packages already packed keep
what they had until they are repacked.

When an image includes `fox`, the installer copies the cookbook's public
key (`build/id_ed25519.pub.toml`) to `/etc/fox/keys/cookbook.toml`, so the
target trusts the key that signed its packages. The installer still places
packages with `pkg`'s library. On first boot, `fox`'s
`/usr/lib/init.d/05_fox-adopt.service` runs
`fox adopt-pkg --allow-unsigned-metadata`. The unsigned names it accepts are
the ones the installer took from the image configuration. After that,
`fox list`, `fox verify --installed`, `fox install` and `fox remove` cover the
whole system.

On the target, `fox` installs local `.pkgar` files only. It does not fetch
from package servers or resolve dependencies, which `pkg` does. Choose
`packages.manager = "pkgutils"` for that.

The CI smoke image (`config/ferric-smoke.toml`) ships `fox`. Its boot test
requires every package to have been adopted with a signed manifest, then
runs `fox verify --installed`.

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
show it produced the same files.

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
metadata/root/<N>.json            every root version, kept for key rotation
metadata/targets.json             hashes of every file above, and the delegations
metadata/delegations/<name>.json  each package's file, signed by its owners
metadata/snapshot.json            versions and hashes of targets and delegations
metadata/timestamp.json           hash of the snapshot; expires after a day
```

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
still-valid metadata in place. For a new root, completeness counts both the
previous and the new root keys, and a root that is not yet complete is
amended by `set-role` rather than followed by another version.
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
| `check [name...]` | What registry CI runs on a pull request: re-checks every recorded release's tag signature, commit and tree hash, and that the owners match the delegation's keys |
| `yank <name> <version>` | Marks a release yanked (`--undo` reverses it) |
| `set-role <role> --key K... [--threshold N]` | Writes a new root that gives the role new keys; clients accept it once the old and the new root keys have signed |
| `sign [--key F...] [--bump ROLE...] [--complete]` | Updates and signs the metadata |
| `verify [--root FILE]` | Runs the client checks, trusting `metadata/root/1.json` or the given root |

| `configure [--builder LABEL=KEY...] [--remove-builder LABEL...] [--artifact-url URL...] [--min-builders N]` | Edits `registry.toml`: the builders whose attestations count, the artifact store and how many builders an artifact needs |
| `build [name@version...] --key F --label L --blobs DIR` | The registry builder: builds each release (by default every one with no artifact from `L` yet) from its checked tag with the hermetic pipeline, signs the archive and attestation, stores the three blobs as `DIR/blake3/<hash>` and records the attestation in the release's artifact list (`--target`, `--work-dir` optional) |
| `init --github` | Also writes a README and GitHub Actions workflows: `check` on pull requests, `sign` after merges (build missing artifacts, upload the blobs to a `blake3` release, sign and push) and `timestamp` every six hours |

`release`, `check` and `build` fetch into `~/.cache/fox-registry/git` (or
`$XDG_CACHE_HOME`; `--cache` overrides it). `build` refuses a key that
`registry.toml` does not list under that label, and artifact lists merge:
a second builder attesting to the same archive hash adds its attestation
to the same artifact. No `rexlunae/ferric-registry` repository is published
yet; `init --github` is the template for one.

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
update. An update exports the new head to `tree.new/` and replaces `tree/`
and the state only once it verifies, so a failed update keeps the last good
checkout and the rollback protection.

Installing by name looks the package up in every configured registry (or
the named one), updating any whose checkout is older than
`require-fresh-metadata`, and picks the highest release matching the
requirement (any version if none is given; yanked releases only with
`allow-yanked`). It then:

1. unless `build = "always"`, looks for an artifact of that release for the
   target and feature set, downloads its archive, attestation and signature
   by hash from the registry's `artifact-base-urls`, and installs it when
   enough distinct builders listed in `registry.toml` signed attestations of
   that exact archive, built from the release's commit and tree;
2. otherwise, unless `build = "never"`, fetches the package's repository,
   checks that the release tag is SSH-signed by one of the owners the
   registry lists and still names the recorded commit and tree, and builds
   it as a [git source](#git-sources). The owners come from the signed
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
```

The builder threshold is the larger of the policy's and the registry's
`min-builder-signatures`, and never less than one. `allow-unsigned-git`
has the effect of `install --allow-unsigned` for git sources.

## SBOM scope and unfinished packaging work

`fox sbom` output describes the verified package, archive hash, signer,
source, declared runtime dependencies and individual files/hashes. It is
reproducible: there is no timestamp, and the serial number derives from the
archive hash. It is built from the archive's own metadata; the crate graph
is in the build SBOM that `cargo fox package` packs into archives it builds.
Reading embedded `cargo auditable` data is not implemented.

Git sources are built without a sandbox (see
[Hermetic builds and attestations](#hermetic-builds-and-attestations)), and
only release tags are signature-checked, not individual commits.
No official registry is published, and `fox` resolves only the requested
package, not its dependencies. Downloading artifacts over HTTPS is not
built for the target, where only `file://` artifact stores work. A dependency solver, hermetic recipe builds,
content-addressed generations, rollback and boot-time package verification
are not implemented by these crates. Some source comments refer to packaging
design phases and `packaging/packaging-design.md`; that design file is not
tracked in this checkout. Use the implemented APIs and tests, and do not treat
those phase references as delivered functionality.
