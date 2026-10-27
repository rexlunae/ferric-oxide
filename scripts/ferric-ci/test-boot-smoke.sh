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
# The timeout fixture serves a real monitor socket, so the runner's Python probe
# exercises register parsing, CPU selection and memory reads end to end.
if [ -n "${FAKE_QEMU_MONITOR:-}" ]; then
    exec python3 "$FAKE_QEMU_MONITOR" "$@"
fi
# FAKE_QEMU_SPLIT prints that many lines, pauses FAKE_QEMU_SPLIT_DELAY seconds,
# and prints the rest, so a test can put a line after the runner has already read
# an earlier one. The delay stays under the runner's one-second marker watch,
# which reads the log every 0.1 seconds, so the marker lands inside that window
# whatever the runner's first pass saw.
if [ -n "${FAKE_QEMU_SPLIT:-}" ]; then
    head -n "$FAKE_QEMU_SPLIT" "$FAKE_QEMU_LOG"
    sleep "${FAKE_QEMU_SPLIT_DELAY:-0.5}"
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
# ld.so quotes the path, as crates/relibc/src/ld_so/start.rs prints it.
check smoke-shell-unlinked 1 \
    $'FERRIC_TEST_PASS iommu-dma\r\n[ld.so]: failed to link \'/usr/bin/ion\': NotFound\r\ninit: cd "/" && env -i "ion" "/usr/lib/ferric-smoke.ion" failed with exit status: 1' \
    'the smoke shell could not start'
# The shell's path may also be reported without a directory part.
check smoke-shell-unlinked-bare-path 1 \
    $'FERRIC_TEST_PASS iommu-dma\n[ld.so]: failed to link \'/bin/ion\': NotFound' \
    'the smoke shell could not start'
# The same message after the smoke test passed says nothing about the run.
check link-failure-after-marker 0 \
    $'FERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK\n[ld.so]: failed to link \'/usr/bin/ion\': NotFound'
# Nor does one that reaches the log while the runner is watching for the marker
# after it saw the line: the guest printed the marker during that watch.
check marker-during-wait 0 \
    $'[ld.so]: failed to link \'/usr/bin/ion\': NotFound\nFERRIC_TEST_PASS xstate\nFERRIC_SMOKE_OK' '' 1
# A program the smoke script starts can fail to link too; that is the script's
# test result, and the run must not be killed for it.
check child-link-failure 0 \
    $'FERRIC_SMOKE_BEGIN\n[ld.so]: failed to link \'/usr/bin/cat\': NotFound\nFERRIC_TEST_SKIP profile\nFERRIC_SMOKE_OK'

# A boot that never prints the marker times out. The NMI the runner sends then makes the
# kernel list every context. The runner copies those rows, and only those, to stderr,
# because trace-overhead discards its stdout and the failure annotation keeps only the
# console's tail. A row the NMI could not lock reads "busy", and "context list busy"
# means it got none.
printf '%s\n' $'FERRIC_TEST_PASS xstate\r' 'NMI: 3 contexts' \
    '  pid 1 "/scheme/initfs/bin/init" Blocked "UserInner::read (v2)" running=false queued=false cpu=Some(0) syscall=None' \
    '  0xffffff7f8013bb60: busy' \
    $'  pid 40 "/usr/bin/ferric-ext4-test" Runnable "" running=false queued=false cpu=Some(2) syscall=None\r' \
    'not a context row' > "$FAKE_QEMU_LOG"
cat > "$work/monitor.py" <<'PYMONITOR'
import os
import socket
import sys

args = sys.argv[1:]
address = args[args.index("-monitor") + 1].removeprefix("unix:").split(",")[0]
with socket.socket(socket.AF_UNIX) as server:
    server.bind(address)
    server.listen(1)
    # Printing serial output only after listen makes readiness deterministic.
    with open(os.environ["FAKE_QEMU_LOG"]) as log:
        print(log.read(), end="", flush=True)
    connection, _ = server.accept()
    with connection, connection.makefile("rb") as commands, open(os.environ["FAKE_MONITOR_COMMANDS"], "w") as trace:
        connection.sendall(b"QEMU monitor\n(qemu) ")
        for line in commands:
            command = line.decode().strip()
            trace.write(command + "\n")
            trace.flush()
            reply = ""
            if command == "info registers -a":
                reply = (
                    "CPU#0\nRIP=0000000000400100 RBP=0000000000007000 RFL=00000202 CPL=3 HLT=0\n"
                    "CPU#1\nRIP=0000000000400200 RBP=0000000000007001 RFL=00000202 CPL=3 HLT=0\n"
                    "CPU#2\nRIP=0000000000400300 RBP=ffff800000007000 RFL=00000202 CPL=3 HLT=0\n"
                    "CPU#3\nRIP=0000000000400400 RBP=0000000000009000 RFL=00000202 CPL=3 HLT=1\n"
                )
            elif command == "x /2gx 0x7000":
                reply = "0000000000007000: 0x0000800000000000 0x0000000000401234\n"
            elif command.startswith("x /2gx"):
                raise AssertionError("probe followed an invalid, unaligned or halted frame: " + command)
            connection.sendall((reply + "(qemu) ").encode())
PYMONITOR
status=0
FAKE_QEMU_MONITOR="$work/monitor.py" FAKE_MONITOR_COMMANDS="$work/monitor-commands" \
BOOT_TIMEOUT=2 bash "$runner" "$work/image" "$work/serial" host \
    > "$work/stdout" 2> "$work/stderr" || status=$?
[ "$status" -eq 1 ] || { echo "timeout-contexts: expected 1, got $status" >&2; cat "$work/stderr" >&2; exit 1; }
grep -qF 'timed out after 2s' "$work/stderr" \
    || { echo "timeout-contexts: no timeout message" >&2; cat "$work/stderr" >&2; exit 1; }
sed -n '/^boot-smoke: contexts at the timeout$/,/^boot-smoke: FAIL/p' "$work/stderr" \
    | grep -v '^boot-smoke: ' > "$work/rows"
expected_rows=$(tr -d '\r' < "$FAKE_QEMU_LOG" | grep -v -e '^FERRIC_' -e '^not a context row$')
[ "$(cat "$work/rows")" = "$expected_rows" ] || {
    echo "timeout-contexts: the context rows on stderr were:" >&2
    cat "$work/rows" >&2
    exit 1
}
echo "PASS: timeout-contexts"
grep -qF 'boot-smoke-cpu: cpu 0 user frame 0 ret=0x401234' "$work/stderr" \
    || { echo "timeout-user-frames: missing CPL3 frame" >&2; cat "$work/stderr" >&2; exit 1; }
[ "$(grep -c '^x /2gx' "$work/monitor-commands")" -eq 1 ] \
    || { echo "timeout-user-frames: invalid frame dereferenced" >&2; cat "$work/monitor-commands" >&2; exit 1; }
grep -qx nmi "$work/monitor-commands" \
    || { echo "timeout-user-frames: no NMI after sampling" >&2; exit 1; }
if grep -q 'Traceback' "$work/stderr"; then
    cat "$work/stderr" >&2
    exit 1
fi
echo "PASS: timeout-user-frames"

# The smoke image's audio check needs both controllers on the QEMU command line.
grep -q -- '-device ich9-intel-hda,id=hda0' "$FAKE_QEMU_ARGS" \
    && grep -q -- '-device ich9-intel-hda,id=hda1' "$FAKE_QEMU_ARGS" \
    || { echo "runner did not add both HDA controllers" >&2; exit 1; }
echo "PASS: two HDA controllers"
