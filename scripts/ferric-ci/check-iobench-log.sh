#!/usr/bin/env bash
# Check ferric-iobench's lines in a boot-smoke.sh serial log: the roadmap Phase 2 disk
# benchmark (fio-style jobs on the NVMe disk with each of nvmed's queue policies).
#
# Usage: check-iobench-log.sh MODE LOG
#   MODE names the boot in the annotations and the summary table (none, vtd, enforce, ...).
#
# Every default job must print a line per queue policy (main: the one-queue path; adaptive:
# the per-CPU default; workers) and an A/B line. drivers.nvme_percore is on in every CI image,
# so the A/B must have run (ab=pass or ab=fail, not ab=none), and the per-CPU default must
# reach the single queue's IOPS within the same-boot noise (ferric_blkqueue::ab). The figures
# go out as a notice and, when GITHUB_STEP_SUMMARY is set, as a table in the job summary.
set -euo pipefail

mode="$1"
log="$2"
clean="$(tr -d '\r' < "$log")"
jobs="randread-4k-qd1 randread-4k-qd32 read-128k-qd1"

fail() {
    echo "::error title=iobench ($mode)::$*" >&2
    exit 1
}

need() {
    grep -aE "$1" <<< "$clean" || fail "missing from the log: $1"
}

if grep -aE "FERRIC_TEST_FAIL iobench" <<< "$clean"; then
    fail "ferric-iobench failed"
fi
need "FERRIC_TEST_PASS iobench"

fields="ios=[0-9]+ iops=[0-9]+ mib_s=[0-9]+\.[0-9] lat_min_ns=[0-9]+ lat_mean_ns=[0-9]+ lat_p50_ns=[0-9]+ lat_p99_ns=[0-9]+ lat_p999_ns=[0-9]+ lat_max_ns=[0-9]+"
for job in $jobs; do
    for policy in main adaptive workers; do
        need "FERRIC_BENCH iobench-$job policy=$policy rounds=[0-9]+ $fields"
    done
    need "FERRIC_BENCH iobench-$job ab_baseline_iops=[0-9]+ ab_candidate_iops=[0-9]+ ab_ratio=[0-9]+ ab_tolerance=[0-9]+ ab_rounds=[0-9]+ ab=(pass|fail)"
done

lines="$(grep -aoE "FERRIC_BENCH iobench-.*" <<< "$clean" | sed 's/^FERRIC_BENCH //')"
echo "::notice title=iobench ($mode)::$(paste -sd ';' - <<< "$lines")"

# A field's value in a key=value line, or nothing when the line has no such field.
field() {
    { grep -oE "(^| )$1=[^ ]+" <<< "$2" || true; } | sed 's/^ //; s/^[^=]*=//'
}
us() {
    awk -v ns="$1" 'BEGIN { printf "%.1f", ns / 1000 }'
}
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "### ferric-iobench ($mode)"
        echo
        echo "| Job | Policy | IOPS | MiB/s | p50 µs | p99 µs | p99.9 µs | max µs | A/B |"
        echo "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |"
        # One row per job and policy; the A/B lines have no policy.
        while read -r line; do
            job="${line%% *}"
            policy="$(field policy "$line")"
            [ -n "$policy" ] || continue
            ab="$(grep -aE "^$job ab_" <<< "$lines" | head -n 1 || true)"
            verdict=""
            if [ "$policy" = adaptive ]; then
                verdict="$(field ab "$ab") (ratio $(field ab_ratio "$ab")‰, tolerance $(field ab_tolerance "$ab")‰)"
            fi
            echo "| ${job#iobench-} | $policy | $(field iops "$line") | $(field mib_s "$line") | $(us "$(field lat_p50_ns "$line")") | $(us "$(field lat_p99_ns "$line")") | $(us "$(field lat_p999_ns "$line")") | $(us "$(field lat_max_ns "$line")") | $verdict |"
        done <<< "$lines"
        echo
    } >> "$GITHUB_STEP_SUMMARY"
fi

if grep -aE "^iobench-[^ ]+ ab_.* ab=fail" <<< "$lines"; then
    fail "nvmed's per-CPU default policy is slower than its single queue in the same boot"
fi
echo "check-iobench-log: $mode OK"
