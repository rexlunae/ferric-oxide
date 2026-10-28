#!/usr/bin/env bash
# Offline regression tests: never install tools or download runtime archives.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT
mock="$temp/mock"
rust_root="$temp/rust"
prefix="$temp/prefix with a 'quote"
target=x86_64-unknown-redox
mkdir -p "$mock" "$rust_root/bin" "$rust_root/lib/rustlib/mock-host/bin" "$prefix"
cat > "$mock/rustup" <<'STUB'
#!/usr/bin/env bash
case "$1" in
    which) printf '%s/bin/rustc\n' "$FAKE_RUST_ROOT" ;;
    component|target) ;;
    *) exit 90 ;;
esac
STUB
cat > "$rust_root/bin/rustc" <<'STUB'
#!/usr/bin/env bash
printf 'host: mock-host\n'
STUB
for tool in cargo rustdoc; do cp "$rust_root/bin/rustc" "$rust_root/bin/$tool"; done
cat > "$rust_root/bin/cargo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$FAKE_CARGO_ARGS"
STUB
for tool in ar nm objcopy objdump strip; do touch "$rust_root/lib/rustlib/mock-host/bin/llvm-$tool"; done
touch "$rust_root/lib/rustlib/mock-host/bin/rust-lld"
cat > "$mock/clang" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$FAKE_CLANG_ARGS"
STUB
cp "$mock/clang" "$mock/clang++"
cat > "$mock/gcc" <<'STUB'
#!/usr/bin/env bash
exit 91
STUB
# The X.Org util-macros download is replaced by a local fixture whose configure
# and make install only copy xorg-macros.m4; the hash check is driven by
# FAKE_SHA256 so both the pinned-hash and mismatch paths run offline.
util_macros=$(sed -n 's/^util_macros=//p' "$root/scripts/native-toolchain.sh")
util_macros_sha256=$(sed -n 's/^util_macros_sha256=//p' "$root/scripts/native-toolchain.sh")
mkdir -p "$temp/util-macros/$util_macros"
cat > "$temp/util-macros/$util_macros/configure" <<'STUB'
#!/usr/bin/env bash
prefix=${1#--prefix=}
printf 'install:\n\tmkdir -p "%s/share/aclocal"\n\tcp xorg-macros.m4 "%s/share/aclocal/"\n' "$prefix" "$prefix" > Makefile
STUB
chmod +x "$temp/util-macros/$util_macros/configure"
echo 'dnl xorg-macros fixture' > "$temp/util-macros/$util_macros/xorg-macros.m4"
tar -cJf "$temp/util-macros.tar.xz" -C "$temp/util-macros" "$util_macros"
cat > "$mock/curl" <<'STUB'
#!/usr/bin/env bash
while [ "$#" -gt 1 ]; do
    [ "$1" = --output ] && output=$2
    shift
done
case "$1" in https://www.x.org/releases/individual/util/util-macros-*.tar.xz) ;; *) exit 92 ;; esac
cp "$FAKE_UTIL_MACROS" "$output"
STUB
cat > "$mock/sha256sum" <<'STUB'
#!/usr/bin/env bash
printf '%s  %s\n' "$FAKE_SHA256" "$1"
STUB
export FAKE_UTIL_MACROS="$temp/util-macros.tar.xz" FAKE_SHA256="$util_macros_sha256"
chmod +x "$mock/"* "$rust_root/bin/"*
for target in x86_64-unknown-redox aarch64-unknown-redox; do
    prefix="$temp/prefix with a 'quote/$target"
    fixture="$temp/fixture/$target"
    mkdir -p "$prefix" "$fixture/$target/include" "$fixture/$target/lib" "$fixture/lib/gcc/$target/13.2.0" "$fixture/bin" \
        "$fixture/share/aclocal" "$fixture/share/libtool/build-aux"
    touch "$fixture/$target/include/stdio.h" "$fixture/$target/lib/libc.a" "$fixture/lib/gcc/$target/13.2.0/crtbegin.o" "$fixture/bin/gcc" \
        "$fixture/share/aclocal/libtool.m4" "$fixture/share/libtool/build-aux/ltmain.sh"
    printf '#!/bin/sh\npkgauxdir=/usr/share/libtool/build-aux\naclocaldir=/usr/share/aclocal\n' > "$fixture/bin/libtoolize.orig"
    tar -cf "$prefix/target-runtime.tar.gz" -C "$fixture" .
    export FAKE_RUST_ROOT="$rust_root" FAKE_CLANG_ARGS="$temp/clang-args" FAKE_CARGO_ARGS="$temp/cargo-args"
    PATH="$mock:$PATH" bash "$root/scripts/native-toolchain.sh" "$prefix" "$target"
    test ! -e "$prefix/sysroot/bin/gcc"
    test -f "$prefix/sysroot/$target/include/stdio.h"
    test -L "$prefix/sysroot/bin/rustc"
    test -L "$prefix/sysroot/lib/gcc/$target/13.2.0/crtbeginT.o"
    test -f "$prefix/sysroot/share/aclocal/libtool.m4"
    test -f "$prefix/sysroot/share/libtool/build-aux/ltmain.sh"
    test -f "$prefix/sysroot/share/aclocal/xorg-macros.m4"
    test -x "$prefix/sysroot/bin/libtoolize"
    grep -Fx "aclocaldir=$prefix/sysroot/share/aclocal" "$prefix/sysroot/bin/libtoolize" >/dev/null
    if grep -F /usr/share "$prefix/sysroot/bin/libtoolize" >/dev/null; then
        echo 'libtoolize still points at /usr/share' >&2; exit 1
    fi
    # make prefix exports this as LIBTOOLIZE so autoreconf uses Redox's libtool.
    printf 'print-libtoolize:\n\t@printf "%%s\\n" "$$LIBTOOLIZE"\n' > "$temp/print.mk"
    test "$(make --no-print-directory -f "$root/mk/llvm.mk" -f "$temp/print.mk" print-libtoolize \
        TARGET="$target" ROOT=/root-dir)" = "/root-dir/prefix/llvm/$target/sysroot//bin/libtoolize"
    driver="${target%-unknown-redox}-unknown-linux-gnu"
    case "$(cat "$prefix/sysroot/bin/$target.cfg")" in
        *--target="$driver"*--sysroot=*) ;;
        *) echo 'Missing ELF driver or target sysroot' >&2; exit 1 ;;
    esac
    "$prefix/sysroot/bin/$target-clang" --target="$target" -c 'file with spaces.c'
    case "$(cat "$FAKE_CLANG_ARGS")" in
        *--target=*-unknown-redox*|*dynamic-linker*) exit 1 ;;
    esac
    test "$(tail -n 1 "$FAKE_CLANG_ARGS")" = 'file with spaces.c'
    if "$prefix/sysroot/bin/$target-clang" --target=wrong-unknown-redox -c source.c; then
        echo 'Mismatched compiler target was accepted' >&2; exit 1
    fi
    "$prefix/sysroot/bin/$target-clang" source.c -o app
    interpreter=ld64.so.1
    if [ "$target" = aarch64-unknown-redox ]; then
        interpreter=ld.so.1
        grep -F 'unstable.build-std=["std","panic_abort"]' "$prefix/sysroot/bin/cargo" >/dev/null
        "$prefix/sysroot/bin/cargo" build --target "$target" -Zbuild-std=core,alloc
        grep -x 'unstable.build-std=\["std","panic_abort"\]' "$FAKE_CARGO_ARGS" >/dev/null
        grep -x -- '-Zbuild-std=core,alloc' "$FAKE_CARGO_ARGS" >/dev/null
    fi
    case "$(cat "$FAKE_CLANG_ARGS")" in
        *--dynamic-linker=/usr/lib/"$interpreter"*) ;;
        *) exit 1 ;;
    esac
    "$prefix/sysroot/bin/$target-clang" -static source.c -o app
    case "$(cat "$FAKE_CLANG_ARGS")" in *dynamic-linker*) exit 1 ;; esac
done
# A util-macros archive that does not match the pinned hash is rejected.
mismatch="$temp/prefix-mismatch/$target"
mkdir -p "$mismatch"
cp "$temp/prefix with a 'quote/$target/target-runtime.tar.gz" "$mismatch/"
if FAKE_SHA256=0000 PATH="$mock:$PATH" bash "$root/scripts/native-toolchain.sh" "$mismatch" "$target" 2>"$temp/mismatch.err"; then
    echo 'util-macros archive with the wrong hash was accepted' >&2; exit 1
fi
grep -F 'does not have SHA-256' "$temp/mismatch.err" >/dev/null
if PATH="$mock:$PATH" bash "$root/scripts/native-toolchain.sh" "$prefix" riscv64gc-unknown-redox; then
    echo 'Unsupported target was accepted' >&2; exit 1
fi

# Exercise the actual Make prerequisite list without building or installing.
inputs="$temp/inputs"
ready="prefix/llvm/$target/ready"
mkdir -p "$inputs/$(dirname "$ready")"
input_files=(
    crates/relibc/src/lib.rs crates/relibc/src/debug.rs
    crates/relibc/include/stdio.h crates/relibc/Cargo.toml
    crates/relibc/Cargo.lock crates/relibc/Makefile crates/relibc/config.mk
    crates/relibc/build.rs crates/relibc/cbindgen.globdefs.toml
    crates/relibc/.cargo/config.toml crates/relibc/redox-rt/Cargo.toml
    crates/relibc/redox-rt/src/lib.rs
    crates/relibc/ld_so/src/lib.rs crates/relibc/redox-ioctl/src/lib.rs
    crates/relibc/generic-rt/src/lib.rs crates/relibc/openlibm/src/math.c
    crates/relibc/dlmalloc-rs/src/lib.rs crates/hwrand/src/lib.rs
    crates/procinfo/src/lib.rs crates/abi/src/lib.rs crates/kernel/syscall/src/lib.rs
    scripts/native-toolchain.sh scripts/native/runtime.ld rust-toolchain.toml
    recipes/core/relibc/recipe.toml
)
for input in "${input_files[@]}"; do
    mkdir -p "$inputs/$(dirname "$input")"
    touch "$inputs/$input"
done
touch "$inputs/host-ready"
for cache in target build sysroot prefix .git; do
    mkdir -p "$inputs/crates/relibc/$cache"
    touch "$inputs/crates/relibc/$cache/generated"
done
mkdir -p "$inputs/crates/relibc/openlibm/.git" "$inputs/crates/relibc/dlmalloc-rs/target"
touch "$inputs/crates/relibc/openlibm/.git/index" "$inputs/crates/relibc/dlmalloc-rs/target/cache"
find "$inputs" -exec touch -t 202001010000 {} +
touch -t 202001020000 "$inputs/$ready"
check_ready() {
    local status=0
    (cd "$inputs" && make --no-print-directory -f "$root/mk/llvm.mk" \
        -q "$ready" TARGET="$target" FSTOOLS_TAG=host-ready) || status=$?
    if [ "$status" -ne "$1" ]; then
        echo "Unexpected native sysroot freshness ($status, expected $1): $2" >&2
        exit 1
    fi
}
check_ready 0 'unchanged inputs'
for input in "${input_files[@]}"; do
    touch -t 202001030000 "$inputs/$input"
    check_ready 1 "$input changed"
    touch -t 202001010000 "$inputs/$input"
done
# Parent directory timestamps detect additions/removals, even of old files.
rm "$inputs/crates/relibc/include/stdio.h"
touch -t 202001030000 "$inputs/crates/relibc/include"
check_ready 1 'header removed'
touch -t 202001010000 "$inputs/crates/relibc/include"
find "$inputs/crates/relibc" -name generated -o -name cache -o -name index | while IFS= read -r output; do
    touch -t 202001030000 "$output"
done
check_ready 0 'generated caches changed'

# QEMU must use the selected image/CPU and accelerate only matching guests.
cat > "$mock/qemu" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$FAKE_QEMU_ARGS"
STUB
chmod +x "$mock/qemu"
export FAKE_QEMU_ARGS="$temp/qemu-args"
for arch in aarch64 x86_64; do
    image_dir="$temp/qemu-$arch"
    mkdir -p "$image_dir"
    touch "$image_dir/harddrive.img" "$image_dir/redox-live.iso" "$image_dir/extra.img" "$image_dir/firmware.fd"
    make --no-print-directory -f "$root/mk/qemu.mk" qemu-run ARCH="$arch" \
        HOST_ARCH=arm64 UNAME=Darwin BUILD="$image_dir" gpu=no PREFIX_TOOLCHAIN=llvm \
        FIRMWARE= PFLASH0="$image_dir/firmware.fd" QEMU="$mock/qemu" >/dev/null
    image=harddrive.img
    [ "$arch" = aarch64 ] && image=redox-live.iso
    case "$(cat "$FAKE_QEMU_ARGS")" in
        *"$image_dir/$image"*) ;;
        *) echo 'Wrong guest image selected' >&2; exit 1 ;;
    esac
    if grep -x hvf "$FAKE_QEMU_ARGS" >/dev/null; then
        echo 'Unexpected hardware acceleration in software-only defaults' >&2; exit 1
    fi
    grep -x max "$FAKE_QEMU_ARGS" >/dev/null
    if [ "$arch" = aarch64 ]; then
        make --no-print-directory -f "$root/mk/qemu.mk" qemu-run ARCH="$arch" \
            HOST_ARCH=arm64 UNAME=Darwin BUILD="$image_dir" gpu=no kvm=yes PREFIX_TOOLCHAIN=llvm \
            FIRMWARE= PFLASH0="$image_dir/firmware.fd" QEMU="$mock/qemu" >/dev/null
        grep -x hvf "$FAKE_QEMU_ARGS" >/dev/null
        grep -x host "$FAKE_QEMU_ARGS" >/dev/null
        make --no-print-directory -f "$root/mk/qemu.mk" qemu-run ARCH="$arch" \
            HOST_ARCH=arm64 UNAME=Darwin BUILD="$image_dir" gpu=no FERRIC_QEMU_LIVE=no PREFIX_TOOLCHAIN=llvm \
            FIRMWARE= PFLASH0="$image_dir/firmware.fd" QEMU="$mock/qemu" >/dev/null
        grep -F "$image_dir/harddrive.img" "$FAKE_QEMU_ARGS" >/dev/null
    fi
done
# No native toolchain or generated configuration: retain upstream CPU defaults.
image_dir="$temp/qemu-x86_64"
make --no-print-directory -f "$root/mk/qemu.mk" qemu-run ARCH=x86_64 \
    HOST_ARCH=arm64 UNAME=Darwin BUILD="$image_dir" gpu=no PREFIX_TOOLCHAIN=recipe \
    FIRMWARE= PFLASH0="$image_dir/firmware.fd" QEMU="$mock/qemu" >/dev/null
grep -x core2duo "$FAKE_QEMU_ARGS" >/dev/null
image_dir="$temp/qemu-aarch64"
make --no-print-directory -f "$root/mk/qemu.mk" qemu-run ARCH=aarch64 \
    HOST_ARCH=arm64 UNAME=Darwin BUILD="$image_dir" gpu=no PREFIX_TOOLCHAIN=recipe \
    FIRMWARE= PFLASH0="$image_dir/firmware.fd" QEMU="$mock/qemu" >/dev/null
grep -x hvf "$FAKE_QEMU_ARGS" >/dev/null
grep -x max "$FAKE_QEMU_ARGS" >/dev/null
# An explicit CPU model still overrides the native LLVM default.
image_dir="$temp/qemu-x86_64"
make --no-print-directory -f "$root/mk/qemu.mk" qemu-run ARCH=x86_64 \
    HOST_ARCH=arm64 UNAME=Darwin BUILD="$image_dir" gpu=no PREFIX_TOOLCHAIN=llvm \
    QEMU_CPU=custom-model FIRMWARE= PFLASH0="$image_dir/firmware.fd" QEMU="$mock/qemu" >/dev/null
grep -x custom-model "$FAKE_QEMU_ARGS" >/dev/null
# A board ROM replaces generic UEFI firmware; no download is needed for check.
make --no-print-directory -f "$root/mk/qemu.mk" qemu-check ARCH=aarch64 \
    BOARD=raspi3bp QEMU="$mock/qemu" FIRMWARE= PFLASH0= >/dev/null
if make --no-print-directory -f "$root/mk/qemu.mk" qemu-check ARCH=aarch64 \
    QEMU="$mock/qemu" FIRMWARE= PFLASH0= >/dev/null 2>&1; then
    echo 'Missing ARM64 firmware was accepted' >&2; exit 1
fi
printf 'Native toolchain regression tests passed\n'
