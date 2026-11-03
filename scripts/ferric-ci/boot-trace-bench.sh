#!/usr/bin/env bash
# Boots the trace-overhead job's two smoke images (image.yml), once each per run, for the
# runs numbered FIRST to LAST: the images without (notrace/harddrive.img) and with
# (trace/harddrive.img) kernel.trace, as back-to-back pairs whose two boots see about the
# same runner load. check-trace-overhead.sh judges the pairs.
#
# Usage: boot-trace-bench.sh FIRST LAST
#
# Each boot's serial log is bench-<image>-<run>.log, with the run number in two digits so
# the two images' log globs list the pairs in the same order (at most 99 runs). Its bench
# and trace result lines are appended to results-<image>.txt, which the job posts as
# notices. Log downloads are not always readable, so every failure is an annotation. Exits
# 1 if any boot failed, or the kernel without kernel.trace did not make the trace tests
# skip.
set -euo pipefail

first="$1"
last="$2"
if [ "$last" -gt 99 ]; then
    echo "boot-trace-bench: at most 99 runs, so the log names keep two digits" >&2
    exit 2
fi
dir="$(cd "$(dirname "$0")" && pwd)"

failed=0
for run in $(seq "$first" "$last"); do
    # Alternating which image goes first keeps a steady drift in the runner's load from
    # always landing on the same image.
    if [ $((run % 2)) -eq 1 ]; then order="notrace trace"; else order="trace notrace"; fi
    for image in $order; do
        log="bench-$image-$(printf '%02d' "$run").log"
        status=0
        "$dir/boot-smoke.sh" "$image/harddrive.img" "$log" host > /dev/null || status=$?
        tr -d '\r' < "$log" | sed 's/\x1b\[[0-9;]*[A-Za-z]//g' > "$log.clean" || true
        results="$(grep -aoE 'FERRIC_BENCH (io|syscall|lost) .*|FERRIC_TEST_[A-Z]+ trace(-bench|-capture|:).*' "$log.clean" || true)"
        printf 'boot %s exit %s\n%s\n' "$run" "$status" "$results" >> "results-$image.txt"
        # Test results and crash lines first: an unhandled exception's process name is
        # printed above its register dump, out of reach of a short tail.
        tail="$({ grep -aE 'FERRIC_TEST_|EXCEPTION|panic|PANIC' "$log.clean" | grep -av 'funmap passed' | tail -n 40
            grep -av 'funmap passed' "$log.clean" | tail -n 30; } | sed 's/%/%25/g' | sed ':a;N;$!ba;s/\n/%0A/g')"
        if [ "$status" -ne 0 ] || ! grep -qaF "FERRIC_TEST_PASS trace-bench" "$log.clean"; then
            failed=1
            echo "::error title=$image boot $run::boot exit $status, or no FERRIC_TEST_PASS trace-bench. Results and crash lines, then the serial tail:%0A$tail"
            # The boot's own output went to /dev/null above, and the annotation keeps only
            # the last markers: print every failed test with the 40 lines before it, so a
            # failure early in the boot can be read from the job log.
            "$dir/print-test-fails.sh" "$log.clean" "$image boot $run"
        fi
        # The kernel without kernel.trace must make ferric-trace-test skip, not fail.
        if [ "$image" = notrace ] && ! grep -qaF "FERRIC_TEST_SKIP trace: no" "$log.clean"; then
            failed=1
            echo "::error title=$image boot $run::ferric-trace-test did not skip on the kernel without kernel.trace"
        fi
        if [ "$image" = notrace ] && ! grep -qaF "FERRIC_TEST_SKIP trace-capture: no" "$log.clean"; then
            failed=1
            echo "::error title=$image boot $run::the trace capture check did not skip on the kernel without kernel.trace"
        fi
    done
done
exit "$failed"
