#!/usr/bin/env bash
# Compile the actual loader errors without linking relibc over the host libc.
# The ELF image module needs the `object` crate, built for the host from relibc's
# own workspace and lockfile, so it is the version and features relibc uses. relibc's
# profile aborts on panic; the test harness needs unwinding, so this build unwinds.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cargo build --locked --quiet --manifest-path "$root/crates/relibc/Cargo.toml" -p object \
    --config 'profile.dev.panic="unwind"' --target-dir "$work/object"
rustc --edition=2024 --test -D warnings "$root/scripts/ferric-ci/test-ld-so-errors.rs" \
    --extern object="$work/object/debug/libobject.rlib" \
    -L dependency="$work/object/debug/deps" -o "$work/test"
"$work/test"
