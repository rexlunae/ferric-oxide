#!/usr/bin/env bash
# Exercise the real runner with a fake QEMU, without firmware or virtualization.
set -euo pipefail
runner="$(cd "$(dirname "$0")" && pwd)/boot-smoke.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
touch "$work/image" "$work/firmware"
cat > "$work/bin/qemu-system-x86_64" <<'QEMU'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_QEMU_ARGS"
# FAKE_QEMU_SPLIT prints that many lines, pauses, and prints the rest, so a test
# can put a line after the runner has already read an earlier one.
if [ -n "${FAKE_QEMU_SPLIT:-}" ]; then
    head -n "$FAKE_QEMU_SPLIT" "$FAKE_QEMU_LOG"
    sleep 0.5
    tail -n +"$((FAKE_QEMU_SPLIT + 1))" "$FAKE_QEMU_LOG"
else
    cat "$FAKE_QEMU_LOG"
fi
# Stay alive for a moment like a guest that is still shutting down, so the
# runner's watch loop reads the log instead of finding QEMU already gone.
sleep "${FAKE_QEMU_SLEEP:-1.2}"
QEMU
chmod +x "$work/bin/qemu-system-x86_64"
export PATH="$work/bin:$PATH" OVMF_FIRMWARE="$work/firmware" BOOT_TIMEOUT=5
export FAKE_QEMU_LOG="$work/fixture" FAKE_QEMU_ARGS="$work/qemu-args"
check() {
    local name="$1" expected="$2" contents="$3" text="${4:-}" split="${5:-}" status=0
    printf '%s\n' "$contents" > "$FAKE_QEMU_LOG"
    FAKE_QEMU_SPLIT="$split" bash "$runner" "$work/image" "$work/serial" host \
        > "$work/output" 2>&1 || status=$?
    if [ "$status" -ne "$expected" ]; then
        echo "$name: expected $expected, got $status" >&2
        cat "$work/output" >&2
        exit 1
    fi
    if [ -n "$text" ] && ! grep -qF -- "$text" "$work/output"; then
        echo "$name: expected the output to contain: $text" >&2
        cat "$work/output" >&2
        exit 1
    fi
    echo "PASS: $name"
}
check pass 0 $'FERRIC_TEST_PASS xstate\r\nFERRIC_SMOKE_OK\r'
check skip 0 $'FERRIC_TEST_SKIP xstate\nFERRIC_SMOKE_OK'
check missing-result 1 'FERRIC_SMOKE_OK'
check failure 1 $'FERRIC_TEST_FAIL xstate\nFERRIC_SMOKE_OK'
check panic-after-marker 1 $'FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK\nKERNEL PANIC'
check missing-marker 1 'FERRIC_TEST_PASS xstate'
# The boot in #155: the linker could not load the shell that runs the smoke
# script, so the marker could never appear. The run says so instead of timing out.
check smoke-shell-unlinked 1 \
    $'FERRIC_TEST_PASS iommu-dma\r\n[ld.so]: failed to link /usr/bin/ion: NotFound\r\ninit: cd "/" && env -i "ion" "/usr/lib/ferric-smoke.ion" failed with exit status: 1' \
    'the smoke shell could not start'
# The same message after the smoke test passed says nothing about the run.
check link-failure-after-marker 0 \
    $'FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK\n[ld.so]: failed to link /usr/bin/ion: NotFound'
# Nor does one that reaches the log while the runner is waiting out the second
# after it saw the line: the guest printed the marker in that second.
check marker-during-wait 0 \
    $'[ld.so]: failed to link /usr/bin/ion: NotFound\nFERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK' '' 1
# A program the smoke script starts can fail to link too; that is the script's
# test result, and the run must not be killed for it.
check child-link-failure 0 \
    $'FERRIC_SMOKE_BEGIN\n[ld.so]: failed to link /usr/bin/cat: NotFound\nFERRIC_TEST_SKIP profile\nFERRIC_SMOKE_OK'

# The smoke image's audio check needs both controllers on the QEMU command line.
grep -q -- '-device ich9-intel-hda,id=hda0' "$FAKE_QEMU_ARGS" \
    && grep -q -- '-device ich9-intel-hda,id=hda1' "$FAKE_QEMU_ARGS" \
    || { echo "runner did not add both HDA controllers" >&2; exit 1; }
echo "PASS: two HDA controllers"
