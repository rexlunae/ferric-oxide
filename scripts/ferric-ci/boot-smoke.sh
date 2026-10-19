#!/usr/bin/env bash
# Boot a harddrive image in QEMU and wait for the smoke-test marker on the
# serial console (see config/ferric-smoke.toml).
#
# Usage: boot-smoke.sh IMAGE [LOG] [CPU]
#   CPU defaults to "host" with KVM, or "max" under TCG when /dev/kvm is missing.
#   BOOT_IOMMU=vtd adds an emulated Intel VT-d IOMMU (DMA and interrupt
#   remapping), for kernels built with kernel.iommu = "auto" or "enforce".
#   BOOT_IOMMU=amdvi adds an emulated AMD IOMMU (DMA remapping only) instead. Its
#   DMA translation and I/O page fault events need QEMU 10 or later (dma-remap).
#   BOOT_REBOOT=1 lets the guest reset and boot again (no -no-reboot), and does not
#   treat a kernel panic as a failure: for the crash-dump image, whose first boot
#   panics on purpose and resets (see image.yml's crash job, which checks the log).
set -euo pipefail

image="$1"
log="${2:-serial.log}"
cpu="${3:-}"
timeout_secs="${BOOT_TIMEOUT:-300}"

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
    # TCG cannot emulate "host"; "max" is the closest it has.
    if [ -z "$cpu" ] || [ "$cpu" = host ]; then cpu=max; fi
fi

# QEMU's intel-iommu needs the q35 machine and, under KVM, an irqchip split
# between KVM and QEMU, and it must come before the devices it translates for.
machine=q35
iommu=()
case "${BOOT_IOMMU:-none}" in
    none) ;;
    vtd)
        machine=q35,kernel-irqchip=split
        # Interrupt remapping needs the split irqchip, so QEMU's I/O APIC sends its
        # interrupts through the IOMMU.
        iommu=(-device intel-iommu,intremap=on)
        ;;
    amdvi)
        # dma-remap=on makes QEMU translate DMA through the device table and page
        # tables (and log I/O page faults). The kernel does not remap interrupts on
        # AMD-Vi yet, so interrupt remapping stays off and needs no split irqchip.
        iommu=(-device amd-iommu,intremap=off,dma-remap=on)
        ;;
    *)
        echo "boot-smoke: unknown BOOT_IOMMU=$BOOT_IOMMU (expected none, vtd or amdvi)" >&2
        exit 2
        ;;
esac

# QEMU's edu device can DMA between its own buffer and memory, which
# ferric-iommu-test (recipes/tests) drives to test DMA remapping. Its default
# 28-bit DMA mask would clamp addresses; let it reach all of memory. A fixed
# slot gives it a known requester ID, 00:10.0.
edu=(-device edu,addr=10.0,dma_mask=0xffffffffffffffff)

# The disk and network controllers sit at fixed slots too, so check-iommu-log.sh can
# find the DMA domains of their drivers by requester ID.
disk_dev=(-device nvme,drive=drv0,serial=NVME_SERIAL,addr=4.0)
net_dev=(-device e1000,netdev=net0,addr=5.0)

reboot=(-no-reboot)
crash_pattern='UNHANDLED EXCEPTION|KERNEL PANIC|kernel panicked|panicked at'
if [ "${BOOT_REBOOT:-0}" = 1 ]; then
    reboot=()
    # The intended panic is checked by the caller; only an exception still ends the run.
    crash_pattern='UNHANDLED EXCEPTION'
fi

# Work on a copy so the artifact stays pristine.
disk="$(mktemp --suffix=.img)"
cp "$image" "$disk"
# A QEMU monitor of its own, so a run that times out can dump where each CPU is.
monitor="$(mktemp -u --suffix=.mon)"
trap 'rm -f "$disk" "$monitor"' EXIT

# Run QEMU in the background and watch the log, so a crash or the shutdown
# ends the run at once instead of waiting for the timeout.
: > "$log"
qemu-system-x86_64 \
    "${accel[@]}" -cpu "$cpu" -machine "$machine" -smp 4 -m 2048 \
    -bios "$firmware" \
    "${iommu[@]}" \
    -drive file="$disk",format=raw,if=none,id=drv0 "${disk_dev[@]}" \
    -netdev user,id=net0 "${net_dev[@]}" \
    "${edu[@]}" \
    -nographic -vga none "${reboot[@]}" \
    -monitor unix:"$monitor",server,nowait \
    -serial stdio < /dev/null > "$log" 2>&1 &
qemu_pid=$!
tail -n +1 -f "$log" --pid="$qemu_pid" &
tail_pid=$!

status=""
crash=""
for ((i = 0; i < timeout_secs; i++)); do
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
        status=0
        wait "$qemu_pid" || status=$?
        break
    fi
    if grep -qaE "$crash_pattern" "$log"; then
        crash=1
        sleep 2 # let the rest of the backtrace reach the log
        break
    fi
    sleep 1
done
if [ -z "$status" ]; then
    # Where every CPU is when the run times out: a guest stuck in the kernel shows kernel
    # addresses (and IF clear when it spins with interrupts off).
    echo "boot-smoke: CPU state at the timeout" >&2
    python3 - "$monitor" <<'PY' >&2 || true
import socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.settimeout(2)
# An NMI first: the kernel prints where each CPU was and its frames on the serial log.
for cmd, wait in ((b"nmi\n", 3), (b"info cpus\n", 1), (b"info registers -a\n", 1)):
    s.sendall(cmd)
    time.sleep(wait)
out = b""
try:
    while True:
        chunk = s.recv(65536)
        if not chunk:
            break
        out += chunk
except OSError:
    pass
for line in out.decode(errors="replace").splitlines():
    if line.startswith(("*", " ", "CPU", "RIP", "RSP", "RFL", "CS", "CR3")) or "thread_id" in line:
        print("boot-smoke-cpu: " + line.strip())
PY
    kill "$qemu_pid" 2>/dev/null || true
    wait "$qemu_pid" 2>/dev/null || true
    status=124
    # KERNEL_ELF (the image's kernel, with its symbol table) names the functions the NMI
    # found the CPUs in.
    if [ -n "${KERNEL_ELF:-}" ] && [ -f "$KERNEL_ELF" ]; then
        grep -aoE 'NMI: interrupted at 0x[0-9a-f]+' "$log" | awk '{ print $4 }' \
            | python3 -c '
import bisect, subprocess, sys
syms = []
for line in subprocess.run(["nm", "-n", "-C", "--defined-only", sys.argv[1]],
                           capture_output=True, text=True).stdout.splitlines():
    parts = line.split(" ", 2)
    if len(parts) == 3 and parts[1] in "tTwW":
        syms.append((int(parts[0], 16), parts[2]))
keys = [a for a, _ in syms]
for addr in sys.stdin:
    a = int(addr, 16)
    i = bisect.bisect_right(keys, a) - 1
    name = f"{syms[i][1]}+{a - syms[i][0]:#x}" if i >= 0 else "?"
    print(f"boot-smoke-cpu: NMI at {a:#x} is in {name}")
' "$KERNEL_ELF" >&2 || true
    fi
fi
kill "$tail_pid" 2>/dev/null || true

# The console mixes in CRs and escape sequences; normalize before matching.
clean="$(tr -d '\r' < "$log")"
# Markers can share a line with other console output (the login prompt, say),
# so match them wherever they start, one per line.
markers="$(grep -ao 'FERRIC_\(TEST_[A-Z]*\|SMOKE_[A-Z]*\).*' <<< "$clean" || true)"
# Test programs report FERRIC_TEST_PASS/SKIP/FAIL lines; any FAIL fails the run.
grep '^FERRIC_TEST_' <<< "$markers" || true
if grep -q '^FERRIC_TEST_FAIL' <<< "$markers"; then
    echo "boot-smoke: FAIL, a test reported failure" >&2
    exit 1
fi
# A crash after the marker (during shutdown, say) still fails the run, even
# when QEMU exited before the watch loop saw it.
if [ -n "$crash" ] || grep -qaE "$crash_pattern" <<< "$clean"; then
    echo "boot-smoke: FAIL, crash on the console (see above)" >&2
    exit 1
fi
if ! grep -q '^FERRIC_TEST_\(PASS\|SKIP\)' <<< "$markers"; then
    echo "boot-smoke: FAIL, no test reported a result" >&2
    exit 1
fi
if grep -q '^FERRIC_SMOKE_OK' <<< "$markers"; then
    echo "boot-smoke: PASS (cpu=$cpu, iommu=${BOOT_IOMMU:-none}, qemu exit $status)"
    exit 0
fi
if [ "$status" = 124 ]; then
    echo "boot-smoke: FAIL, timed out after ${timeout_secs}s without the marker" >&2
else
    echo "boot-smoke: FAIL, QEMU exited with $status without the marker" >&2
fi
exit 1
