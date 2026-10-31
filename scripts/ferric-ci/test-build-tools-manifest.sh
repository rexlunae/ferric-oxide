#!/usr/bin/env bash
# Exercise the real manifest rule when recipe placement changes, without a cook.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/tools/installer/src" "$work/tools/cookbook/src" "$work/tools/redoxer/src" \
    "$work/tools/fox/src" "$work/crates/fox-core" "$work/crates/fox-store" "$work/recipes/tool" "$work/mk"
ln -s "$root/mk/fstools.mk" "$work/mk/fstools.mk"
touch "$work/Cargo.toml" "$work/Cargo.lock" "$work/filesystem.toml" "$work/ready"
cat > "$work/repo" <<'REPO'
#!/usr/bin/env bash
set -euo pipefail
test "$1" = build-tools-list
if ! grep -q '"host"' recipes/tool/recipe.toml; then echo tool; fi
echo queried >> queries
REPO
chmod +x "$work/repo"
cat > "$work/check.mk" <<'MAKE'
.PHONY: FORCE
FORCE:
after: $(GUEST_TOOL_CONFIG) $(GUEST_TOOL_MANIFEST)
	@echo rebuilt >> rebuilds
	@touch $@
MAKE
run() {
    (cd "$work" && make --no-print-directory -f "$root/mk/fstools.mk" -f check.mk after \
        ROOT="$root" BUILD=image FSTOOLS_TAG=ready FSTOOLS=installer \
        FILESYSTEM_CONFIG=filesystem.toml REPO_BIN=./repo HOST_CARGO=false \
        ARCH=x86_64 HOST_ARCH=x86_64 FERRIC_BUILD_TOOLS=guest FERRIC_GUEST_BUILD_ENV=1 \
        FSTOOLS_IN_PODMAN=0 PODMAN_BUILD=0)
}
printf '[build]\ntool-runs-on = "host"\n' > "$work/recipes/tool/recipe.toml"
run
test ! -s "$work/image/build-tools/manifest.txt"
test "$(wc -l < "$work/rebuilds")" -eq 1
run
test "$(wc -l < "$work/rebuilds")" -eq 1
# Neither the config nor cookbook stamp changes; only a recipe is edited.
printf '[build]\ntool-runs-on = "configured"\n' > "$work/recipes/tool/recipe.toml"
run
test "$(cat "$work/image/build-tools/manifest.txt")" = tool
test "$(wc -l < "$work/rebuilds")" -eq 2
run
test "$(wc -l < "$work/rebuilds")" -eq 2
printf '[build]\ntool-runs-on = "host"\n' > "$work/recipes/tool/recipe.toml"
run
test ! -s "$work/image/build-tools/manifest.txt"
test "$(wc -l < "$work/rebuilds")" -eq 3
test "$(wc -l < "$work/queries")" -eq 5
printf 'Build-tool manifest regression tests passed\n'
