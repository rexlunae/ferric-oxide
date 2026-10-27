#!/usr/bin/env bash
# Compile the actual loader errors without linking relibc over the host libc.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
rustc --edition=2024 --test -D warnings "$root/scripts/ferric-ci/test-ld-so-errors.rs" -o "$work/test"
"$work/test"
