#!/usr/bin/env bash
# Check a ferric-sched-bench image boot log (config/ferric-sched-bench.toml) for the
# scheduler/timer baseline protocol: the markers, the per-phase trace counts of the
# periodic tick, Pit broadcast and wakeup IPIs, the waiting-ring scale runs, and the
# benchmark lines. The measurements themselves are printed as a notice; this script
# only checks that every phase ran and reported.
#
# Usage: check-sched-bench-log.sh LOG [CPUS]
#   CPUS (default 4) names the boot's vCPU count; a 1-vCPU boot has no Pit broadcast
#   or cross-CPU wakeup IPIs, so those lines are not expected there.
set -euo pipefail

log="$1"
cpus="${2:-4}"
clean="$(tr -d '\r\000' < "$log")"

fail() {
    echo "::error title=sched-bench::$*" >&2
    exit 1
}

need() {
    grep -aE "$1" <<< "$clean" || fail "missing from the log: $1"
}

need "FERRIC_SCHED_BENCH_BEGIN"
need "FERRIC_SCHED_BENCH_DONE"
need "FERRIC_SMOKE_OK"
if grep -aE "FERRIC_TEST_FAIL" <<< "$clean"; then
    fail "a FERRIC_TEST_FAIL line is present"
fi

# The six windows (idle, burn, four ring counts) each report the periodic tick and
# its inter-tick gaps (the tick period and jitter the tickless work must stop).
ticks="$(grep -acE '^counts sched\.tick cpu=all count=[0-9]+ rate_per_s=[0-9]+ gap_ns_p99=[0-9]+' <<< "$clean")"
if [ "$ticks" -lt 6 ]; then
    fail "expected six per-window sched.tick count lines, saw $ticks"
fi
if [ "$cpus" -gt 1 ]; then
    need '^counts sched\.ipi_pit cpu=all count=[1-9][0-9]* rate_per_s=[0-9]+ gap_ns_p99=[0-9]+'
fi

# The burn window shows the periodic preemption of a greedy context: switches.
need '^counts sched\.switch cpu=all count=[1-9][0-9]* .*gap_ns_p99=[0-9]+'
# The waiting-ring windows show the wake bursts and the ring worker churn.
need '^counts sched\.ipi_wake cpu=all count=[1-9][0-9]* .*gap_ns_p99=[0-9]+'
need '^counts sched\.context_create cpu=all count=[1-9][0-9]*'

# The scale runs: setup and wake markers, the wake-latency line, and the pass, for
# each ring count.
for rings in 1 16 64 256; do
    need "FERRIC_SCALE_SETUP rings=$rings"
    need "FERRIC_SCALE_WAKE rings=$rings"
    need "FERRIC_BENCH ring-scale rings=$rings setup_ms=[0-9]+ wake_ns_mean=[0-9]+ wake_ns_p50=[0-9]+ wake_ns_p99=[0-9]+ wake_ns_p999=[0-9]+ wake_ns_max=[0-9]+"
    need "FERRIC_TEST_PASS ring-scale"
done

# The burn phase ran a whole busy window.
need "FERRIC_BURN_BEGIN seconds=10"
need "FERRIC_BURN_END seconds=10 spins=[1-9][0-9]*"

# The benchmark phases report their usual lines with tracing on.
need "FERRIC_BENCH ring null-write-64"
need "FERRIC_BENCH ring file-read-4k"
need "FERRIC_BENCH trace-bench"
need "FERRIC_BENCH iobench-"

lines="$(grep -aoE "FERRIC_BENCH (ring-scale|iobench-|ring |trace-bench)|counts sched\.(tick|ipi_pit|ipi_wake|switch|context_create) cpu=all.*" <<< "$clean")"
echo "::notice title=sched-bench::$lines"
