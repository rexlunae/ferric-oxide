#!/usr/bin/env bash
# Tracing overhead budget (roadmap Phase T): compares the FERRIC_BENCH lines
# that ferric-trace-bench (recipes/tests/ferric-trace-bench) prints in the
# serial logs of boots of kernels built without and with kernel.trace.
#
# Usage: check-trace-overhead.sh --out LOG... --in LOG...
#   --out  logs of boots of a kernel built without kernel.trace (state "out")
#   --in   logs of boots of a kernel with kernel.trace (states "off", "pairoff" and "on")
#
# Each boot's fastest round of a workload and state is its result, and the
# median of those over all given boots counts: boot-to-boot variation (kernel
# image KASLR moves the code, the runner's neighbours come and go) is several
# times the budget, and the median of many boots keeps one lucky or unlucky
# boot from deciding. The budgets:
#   compiled in, all off ("off") at most OFF_BUDGET_PCT (default 1) percent
#   slower than compiled out ("out"), on the "syscall" workload, a null system
#   call;
#   every compiled-in event on ("on") at most ON_BUDGET_PCT (default 5)
#   percent slower than all off in the rounds paired with it ("pairoff"), on
#   the "io" workload, requests to RedoxFS.
# Every result is reported. "off" and "out" run on two kernels built
# separately, so their comparison also measures where the linker put the hot
# code, and which runner the job got. On the null system call that is small
# and repeatable (+0.53% and +0.54% in two runs), and the tracepoints are the
# largest share of the work, so a disabled tracepoint that costs anything
# shows there first. On "io", which runs through far more kernel code, the
# same comparison went from -4.25% to +1.12% between those two runs with no
# tracepoint changed. It is reported, not judged, because judging it would
# judge the layout. "on" and "pairoff" run on the same kernel.
set -euo pipefail

off_budget="${OFF_BUDGET_PCT:-1}"
on_budget="${ON_BUDGET_PCT:-5}"

out_logs=()
in_logs=()
list=""
for arg in "$@"; do
    case "$arg" in
        --out) list=out ;;
        --in) list=in ;;
        *)
            case "$list" in
                out) out_logs+=("$arg") ;;
                in) in_logs+=("$arg") ;;
                *) echo "check-trace-overhead: give --out or --in before $arg" >&2; exit 2 ;;
            esac
            ;;
    esac
done
if [ "${#out_logs[@]}" -eq 0 ] || [ "${#in_logs[@]}" -eq 0 ]; then
    echo "usage: check-trace-overhead.sh --out LOG... --in LOG..." >&2
    exit 2
fi

# Sets `result` to the median over LOGS of each log's first min_ns for WORKLOAD
# in STATE, and `missing` to the logs that have no such line. `result` is empty
# when any log is missing one, so a workload is never judged on fewer boots
# than the job ran. It runs in the current shell, not in a command
# substitution, so a log without a match is reported instead of ending the
# script under `set -e` and `pipefail`.
best() {
    local workload="$1" state="$2" log value values=()
    shift 2
    missing=()
    for log in "$@"; do
        value="$(tr -d '\r' < "$log" \
            | { grep -ao "FERRIC_BENCH $workload $state min_ns=[0-9.]*" || true; } \
            | sed 's/.*min_ns=//' | head -n 1)"
        if [ -n "$value" ]; then
            values+=("$value")
        else
            missing+=("$log")
        fi
    done
    result=""
    if [ "${#missing[@]}" -eq 0 ]; then
        result="$(printf '%s\n' "${values[@]}" | sort -g | awk '{ v[NR] = $1 } END {
            if (NR % 2) print v[(NR + 1) / 2]; else printf "%.3f\n", (v[NR / 2] + v[NR / 2 + 1]) / 2
        }')"
    fi
}

# True when MEASURED is more than BUDGET percent above BASE. It compares the
# unrounded values: the percentages printed in the summary are rounded to three
# places, and an increase just over a budget must not pass once rounded.
over_budget() {
    awk -v m="$1" -v base="$2" -v b="$3" 'BEGIN { exit !(m > base * (1 + b / 100)) }'
}

status=0
for workload in io syscall; do
    complete=1
    for state in out off pairoff on; do
        if [ "$state" = out ]; then
            best "$workload" "$state" "${out_logs[@]}"
        else
            best "$workload" "$state" "${in_logs[@]}"
        fi
        printf -v "$state" '%s' "$result"
        if [ "${#missing[@]}" -gt 0 ]; then
            echo "::error title=trace overhead::missing $workload $state results in ${missing[*]}"
            complete=0
        fi
    done
    if [ "$complete" -eq 0 ]; then
        status=1
        continue
    fi
    read -r off_pct on_pct < <(awk -v out="$out" -v off="$off" -v pairoff="$pairoff" -v on="$on" \
        'BEGIN { printf "%.3f %.3f\n", (off / out - 1) * 100, (on / pairoff - 1) * 100 }')
    summary="$workload (median of ${#out_logs[@]}+${#in_logs[@]} boots' fastest rounds): out ${out} ns, off ${off} ns (${off_pct}% vs out), pairoff ${pairoff} ns, on ${on} ns (${on_pct}% vs pairoff)"
    echo "$summary"
    echo "::notice title=trace overhead ($workload)::$summary"
    if [ "$workload" = syscall ] && over_budget "$off" "$out" "$off_budget"; then
        echo "::error title=trace overhead::a system call with tracing compiled in but off is ${off_pct}% slower than compiled out (budget ${off_budget}%): $summary"
        status=1
    fi
    if [ "$workload" = io ]; then
        if over_budget "$on" "$pairoff" "$on_budget"; then
            echo "::error title=trace overhead::io with every event on is ${on_pct}% slower than all off in paired rounds (budget ${on_budget}%): $summary"
            status=1
        fi
    fi
done
# A full ring drops records, which is cheaper than writing them, so losses make "on" look
# better than it is.
# Every traced boot prints its count, so a log without one is incomplete, not
# a boot that lost nothing.
lost=0
no_count=()
for log in "${in_logs[@]}"; do
    count="$(tr -d '\r' < "$log" | { grep -ao 'FERRIC_BENCH lost records=[0-9]*' || true; } \
        | sed 's/.*=//' | head -n 1)"
    if [ -n "$count" ]; then
        lost=$((lost + count))
    else
        no_count+=("$log")
    fi
done
if [ "${#no_count[@]}" -gt 0 ]; then
    echo "::error title=trace overhead::no FERRIC_BENCH lost records= line in ${no_count[*]}"
    status=1
elif [ "$lost" -gt 0 ]; then
    echo "::warning title=trace overhead::$lost records were dropped during the on rounds; the on figure is low"
fi
exit "$status"
