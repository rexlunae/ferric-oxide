# Rust Style

Follow the standard Rust formatting and lints:

- Format with `cargo fmt`. CI runs `cargo fmt --all --check` on the host
  workspace.
- Keep `cargo clippy` clean in the code you touch. CI runs Clippy with
  `-D warnings` on the Ferric Oxide host crates, and the kernel has a lint
  ratchet (`scripts/ferric-ci/kernel-lint-ratchet.sh`) so its warning counts
  can only go down.
- Don't mix formatting-only changes to imported code with real changes. Put
  them in a separate commit or pull request so the real change stays readable.

See [Validation and Debugging](./ferric-testing.md) for the full list of checks.
