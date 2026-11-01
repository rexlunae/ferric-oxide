#!/usr/bin/env bash
# Exercise cleanup against disposable directories, never the real SDK mounts.
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/prepare-image-runner.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
sdk_root="$work/sdk root"
test_bin="$work/bin"
mkdir -p "$test_bin" "$work/outside"
echo preserved > "$work/outside/marker"

# Only mount discovery is mocked. The actual find/delete implementation runs
# on the disposable fixture, including nested/hidden files and symlinks.
cat > "$test_bin/mountpoint" <<'SH'
#!/usr/bin/env bash
[ "$#" -eq 2 ] && [ "$1" = -q ] || exit 2
[ "$2" != "${SDK_TEST_MISSING:-}" ]
SH
chmod +x "$test_bin/mountpoint"
export PATH="$test_bin:$PATH"

fixture() {
    rm -rf "$sdk_root"
    mkdir -p "$sdk_root"
    for sdk in android dotnet ghc swift toolcache; do
        mkdir -p "$sdk_root/$sdk/nested"
        echo data > "$sdk_root/$sdk/nested/object"
        echo data > "$sdk_root/$sdk/.hidden"
        ln -s "$work/outside" "$sdk_root/$sdk/external"
    done
}

fail() {
    echo "FAIL: $*" >&2
    cat "$work/output" >&2
    exit 1
}

refuse() {
    local name="$1"
    shift
    fixture
    if env GITHUB_ACTIONS=true RUNNER_ENVIRONMENT=github-hosted RUNNER_OS=Linux \
        "$@" bash "$script" "$sdk_root" > "$work/output" 2>&1; then
        fail "$name accepted"
    fi
    for sdk in android dotnet ghc swift toolcache; do
        [ -f "$sdk_root/$sdk/nested/object" ] || fail "$name deleted before validation"
    done
    echo "PASS: $name refused without deletion"
}

refuse "outside Actions" GITHUB_ACTIONS=false
refuse "self-hosted runner" RUNNER_ENVIRONMENT=self-hosted
refuse "non-Linux runner" RUNNER_OS=Windows
refuse "missing bind mount" SDK_TEST_MISSING="$sdk_root/ghc"

fixture
rm -rf "$sdk_root/ghc"
ln -s "$work/outside" "$sdk_root/ghc"
if env GITHUB_ACTIONS=true RUNNER_ENVIRONMENT=github-hosted RUNNER_OS=Linux \
    bash "$script" "$sdk_root" > "$work/output" 2>&1; then
    fail "symlink mount accepted"
fi
[ -f "$sdk_root/android/nested/object" ] || fail "symlink case partly deleted SDKs"
[ -f "$work/outside/marker" ] || fail "symlink target deleted"
echo "PASS: symlink mount refused without deletion"

fixture
env GITHUB_ACTIONS=true RUNNER_ENVIRONMENT=github-hosted RUNNER_OS=Linux \
    bash "$script" "$sdk_root" > "$work/output" 2>&1
for sdk in android dotnet ghc swift toolcache; do
    [ -d "$sdk_root/$sdk" ] || fail "mount root removed"
    [ -z "$(find "$sdk_root/$sdk" -mindepth 1 -print -quit)" ] || fail "SDK contents retained"
done
[ -f "$work/outside/marker" ] || fail "symlink followed outside SDK"
grep -q "disk before" "$work/output" || fail "before capacity absent"
grep -q "disk after" "$work/output" || fail "after capacity absent"
echo "PASS: SDK contents removed, roots and symlink targets preserved"
