#!/usr/bin/env bash
# Exercise relibc's generated socket macros without building or linking its libc.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/include/sys" "$work/include/bits"
for header in sys_socket bits_iovec bits_size-t bits_safamily-t bits_socklen-t bits_uint32-t bits_ssize-t; do
    source_dir="$root/crates/relibc/src/header/$header"
    cat "$source_dir/cbindgen.toml" "$root/crates/relibc/cbindgen.globdefs.toml" > "$work/cbindgen.toml"
    cbindgen "$source_dir/mod.rs" --config "$work/cbindgen.toml" \
        --output "$work/include/${header/_/\/}.h"
done
"${CC:-cc}" -std=c11 -Wall -Wextra -Wsign-compare -Werror -nostdinc -I "$work/include" \
    "$root/crates/relibc/tests/sys_socket/cmsg_nxthdr.c" -o "$work/cmsg_nxthdr"
"$work/cmsg_nxthdr"
printf 'Socket header regression tests passed\n'
