#!/usr/bin/env bash
# Checks kernel image KASLR's relocations: links the kernel twice, once at its
# usual address and once SLIDE bytes higher, moves the first with
# ferric-elfreloc (as the bootloader does) and requires the loaded sections
# of both to be byte-for-byte identical. A relocation the bootloader cannot
# replay (a new type, or one missing from --emit-relocs) shows up here.
#
# Usage: check-kernel-relocs.sh [SLIDE]   (default 0x2f001000)
# Needs objcopy and the kernel's toolchain; run from the repository root.
set -euo pipefail

slide="${1:-0x2f001000}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

objcopy="${OBJCOPY:-objcopy}"
link_args="-C link-arg=--emit-relocs"

# The moved link keeps KERNEL_LINK_BASE (an absolute symbol the kernel reads
# to work out its slide) and starts the image SLIDE bytes above it, which is
# exactly what a relocated image looks like.
sed -e "s/^\(    \. = KERNEL_LINK_BASE\);/\1 + $slide;/" \
    -e "s/- KERNEL_LINK_BASE)/- (KERNEL_LINK_BASE + $slide))/" \
    crates/kernel/linkers/x86_64.ld > "$work/moved.ld"
if cmp -s crates/kernel/linkers/x86_64.ld "$work/moved.ld"; then
    echo "check-kernel-relocs: could not move the linker script" >&2
    exit 2
fi

[ -f crates/kernel/config.toml ] || cp crates/kernel/config.toml.example crates/kernel/config.toml
make -s -C crates/kernel BUILD="$work/base" OBJCOPY="$objcopy" KERNEL_LINK_ARGS="$link_args" "$work/base/kernel"
make -s -C crates/kernel BUILD="$work/moved" OBJCOPY="$objcopy" KERNEL_LINK_ARGS="$link_args" \
    LD_SCRIPT="$work/moved.ld" "$work/moved/kernel"

cargo run --quiet -p ferric-elfreloc --example relocate -- \
    "$work/base/kernel" "$slide" "$work/relocated"

"$objcopy" -O binary "$work/relocated" "$work/relocated.bin"
"$objcopy" -O binary "$work/moved/kernel" "$work/moved.bin"
if ! cmp "$work/relocated.bin" "$work/moved.bin"; then
    echo "check-kernel-relocs: FAIL, the relocated kernel differs from one linked at +$slide" >&2
    cmp -l "$work/relocated.bin" "$work/moved.bin" | head -20 >&2
    exit 1
fi
echo "check-kernel-relocs: OK, relocated kernel matches one linked at +$slide"
