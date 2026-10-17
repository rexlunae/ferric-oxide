#!/usr/bin/env bash
# Point one recipe at a specific git checkout for this build.
#
# Usage: use-local-source.sh RECIPE GIT_URL [REF]
#
# Clones GIT_URL (with submodules) into the recipe's source directory and marks
# the recipe "local" in the CI filesystem config, so the cookbook builds that
# tree as-is instead of fetching the recipe's own source.
set -euo pipefail

recipe="$1"
url="$2"
ref="${3:-}"
config="config/${CONFIG_NAME:-ferric-smoke}.toml"

dir="$(dirname "$(find recipes -path "*/$recipe/recipe.toml" -print -quit)")"
if [ -z "$dir" ] || [ "$dir" = "." ]; then
    echo "use-local-source: no recipe named $recipe" >&2
    exit 1
fi

rm -rf "$dir/source"
git clone --recurse-submodules "$url" "$dir/source"
if [ -n "$ref" ]; then
    git -C "$dir/source" fetch origin "$ref"
    git -C "$dir/source" checkout --recurse-submodules FETCH_HEAD
fi
git -C "$dir/source" log -1 --oneline

if grep -q "^$recipe = " "$config"; then
    sed -i "s/^$recipe = .*/$recipe = \"local\"/" "$config"
else
    sed -i "s/^\[packages\]$/[packages]\n$recipe = \"local\"/" "$config"
fi
grep -n "^$recipe = " "$config"
