#!/usr/bin/env bash
# Exercise check-oneshot-log.sh with fixture serial logs: a complete log passes, and a
# missing or failed calibration, a missing clock source, a missing CPU, a short series, an
# "(incomplete)" line, a CPU without a timer and each failed selftest round fail.
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

prefix='kernel::arch::x86_shared::time::continuous:INFO -- '
tsc_line="${prefix}time: tsc calibrated at 2999999000 Hz against the hpet (5 windows, spread 3 ppm, invariant true)"
source_line="${prefix}time: clock source invariant tsc (2999999000 Hz)"
cpu_line() {
    echo "kernel::arch::x86_shared::timer:INFO -- timer: cpu $1: tsc-deadline fired $2/24 early 0 late min 900 avg 4000 max 31000 ns"
}
rounds() {
    echo 'kernel::arch::x86_shared::timer:INFO -- timer: one-shot selftest round 1: fired after 1000012000 ns (wanted 1000000000, error 12000)'
    echo 'kernel::arch::x86_shared::timer:INFO -- timer: one-shot selftest round 2: a due deadline fired at once'
    echo 'kernel::arch::x86_shared::timer:INFO -- timer: one-shot selftest round 3: a disarmed timer stayed quiet'
}

complete() {
    printf '%s\r\n' "$tsc_line" "$source_line"
    for cpu in 0 1 2 3; do cpu_line "$cpu" 24; done
    rounds
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
complete | grep -v 'tsc calibrated' | expect fail no-calibration
complete | grep -v 'clock source' | expect fail no-clock-source
{ complete; echo 'kernel::x:WARN -- time: tsc calibration failed against the pit (the windows disagree by 9000 ppm); no tsc clock or one-shot timer'; } | expect fail calibration-failed
complete | grep -v 'timer: cpu 2:' | expect fail missing-cpu
{ printf '%s\n' "$tsc_line" "$source_line"; for cpu in 0 1 2; do cpu_line "$cpu" 24; done; cpu_line 3 7; rounds; } | expect fail short-series
{ complete; echo 'kernel::x:WARN -- timer: cpu 1: lapic-countdown fired 2/24 early 1 late min 1 avg 1 max 1 ns (incomplete)'; } | expect fail incomplete
{ complete; echo 'kernel::x:WARN -- timer: cpu 3: no one-shot timer (countdown calibration failed: the windows disagree by 9000 ppm)'; } | expect fail no-timer
complete | grep -v 'round 1' | expect fail round1-missing
{ complete; echo 'kernel::x:WARN -- timer: one-shot selftest round 1: no fire (wedged): 0 handler entries, lvt read-back 0x40030'; } | expect fail round1-wedged
complete | grep -v 'round 2' | expect fail round2-missing
{ complete; echo 'kernel::x:WARN -- timer: one-shot selftest round 3: a disarmed timer fired!'; } | expect fail round3-fired
