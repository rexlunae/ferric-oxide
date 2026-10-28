#!/usr/bin/env bash
# Assemble a native cross toolchain. No host packages or compilers are installed.
set -euo pipefail

prefix=${1:?absolute prefix directory required}
target=${2:?target triple required}
case "$target" in
    x86_64-unknown-redox) driver=x86_64-unknown-linux-gnu; interpreter=ld64.so.1 ;;
    aarch64-unknown-redox) driver=aarch64-unknown-linux-gnu; interpreter=ld.so.1 ;;
    *) echo "Native LLVM supports x86_64-unknown-redox and aarch64-unknown-redox; use the recipe toolchain for $target" >&2; exit 1 ;;
esac
root=$(cd "$(dirname "$0")/.." && pwd)
channel=$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$root/rust-toolchain.toml")
rustup_cmd=(env -u RUSTUP_TOOLCHAIN rustup)
"${rustup_cmd[@]}" component add --toolchain "$channel" rust-src llvm-tools-preview
# ARM64 Redox is a tier-3 target: Cargo builds std from rust-src below.
if [ "$target" = x86_64-unknown-redox ]; then
    "${rustup_cmd[@]}" target add --toolchain "$channel" "$target"
fi
rustc_path=$("${rustup_cmd[@]}" which --toolchain "$channel" rustc)
rust_root=$(cd "$(dirname "$rustc_path")/.." && pwd)
host=$("$rustc_path" -vV | sed -n 's/^host: //p')
llvm_bin="$rust_root/lib/rustlib/$host/bin"
clang_path=$(command -v clang)

# Rust's Redox target and existing C++ packages still link GCC runtime libraries.
# These are target data, independent of the machine executing the compiler. Only
# headers, libraries and startup objects are extracted; Linux executables are not.
mkdir -p "$prefix"
archive="$prefix/target-runtime.tar.gz"
if [ ! -f "$archive" ]; then
    curl --fail --location --retry 5 --output "$archive.partial" \
        "https://static.redox-os.org/toolchain/x86_64-unknown-linux-gnu/$target/gcc-install.tar.gz"
    mv "$archive.partial" "$archive"
fi
staging="$prefix/runtime.partial"
rm -rf "$staging"
mkdir -p "$staging"
# Redox's libtool (its m4 macros, libtoolize and ltmain.sh) knows how to link
# shared libraries for Redox; autotools recipes regenerate with it.
tar -xf "$archive" -C "$staging" --strip-components=1 \
    "./$target/include" "./$target/lib" "./lib/gcc/$target" \
    ./share/aclocal ./share/libtool ./bin/libtoolize.orig
gcc_lib=$(find "$staging/lib/gcc/$target" -mindepth 1 -maxdepth 1 -type d | head -n 1)
[ -n "$gcc_lib" ] || { echo 'Target runtime archive has no GCC runtime objects' >&2; exit 1; }
version=$(basename "$gcc_lib")
[ -e "$gcc_lib/crtbeginT.o" ] || ln -s crtbegin.o "$gcc_lib/crtbeginT.o"
sysroot="$prefix/sysroot"
rm -rf "$sysroot"
mkdir -p "$sysroot/bin" "$sysroot/lib"
cp -R "$staging/$target" "$sysroot/$target"
cp -R "$staging/lib/gcc" "$sysroot/lib/gcc"
mkdir -p "$sysroot/share"
cp -R "$staging/share/aclocal" "$staging/share/libtool" "$sysroot/share/"
# libtoolize was installed for /usr; point it at this sysroot's copy.
sed "s|/usr/share|$sysroot/share|g" "$staging/bin/libtoolize.orig" > "$sysroot/bin/libtoolize"
chmod +x "$sysroot/bin/libtoolize"
rm -rf "$staging"

# X.Org's autotools macros (xorg-macros.m4), which X11 recipes need to
# regenerate their build files. Podman images get them from xutils-dev.
util_macros=util-macros-1.20.2
util_macros_sha256=9ac269eba24f672d7d7b3574e4be5f333d13f04a7712303b1821b2a51ac82e8e
util_macros_archive="$prefix/$util_macros.tar.xz"
if [ ! -f "$util_macros_archive" ]; then
    curl --fail --location --retry 5 --output "$util_macros_archive.partial" \
        "https://www.x.org/releases/individual/util/$util_macros.tar.xz"
    mv "$util_macros_archive.partial" "$util_macros_archive"
fi
if command -v sha256sum >/dev/null; then
    actual=$(sha256sum "$util_macros_archive")
else
    actual=$(shasum -a 256 "$util_macros_archive")
fi
if [ "${actual%% *}" != "$util_macros_sha256" ]; then
    echo "$util_macros_archive does not have SHA-256 $util_macros_sha256" >&2
    exit 1
fi
util_macros_build="$prefix/$util_macros.partial"
rm -rf "$util_macros_build"
mkdir -p "$util_macros_build"
tar -xf "$util_macros_archive" -C "$util_macros_build" --strip-components=1
(cd "$util_macros_build" && ./configure --prefix="$sysroot" >/dev/null && make install >/dev/null)
rm -rf "$util_macros_build"
ln -s "$rust_root/lib/rustlib" "$sysroot/lib/rustlib"
for tool in rustc rustdoc; do
    ln -s "$rust_root/bin/$tool" "$sysroot/bin/$tool"
done
quote_sh() { printf "'%s'" "${1//\'/\'\\\'\'}"; }
if [ "$target" = aarch64-unknown-redox ]; then
    {
        printf '#!/usr/bin/env bash\nexec '
        quote_sh "$rust_root/bin/cargo"
        printf ' --config '\''unstable.build-std=["std","panic_abort"]'\'' "$@"\n'
    } > "$sysroot/bin/cargo"
    chmod +x "$sysroot/bin/cargo"
else
    ln -s "$rust_root/bin/cargo" "$sysroot/bin/cargo"
fi
for tool in ar nm objcopy objdump strip; do
    ln -s "$llvm_bin/llvm-$tool" "$sysroot/bin/llvm-$tool"
    ln -s "llvm-$tool" "$sysroot/bin/$target-$tool"
done
ln -s llvm-ar "$sysroot/bin/llvm-ranlib"
ln -s "$llvm_bin/rust-lld" "$sysroot/bin/ld.lld"
ln -s ld.lld "$sysroot/bin/$target-ld"
# Use a config file so paths containing spaces stay single arguments.
quote_cfg() { printf '"%s"\n' "${1//\"/\\\"}"; }
{
    # Stock Clang has no Redox driver. Select its ELF/Linux driver for startup
    # and linker handling, then expose the Redox predefines to C/C++ sources.
    quote_cfg "--target=$driver"
    for macro in linux __linux __linux__ __gnu_linux__; do quote_cfg "-U$macro"; done
    quote_cfg "-D__redox__=1"
    quote_cfg "-Qunused-arguments"
    quote_cfg "--sysroot=$sysroot/$target"
    quote_cfg "--ld-path=$sysroot/bin/ld.lld"
    quote_cfg "--gcc-install-dir=$sysroot/lib/gcc/$target/$version"
} > "$sysroot/bin/$target.cfg"
# Shell quoting is separate from Clang config-file quoting.
for tool in clang clang++; do
    compiler="$clang_path"
    [ "$tool" = clang ] || compiler=$(command -v clang++)
    {
        printf '#!/usr/bin/env bash\n'
        printf 'redox_target='
        quote_sh "$target"
        printf '\n'
        cat <<'WRAPPER'
# redoxer supplies the Redox target too; keep it from overriding our ELF driver.
args=()
link_executable=yes
for arg in "$@"; do
    case "$arg" in
        --target="$redox_target") ;;
        --target=*-unknown-redox) echo "Compiler target mismatch: $arg (expected $redox_target)" >&2; exit 1 ;;
        -c|-S|-E|-shared|-static|-static-pie) link_executable=no; args+=("$arg") ;;
        *) args+=("$arg") ;;
    esac
done
WRAPPER
        printf 'if [ "$link_executable" = yes ]; then args+=(-Wl,--dynamic-linker=/usr/lib/%s -Wl,-T,' "$interpreter"
        quote_sh "$root/scripts/native/runtime.ld"
        printf '); fi\nexec '
        quote_sh "$compiler"
        printf ' --config='
        quote_sh "$sysroot/bin/$target.cfg"
        printf ' "${args[@]}"\n'
    } > "$sysroot/bin/$target-$tool"
    chmod +x "$sysroot/bin/$target-$tool"
done
# C/C++ commands selected by redoxer must also load the target config.
ln -s "$target-clang" "$sysroot/bin/clang"
ln -s "$target-clang++" "$sysroot/bin/clang++"
# ELF shared-library startup code comes from relibc, not the Darwin SDK.
mkdir -p "$sysroot/$target/usr"
ln -s ../include "$sysroot/$target/usr/include"
ln -s ../lib "$sysroot/$target/usr/lib"
echo "Native Clang/Rust toolchain assembled at $sysroot"
