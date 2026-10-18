# Package tooling: fox

`fox` operates on local `.pkgar` archives and an in-place install root. Its
command and libraries are host-workspace members, and `recipes/core/fox`
builds the same command for the target. With `ferric.toml`, images ship
`fox` instead of upstream `pkgutils` by default (see
[Target images](#target-images)). The image build itself still uses the
cookbook and the installer.

## Components and trust

| Component | Responsibility |
| --- | --- |
| `crates/fox-core/` | Names, manifests, BLAKE3 digests, trusted Ed25519 keys, signed-head and full archive verification |
| `crates/fox-store/` | In-place file installation, ownership checks, installed database and re-verification |
| `crates/fox-sbom/` | Reproducible CycloneDX 1.5 JSON from verified package information |
| `tools/fox/` | CLI and text/JSON reporting |

The archive format is upstream pkgar. A trusted Ed25519 signature covers the
header and its BLAKE3 entry-table hash; entries carry hashes for file content.
`Archive::open` verifies signer trust, signature, entry-table hash, safe paths,
every entry's data and framing, then package metadata. LZMA2-compressed archives
are supported. Absolute paths, traversal, duplicate entries and an entry
nested beneath another entry are rejected.

The signer key carried inside an archive is not trusted merely because it
verifies that archive. Accepted keys come from pkgar public-key files under
`<root>/etc/fox/keys/`, plus explicit repeatable `--key <file>` arguments.

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
```

All commands accept `--root`, `--key` and `--json`. `verify --installed`
accepts optional package names; without them it checks all installed packages.
`install` and `remove` accept multiple inputs. Verification failures produce
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

## SBOM scope and unfinished packaging work

SBOM output describes the verified package, archive hash, signer, source,
declared runtime dependencies and individual files/hashes. It is reproducible:
there is no timestamp, and the serial number derives from the archive hash.
It does not inventory transitive build dependencies from `Cargo.lock` or
embedded `cargo auditable` information.

Repository fetching/search, a dependency solver, hermetic recipe builds,
content-addressed generations, rollback and boot-time package verification
are not implemented by these crates. Some source comments refer to packaging
design phases and `packaging/packaging-design.md`; that design file is not
tracked in this checkout. Use the implemented APIs and tests, and do not treat
those phase references as delivered functionality.
