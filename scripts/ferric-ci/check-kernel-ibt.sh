#!/usr/bin/env bash
# Checks that a kernel built with kernel.cpu.ibt on (arch.x86_64.features.ibt in the kernel's
# config.toml) is ready for Intel CET indirect branch tracking: every place the CPU or an
# indirect branch enters starts with endbr64, and nothing uses `notrack`. A miss is a #CP at
# boot on CET hardware, which CI cannot run (see crates/kernel/src/arch/x86_64/ibt.rs).
#
# Usage: scripts/ferric-ci/check-kernel-ibt.sh path/to/kernel.all
set -euo pipefail

kernel="${1:?usage: $0 path/to/kernel.all}"
dis="$(mktemp)"
trap 'rm -f "$dis"' EXIT
objdump -d -C -M intel --no-show-raw-insn "$kernel" > "$dis"

failed=0
checked=0

# Prints the first instruction of function $1, or nothing if there is no such symbol.
first_insn() {
    awk -v want=" <$1>:" '
        found { sub(/^[ \t]*[0-9a-f]+:[ \t]*/, ""); print; exit }
        length($0) >= length(want) && substr($0, length($0) - length(want) + 1) == want { found = 1 }
    ' "$dis"
}

expect_endbr() {
    local insn
    insn="$(first_insn "$1")"
    checked=$((checked + 1))
    if [ "$insn" != "endbr64" ]; then
        echo "MISSING endbr64: $1 starts with '${insn:-<symbol not found>}'"
        failed=1
    fi
}

# Hand-written entry points: SYSCALL (IA32_LSTAR), the bootloader and AP trampoline jumps.
expect_endbr kernel::arch::x86_64::interrupt::syscall::syscall_instruction
expect_endbr kstart
expect_endbr kernel::arch::x86_shared::start::kstart_ap

# Every interrupt and exception stub the IDT points at (the naked functions, not their `inner`).
handlers="$(nm -C "$kernel" | sed -n 's/^[0-9a-f]* [tT] \(kernel::arch::x86_shared::interrupt::\(exception\|irq\|ipi\)::[a-z0-9_]*\)$/\1/p')"
count="$(grep -c . <<< "$handlers" || true)"
if [ "$count" -lt 30 ]; then
    echo "Found only $count interrupt handlers; did their names change?"
    failed=1
fi
while read -r handler; do
    expect_endbr "$handler"
done <<< "$handlers"

# The 224 generic IRQ stubs: one every 16 bytes from __generic_interrupts_start.
start="$(nm "$kernel" | awk '$3 == "__generic_interrupts_start" { print $1 }')"
end="$(nm "$kernel" | awk '$3 == "__generic_interrupts_end" { print $1 }')"
stubs="$(objdump -d -M intel --no-show-raw-insn --start-address="0x$start" --stop-address="0x$end" "$kernel" \
    | awk -v start="$start" 'tolower($1) ~ /^[0-9a-f]+:$/ && $2 == "endbr64" { print $1 }' \
    | while read -r addr; do echo $(( (16#${addr%:} - 16#$start) )); done)"
expected="$(seq 0 16 $((223 * 16)))"
checked=$((checked + 224))
if [ "$stubs" != "$expected" ]; then
    echo "The generic interrupt stubs do not each start with endbr64 at 16-byte spacing"
    failed=1
fi

# A sample of compiled functions from the kernel and from build-std's core and alloc, which
# only get endbr64 if RUSTFLAGS reached them. Only functions whose address is taken or that
# are exported qualify: LLVM leaves endbr64 off local functions that are only called directly
# (such as kmain), which IBT never checks.
expect_endbr kernel::context::switch::switch_finish_hook
expect_endbr core::fmt::write
expect_endbr memcpy

# Jump tables are reached with `notrack jmp`, which IBT would have to trust; they must be off.
if grep -q notrack "$dis"; then
    echo "Found notrack-prefixed branches; jump tables are not disabled:"
    grep notrack "$dis" | head
    failed=1
fi

if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo "IBT: $checked entry points start with endbr64, no notrack branches"
