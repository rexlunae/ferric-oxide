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
chmod +x "$mock/"* "$rust_root/bin/"*
fixture="$temp/fixture"
mkdir -p "$fixture/$target/include" "$fixture/$target/lib" "$fixture/lib/gcc/$target/13.2.0" "$fixture/bin"
touch "$fixture/$target/include/stdio.h" "$fixture/$target/lib/libc.a" "$fixture/lib/gcc/$target/13.2.0/crtbegin.o" "$fixture/bin/gcc"
tar -cf "$prefix/target-runtime.tar.gz" -C "$fixture" .
export FAKE_RUST_ROOT="$rust_root" FAKE_CLANG_ARGS="$temp/clang-args"
PATH="$mock:$PATH" bash "$root/scripts/native-toolchain.sh" "$prefix" "$target"
test ! -e "$prefix/sysroot/bin/gcc"
test -f "$prefix/sysroot/$target/include/stdio.h"
test -L "$prefix/sysroot/bin/rustc"
test -L "$prefix/sysroot/lib/gcc/$target/13.2.0/crtbeginT.o"
case "$(cat "$prefix/sysroot/bin/$target.cfg")" in
    *--target=x86_64-unknown-linux-gnu*--sysroot=*) ;;
    *) echo 'Missing ELF driver or target sysroot' >&2; exit 1 ;;
esac
"$prefix/sysroot/bin/$target-clang" --target="$target" -c 'file with spaces.c'
case "$(cat "$FAKE_CLANG_ARGS")" in
    *--target=x86_64-unknown-redox*|*dynamic-linker*) exit 1 ;;
esac
test "$(tail -n 1 "$FAKE_CLANG_ARGS")" = 'file with spaces.c'
"$prefix/sysroot/bin/$target-clang" source.c -o app
case "$(cat "$FAKE_CLANG_ARGS")" in
    *--dynamic-linker=/usr/lib/ld64.so.1*) ;;
    *) exit 1 ;;
esac
"$prefix/sysroot/bin/$target-clang" -static source.c -o app
case "$(cat "$FAKE_CLANG_ARGS")" in *dynamic-linker*) exit 1 ;; esac
if PATH="$mock:$PATH" bash "$root/scripts/native-toolchain.sh" "$prefix" aarch64-unknown-redox; then
    echo 'Unsupported target was accepted' >&2; exit 1
fi
printf 'Native toolchain regression tests passed\n'
