#!/usr/bin/env bash
# Exercise PERF_AB (perf-ab.sh) through the two boot checks that gate on a same-boot A/B,
# with fixture serial logs: a failed A/B fails by default and with PERF_AB=enforce, only
# warns with PERF_AB=report, and any other value is refused.
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A no-IOMMU blkgrant log whose 4-thread A/B is AB (pass or fail).
blkgrant_log() {
    cat <<LOG
nvmed: MSI-X entries 1..=4 spread over CPUs [0, 1, 2, 3] in batches of 2 and 2, as irq:cpus and ferric_irq::spread give
nvmed: 4 I/O queue pairs (controller grants 4): pair 1 on the main thread, 3 per-CPU, workers on CPUs [1, 2, 3]
FERRIC_BENCH nvme-read-4k-t1 copy_iops=1000 ab=pass
FERRIC_BENCH nvme-read-4k-t2 copy_iops=1000 ab=pass
FERRIC_BENCH nvme-read-4k-t4 copy_iops=800 ab=$1
FERRIC_TEST_SKIP nvme-grant: nvmed has no DMA domain (no IOMMU)
FERRIC_BENCH nvme-read-128k copy_mib_s=100
FERRIC_BENCH nvme-read-4k copy_iops=1000
LOG
}

# An iobench log whose randread-4k-qd32 A/B is AB (pass or fail).
iobench_log() {
    local fields="ios=10 iops=1000 mib_s=3.9 lat_min_ns=1 lat_mean_ns=2 lat_p50_ns=2 lat_p99_ns=3 lat_p999_ns=4 lat_max_ns=5"
    for job in randread-4k-qd1 randread-4k-qd32 read-128k-qd1; do
        for policy in main adaptive workers; do
            echo "FERRIC_BENCH iobench-$job policy=$policy rounds=3 $fields"
        done
        ab=pass
        [ "$job" = randread-4k-qd32 ] && ab="$1"
        echo "FERRIC_BENCH iobench-$job ab_baseline_iops=1000 ab_candidate_iops=800 ab_ratio=800 ab_tolerance=900 ab_rounds=5 ab=$ab"
    done
    for job in file-randwrite-4k-qd1 file-write-128k-qd1; do
        for path in fs cache; do
            echo "FERRIC_BENCH iobench-$job path=$path rounds=3 $fields"
        done
        echo "FERRIC_BENCH iobench-$job ab_baseline_iops=1000 ab_candidate_iops=1000 ab_ratio=1000 ab_tolerance=50 ab_rounds=3 ab=pass"
    done
    echo "FERRIC_TEST_PASS iobench"
}

failures=0
# expect STATUS CHECK LOG [PERF_AB]: the check's exit status, an error from the A/B (or the
# PERF_AB value) when it fails, and a warning when it passes on a failed A/B.
expect() {
    local want="$1" check="$2" log="$3" out status=0
    out="$(env -u GITHUB_STEP_SUMMARY -u PERF_AB ${4:+PERF_AB="$4"} "$dir/$check" none "$log" 2>&1)" || status=$?
    if [ "$status" != "$want" ]; then
        echo "FAIL: $check $(basename "$log") PERF_AB=${4:-unset}: exit $status, want $want"
        echo "$out"
        failures=$((failures + 1))
    elif [ "$want" = 1 ] && ! grep -qE "^::error .*(slower than its single queue|PERF_AB must be)" <<< "$out"; then
        echo "FAIL: $check $(basename "$log") PERF_AB=${4:-unset}: failed for another reason"
        echo "$out"
        failures=$((failures + 1))
    elif [ "$want" = 0 ] && grep -q "ab=fail" "$log" && ! grep -q "^::warning" <<< "$out"; then
        echo "FAIL: $check $(basename "$log") PERF_AB=${4:-unset}: passed without a warning"
        failures=$((failures + 1))
    else
        echo "ok: $check $(basename "$log") PERF_AB=${4:-unset} exits $status"
    fi
}

for check in blkgrant iobench; do
    "${check}_log" pass > "$work/$check-pass.log"
    "${check}_log" fail > "$work/$check-fail.log"
    script="check-$check-log.sh"
    expect 0 "$script" "$work/$check-pass.log"
    expect 0 "$script" "$work/$check-pass.log" report
    expect 1 "$script" "$work/$check-fail.log"
    expect 1 "$script" "$work/$check-fail.log" enforce
    expect 0 "$script" "$work/$check-fail.log" report
    expect 1 "$script" "$work/$check-fail.log" warn
done

# File jobs are reported rather than gated, but their records remain mandatory.
sed '/iobench-file-.*ab=pass/s/ab=pass/ab=fail/' "$work/iobench-pass.log" > "$work/file-fail.log"
if ! env -u GITHUB_STEP_SUMMARY -u PERF_AB "$dir/check-iobench-log.sh" none "$work/file-fail.log" > "$work/out" 2>&1; then
    echo "FAIL: file A/B should be reported without failing disk gates"
    failures=$((failures + 1))
fi
sed '/iobench-file-/d' "$work/iobench-pass.log" > "$work/file-missing.log"
if env -u GITHUB_STEP_SUMMARY PERF_AB=report "$dir/check-iobench-log.sh" none "$work/file-missing.log" > "$work/out" 2>&1; then
    echo "FAIL: report mode must still reject missing file benchmark records"
    failures=$((failures + 1))
fi

[ "$failures" = 0 ] || { echo "test-perf-ab: $failures failed"; exit 1; }
echo "test-perf-ab: OK"
