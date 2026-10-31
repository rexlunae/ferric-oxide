#!/usr/bin/env bash
# Checks bin/<target>-llvm-config, the wrapper that answers a target recipe's
# llvm-config questions (Mesa's) with a host llvm-config and rewrites its paths
# to the recipe's target sysroot. Fake llvm-config programs stand in for both
# places a host llvm-config can come from: the recipe's toolchain (a host build
# of the llvm21 recipe) and the host sysroot (the upstream prefix toolchain).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
wrapper="$root/bin/x86_64-unknown-redox-llvm-config"
temp=$(mktemp -d)
trap 'rm -rf "$temp"' EXIT

fail() {
    echo "llvm-config wrapper: $*" >&2
    exit 1
}

# A fake llvm-config that reports paths under its own prefix, lists libraries
# for two backends, and records the arguments it was given.
fake_llvm_config() {
    mkdir -p "$(dirname "$1")"
    cat > "$1" <<'STUB'
#!/usr/bin/env bash
prefix=$(cd "$(dirname "$0")/.." && pwd)
printf '%s\n' "$@" > "$prefix/args"
case "$1" in
    --cppflags) echo "-I$prefix/include -D__STDC_LIMIT_MACROS" ;;
    --libdir) echo "$prefix/lib" ;;
    --libs) echo "-lLLVMCore -lLLVMX86CodeGen -lLLVMAArch64CodeGen" ;;
    --components) echo "core x86codegen aarch64codegen" ;;
    --version) echo "21.1.2" ;;
esac
STUB
    chmod +x "$1"
}

host_sysroot="$temp/host-sysroot"
toolchain="$temp/recipe/toolchain"
sysroot="$temp/recipe/sysroot"
fake_llvm_config "$host_sysroot/bin/llvm-config"
mkdir -p "$sysroot/usr"

run() {
    env COOKBOOK_HOST_SYSROOT="$host_sysroot" COOKBOOK_SYSROOT="$sysroot" "$@"
}

# Without a host build of llvm21 in the recipe's toolchain, the host sysroot's
# llvm-config answers, and its paths become the target sysroot's.
out=$(run TARGET=x86_64-unknown-redox python3 "$wrapper" --cppflags)
[ "$out" = "-I$sysroot/include -D__STDC_LIMIT_MACROS" ] || fail "host sysroot paths not rewritten: $out"

# With one, the toolchain's llvm-config answers. A toolchain lays packages out
# under usr/, like the target sysroot, so usr/ maps onto usr/.
fake_llvm_config "$toolchain/usr/bin/llvm-config"
out=$(run COOKBOOK_TOOLCHAIN="$toolchain" TARGET=x86_64-unknown-redox python3 "$wrapper" --cppflags)
[ "$out" = "-I$sysroot/usr/include -D__STDC_LIMIT_MACROS" ] || fail "toolchain paths not rewritten: $out"
out=$(run COOKBOOK_TOOLCHAIN="$toolchain" TARGET=x86_64-unknown-redox python3 "$wrapper" --bindir)
[ "$out" = "$toolchain/usr/bin" ] || fail "toolchain bindir: $out"
[ ! -e "$host_sysroot/args" ] || [ "$(cat "$host_sysroot/args")" = "--cppflags" ] \
    || fail "the host sysroot's llvm-config ran when the toolchain had one"

# The libraries and components of other backends are filtered out for the
# target's architecture, in both directions.
out=$(run COOKBOOK_TOOLCHAIN="$toolchain" TARGET=x86_64-unknown-redox python3 "$wrapper" --libs core)
[ "$out" = "-lLLVMCore -lLLVMX86CodeGen" ] || fail "x86_64 libs: $out"
out=$(run COOKBOOK_TOOLCHAIN="$toolchain" TARGET=aarch64-unknown-redox python3 "$wrapper" --libs core)
[ "$out" = "-lLLVMCore -lLLVMAArch64CodeGen" ] || fail "aarch64 libs: $out"
out=$(run COOKBOOK_TOOLCHAIN="$toolchain" TARGET=aarch64-unknown-redox python3 "$wrapper" --components)
[ "$out" = "core aarch64codegen" ] || fail "aarch64 components: $out"

# "native" means the host's backend to a host llvm-config; the wrapper asks for
# the target's instead, so a cross build links the target's backend.
run COOKBOOK_TOOLCHAIN="$toolchain" TARGET=aarch64-unknown-redox python3 "$wrapper" --libs core native nativecodegen > /dev/null
[ "$(tr '\n' ' ' < "$toolchain/usr/args")" = "--libs core aarch64 aarch64codegen " ] \
    || fail "native was passed through: $(tr '\n' ' ' < "$toolchain/usr/args")"
run COOKBOOK_TOOLCHAIN="$toolchain" TARGET=x86_64-unknown-redox python3 "$wrapper" --libs native > /dev/null
[ "$(tr '\n' ' ' < "$toolchain/usr/args")" = "--libs x86 " ] || fail "x86_64 native: $(cat "$toolchain/usr/args")"

echo "llvm-config wrapper tests passed"
