#!/usr/bin/env bash
# Checks check-sched-bench-log.sh against fixture sched-bench serial logs whose
# counts lines have the fields `ferric-trace counts` prints
# (tools/ferric-trace/src/counts.rs).
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/check-sched-bench-log.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail() {
    echo "FAIL: $1: $2" >&2
    cat "$work/output" >&2
    exit 1
}

gaps="gap_ns_min=3990000 gap_ns_mean=4000000 gap_ns_p50=4000000 gap_ns_p99=4100000 gap_ns_p999=4200000 gap_ns_max=12000000"

# A complete log of a boot with CPUS vCPUs, as the image prints it (with the console's CRs).
log() {
    local cpus="$1" window events
    echo "FERRIC_SCHED_BENCH_BEGIN"
    echo "FERRIC_SCHED_BENCH_TSC tsc_mhz=2800.213"
    for window in idle burn 1 16 64 256; do
        events="tick switch"
        if [ "$cpus" -gt 1 ]; then
            events="tick ipi_pit switch"
            case "$window" in idle | burn) ;; *) events="$events ipi_wake" ;; esac
        fi
        case "$window" in idle | burn) ;; *) events="$events context_create" ;; esac
        for event in $events; do
            echo "counts sched.$event cpu=0 count=2440 rate_per_s=243 $gaps lost_before=0 lost_during=0"
            echo "counts sched.$event cpu=all count=2440 rate_per_s=243 $gaps"
        done
        case "$window" in
            burn)
                echo "FERRIC_BURN_BEGIN seconds=10"
                echo "FERRIC_BURN_END seconds=10 spins=123456"
                ;;
            idle) ;;
            *)
                echo "FERRIC_SCALE_SETUP rings=$window"
                echo "FERRIC_SCALE_WAKE rings=$window"
                echo "FERRIC_BENCH ring-scale rings=$window setup_ms=3 wake_ns_mean=9000 wake_ns_p50=8000 wake_ns_p99=20000 wake_ns_p999=30000 wake_ns_max=40000"
                echo "FERRIC_TEST_PASS ring-scale"
                ;;
        esac
    done
    echo "FERRIC_BENCH ring null-write-64 min_ns=1"
    echo "FERRIC_BENCH ring file-read-4k min_ns=1"
    echo "FERRIC_BENCH trace-bench io off min_ns=1"
    echo "FERRIC_BENCH iobench-seq-read min_ns=1"
    echo "FERRIC_SCHED_BENCH_DONE"
    echo "FERRIC_SMOKE_OK"
}

# Runs the checker on LOG with CPUS and expects STATUS (0 or 1); NAME labels the case.
expect() {
    local name="$1" status="$2" file="$3" cpus="$4" got=0
    bash "$script" "$file" "$cpus" > "$work/output" 2>&1 || got=$?
    [ "$got" = "$status" ] || fail "$name" "exit $got, expected $status"
    echo "PASS: $name"
}

# Complete baselines pass. Issue #285: the six gap fields broke the tick match, so every
# complete baseline failed, and the 1-vCPU one also needed cross-CPU wakeup IPIs.
log 4 | sed 's/$/\r/' > "$work/smp.log"
expect "4-vCPU baseline" 0 "$work/smp.log" 4
log 1 | sed 's/$/\r/' > "$work/up.log"
expect "1-vCPU baseline" 0 "$work/up.log" 1
grep -qF "counts sched.tick cpu=all" "$work/output" || fail "1-vCPU baseline" "counts lines not in the notice"
grep -qF "FERRIC_SCHED_BENCH_TSC tsc_mhz=2800.213" "$work/output" || fail "1-vCPU baseline" "TSC rate not in the notice"

# A 4-vCPU boot still needs its IPIs.
expect "4-vCPU log without IPIs" 1 "$work/up.log" 4
grep -qF "sched\\.ipi_pit" "$work/output" || fail "4-vCPU log without IPIs" "did not name the missing line"
grep -v "sched.ipi_wake cpu=all" "$work/smp.log" > "$work/no-wake.log"
expect "4-vCPU log without wakeup IPIs" 1 "$work/no-wake.log" 4

# Five tick windows instead of six: reported, not a silent exit.
awk '/sched.tick cpu=all/ && !dropped { dropped = 1; next } { print }' "$work/smp.log" > "$work/five.log"
expect "five tick windows" 1 "$work/five.log" 4
grep -qF "saw 5" "$work/output" || fail "five tick windows" "no count in the message"
# No tick lines at all (as the old regex saw a real log): the message, not a silent exit.
grep -v "sched.tick cpu=all" "$work/smp.log" > "$work/no-ticks.log"
expect "no tick windows" 1 "$work/no-ticks.log" 4
grep -qF "saw 0" "$work/output" || fail "no tick windows" "no count in the message"
# A tick line missing a gap field does not count.
sed '0,/sched.tick cpu=all/s/ gap_ns_p50=[0-9]*//' "$work/smp.log" > "$work/short.log"
expect "tick line without p50" 1 "$work/short.log" 4

# The measured TSC rate must be in the log: the counts lines' nanoseconds depend on it.
grep -v "FERRIC_SCHED_BENCH_TSC" "$work/smp.log" > "$work/no-tsc.log"
expect "no TSC rate" 1 "$work/no-tsc.log" 4
sed 's/tsc_mhz=2800.213/tsc_mhz=/' "$work/smp.log" > "$work/empty-tsc.log"
expect "empty TSC rate" 1 "$work/empty-tsc.log" 4

# A failed test anywhere fails the log.
{ cat "$work/smp.log"; echo "FERRIC_TEST_FAIL ring exited with an error"; } > "$work/test-fail.log"
expect "FERRIC_TEST_FAIL" 1 "$work/test-fail.log" 4
