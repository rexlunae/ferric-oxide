#!/usr/bin/env bash
# Tracing overhead budget (roadmap Phase T): compares the FERRIC_BENCH lines
# that ferric-trace-bench (recipes/tests/ferric-trace-bench) prints in the
# serial logs of boots of kernels built without and with kernel.trace.
#
# Usage: check-trace-overhead.sh --out LOG... --in LOG...
#   --out  logs of boots of a kernel built without kernel.trace (state "out")
#   --in   logs of boots of a kernel with kernel.trace (states "off" and "on")
#
# For each workload and state the fastest round over all given boots counts.
# The budgets apply to the "io" workload, requests to RedoxFS:
#   compiled in, all off ("off") at most OFF_BUDGET_PCT (default 1) percent
#   slower than compiled out ("out");
#   every compiled-in event on ("on") at most ON_BUDGET_PCT (default 5)
#   percent slower than all off.
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

# The smallest min_ns of WORKLOAD in STATE across LOGS, or nothing.
best() {
    local workload="$1" state="$2"
    shift 2
    tr -d '\r' < <(cat "$@") \
        | grep -ao "FERRIC_BENCH $workload $state min_ns=[0-9.]*" \
        | sed 's/.*min_ns=//' | sort -g | head -n 1
}

status=0
for workload in io syscall; do
    out="$(best "$workload" out "${out_logs[@]}")"
    off="$(best "$workload" off "${in_logs[@]}")"
    on="$(best "$workload" on "${in_logs[@]}")"
    if [ -z "$out" ] || [ -z "$off" ] || [ -z "$on" ]; then
        echo "check-trace-overhead: missing $workload results (out='$out' off='$off' on='$on')" >&2
        status=1
        continue
    fi
    read -r off_pct on_pct < <(awk -v out="$out" -v off="$off" -v on="$on" \
        'BEGIN { printf "%.2f %.2f\n", (off / out - 1) * 100, (on / off - 1) * 100 }')
    summary="$workload: out ${out} ns, off ${off} ns (${off_pct}% vs out), on ${on} ns (${on_pct}% vs off)"
    echo "$summary"
    echo "::notice title=trace overhead ($workload)::$summary"
    if [ "$workload" = io ]; then
        if awk -v p="$off_pct" -v b="$off_budget" 'BEGIN { exit !(p > b) }'; then
            echo "::error title=trace overhead::io with tracing compiled in but off is ${off_pct}% slower than compiled out (budget ${off_budget}%)"
            status=1
        fi
        if awk -v p="$on_pct" -v b="$on_budget" 'BEGIN { exit !(p > b) }'; then
            echo "::error title=trace overhead::io with every event on is ${on_pct}% slower than all off (budget ${on_budget}%)"
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
