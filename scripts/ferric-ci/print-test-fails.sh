#!/usr/bin/env bash
# Usage: print-test-fails.sh LOG TITLE
#
# Prints every FERRIC_TEST_FAIL line of the console log LOG with the 40 lines
# before it, in a GitHub Actions log group named TITLE, so a test that failed
# early in a boot can be read from the job log when a step keeps only the
# boot's last lines. Workflow commands are stopped while the log is printed, so
# no console line is taken as one. Prints nothing when no test failed.
set -euo pipefail
log="$1"
title="$2"
grep -qaF "FERRIC_TEST_FAIL" "$log" || exit 0
echo "::group::$title: each FERRIC_TEST_FAIL with the 40 lines before it"
stop="console-$$-$RANDOM"
echo "::stop-commands::$stop"
grep -aF -B 40 "FERRIC_TEST_FAIL" "$log" || true
echo "::$stop::"
echo "::endgroup::"
