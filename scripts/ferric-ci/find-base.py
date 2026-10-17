"""Find load bases of a shared object that turn a list of return addresses into call sites."""
import bisect, re, subprocess, sys
lib, pcs = sys.argv[1], [int(x, 16) for x in sys.argv[2].split(",")]
dis = subprocess.run(["objdump", "-d", "--no-show-raw-insn", lib], capture_output=True, text=True).stdout
insns = []
for line in dis.splitlines():
    m = re.match(r"\s+([0-9a-f]+):\s+(\S.*)", line)
    if m:
        insns.append((int(m.group(1), 16), m.group(2)))
after_call = {insns[i + 1][0] for i in range(len(insns) - 1) if insns[i][1].startswith("call")}
syms = []
for line in subprocess.run(["nm", "-C", "-n", "--defined-only", lib], capture_output=True, text=True).stdout.splitlines():
    p = line.split(" ", 2)
    if len(p) == 3 and p[1] in "tTwW":
        syms.append((int(p[0], 16), p[2]))
keys = [s[0] for s in syms]
def sym(a):
    i = bisect.bisect_right(keys, a) - 1
    return f"{syms[i][1]}+{a - syms[i][0]:#x}" if i >= 0 else "?"
hi = max(insns)[0]
for base in range(max(0, min(pcs) - hi) & ~0xfff, min(pcs) + 1, 0x1000):
    rets = [p - base for p in pcs[1:]]
    good = [r in after_call for r in rets]
    if sum(good) >= len(rets) - 1 and 0 <= pcs[0] - base <= hi:
        print(f"base {base:#x}: {sum(good)}/{len(rets)} return addresses follow calls")
        for p in pcs:
            print(f"  {p:#x} -> {sym(p - base)}")
        off = pcs[0] - base
        for a, t in insns:
            if off - 0x60 <= a <= off + 0x10:
                print(f"    {a:#x}{' <==' if a == off else ''}  {t}")
