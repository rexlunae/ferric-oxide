#!/usr/bin/env bash
# Exercise check-oneshot-log.sh with fixture serial logs: a complete log passes, and a
# missing calibration, a failed calibration, a missing CPU, a short series and an
# "(incomplete)" line each fail.
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

tsc_line='kernel::arch::x86_shared::device::oneshot:INFO -- oneshot: TSC 2999999000 Hz from HPET (5 windows, spread 3 ppm, invariant true, CPUID 15H says None)'
cpu_line() {
    echo "kernel::arch::x86_shared::device::oneshot:INFO -- oneshot: cpu $1: tsc-deadline fired $2/24 early 0 late min 900 avg 4000 max 31000 ns"
}

complete() {
    printf '%s\r\n' "$tsc_line"
    for cpu in 0 1 2 3; do cpu_line "$cpu" 24; done
}

expect() {
    local want="$1" name="$2" log="$work/$2.log"
    cat > "$log"
    if "$dir/check-oneshot-log.sh" "$log" 4 > "$work/out" 2>&1; then got=pass; else got=fail; fi
    if [ "$got" != "$want" ]; then
        echo "$name: expected $want, got $got" >&2
        cat "$work/out" >&2
        exit 1
    fi
    echo "ok $name ($want)"
}

complete | expect pass complete
complete | grep -v 'oneshot: TSC' | expect fail no-calibration
{ complete; echo 'kernel::x:WARN -- oneshot: TSC calibration failed (the windows disagree by 9000 ppm); one-shot timers stay off'; } | expect fail calibration-failed
complete | grep -v 'oneshot: cpu 2:' | expect fail missing-cpu
{ printf '%s\n' "$tsc_line"; for cpu in 0 1 2; do cpu_line "$cpu" 24; done; cpu_line 3 7; } | expect fail short-series
{ complete; echo 'kernel::x:WARN -- oneshot: cpu 1: lapic-countdown fired 2/24 early 1 late min 1 avg 1 max 1 ns (incomplete)'; } | expect fail incomplete
