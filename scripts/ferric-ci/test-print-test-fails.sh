#!/usr/bin/env bash
# Checks print-test-fails.sh against fixture console logs.
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/print-test-fails.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail() {
    echo "$1: $2" >&2
    cat "$work/output" >&2
    exit 1
}

# No failed test: nothing printed.
printf 'line 1\nFERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK\n' > "$work/pass.log"
bash "$script" "$work/pass.log" "boot 1" > "$work/output"
[ ! -s "$work/output" ] || fail pass "expected no output"
echo "PASS: pass"

# Two failures, the first 60 lines in, and a console line that looks like a
# workflow command.
{
    for i in $(seq 1 60); do echo "line $i"; done
    echo "FERRIC_TEST_FAIL tlb: shootdown lost"
    echo "::error::from the console"
    echo "line 61"
    echo "FERRIC_TEST_FAIL futex: timed out"
    for i in $(seq 62 70); do echo "line $i"; done
} > "$work/fail.log"
bash "$script" "$work/fail.log" "trace boot 2" > "$work/output"
head -n 1 "$work/output" | grep -qxF "::group::trace boot 2: each FERRIC_TEST_FAIL with the 40 lines before it" ||
    fail fail "no group header"
stop="$(sed -n '2s/^::stop-commands::\(.*\)$/\1/p' "$work/output")"
[ -n "$stop" ] || fail fail "workflow commands not stopped before the log"
# Exactly the 40 lines before the first failure, then everything up to the second.
grep -qxF "line 21" "$work/output" || fail fail "line 21, 40 before the first failure, missing"
! grep -qxF "line 20" "$work/output" || fail fail "line 20 is more than 40 lines before"
grep -qxF "FERRIC_TEST_FAIL tlb: shootdown lost" "$work/output" || fail fail "first failure missing"
grep -qxF "FERRIC_TEST_FAIL futex: timed out" "$work/output" || fail fail "second failure missing"
! grep -qxF "line 62" "$work/output" || fail fail "a line after the last failure was printed"
# The console's command-like line comes after the stop and before the resume.
awk -v stop="::$stop::" '
    /^::stop-commands::/ { stopped = 1 }
    $0 == stop { stopped = 0; resumed = 1 }
    $0 == "::error::from the console" { if (!stopped) exit 1; seen = 1 }
    END { exit !(seen && resumed) }' "$work/output" ||
    fail fail "the console line was printed while workflow commands were on"
tail -n 1 "$work/output" | grep -qxF "::endgroup::" || fail fail "group not closed"
echo "PASS: fail"
