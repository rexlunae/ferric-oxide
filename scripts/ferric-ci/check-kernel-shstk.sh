#!/usr/bin/env bash
# Checks that a kernel built with kernel.cpu.shstk on (arch.x86_64.features.shstk "auto"/"always"
# in the kernel's config.toml, plus the self_modifying feature) carries the Intel CET supervisor
# shadow-stack machinery the boot code patches in. CI cannot run CET hardware, so this is a static
# check of the compiled kernel; see crates/kernel/src/arch/x86_64/shstk.rs.
#
# It verifies:
#   - the alternative code pool (.rodata, __altcode_start..__altcode_end) holds the context-switch
#     token protocol (rstorssp, saveprevssp) and the SYSCALL token ops (setssbsy, clrssbsy), which
#     alternative::early_init copies over the NOP slots once the shstk feature is selected;
#   - the context switch (context::arch::switch_to_inner) still has its NOP slot for that altcode;
#   - the per-CPU enable sequence `bts rax, 0x17; mov cr4, rax; setssbsy` (set CR4.CET, then make
#     the supervisor token busy) is present in .text for both the BSP and the AP path;
#   - the SYSCALL exit's CLRSSBSY is skipped when the saved RFLAGS has the trap flag set (the
#     single-step IRETQ exit keeps SSP at the busy token for IRETQ to free): the altcode pool has
#     `test BYTE PTR [rsp+0x89], 1; jne` right before `rdsspq rax; clrssbsy [rax]`;
#   - `wrssq` is in .text (shadow-stack writes once CET is on, and the saved-RIP shadow sync);
#   - the #CP handler (exception::control_protection, IDT vector 21) is linked in and its address
#     is taken in .text (where set_exceptions installs it), whether or not IBT is also built in.
#     A shadow-stack-only kernel without it would escalate every #CP to a double fault.
#
# Usage: scripts/ferric-ci/check-kernel-shstk.sh path/to/kernel.all
set -euo pipefail

kernel="${1:?usage: $0 path/to/kernel.all}"

failed=0

addr_of() { nm "$kernel" | awk -v s="$1" '$3 == s { print $1 }'; }

start="$(addr_of __altcode_start)"
end="$(addr_of __altcode_end)"
if [ -z "$start" ] || [ -z "$end" ]; then
    echo "MISSING __altcode_start/__altcode_end; is this a self_modifying kernel?"
    exit 1
fi

# The altcode pool lives in .rodata, so disassemble that address range explicitly with -D.
altcode="$(objdump -D -M intel --no-show-raw-insn \
    --start-address="0x$start" --stop-address="0x$end" "$kernel")"
for insn in rstorssp saveprevssp setssbsy clrssbsy; do
    if ! grep -qiw "$insn" <<< "$altcode"; then
        echo "MISSING $insn in the alternative code pool (shstk sequence not emitted)"
        failed=1
    fi
done

# The SYSCALL exit sequence: trap-flag test (RFLAGS byte 1 at InterruptStack offset 137), skip,
# then the token clear.
exit_seq="$(awk '
    /test[[:space:]]+BYTE PTR \[rsp\+0x89\],0x1$/ { s = 1; next }
    s == 1 && /jne[[:space:]]/ { s = 2; next }
    s == 2 && /rdsspq[[:space:]]+rax$/ { s = 3; next }
    s == 3 && /clrssbsy[[:space:]]+QWORD PTR \[rax\]$/ { n++ }
    { s = 0 }
    END { print n + 0 }
' <<< "$altcode")"
if [ "$exit_seq" -lt 1 ]; then
    echo "MISSING the trap-flag test before the SYSCALL exit's clrssbsy in the alternative code pool"
    failed=1
fi

# Full .text disassembly, demangled, for the slot and enable-sequence checks.
dis="$(objdump -d -C -M intel --no-show-raw-insn "$kernel")"

# The context switch must still carry its NOP slot (early_init overwrites it at boot). The shstk
# altcode is ~25 bytes, emitted as single-byte NOPs; require a healthy run of them in the function.
nops="$(awk '
    /<.*context::arch::switch_to_inner>:/ { f = 1 }
    f && /^[[:space:]]*[0-9a-f]+:[[:space:]]+nop$/ { n++ }
    f && /^$/ { exit }
    END { print n + 0 }
' <<< "$dis")"
if [ "$nops" -lt 20 ]; then
    echo "switch_to_inner has only $nops single-byte NOPs; the shstk alternative slot is missing"
    failed=1
fi

# `bts rax, 0x17` (set CR4.CET) immediately followed by `mov cr4, rax` and then `setssbsy`: the
# per-CPU enable, emitted once on the BSP path and once on the AP path.
enables="$(awk '
    /bts[[:space:]]+rax,0x17$/ { want_mov = 1; next }
    want_mov && /mov[[:space:]]+cr4,rax$/ { want_set = 1; want_mov = 0; next }
    want_set && /setssbsy$/ { n++; want_set = 0; next }
    { want_mov = 0; want_set = 0 }
    END { print n + 0 }
' <<< "$dis")"
if [ "$enables" -lt 2 ]; then
    echo "found $enables `bts rax,0x17; mov cr4,rax; setssbsy` sequences in .text, expected >= 2 (BSP + AP)"
    failed=1
fi

if ! grep -qw wrssq <<< "$dis"; then
    echo "MISSING wrssq in .text (shadow-stack writes and the saved-RIP shadow sync)"
    failed=1
fi

# The #CP handler must exist and its address must be taken by an instruction (set_exceptions
# builds the IDT entry from it), as an absolute immediate (`mov reg, 0x<addr>`) or a RIP-relative
# `lea` that objdump annotates with the symbol, not just survive as an unreferenced symbol.
cp_handler="$(nm -C "$kernel" | awk '$3 ~ /interrupt::exception::control_protection$/ { print $1; exit }')"
cp_hex="$(sed 's/^0*//' <<< "$cp_handler")"
if [ -z "$cp_handler" ]; then
    echo "MISSING the #CP handler (exception::control_protection); vector 21 is not installed"
    failed=1
elif ! grep -Eq "(mov|lea)[[:space:]].*(0x0*${cp_handler#"${cp_handler%%[!0]*}"}\b|<[^>]*interrupt::exception::control_protection>)" <<< "$dis"; then
    echo "the #CP handler (exception::control_protection) is never referenced from .text"
    failed=1
fi

if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo "shstk: altcode pool has rstorssp/saveprevssp/setssbsy/clrssbsy, switch_to_inner keeps its"
echo "       NOP slot, $enables CR4.CET+setssbsy enable sequences are present, and the #CP"
echo "       handler is installed"
