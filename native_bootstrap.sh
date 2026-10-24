#!/usr/bin/env bash
# Native setup installs Rust components and Cargo utilities, never OS packages.
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
    echo 'Install Rust with rustup (https://rustup.rs), then rerun this script.' >&2
    exit 1
fi
exec cargo xtask setup "$@"
