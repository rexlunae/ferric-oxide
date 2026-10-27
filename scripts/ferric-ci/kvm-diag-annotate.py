#!/usr/bin/env python3
"""Diagnostic: KVMDIAG lines as short ::notice:: annotations, readable through the REST API.

Per round: iops, mmio_exits, exits, and mmio_exits per request after taking off the same
thread count's idle round (each round is BENCH_ROUND_BYTES / 4 KiB = 1024 preads, and its
delta covers 400 ms of idle before the reads, like the idle round). Usage: LOG [TITLE]
"""
import re
import sys

REQS = 1024
title = sys.argv[2] if len(sys.argv) > 2 else "kvmdiag"
idle = {}
out = []
for line in open(sys.argv[1], errors="replace"):
    m = re.match(r"KVMDIAG policy=(\S+) threads=(\d+) round=(\d+) iops=(\d+) host_s=(\S+) (.*)", line)
    if not m:
        continue
    policy, threads, rnd, iops, secs, rest = m.groups()
    kv = dict(p.split("=", 1) for p in rest.split() if "=" in p)
    mmio = int(kv.get("mmio_exits", 0))
    exits = int(kv.get("exits", 0))
    if policy == "idle":
        idle[threads] = (mmio, exits)
        out.append(f"t{threads} idle mmio={mmio} exits={exits}")
        continue
    im, ie = idle.get(threads, (0, 0))
    out.append(f"t{threads} {policy} r{rnd} iops={iops} mmio={mmio} exits={exits} "
               f"mmio/req={(mmio - im) / REQS:.2f} exits/req={(exits - ie) / REQS:.2f}")
for i in range(0, len(out), 18):
    body = "%0A".join(out[i:i + 18])
    print(f"::notice title={title} {i // 18}::{body}")
if not out:
    print(f"::warning title={title}::no KVMDIAG rounds")
