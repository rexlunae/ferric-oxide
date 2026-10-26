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
#   BOOT_NIC=virtio gives the guest a virtio-net card (virtio-netd) in place of the
#   default e1000 (BOOT_NIC=e1000, e1000d), at the same slot.
#   BOOT_REBOOT=1 lets the guest reset and boot again (no -no-reboot), and does not
#   treat a kernel panic as a failure: for the crash-dump image, whose first boot
#   panics on purpose and resets (see image.yml's crash job, which checks the log).
set -euo pipefail

image="$1"
log="${2:-serial.log}"
cpu="${3:-}"
timeout_secs="${BOOT_TIMEOUT:-300}"

firmware="${OVMF_FIRMWARE:-}"
for f in /usr/share/ovmf/OVMF.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/qemu/OVMF.fd; do
    if [ -n "$firmware" ]; then break; fi
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
case "${BOOT_NIC:-e1000}" in
    e1000) net_dev=(-device e1000,netdev=net0,addr=5.0) ;;
    virtio) net_dev=(-device virtio-net-pci,netdev=net0,addr=5.0) ;;
    *)
        echo "boot-smoke: unknown BOOT_NIC=$BOOT_NIC (expected e1000 or virtio)" >&2
        exit 2
        ;;
esac

# Two HDA controllers for the audio check in config/ferric-smoke.toml: the
# first driver registers audiohw, the second must use audiohw.<pci-address>.
hda_dev=(-device ich9-intel-hda,id=hda0 -device hda-output,bus=hda0.0
         -device ich9-intel-hda,id=hda1 -device hda-output,bus=hda1.0)

reboot=(-no-reboot)
crash_pattern='UNHANDLED EXCEPTION|KERNEL PANIC|kernel panicked|panicked at'
# A boot that cannot start the smoke shell never prints the marker. ld.so names a
# file it could not link, and init reports the smoke command's exit status, so the
# run can stop there and say why instead of waiting out the timeout (#155).
# The linker line is fatal only for the shell itself: every program reports the
# same prefix, and a program the script starts that fails to link is one of its
# test results, not a reason to kill the guest.
# ld.so quotes the path it could not link, as in `failed to link '/usr/bin/ion':`,
# so the quotes are part of the pattern; the path may or may not have a directory.
smoke_dead_pattern="\[ld\.so\]: failed to link '([^']*/)?bin/ion':|init: .*ferric-smoke\.ion.* failed with exit status"
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
    "${hda_dev[@]}" \
    -nographic -vga none "${reboot[@]}" \
    -monitor unix:"$monitor",server,nowait \
    -serial stdio < /dev/null > "$log" 2>&1 &
qemu_pid=$!
tail -n +1 -f "$log" --pid="$qemu_pid" &
tail_pid=$!

status=""
crash=""
deadend=""
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
    if grep -qaE "$smoke_dead_pattern" "$log"; then
        # A guest that is already shutting down can print a link failure for a
        # program it stops, and it may print the marker in the same second. Watch
        # the marker for a second, then give the line its own meaning, so the
        # answer does not depend on when this pass happened to read the log.
        marker=""
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            if grep -qa 'FERRIC_SMOKE_OK' "$log"; then
                marker=1
                break
            fi
            sleep 0.1
        done
        if [ -n "$marker" ]; then
            continue
        fi
        deadend=1
        break
    fi
    sleep 1
done
# The same check after the loop covers a guest that exited before the watch saw it,
# and only applies while no marker has been printed: a link failure after the smoke
# test finished is not a reason to fail a boot that passed.
if [ -z "$deadend" ] && ! grep -qa 'FERRIC_SMOKE_OK' "$log" \
    && grep -qaE "$smoke_dead_pattern" "$log"; then
    deadend=1
fi
if [ -n "$deadend" ]; then
    kill "$qemu_pid" 2>/dev/null || true
    wait "$qemu_pid" 2>/dev/null || true
    tr -d '\r' < "$log" | grep -aE "$smoke_dead_pattern" | tail -n 5 >&2 || true
    echo "boot-smoke: FAIL, the smoke shell could not start (see the console lines above)" >&2
    exit 1
fi
if [ -z "$status" ]; then
    # Where every CPU is when the run times out: a guest stuck in the kernel shows kernel
    # addresses (and IF clear when it spins with interrupts off).
    echo "boot-smoke: CPU state at the timeout" >&2
    # Before anything disturbs the guest: every CPU's registers, sampled a few times, and for
    # each CPU spinning (not halted, in the kernel or in user space) its frame-pointer chain,
    # read through the monitor. Only then an NMI, whose dump on the serial log gives the
    # kernel image's KASLR slide (its RIP against the link address it prints), so the raw
    # addresses can be named below.
    probe_log="$(mktemp)"
    python3 - "$monitor" <<'PY' | tee "$probe_log" >&2 || true
import re, socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.settimeout(2)

def cmd(line):
    s.sendall(line.encode() + b"\n")
    out = b""
    deadline = time.time() + 5
    while not out.endswith(b"(qemu) ") and time.time() < deadline:
        try:
            chunk = s.recv(65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    return re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", out.decode(errors="replace"))

cmd("")
def cpus(text):
    found = {}
    for block in re.split(r"(?m)^CPU#", text)[1:]:
        n = int(block.split("\n", 1)[0])
        regs = dict(re.findall(r"([A-Z0-9]+)=([0-9a-f]+)", block))
        found[n] = regs
    return found

last = {}
for sample in range(5):
    last = cpus(cmd("info registers -a"))
    for n, r in sorted(last.items()):
        print(f"boot-smoke-cpu: sample {sample} cpu {n} rip=0x{r.get('RIP', '0')} "
              f"rbp=0x{r.get('RBP', '0')} rfl={r.get('RFL', '?')} cpl={r.get('CPL', '?')} "
              f"hlt={r.get('HLT', '?')}")
    time.sleep(0.2)
# A CPU spinning in user space (CPL 3) gets its user frame-pointer chain too: the NMI's
# context list names the program running there, and the return addresses name the loop.
for n, r in sorted(last.items()):
    if r.get("CPL") not in ("0", "3") or r.get("HLT") != "0":
        continue
    user = r.get("CPL") == "3"
    cmd(f"cpu {n}")
    rbp = int(r.get("RBP", "0"), 16)
    for depth in range(24):
        in_range = (0 < rbp < 0x0000800000000000) if user else rbp >= 0xffff800000000000
        if not in_range or rbp % 8:
            break
        m = re.search(r":\s*0x([0-9a-f]+)\s+0x([0-9a-f]+)", cmd(f"x /2gx {rbp:#x}"))
        if not m:
            break
        rbp, ret = int(m.group(1), 16), int(m.group(2), 16)
        print(f"boot-smoke-cpu: cpu {n} {'user ' if user else ''}frame {depth} ret=0x{ret:x}")
cmd("cpu 0")
cmd("nmi")
time.sleep(3)
PY
    kill "$qemu_pid" 2>/dev/null || true
    wait "$qemu_pid" 2>/dev/null || true
    status=124
    # The NMI's list of contexts, whole, on stderr: the failure annotation keeps only the last
    # lines of the console, and some callers discard stdout, so this is the copy that names the
    # context a hang with every CPU idle is stuck on.
    echo "boot-smoke: contexts at the timeout" >&2
    tr -d '\r' < "$log" | grep -aE '^(NMI: |  pid |  0x[0-9a-f]+: busy$)' >&2 || true
    # KERNEL_ELF (the image's kernel, with its symbol table) names the functions: the NMI's
    # own link addresses, and the sampled addresses less the slide the first NMI dump shows.
    if [ -n "${KERNEL_ELF:-}" ] && [ -f "$KERNEL_ELF" ]; then
        tr -d '\r' < "$log" | python3 -c '
import bisect, re, subprocess, sys
syms = []
for line in subprocess.run(["nm", "-n", "-C", "--defined-only", sys.argv[1]],
                           capture_output=True, text=True).stdout.splitlines():
    parts = line.split(" ", 2)
    if len(parts) == 3 and parts[1] in "tTwW":
        syms.append((int(parts[0], 16), parts[2]))
keys = [a for a, _ in syms]
def name(a):
    i = bisect.bisect_right(keys, a) - 1
    return f"{syms[i][1]}+{a - syms[i][0]:#x}" if i >= 0 else "?"
log = sys.stdin.read()
slide = None
m = re.search(r"NMI: interrupted at 0x([0-9a-f]+)\s+RFLAG:[^\n]*\n(?:[^\n]*\n)*?RIP:\s+([0-9a-f]+)", log)
if m:
    slide = int(m.group(2), 16) - int(m.group(1), 16)
    print(f"boot-smoke-cpu: kernel image slide {slide:#x}")
for link in re.findall(r"NMI: interrupted at 0x([0-9a-f]+)", log):
    print(f"boot-smoke-cpu: NMI at 0x{link} is in {name(int(link, 16))}")
if slide is not None:
    with open(sys.argv[2]) as probe:
        for line in probe:
            m = re.search(r"(sample \d+ cpu \d+ rip|cpu \d+ frame \d+ ret)=0x([0-9a-f]+)", line)
            if m and int(m.group(2), 16) >= 0xffffffff80000000:
                a = int(m.group(2), 16) - slide
                print(f"boot-smoke-cpu: {m.group(1)} {a:#x} is in {name(a)}")
' "$KERNEL_ELF" "$probe_log" >&2 || true
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
    echo "boot-smoke: PASS (cpu=$cpu, iommu=${BOOT_IOMMU:-none}, nic=${BOOT_NIC:-e1000}, qemu exit $status)"
    exit 0
fi
if [ "$status" = 124 ]; then
    echo "boot-smoke: FAIL, timed out after ${timeout_secs}s without the marker" >&2
else
    echo "boot-smoke: FAIL, QEMU exited with $status without the marker" >&2
fi
exit 1
