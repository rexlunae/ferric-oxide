# Package tooling: fox

`fox` currently operates on local `.pkgar` archives and an in-place install
root. The OS image build still uses cookbook and the installer, and inherited
images may include upstream `pkgutils`. `fox` has no recipe or default image
integration in this checkout; its command and libraries are host-workspace
members.

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

Current cookbook archives do not embed this manifest. For those, the reader
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
```

All commands accept `--root`, `--key` and `--json`. `verify --installed`
accepts optional package names; without them it checks all installed packages.
`install` and `remove` accept multiple inputs. Verification failures produce
a failing exit status. `install --force` permits replacement of otherwise
conflicting unowned files; `remove --force` bypasses dependent-package refusal.
Neither flag makes an untrusted signature acceptable.

## Store behavior and persistence

| Path beneath the install root | Contents |
| --- | --- |
| `var/lib/fox/installed.toml` | `fox-installed/1` database: package metadata, signer/hash information, file records and removal state |
| `var/lib/fox/heads/<name>.pkgar_head` | Retained signed header and entry table for installed verification |
| `var/lib/fox/lock` | Writer lock for database changes |
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
