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
cat "$FAKE_QEMU_LOG"
QEMU
chmod +x "$work/bin/qemu-system-x86_64"
export PATH="$work/bin:$PATH" OVMF_FIRMWARE="$work/firmware" BOOT_TIMEOUT=5
export FAKE_QEMU_LOG="$work/fixture" FAKE_QEMU_ARGS="$work/qemu-args"
check() {
    local name="$1" expected="$2" contents="$3" status=0
    printf '%s\n' "$contents" > "$FAKE_QEMU_LOG"
    bash "$runner" "$work/image" "$work/serial" host > "$work/output" 2>&1 || status=$?
    if [ "$status" -ne "$expected" ]; then
        echo "$name: expected $expected, got $status" >&2
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

# The smoke image's audio check needs both controllers on the QEMU command line.
grep -q -- '-device ich9-intel-hda,id=hda0' "$FAKE_QEMU_ARGS" \
    && grep -q -- '-device ich9-intel-hda,id=hda1' "$FAKE_QEMU_ARGS" \
    || { echo "runner did not add both HDA controllers" >&2; exit 1; }
echo "PASS: two HDA controllers"
