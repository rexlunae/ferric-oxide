#!/usr/bin/env bash
# Tracing overhead budget (roadmap Phase T): compares the FERRIC_BENCH lines
# that ferric-trace-bench (recipes/tests/ferric-trace-bench) prints in the
# serial logs of boots of kernels built without and with kernel.trace.
#
# Usage: check-trace-overhead.sh --out LOG... --in LOG...
#   --out  logs of boots of a kernel built without kernel.trace (state "out")
#   --in   logs of boots of a kernel with kernel.trace (states "off", "pairoff" and "on")
# Give as many --in logs as --out logs: the Nth of each is a pair, two boots
# run back to back on the same runner (image.yml's trace-overhead job boots
# the two images in turn).
#
# Each boot's fastest round of a workload and state is its result. Every
# comparison is paired, and the median of the pairs' differences counts:
#   "off" vs "out" pairs the two boots of a run, because the two kernels can
#   only run in separate boots. A runner's load changes over the job (the
#   neighbours on a nested-KVM host come and go), and two adjacent boots see
#   about the same load, so each pair's difference cancels it where the
#   difference of two medians over all boots does not.
#   "on" vs "pairoff" pairs the two states of one boot, whose rounds alternate.
# The median of the paired differences keeps one unlucky pair (a stall that
# hit one boot of it) from deciding. The budgets:
#   compiled in, all off ("off") at most OFF_BUDGET_PCT (default 1) percent
#   slower than compiled out ("out"), on the "syscall" workload, a null system
#   call;
#   every compiled-in event on ("on") at most ON_BUDGET_PCT (default 5)
#   percent slower than all off in the rounds paired with it ("pairoff"), on
#   the "io" workload, requests to RedoxFS.
# Every result is reported, with each state's median over the boots for
# reference. "off" and "out" run on two kernels built separately, so their
# comparison also measures where the linker put the hot code. On the null
# system call that is small and repeatable (+0.53% and +0.54% in two runs),
# and the tracepoints are the largest share of the work, so a disabled
# tracepoint that costs anything shows there first. On "io", which runs
# through far more kernel code, the same comparison went from -4.25% to +1.12%
# between those two runs with no tracepoint changed, and to -10.9% in a
# third. It is reported, not judged, because judging it would judge the
# layout.
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
if [ "${#out_logs[@]}" -ne "${#in_logs[@]}" ]; then
    echo "check-trace-overhead: ${#out_logs[@]} --out logs but ${#in_logs[@]} --in logs; the comparison pairs them" >&2
    exit 2
fi
pairs="${#out_logs[@]}"

# Sets `result` to each log's first min_ns for WORKLOAD in STATE, one per line
# in LOGS order, and `missing` to the logs that have no such line. `result` is
# empty when any log is missing one, so a workload is never judged on fewer
# boots than the job ran. It runs in the current shell, not in a command
# substitution, so a log without a match is reported instead of ending the
# script under `set -e` and `pipefail`.
results() {
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
        result="$(printf '%s\n' "${values[@]}")"
    fi
}

# The median of the numbers on standard input, one per line, printed with
# FORMAT (default three decimal places).
median() {
    sort -g | awk -v fmt="${1:-%.3f}\n" '{ v[NR] = $1 } END {
        if (NR % 2) printf fmt, v[(NR + 1) / 2]; else printf fmt, (v[NR / 2] + v[NR / 2 + 1]) / 2
    }'
}

# Each pair's percent difference of the MEASURED list from the BASE list (one
# value per line, in pair order), unrounded, one per line.
paired_pct() {
    paste -d ' ' <(printf '%s\n' "$1") <(printf '%s\n' "$2") \
        | awk '{ printf "%.9f\n", ($1 / $2 - 1) * 100 }'
}

# True when the median paired difference PCT is more than BUDGET percent. It
# compares the unrounded value: the percentages printed in the summary are
# rounded to three places, and an increase just over a budget must not pass
# once rounded.
over_budget() {
    awk -v pct="$1" -v b="$2" 'BEGIN { exit !(pct > b) }'
}

# The paired differences, rounded for the summary: "+0.123 -0.456 ...".
listed() {
    awk '{ printf "%s%+.2f", (NR > 1 ? " " : ""), $1 } END { print "" }' <<< "$1"
}

status=0
for workload in io syscall; do
    complete=1
    for state in out off pairoff on; do
        if [ "$state" = out ]; then
            results "$workload" "$state" "${out_logs[@]}"
        else
            results "$workload" "$state" "${in_logs[@]}"
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
    off_pairs="$(paired_pct "$off" "$out")"
    on_pairs="$(paired_pct "$on" "$pairoff")"
    off_pct="$(median <<< "$off_pairs")"
    on_pct="$(median <<< "$on_pairs")"
    summary="$workload ($pairs pairs of boots' fastest rounds): off ${off_pct}% vs out (median of pairs; each pair $(listed "$off_pairs")), on ${on_pct}% vs pairoff (median of pairs; each boot $(listed "$on_pairs")); medians over boots: out $(median <<< "$out") ns, off $(median <<< "$off") ns, pairoff $(median <<< "$pairoff") ns, on $(median <<< "$on") ns"
    echo "$summary"
    echo "::notice title=trace overhead ($workload)::$summary"
    if [ "$workload" = syscall ] && over_budget "$(median %.9f <<< "$off_pairs")" "$off_budget"; then
        echo "::error title=trace overhead::a system call with tracing compiled in but off is ${off_pct}% slower than compiled out, as the median of paired boots (budget ${off_budget}%): $summary"
        status=1
    fi
    if [ "$workload" = io ] && over_budget "$(median %.9f <<< "$on_pairs")" "$on_budget"; then
        echo "::error title=trace overhead::io with every event on is ${on_pct}% slower than all off in paired rounds (budget ${on_budget}%): $summary"
        status=1
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
