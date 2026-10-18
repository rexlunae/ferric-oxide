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
# boot from deciding. The budgets apply to the "io" workload, requests to
# RedoxFS:
#   compiled in, all off ("off") at most OFF_BUDGET_PCT (default 1) percent
#   slower than compiled out ("out");
#   every compiled-in event on ("on") at most ON_BUDGET_PCT (default 5)
#   percent slower than all off in the rounds paired with it ("pairoff").
# The "syscall" workload, a null system call, is reported but not budgeted:
# with nothing else to do, its tracepoints are a large share of its time.
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

# The median over LOGS of each log's min_ns for WORKLOAD in STATE, or nothing.
best() {
    local workload="$1" state="$2" log
    shift 2
    for log in "$@"; do
        tr -d '\r' < "$log" \
            | grep -ao "FERRIC_BENCH $workload $state min_ns=[0-9.]*" \
            | sed 's/.*min_ns=//' | head -n 1
    done | sort -g | awk '{ v[NR] = $1 } END {
        if (NR == 0) exit
        if (NR % 2) print v[(NR + 1) / 2]; else printf "%.1f\n", (v[NR / 2] + v[NR / 2 + 1]) / 2
    }'
}

status=0
for workload in io syscall; do
    out="$(best "$workload" out "${out_logs[@]}")"
    off="$(best "$workload" off "${in_logs[@]}")"
    pairoff="$(best "$workload" pairoff "${in_logs[@]}")"
    on="$(best "$workload" on "${in_logs[@]}")"
    if [ -z "$out" ] || [ -z "$off" ] || [ -z "$pairoff" ] || [ -z "$on" ]; then
        echo "::error title=trace overhead::missing $workload results (out='$out' off='$off' pairoff='$pairoff' on='$on')"
        status=1
        continue
    fi
    read -r off_pct on_pct < <(awk -v out="$out" -v off="$off" -v pairoff="$pairoff" -v on="$on" \
        'BEGIN { printf "%.2f %.2f\n", (off / out - 1) * 100, (on / pairoff - 1) * 100 }')
    summary="$workload (median of ${#out_logs[@]}+${#in_logs[@]} boots' fastest rounds): out ${out} ns, off ${off} ns (${off_pct}% vs out), pairoff ${pairoff} ns, on ${on} ns (${on_pct}% vs pairoff)"
    echo "$summary"
    echo "::notice title=trace overhead ($workload)::$summary"
    if [ "$workload" = io ]; then
        if awk -v p="$off_pct" -v b="$off_budget" 'BEGIN { exit !(p > b) }'; then
            echo "::error title=trace overhead::io with tracing compiled in but off is ${off_pct}% slower than compiled out (budget ${off_budget}%): $summary"
            status=1
        fi
        if awk -v p="$on_pct" -v b="$on_budget" 'BEGIN { exit !(p > b) }'; then
            echo "::error title=trace overhead::io with every event on is ${on_pct}% slower than all off in paired rounds (budget ${on_budget}%): $summary"
            status=1
        fi
    fi
done
# A full ring drops records, which is cheaper than writing them, so losses make "on" look
# better than it is.
lost="$(tr -d '\r' < <(cat "${in_logs[@]}") | grep -ao 'FERRIC_BENCH lost records=[0-9]*' \
    | sed 's/.*=//' | awk '{ s += $1 } END { print s + 0 }')"
if [ "$lost" -gt 0 ]; then
    echo "::warning title=trace overhead::$lost records were dropped during the on rounds; the on figure is low"
fi
exit "$status"
