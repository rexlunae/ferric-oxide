# Best Practices and Guidelines

These practices decide, more than anything else, whether a contribution gets
merged. The authoritative versions are the project rules and Rust conventions
in [AGENTS.md](https://github.com/rexlunae/ferric-oxide/blob/main/AGENTS.md)
and the expectations in
[CONTRIBUTING.md](https://github.com/rexlunae/ferric-oxide/blob/main/CONTRIBUTING.md).
The pages in this section expand on them.

In short:

- **Keep it MIT.** No GPL, LGPL or AGPL code or dependencies in what Ferric
  Oxide ships. A tool that is only referenced and not distributed with the
  system, such as a host-side utility, can use another license. Linux-compatible
  features are written clean-room from published specifications.
- **Make optional behavior a feature option** in `features.toml` rather than
  custom Make or environment-variable logic. Without a `ferric.toml`, the build
  must behave as it did before your change.
- **Reuse before writing.** Shared code goes in documented, `no_std`-first
  crates under `crates/`; prefer a well-maintained crates.io dependency over a
  home-grown equivalent.
- **Document public APIs, and update the book with the code.** A change to
  behavior, an interface, a feature default or a build command updates the
  relevant chapter in the same pull request.
- **Use typed errors.** Strings are for display, not for matching on.
- **Measure performance claims.** A change that claims a speedup includes
  before-and-after numbers and how they were measured.
- **Test what you change**, and say exactly what you built, booted and ran.
- **Target modern CPUs.** User space builds for x86-64-v3 by default; don't add
  paths for older CPUs, i586 or BIOS boot. Faster paths behind runtime feature
  detection are welcome.
