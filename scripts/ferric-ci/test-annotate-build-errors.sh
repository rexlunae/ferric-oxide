#!/usr/bin/env bash
# Checks annotate-build-errors.sh against fixture build logs.
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/annotate-build-errors.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail() {
    echo "$1: $2" >&2
    cat "$work/output" >&2
    exit 1
}
# The annotation's message, one line per log line again.
message() { sed -e 's/^::error title=[^:]*:://' -e 's/%0A/\n/g' -e 's/%25/%/g' "$work/output"; }
esc=$'\e'

# A failed image build: an early rustc error with quoted source, a download whose
# progress bar is redrawn with CRs, then pages of strip lines after the error.
{
    for i in $(seq 1 100); do echo "noise $i"; done
    echo "error[E0308]: mismatched types"
    echo "  --> src/main.rs:3:5"
    echo '3 |     log("event result is error: {:?}", e);'
    for i in $(seq 101 200); do echo "noise $i"; done
    echo 'warning: unused `Result`'
    echo '172 |   .map_err(|e| anyhow!("event result is error: {:?}", e))'
    for i in $(seq 201 300); do echo "noise $i"; done
    printf '###   4.7%%\r########   11.2%%curl: (92) HTTP/2 stream 1 was not closed cleanly\n'
    echo "${esc}[1m${esc}[38;5;196mcook ca-certificates - failed ${esc}[39m${esc}[m"
    echo 'Failed to run ["curl" "--fail" "https://example.org/ca-certificates.pkgar"]: exited with status 92'
    echo "make[1]: *** [mk/repo.mk:12: cook] Error 1"
    echo "make: *** [mk/repo.mk:4: build/x86_64/ferric-smoke/repo.tag] Error 2"
    for i in $(seq 1 500); do echo "strip: copied kernel section $i"; done
} > "$work/build.log"
bash "$script" "$work/build.log" "Build image failed" > "$work/output"
[ "$(wc -l < "$work/output")" -eq 1 ] || fail build "expected exactly one annotation line"
[[ "$(cat "$work/output")" == "::error title=Build image failed::Error lines from the build log"* ]] ||
    fail build "no error annotation"
message | grep -qxF "error[E0308]: mismatched types" || fail build "rustc error missing"
message | grep -qxF "noise 96" || fail build "noise 96, 5 lines before the rustc error, missing"
! message | grep -qxF "noise 95" || fail build "noise 95 is more than 5 lines before"
message | grep -qxF "  --> src/main.rs:3:5" || fail build "line after the rustc error missing"
! message | grep -qxF "noise 102" || fail build "noise 102 is more than 2 lines after the error"
! message | grep -qF 'warning: unused' || fail build "quoted source with error: was taken as an error"
message | grep -qxF "cook ca-certificates - failed " || fail build "colour codes not removed from the cook line"
! message | grep -qF "$esc" || fail build "an escape character reached the annotation"
message | grep -qxF "########   11.2%curl: (92) HTTP/2 stream 1 was not closed cleanly" ||
    fail build "the progress line is not what the terminal last showed"
message | grep -qF 'Failed to run ["curl"' || fail build "Failed to run line missing"
message | grep -qxF "make: *** [mk/repo.mk:4: build/x86_64/ferric-smoke/repo.tag] Error 2" ||
    fail build "make line missing"
message | grep -qxF "strip: copied kernel section 2" || fail build "context after the last error missing"
! message | grep -qxF "strip: copied kernel section 3" || fail build "the log tail was included"
[ "$(message | grep -cxF -- "--")" -eq 1 ] || fail build "expected one gap, between the two excerpts"
[ "$(message | grep -cxF "cook ca-certificates - failed ")" -eq 1 ] || fail build "overlapping excerpts repeated a line"
grep -qF "11.2%25curl" "$work/output" || fail build "% not escaped"
echo "PASS: build"

# No error line at all: the end of the log instead.
{ for i in $(seq 1 100); do echo "step $i"; done; } > "$work/plain.log"
bash "$script" "$work/plain.log" "Build image failed" > "$work/output"
message | grep -qF "No error line found" || fail tail "no fallback heading"
message | grep -qxF "step 61" || fail tail "step 61, 40 lines from the end, missing"
! message | grep -qxF "step 60" || fail tail "more than 40 lines kept"
echo "PASS: tail"

# More errors than fit: the first ones are kept and the cut is reported.
{ for i in $(seq 1 400); do echo "error: failure number $i"; done; } > "$work/many.log"
bash "$script" "$work/many.log" "Build image failed" > "$work/output"
message | grep -qxF "error: failure number 1" || fail budget "first error dropped"
! message | grep -qxF "error: failure number 400" || fail budget "budget not applied"
message | grep -qE '^\[\.\.\. [0-9]+ more lines; see the job log\]$' || fail budget "cut not reported"
[ "$(wc -c < "$work/output")" -lt 4500 ] || fail budget "annotation too long"
echo "PASS: budget"

# A long line is cut, and the title's : and , are escaped.
{ printf 'error: %0600d\n' 0; } > "$work/long.log"
bash "$script" "$work/long.log" "Build: image, enforce" > "$work/output"
[[ "$(cat "$work/output")" == "::error title=Build%3A image%2C enforce::"* ]] || fail long "title not escaped"
message | grep -qE '^error: 0{393} \[\.\.\.\]$' || fail long "long line not cut at 400 characters"
echo "PASS: long"
