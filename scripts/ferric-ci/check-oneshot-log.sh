#!/usr/bin/env bash
# Check the kernel.oneshot_timer boot lines in a boot-smoke.sh serial log (issue #186).
#
# Usage: check-oneshot-log.sh LOG [CPUS]
#   CPUS is how many CPUs the guest has (boot-smoke.sh starts QEMU with -smp 4).
#
# The BSP must have calibrated the TSC ("oneshot: TSC ... Hz from HPET|PIT"), and every CPU
# must have finished its one-shot self-test: "oneshot: cpu N: <backend> fired 24/24 ...".
# An "(incomplete)" self-test or a failed calibration fails the check. The lateness figures
# are reported in a notice, not judged: a guest's timer interrupts are late by however long
# the host takes to run the vCPU.
set -euo pipefail

log="$1"
cpus="${2:-4}"
clean="$(tr -d '\r' < "$log")"

fail() {
    echo "::error::$*" >&2
    exit 1
}

tsc="$(grep -aoE 'oneshot: TSC [0-9]+ Hz from (HPET|PIT).*' <<< "$clean" | head -n 1 || true)"
[ -n "$tsc" ] || fail "no TSC calibration line (oneshot: TSC ... Hz from ...)"
if grep -aqE 'oneshot: (TSC calibration failed|cpu [0-9]+: countdown calibration failed)' <<< "$clean"; then
    fail "$(grep -aoE 'oneshot: .*calibration failed.*' <<< "$clean" | head -n 1)"
fi
if grep -aqF '(incomplete)' <<< "$(grep -aF 'oneshot: cpu ' <<< "$clean")"; then
    fail "$(grep -aoE 'oneshot: cpu [0-9]+: .*\(incomplete\)' <<< "$clean" | head -n 1)"
fi

lines="$(grep -aoE 'oneshot: cpu [0-9]+: [a-z-]+ fired [0-9]+/[0-9]+ .*' <<< "$clean" || true)"
for ((cpu = 0; cpu < cpus; cpu++)); do
    line="$(grep -E "^oneshot: cpu $cpu: " <<< "$lines" | head -n 1 || true)"
    [ -n "$line" ] || fail "cpu $cpu has no one-shot self-test line"
    read -r fired expected < <(sed -E 's/.* fired ([0-9]+)\/([0-9]+) .*/\1 \2/' <<< "$line")
    [ "$fired" = "$expected" ] || fail "cpu $cpu: only $fired of $expected one-shot deadlines fired"
done

echo "::notice title=one-shot timers::$tsc%0A$(sed ':a;N;$!ba;s/\n/%0A/g' <<< "$lines")"
