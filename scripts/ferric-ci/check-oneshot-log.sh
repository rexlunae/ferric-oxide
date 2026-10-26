#!/usr/bin/env bash
# Check the kernel.time.one_shot boot lines in a boot-smoke.sh serial log (issue #186).
#
# Usage: check-oneshot-log.sh LOG [CPUS]
#   CPUS is how many CPUs the guest has (boot-smoke.sh starts QEMU with -smp 4).
#
# The BSP must have calibrated the TSC ("time: tsc calibrated at ... Hz against the ...")
# and picked a clock source ("time: clock source ..."). Every CPU must have finished its
# one-shot lateness series ("timer: cpu N: <backend> fired 24/24 ..."), and the BSP's
# selftest rounds must have passed (a one-second deadline fired, an already-due deadline
# fired at once, a disarmed timer stayed quiet). A failed calibration, a CPU without a
# one-shot timer, an "(incomplete)" series or a failed round fails the check. The
# lateness figures and the clock source are reported in a notice, not judged: a guest's
# timer interrupts are late by however long the host takes to run the vCPU.
set -euo pipefail

log="$1"
cpus="${2:-4}"
clean="$(tr -d '\r' < "$log")"

fail() {
    echo "::error::$*" >&2
    exit 1
}

first() {
    grep -aoE "$1" <<< "$clean" | head -n 1 || true
}

if [ -n "$(first 'time: tsc calibration failed.*')" ]; then
    fail "$(first 'time: tsc calibration failed.*')"
fi
tsc="$(first 'time: tsc calibrated at [0-9]+ Hz against the [a-z ]+ .*')"
[ -n "$tsc" ] || fail "no TSC calibration line (time: tsc calibrated at ... Hz against the ...)"
source="$(first 'time: (clock source .*|no continuous clock source.*)')"
[ -n "$source" ] || fail "no clock source line (time: clock source ...)"

for bad in 'timer: cpu [0-9]+: no one-shot timer.*' \
    'timer: cpu [0-9]+: .*\(incomplete\)' \
    'timer: one-shot selftest round [0-9]: (no fire|expected Fired|a disarmed timer fired).*' \
    'timer: one-shot selftest round 1 arm: .*' \
    'timer: one-shot selftest skipped.*'; do
    line="$(first "$bad")"
    [ -z "$line" ] || fail "$line"
done

lines="$(grep -aoE 'timer: cpu [0-9]+: [a-z-]+ fired [0-9]+/[0-9]+ .*' <<< "$clean" || true)"
for ((cpu = 0; cpu < cpus; cpu++)); do
    line="$(grep -E "^timer: cpu $cpu: " <<< "$lines" | head -n 1 || true)"
    [ -n "$line" ] || fail "cpu $cpu has no one-shot lateness line"
    read -r fired expected < <(sed -E 's/.* fired ([0-9]+)\/([0-9]+) .*/\1 \2/' <<< "$line")
    [ "$fired" = "$expected" ] || fail "cpu $cpu: only $fired of $expected one-shot deadlines fired"
done

round1="$(first 'timer: one-shot selftest round 1: fired after .*')"
[ -n "$round1" ] || fail "the BSP's one-second one-shot deadline did not report (round 1)"
[ -n "$(first 'timer: one-shot selftest round 2: a due deadline fired at once')" ] ||
    fail "the BSP's already-due deadline did not fire at once (round 2)"
[ -n "$(first 'timer: one-shot selftest round 3: a disarmed timer stayed quiet')" ] ||
    fail "the BSP's disarmed timer was not checked (round 3)"

echo "::notice title=one-shot timers::$tsc%0A$source%0A$round1%0A$(sed ':a;N;$!ba;s/\n/%0A/g' <<< "$lines")"
