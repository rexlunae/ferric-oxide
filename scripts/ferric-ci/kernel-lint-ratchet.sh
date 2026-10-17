#!/usr/bin/env bash
# Lint and unsafe-code ratchet for the kernel.
#
# The kernel is an upstream subtree with hundreds of existing warnings for
# lints the roadmap wants denied (arithmetic_side_effects, indexing_slicing,
# unwrap_used). Fixing them all at once would diverge far from upstream, so CI
# holds each count at or below the baseline in ci/kernel-lints.json instead:
# new code cannot add warnings, and each fix lowers the bar for good.
#
# Usage: scripts/ferric-ci/kernel-lint-ratchet.sh [--update]
#   --update  rewrite the baseline with the current counts (run after fixing
#             warnings, and commit the result)
#
# Run from the repository root with the kernel's toolchain installed.
set -euo pipefail

baseline="ci/kernel-lints.json"
update=false
if [ "${1-}" = "--update" ]; then
    update=true
fi

# Lints that are not in crates/kernel/Cargo.toml but are tracked here, so unsafe code
# without a SAFETY comment is counted too.
extra_lints=(-W clippy::undocumented_unsafe_blocks)

messages="$(mktemp)"
trap 'rm -f "$messages"' EXIT

(
    cd crates/kernel
    RUST_TARGET_PATH="$PWD/targets" cargo clippy \
        --bin kernel \
        --target targets/x86_64-unknown-kernel.json \
        -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem \
        -Z json-target-spec \
        --message-format json \
        -- "${extra_lints[@]}"
) > "$messages"

# One entry per distinct warning site, so duplicate reports count once.
current="$(
    jq -s '
        [ .[]
          | select(.reason == "compiler-message" and .message.level == "warning")
          | .message
          | select(.code != null)
          | { code: .code.code,
              site: ((.spans[0].file_name // "") + ":" + ((.spans[0].line_start // 0) | tostring)
                     + ":" + ((.spans[0].column_start // 0) | tostring)) } ]
        | unique
        | group_by(.code)
        | map({ key: .[0].code, value: length })
        | from_entries
    ' "$messages"
)"

# Count of `unsafe` blocks, functions, impls and traits in kernel sources.
unsafe_count="$(grep -rEo '\bunsafe\b' crates/kernel/src --include='*.rs' | wc -l)"
current="$(jq --argjson n "$unsafe_count" '. + { "unsafe keyword": $n }' <<< "$current")"

if $update; then
    jq -S . <<< "$current" > "$baseline"
    echo "Wrote $baseline:"
    cat "$baseline"
    exit 0
fi

report="$(
    jq -rn --argjson base "$(cat "$baseline")" --argjson cur "$current" '
        ($base + $cur | keys) as $keys
        | $keys[]
        | { key: ., base: ($base[.] // 0), cur: ($cur[.] // 0) }
        | "\(if .cur > .base then "UP  " elif .cur < .base then "down" else "same" end) \(.key): \(.base) -> \(.cur)"
    '
)"
echo "$report"

if grep -q '^UP ' <<< "$report"; then
    echo
    echo "Kernel warnings or unsafe uses went up. Fix the new ones (see the" \
        "clippy output with the same command) rather than raising $baseline."
    exit 1
fi
if grep -q '^down' <<< "$report"; then
    echo
    echo "Counts went down. Run '$0 --update' and commit $baseline to lock in the improvement."
fi
