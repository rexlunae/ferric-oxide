#!/usr/bin/env python3
"""Diagnostic: per benchmark round, the host-side cost of the guest's NVMe requests.

Watches the serial log for the guest's `FERRIC_DIAG round-start/round-end` markers and, at
each, snapshots the QEMU process's KVM debugfs counters and its threads' schedstat (run
and run-queue wait time). Prints one `KVMDIAG` line per round with the deltas. Run as root
(debugfs). Usage: kvm-diag-monitor.py LOG
"""
import glob
import os
import re
import sys
import time

log_path = sys.argv[1]
KEYS = ["exits", "mmio_exits", "io_exits", "halt_exits", "irq_exits", "irq_window_exits",
        "halt_successful_poll", "halt_attempted_poll", "halt_wakeup", "irq_injections",
        "halt_poll_success_ns", "halt_poll_fail_ns", "halt_wait_ns", "signal_exits",
        "request_irq_exits", "nmi_window_exits", "l1d_flush", "insn_emulation",
        "hypercalls", "directed_yield_successful", "preemption_reported"]


def qemu_pid():
    for _ in range(600):
        for pid in sorted(os.listdir("/proc"), key=lambda p: -int(p) if p.isdigit() else 0):
            if not pid.isdigit():
                continue
            try:
                exe = os.readlink(f"/proc/{pid}/exe")
            except OSError:
                continue
            if "qemu-system-x86_64" in exe:
                return int(pid)
        time.sleep(0.2)
    sys.exit("kvm-diag: no QEMU")


def read_int(path):
    try:
        with open(path) as f:
            return int(f.read().split()[0])
    except (OSError, ValueError, IndexError):
        return None


def snapshot(pid):
    snap = {}
    for vm in glob.glob(f"/sys/kernel/debug/kvm/{pid}-*"):
        for key in KEYS:
            value = read_int(os.path.join(vm, key))
            if value is not None:
                snap[key] = snap.get(key, 0) + value
    for task in glob.glob(f"/proc/{pid}/task/*"):
        try:
            with open(f"{task}/comm") as f:
                comm = f.read().strip()
            with open(f"{task}/schedstat") as f:
                run, wait, slices = (int(x) for x in f.read().split())
        except (OSError, ValueError):
            continue
        # Pool threads come and go; group them by name.
        name = re.sub(r"\d+$", "", comm).replace(" ", "_").replace("/", "_")
        snap[f"run.{name}"] = snap.get(f"run.{name}", 0) + run
        snap[f"wait.{name}"] = snap.get(f"wait.{name}", 0) + wait
        snap[f"slices.{name}"] = snap.get(f"slices.{name}", 0) + slices
    with open("/proc/stat") as f:
        cpu = [int(x) for x in f.readline().split()[1:]]
    snap["host.busy_ticks"] = sum(cpu) - cpu[3] - cpu[4]
    snap["host.steal_ticks"] = cpu[7] if len(cpu) > 7 else 0
    snap["t_ns"] = time.monotonic_ns()
    return snap


def main():
    pid = qemu_pid()
    print(f"KVMDIAG qemu pid {pid} nproc {os.cpu_count()} halt_poll_ns "
          f"{read_int('/sys/module/kvm/parameters/halt_poll_ns')} "
          f"vm dirs {glob.glob(f'/sys/kernel/debug/kvm/{pid}-*')}", flush=True)
    start = {}
    with open(log_path, "rb") as log:
        buf = b""
        while True:
            chunk = log.read(65536)
            if not chunk:
                if not os.path.exists(f"/proc/{pid}"):
                    return
                # boot-smoke.sh truncates the log as it starts QEMU.
                if os.fstat(log.fileno()).st_size < log.tell():
                    log.seek(0)
                    buf = b""
                time.sleep(0.01)
                continue
            buf += chunk
            *lines, buf = buf.split(b"\n")
            for raw in lines:
                line = raw.decode("utf-8", "replace")
                m = re.search(r"FERRIC_DIAG round-(start|end) (policy=\S+ threads=\d+ round=\d+)(?: iops=(\d+))?", line)
                if not m:
                    continue
                kind, key, iops = m.groups()
                if kind == "start":
                    start[key] = snapshot(pid)
                    continue
                end = snapshot(pid)
                before = start.pop(key, None)
                if before is None:
                    continue
                d = {k: end[k] - before.get(k, 0) for k in end}
                secs = d.pop("t_ns") / 1e9
                parts = [f"{k}={v}" for k, v in sorted(d.items()) if v]
                print(f"KVMDIAG {key} iops={iops} host_s={secs:.3f} " + " ".join(parts), flush=True)


main()
