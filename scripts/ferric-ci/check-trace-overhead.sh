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
# hit one boot of it) from deciding. Each median comes with distribution-free
# one-sided bounds from the sign test (sign-test.awk): with n pairs, the k-th
# smallest difference is a lower bound on the true median paired difference,
# and the k-th largest an upper one. The sign test assumes only that the pairs
# are independent, not that the differences are normal or symmetric: a stall
# only ever slows one boot down. Only the lower bound decides; the upper one
# shows the spread.
#
# The job judges two comparisons (syscall "off" vs "out" and io "on" vs
# "pairoff") on the same boots, and either can fail it, so the 5% chance of a
# false failure is split between them (Bonferroni, which holds however the two
# depend on each other): each bound alone holds with 97.5% confidence
# (together they form a 95% interval), and the job's false-failure chance
# stays at or under 5%.
#
# When the job may judge the same growing set of pairs several times (image.yml
# adds pairs while a comparison is inconclusive), TRACE_OVERHEAD_LOOKS lists the
# pair counts at which it may run, ascending, such as "10 15 20 25 30". Every
# look then uses the same stricter per-look level, chosen so that each
# comparison's chance of a false failure over all the looks is at most 2.5%;
# without it the looks would add up to about 4.7%. Without TRACE_OVERHEAD_LOOKS
# the current pair count is the only look. A pair count not in the list is a usage error.
#
# A judged comparison has three outcomes:
#   pass          the median is within the budget;
#   fail          the lower bound is over the budget: a regression that the
#                 runner's noise cannot explain;
#   inconclusive  the median is over the budget but the lower bound is not:
#                 the runner's noise (the spread of the pairs, shown as the
#                 bounds) is larger than the budget, so the job cannot tell.
#                 It is reported as a warning; image.yml boots more pairs and
#                 runs the check again, and accepts the result with the warning
#                 when the pairs run out.
# Fewer than 6 pairs give no 97.5% bound, so over the budget they are always
# inconclusive. The budgets:
#   compiled in, all off ("off") at most OFF_BUDGET_PCT (default 1) percent
#   slower than compiled out ("out"), on the "syscall" workload, a null system
#   call;
#   every compiled-in event on ("on") at most ON_BUDGET_PCT (default 5)
#   percent slower than all off in the rounds paired with it ("pairoff"), on
#   the "io" workload, requests to RedoxFS.
# Every result is reported, with each state's median over the boots for
# reference.
#
# Exit status: 0 when every judged comparison passes, 1 when one fails or
# results are missing, 2 for a usage error (including a pair count that is not
# in TRACE_OVERHEAD_LOOKS), and 3 when none fails but one is
# inconclusive.
#
# "off" and "out" run on two kernels built separately, so their
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
here="$(cd "$(dirname "$0")" && pwd)"

looks="${TRACE_OVERHEAD_LOOKS:-$pairs}"
# The comparisons judged, among which the job's 5% false-failure chance is split.
judged=2
alpha="$(awk -v j="$judged" 'BEGIN { print 0.05 / j }')"
conf="$(awk -v a="$alpha" 'BEGIN { print (1 - a) * 100 }')%"
looks_note=""
if [ "$looks" != "$pairs" ]; then
    looks_note=" over looks at $looks pairs"
fi
if ! grep -qxF "$pairs" <(tr -s ' ' '\n' <<< "$looks"); then
    echo "check-trace-overhead: $pairs pairs is not one of TRACE_OVERHEAD_LOOKS ($looks)" >&2
    exit 2
fi

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

# The sign test's lower and upper bounds on the median of the numbers on
# standard input, one per line, for the planned looks, unrounded and separated
# by a space; "none none" when there are too few numbers for a bound at the
# level (see sign-test.awk).
bounds() {
    local b
    b="$(awk -v alpha="$alpha" -v looks="$looks" -f "$here/sign-test.awk")"
    echo "${b#* }"
}

# True when PCT is more than BUDGET percent. It compares unrounded values: the
# percentages printed in the summary are rounded to three places, and an
# increase just over a budget must not pass once rounded. A PCT of "none" (no
# bound) is never over.
over_budget() {
    [ "$1" != none ] && awk -v pct="$1" -v b="$2" 'BEGIN { exit !(pct > b) }'
}

# A bound for the summary: "+1.234%", or "none" when there is no bound.
shown() {
    if [ "$1" = none ]; then echo none; else awk -v v="$1" 'BEGIN { printf "%+.3f%%\n", v }'; fi
}

# Judges the paired differences DIFFS (one per line) against BUDGET percent and
# sets `verdict` to pass, fail or inconclusive, `median_pct` to their unrounded
# median and `low` and `high` to its bounds (see `bounds`).
judge() {
    local diffs="$1" budget="$2" b
    median_pct="$(median %.9f <<< "$diffs")"
    b="$(bounds <<< "$diffs")"
    low="${b% *}"
    high="${b#* }"
    if ! over_budget "$median_pct" "$budget"; then
        verdict=pass
    elif over_budget "$low" "$budget"; then
        verdict=fail
    else
        verdict=inconclusive
    fi
}

# The sentence that reports an inconclusive comparison NAME (median MEDIAN,
# bounds LOW and HIGH, BUDGET percent).
inconclusive_text() {
    echo "$1: the median of $pairs pairs, $(shown "$2"), is over the $5% budget, but runner noise exceeds the budget: its one-sided $conf lower bound is $(shown "$3") (upper $(shown "$4")$looks_note), so it is not over the budget and the overhead cannot be judged on this runner (not failed; more pairs narrow the bounds)"
}

# The paired differences, rounded for the summary: "+0.123 -0.456 ...".
listed() {
    awk '{ printf "%s%+.2f", (NR > 1 ? " " : ""), $1 } END { print "" }' <<< "$1"
}

status=0
inconclusive=0
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
    off_bounds="$(bounds <<< "$off_pairs")"
    on_bounds="$(bounds <<< "$on_pairs")"
    summary="$workload ($pairs pairs of boots' fastest rounds): off ${off_pct}% vs out (median of pairs, one-sided $conf bounds $(shown "${off_bounds% *}") and $(shown "${off_bounds#* }")$looks_note; each pair $(listed "$off_pairs")), on ${on_pct}% vs pairoff (median of pairs, one-sided $conf bounds $(shown "${on_bounds% *}") and $(shown "${on_bounds#* }")$looks_note; each boot $(listed "$on_pairs")); medians over boots: out $(median <<< "$out") ns, off $(median <<< "$off") ns, pairoff $(median <<< "$pairoff") ns, on $(median <<< "$on") ns"
    echo "$summary"
    echo "::notice title=trace overhead ($workload)::$summary"
    case "$workload" in
        syscall)
            judge "$off_pairs" "$off_budget"
            name="syscall off vs out"
            failure="a system call with tracing compiled in but off is ${off_pct}% slower than compiled out, as the median of paired boots (budget ${off_budget}%)"
            budget="$off_budget"
            ;;
        io)
            judge "$on_pairs" "$on_budget"
            name="io on vs pairoff"
            failure="io with every event on is ${on_pct}% slower than all off in paired rounds (budget ${on_budget}%)"
            budget="$on_budget"
            ;;
    esac
    case "$verdict" in
        fail)
            echo "::error title=trace overhead::$failure, and its one-sided $conf lower bound, $(shown "$low")$looks_note, is over the budget too: $summary"
            status=1
            ;;
        inconclusive)
            echo "::warning title=trace overhead inconclusive::$(inconclusive_text "$name" "$median_pct" "$low" "$high" "$budget"): $summary"
            inconclusive=1
            ;;
    esac
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
if [ "$status" -eq 0 ] && [ "$inconclusive" -eq 1 ]; then
    status=3
fi
exit "$status"
