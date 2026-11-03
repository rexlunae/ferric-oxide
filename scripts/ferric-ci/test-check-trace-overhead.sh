#!/usr/bin/env bash
# Checks check-trace-overhead.sh against fixture serial logs built from the
# per-boot results of a recorded trace-overhead job: PR #233's job 121869115663
# (issue #280), ten boots of each image on a loaded nested-KVM runner, as its
# "notrace boots" and "trace boots" notices printed them.
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/check-trace-overhead.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fail() {
    echo "FAIL: $1: $2" >&2
    cat "$work/output" >&2
    exit 1
}

# image boot workload state min_ns, as recorded; the trace boots' "lost records" counts follow.
recorded() {
    cat <<'TABLE'
notrace 1 io out 22878.1
notrace 2 io out 23513.5
notrace 3 io out 22600.1
notrace 4 io out 24928.0
notrace 5 io out 23682.5
notrace 6 io out 23626.6
notrace 7 io out 22475.9
notrace 8 io out 22413.8
notrace 9 io out 23342.0
notrace 10 io out 24188.9
notrace 1 syscall out 144.3
notrace 2 syscall out 170.5
notrace 3 syscall out 147.9
notrace 4 syscall out 155.3
notrace 5 syscall out 151.2
notrace 6 syscall out 165.2
notrace 7 syscall out 141.4
notrace 8 syscall out 154.2
notrace 9 syscall out 146.4
notrace 10 syscall out 151.6
trace 1 io off 21467.4
trace 2 io off 20725.1
trace 3 io off 21055.2
trace 4 io off 21260.1
trace 5 io off 20940.6
trace 6 io off 20807.6
trace 7 io off 20004.0
trace 8 io off 20128.8
trace 9 io off 20790.2
trace 10 io off 21568.2
trace 1 io pairoff 21374.1
trace 2 io pairoff 20800.2
trace 3 io pairoff 20484.2
trace 4 io pairoff 21170.8
trace 5 io pairoff 21138.9
trace 6 io pairoff 20926.0
trace 7 io pairoff 19994.2
trace 8 io pairoff 19547.6
trace 9 io pairoff 20875.5
trace 10 io pairoff 21495.1
trace 1 io on 22109.6
trace 2 io on 21472.6
trace 3 io on 20992.9
trace 4 io on 21620.1
trace 5 io on 21722.2
trace 6 io on 21711.4
trace 7 io on 20509.6
trace 8 io on 20142.8
trace 9 io on 21422.5
trace 10 io on 21989.8
trace 1 syscall off 177.2
trace 2 syscall off 165.6
trace 3 syscall off 142.4
trace 4 syscall off 153.2
trace 5 syscall off 150.8
trace 6 syscall off 201.2
trace 7 syscall off 149.6
trace 8 syscall off 160.0
trace 9 syscall off 170.0
trace 10 syscall off 172.3
trace 1 syscall pairoff 150.2
trace 2 syscall pairoff 149.2
trace 3 syscall pairoff 143.4
trace 4 syscall pairoff 149.4
trace 5 syscall pairoff 149.5
trace 6 syscall pairoff 154.6
trace 7 syscall pairoff 144.3
trace 8 syscall pairoff 140.4
trace 9 syscall pairoff 149.1
trace 10 syscall pairoff 152.7
trace 1 syscall on 199.0
trace 2 syscall on 206.5
trace 3 syscall on 173.2
trace 4 syscall on 178.1
trace 5 syscall on 181.3
trace 6 syscall on 201.9
trace 7 syscall on 178.0
trace 8 syscall on 172.1
trace 9 syscall on 182.9
trace 10 syscall on 181.9
trace 1 lost - 35809
trace 2 lost - 35416
trace 3 lost - 35567
trace 4 lost - 35424
trace 5 lost - 35550
trace 6 lost - 35634
trace 7 lost - 35627
trace 8 lost - 35485
trace 9 lost - 35671
trace 10 lost - 35985
TABLE
}

# Writes the table on standard input as serial logs in directory NAME, one per image and boot,
# with the console's CRs, named as image.yml names them (the boot number in two digits, so the
# two globs list the pairs in the same order).
logs() {
    local dir="$work/$1"
    rm -rf "$dir"
    mkdir -p "$dir"
    awk -v dir="$dir" '{
        file = dir "/bench-" $1 "-" sprintf("%02d", $2) ".log"
        if ($3 == "lost") printf "FERRIC_BENCH lost records=%s\r\n", $5 > file
        else printf "FERRIC_BENCH %s %s min_ns=%s median_ns=%s rounds=200\r\n", $3, $4, $5, $5 > file
    }'
}

# Runs the check on the logs in directory NAME, as image.yml does, and expects STATUS.
expect() {
    local name="$1" status="$2" got=0
    (cd "$work/$name" && bash "$script" --out bench-notrace-*.log --in bench-trace-*.log) \
        > "$work/output" 2>&1 || got=$?
    [ "$got" = "$status" ] || fail "$name" "exit $got, expected $status"
}

# Sets the syscall "off" result of each trace boot to its pair's recorded "out" times FACTOR,
# a compiled-in-but-off kernel exactly FACTOR slower than compiled out on the same runner.
off_from_out() {
    awk -v f="$1" '
        $1 == "notrace" && $3 == "syscall" { out[$2] = $5 }
        { row[NR] = $0 }
        END {
            for (i = 1; i <= NR; i++) {
                split(row[i], c, " ")
                if (c[1] == "trace" && c[3] == "syscall" && c[4] == "off")
                    printf "trace %s syscall off %.4f\n", c[2], out[c[2]] * f
                else print row[i]
            }
        }'
}

# 1. The recorded job as it ran. Its on rounds are paired within each boot: a median of
# +2.69% (each boot between +2.12% and +3.75%) is within the 5% budget. Its syscall "off"
# rounds ran right after the io workload's toggling rounds, which "out" never follows
# (fixed in ferric-trace-bench), so they were slow in most boots, and the paired median
# (+4.78%) is over the budget. But four of the ten pairs are faster with tracing compiled
# in, so the sign test's 95% lower bound (-2.87%) is not over the budget: on this runner the
# job cannot tell, and says so instead of passing or failing (exit 3).
recorded | logs recorded
expect recorded 3
grep -qF "on 2.690% vs pairoff (median of pairs, one-sided 95% bounds +2.301% and +3.441%;" "$work/output" ||
    fail recorded "io on paired median and bounds"
grep -qF "off 4.780% vs out (median of pairs, one-sided 95% bounds -2.874% and +21.792%;" "$work/output" ||
    fail recorded "syscall off paired median and bounds"
grep -qF "::warning title=trace overhead inconclusive::syscall off vs out: the median of 10 pairs, +4.780%, is over the 1% budget, but runner noise exceeds the budget: its one-sided 95% lower bound is -2.874% (upper +21.792%)" \
    "$work/output" || fail recorded "inconclusive warning"
! grep -q "::error" "$work/output" || fail recorded "an inconclusive result failed"
grep -qF "::warning title=trace overhead::356168 records were dropped" "$work/output" ||
    fail recorded "lost records"
echo "PASS: recorded job (inconclusive)"

# 2. The recorded runner's boot-to-boot noise (the notrace boots' syscall times vary from
# 141.4 to 170.5 ns, 20%) with a compiled-in-but-off kernel 0.5% slower: within budget.
recorded | off_from_out 1.005 | logs within
expect within 0
grep -qF "off 0.500% vs out" "$work/output" || fail within "paired median"
echo "PASS: recorded noise, 0.5% overhead"

# 3. As 2, with the recorded worst stall (boot 6's 201.2 ns) on two pairs' trace boots, and
# the runner 20% slower for the last half of the job on both images: the median of pairs
# neither sees the stalls nor the change of load. (The difference of the two images' medians
# over all boots, the gate before issue #280, failed this at +5.65%.)
recorded | off_from_out 1.005 | awk '
    $1 == "trace" && $3 == "syscall" && $4 == "off" && ($2 == 3 || $2 == 7) { $5 = 201.2 }
    $3 == "syscall" && $2 > 5 { $5 = $5 * 1.2 }
    { print }' | logs stalls
expect stalls 0
grep -qF "off 0.500% vs out" "$work/output" || fail stalls "paired median"
echo "PASS: recorded noise with stalls and a load change"

# 4. A real regression on the same noise fails, with the median and every pair.
recorded | off_from_out 1.03 | logs regressed
expect regressed 1
grep -qF "compiled in but off is 3.000% slower than compiled out, as the median of paired boots (budget 1%), and its one-sided 95% lower bound, +3.000%, is over the budget too" \
    "$work/output" || fail regressed "error message"
grep -qF "each pair +3.00 +3.00" "$work/output" || fail regressed "per-pair list"
echo "PASS: 3% regression"

# 5. Just over the budget fails although it rounds to the budget's printed precision; just
# under passes.
recorded | off_from_out 1.010004 | logs over
expect over 1
recorded | off_from_out 1.0099 | logs under
expect under 0
echo "PASS: budget edge"

# 6. Every event on 6% slower than its paired off rounds fails the io budget.
recorded | awk '$1 == "trace" && $3 == "io" && $4 == "on" { $5 = sprintf("%.4f", p[$2] * 1.06) }
    $1 == "trace" && $3 == "io" && $4 == "pairoff" { p[$2] = $5 }
    { print }' | off_from_out 1.0 | logs on-regressed
expect on-regressed 1
grep -qF "io with every event on is 6.000% slower than all off in paired rounds (budget 5%)" "$work/output" ||
    fail on-regressed "error message"
echo "PASS: on regression"

# 7. Unpaired or incomplete input is refused.
recorded | awk '!($1 == "trace" && $2 == 10)' | logs unpaired
expect unpaired 2
grep -qF "10 --out logs but 9 --in logs" "$work/output" || fail unpaired "message"
recorded | off_from_out 1.0 | awk '!($1 == "trace" && $2 == 4 && $3 == "syscall" && $4 == "pairoff")' |
    logs missing
expect missing 1
grep -qF "missing syscall pairoff results in bench-trace-04.log" "$work/output" || fail missing "message"
recorded | off_from_out 1.0 | awk '!($1 == "trace" && $2 == 2 && $3 == "lost")' | logs no-lost
expect no-lost 1
grep -qF "no FERRIC_BENCH lost records= line in bench-trace-02.log" "$work/output" || fail no-lost "message"
echo "PASS: unpaired and incomplete input"

# Writes a table of N pairs whose syscall "off" rounds are OFF percent slower than their
# pair's "out" round and whose io "on" rounds are ON percent slower than their boot's
# "pairoff" round, one pair per line of standard input: "OFF ON". The other states are
# constant.
from_pairs() {
    awk '{
        n = NR
        printf "notrace %d io out 23000\nnotrace %d syscall out 150\n", n, n
        printf "trace %d io off 21000\ntrace %d io pairoff 21000\n", n, n
        printf "trace %d io on %.4f\n", n, 21000 * (1 + $2 / 100)
        printf "trace %d syscall off %.6f\n", n, 150 * (1 + $1 / 100)
        printf "trace %d syscall pairoff 150\ntrace %d syscall on 180\ntrace %d lost - 0\n", n, n, n
    }'
}

# 8. Issue #340: PR #374 (relibc only) in run 41384451285, syscall "off vs out" +2.391% with
# these paired differences. The runner's spread (-6.43% to +7.77%) is far larger than the
# 1% budget: inconclusive, not a failure. (The pairs are rounded to two places, so their
# median is +2.390%.)
pr374="-4.35 5.33 6.56 7.08 0.49 -2.24 -6.43 7.77 -2.77 4.29"
printf '%s 2\n' $pr374 | from_pairs | logs pr374
expect pr374 3
grep -qF "off 2.390% vs out (median of pairs, one-sided 95% bounds -4.350% and +7.080%;" "$work/output" ||
    fail pr374 "median and bounds"
grep -qF "::warning title=trace overhead inconclusive::syscall off vs out: the median of 10 pairs, +2.390%, is over the 1% budget" \
    "$work/output" || fail pr374 "inconclusive warning"
! grep -q "::error" "$work/output" || fail pr374 "an inconclusive result failed"
echo "PASS: PR #374's noise is inconclusive"

# 9. The same noise with a real 6% regression on top: every pair but the slowest outlier is
# over the budget and the 95% lower bound is +1.65%, so it fails.
for d in $pr374; do awk -v d="$d" 'BEGIN { printf "%.2f 2\n", d + 6 }'; done | from_pairs |
    logs pr374-regressed
expect pr374-regressed 1
grep -qF "compiled in but off is 8.390% slower than compiled out, as the median of paired boots (budget 1%), and its one-sided 95% lower bound, +1.650%, is over the budget too" \
    "$work/output" || fail pr374-regressed "error message"
echo "PASS: a 6% regression on PR #374's noise fails"

# 10. Issue #340: PR #381 in run 41513059373, syscall "off vs out" +1.688% with pairs from
# -23.97% to +7.36% (the comment gives the median and range; the other pairs are made up
# to match), and io "on vs pairoff" a stable +3.59% with pairs from +3.07% to +4.43%: the
# syscall comparison is inconclusive, and io on passes.
paste -d ' ' <(printf '%s\n' -23.97 -3.10 -1.20 0.40 1.50 1.876 2.90 4.10 5.50 7.36) \
    <(printf '%s\n' 3.07 3.30 3.45 3.52 3.59 3.59 3.70 3.90 4.10 4.43) | from_pairs | logs pr381
expect pr381 3
grep -qF "off 1.688% vs out (median of pairs, one-sided 95% bounds -3.100% and +5.500%;" "$work/output" ||
    fail pr381 "syscall median and bounds"
grep -qF "on 3.590% vs pairoff (median of pairs, one-sided 95% bounds +3.300% and +4.100%;" "$work/output" ||
    fail pr381 "io on median and bounds"
! grep -q "io on vs pairoff" "$work/output" || fail pr381 "io on was not a pass"
echo "PASS: PR #381's noise is inconclusive, its io on passes"

# 11. More pairs narrow the bounds: twenty pairs (k = 6) of PR #374's noise, with a 2%
# regression, fail where ten pairs of the same regression are inconclusive.
for d in $pr374; do awk -v d="$d" 'BEGIN { printf "%.2f 2\n", d + 2 }'; done > "$work/pr374-2"
from_pairs < "$work/pr374-2" | logs ten
expect ten 3
printf '%s 2\n' 1.2 1.4 1.5 1.6 1.8 2.0 2.2 2.4 2.6 3.0 | cat "$work/pr374-2" - | from_pairs | logs twenty
expect twenty 1
grep -qF "one-sided 95% lower bound, +1.400%, is over the budget too" "$work/output" || fail twenty "k = 6 bound"
echo "PASS: more pairs narrow the bounds"

# 12. Fewer than five pairs give no 95% bound, so over the budget they are inconclusive
# however large the difference; five identical pairs (k = 1) are enough to fail.
printf '3 2\n3 2\n3 2\n3 2\n' | from_pairs | logs four
expect four 3
grep -qF "its one-sided 95% lower bound is none (upper none)" "$work/output" || fail four "no bound"
printf '3 2\n3 2\n3 2\n3 2\n3 2\n' | from_pairs | logs five
expect five 1
echo "PASS: too few pairs for a bound"

# 13. Repeated looks (issue #340 review): image.yml judges the same growing set of pairs at
# 10, 15, 20, 25 and 30 pairs. With each look at 95% the chance that some look fails a
# change exactly on the budget is about 6.6%; with TRACE_OVERHEAD_LOOKS every look uses the
# stricter level that keeps the overall chance at or under 5% (k = 10 at 30 pairs, not 11).
signtest="$(cd "$(dirname "$0")" && pwd)/sign-test.awk"
looks="10 15 20 25 30"
[ "$(awk -v mode=k -v looks="$looks" -f "$signtest" /dev/null | tr '\n' ' ')" = "10 2 15 4 20 6 25 8 30 10 " ] ||
    fail looks "per-look k"
single=""
for n in $looks; do single="$single $(awk -v mode=k -v looks="$n" -f "$signtest" /dev/null | cut -d ' ' -f 2)"; done
[ "$single" = " 2 4 6 8 11" ] || fail looks "single-look k: $single"
read -r level adjusted unadjusted < <(awk -v mode=rate -v looks="$looks" -f "$signtest" /dev/null)
awk -v a="$adjusted" -v u="$unadjusted" -v l="$level" 'BEGIN { exit !(a <= 0.05 && u > 0.05 && l < 0.05) }' ||
    fail looks "exact rates: level $level, adjusted $adjusted, unadjusted $unadjusted"
echo "PASS: per-look level for repeated looks (overall $adjusted, unadjusted $unadjusted)"

# 14. The job's stopping rule on null-distributed sign sequences, simulated independently of
# sign-test.awk's dynamic programming: each pair is at or below the true median (the
# budget) with probability 1/2; a look fails when fewer than its k pairs are, and stops
# (passes) when the median is within budget, taken conservatively as more than half the
# pairs at or below. The false-failure rate must stay at or under 5% (three standard errors
# of slack), and the unadjusted per-look k must exceed it.
simulate() {
    awk -v ks="$1" -v looks="$looks" 'BEGIN {
        srand(340); trials = 200000
        m = split(looks, n, " "); split(ks, k, " ")
        for (t = 0; t < trials; t++) {
            below = 0; done = 0
            for (j = 1; j <= m && !done; j++) {
                for (i = (j > 1 ? n[j - 1] : 0); i < n[j]; i++) if (rand() < 0.5) below++
                if (below < k[j]) { fails++; done = 1 }
                else if (2 * below > n[j]) done = 1
            }
        }
        printf "%.5f\n", fails / trials
    }'
}
adjusted_mc="$(simulate "2 4 6 8 10")"
unadjusted_mc="$(simulate "2 4 6 8 11")"
awk -v a="$adjusted_mc" -v u="$unadjusted_mc" 'BEGIN { exit !(a <= 0.05 + 3 * 0.00049 && u > 0.05 + 3 * 0.00049) }' ||
    fail stopping "simulated false failures: adjusted $adjusted_mc, unadjusted $unadjusted_mc"
echo "PASS: stopping rule on null sign sequences (adjusted $adjusted_mc, unadjusted $unadjusted_mc)"

# 15. The checker uses the planned looks: thirty pairs whose 10th smallest difference is
# within the 1% budget and 11th is over fail as a single look but are inconclusive as the
# last of the job's looks. A pair count that is not a planned look is a usage error.
{ for i in $(seq 10); do echo "0.5 2"; done; for i in $(seq 20); do echo "3 2"; done; } | from_pairs | logs thirty
expect thirty 1
TRACE_OVERHEAD_LOOKS="$looks" expect thirty 3
grep -qF "its one-sided 95% lower bound is +0.500% (upper +3.000% over looks at $looks pairs)" "$work/output" ||
    fail thirty "looks note"
printf '%s 2\n' $pr374 | from_pairs | logs pr374
TRACE_OVERHEAD_LOOKS="15 20" expect pr374 2
grep -qF "10 pairs is not one of TRACE_OVERHEAD_LOOKS (15 20)" "$work/output" || fail pr374 "looks usage"
echo "PASS: planned looks"
