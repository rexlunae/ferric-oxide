# Package registry

This repository is a [fox](https://github.com/rexlunae/ferric-oxide) package
registry. Its files are signed with `fox-tuf/1` metadata under `metadata/`,
and clients check every file against it, so the git host and the artifact
mirrors carry no trust.

## Using it

```sh
fox registry add <name> <this repository's URL> --root-blake3 <hash>
fox search <text>
fox install <package>
```

Take the `--root-blake3` value from the maintainers through a channel you
trust; it pins `metadata/root/1.json`.

## Publishing a package

1. A maintainer admits it: `fox-registry admit <git-url> --name <name>
   --owner <id>=<ssh-ed25519 key>`, signed with the targets key.
2. Owners release with `cargo fox release <version>` (an SSH-signed tag),
   then open a pull request that runs `fox-registry release <name> <tag>`
   and `fox-registry sign --key <owner's SSH key>`.
3. The `check` workflow re-verifies the tag signature, commit and tree.
4. After the merge, the `sign` workflow builds missing artifacts (one per
   x86-64 level, v3 and v4), uploads them and signs the targets, snapshot
   and timestamp roles; the `timestamp` workflow renews the timestamp every
   six hours.

## Builders and the artifact store

List the CI builder and where its blobs are published, then sign:

```sh
fox-registry configure --builder ci=<builder public key> \
  --artifact-url https://github.com/<owner>/<repo>/releases/download/blake3
fox-registry sign --key targets.key --key snapshot.key --key timestamp.key
```

Clients use a prebuilt artifact only when `min-builder-signatures`
builders listed here attest to it; otherwise they build from the tag.

### Independent rebuilders

The `sign` workflow runs only this registry's own builder. A second
builder is worth counting only if it runs somewhere else, under someone
else's control, so it has no workflow here. A rebuilder clones this
repository and, on its own machine, runs

```sh
fox-registry rebuild --key rebuilder.key --label <label> --blobs blobs
```

It publishes `blobs/blake3/*` at a URL of its own and opens a pull request
with the changed `artifacts/` files. A maintainer lists it with
`fox-registry configure --builder <label>=<its public key>` and adds its
URL with `--artifact-url` (clients try each URL in turn). After the merge,
the `sign` workflow signs the change. Then raise the threshold with
`configure --min-builders 2`.

## Keys

| Role | Where it lives |
| --- | --- |
| root | offline; used only with `fox-registry set-role` |
| targets, snapshot, timestamp | repository secrets `TARGETS_KEY`, `SNAPSHOT_KEY`, `TIMESTAMP_KEY` (`pkgar-keys` secret key files) |
| builder | repository secret `BUILDER_KEY`, listed in `registry.toml` as `ci` |
