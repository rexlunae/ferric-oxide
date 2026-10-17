#!/usr/bin/env bash
# Boot a harddrive image in QEMU and wait for the smoke-test marker on the
# serial console (see config/ferric-smoke.toml).
#
# Usage: boot-smoke.sh IMAGE [LOG] [CPU]
#   CPU defaults to "host" with KVM, or "max" under TCG when /dev/kvm is missing.
set -euo pipefail

image="$1"
log="${2:-serial.log}"
cpu="${3:-}"
timeout_secs="${BOOT_TIMEOUT:-600}"

firmware=""
for f in /usr/share/ovmf/OVMF.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/qemu/OVMF.fd; do
    if [ -f "$f" ]; then firmware="$f"; break; fi
done
if [ -z "$firmware" ]; then
    echo "boot-smoke: no OVMF firmware found" >&2
    exit 2
fi

accel=()
if [ -w /dev/kvm ]; then
    accel=(-enable-kvm)
    cpu="${cpu:-host}"
else
    echo "boot-smoke: /dev/kvm not writable, falling back to TCG" >&2
    cpu="${cpu:-max}"
fi

# Work on a copy so the artifact stays pristine.
disk="$(mktemp --suffix=.img)"
cp "$image" "$disk"
trap 'rm -f "$disk"' EXIT

set +e
timeout --foreground "$timeout_secs" qemu-system-x86_64 \
    "${accel[@]}" -cpu "$cpu" -machine q35 -smp 4 -m 2048 \
    -bios "$firmware" \
    -drive file="$disk",format=raw,if=none,id=drv0 -device nvme,drive=drv0,serial=NVME_SERIAL \
    -netdev user,id=net0 -device e1000,netdev=net0 \
    -nographic -vga none -no-reboot \
    -serial mon:stdio < /dev/null 2>&1 | tee "$log"
status="${PIPESTATUS[0]}"
set -e

# The console mixes in CRs and escape sequences; normalize before matching.
clean="$(tr -d '\r' < "$log")"
# Test programs report FERRIC_TEST_PASS/SKIP/FAIL lines; any FAIL fails the run.
grep '^FERRIC_TEST_' <<< "$clean" || true
if grep -q '^FERRIC_TEST_FAIL' <<< "$clean"; then
    echo "boot-smoke: FAIL, a test reported failure" >&2
    exit 1
fi
if grep -q '^FERRIC_SMOKE_OK' <<< "$clean"; then
    echo "boot-smoke: PASS (cpu=$cpu, qemu exit $status)"
    exit 0
fi
if [ "$status" = 124 ]; then
    echo "boot-smoke: FAIL, timed out after ${timeout_secs}s without the marker" >&2
else
    echo "boot-smoke: FAIL, QEMU exited with $status without the marker" >&2
fi
exit 1
