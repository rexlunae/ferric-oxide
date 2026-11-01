#!/usr/bin/env bash
# Free unused hosted-runner SDK storage through explicit container bind mounts.
# The image build uses its container toolchain and workspace prefix, not these
# host SDKs. See jobs.<job_id>.container.volumes in the GitHub Actions reference.
#
# Usage: prepare-image-runner.sh [SDK_MOUNT_ROOT]
# The optional root supports disposable fixture tests; every child must still
# be a real mount point. Production uses /ferric-host-sdks.
set -euo pipefail

fail() {
    echo "prepare-image-runner: $*" >&2
    exit 1
}

[ "$#" -le 1 ] || fail "usage: prepare-image-runner.sh [SDK_MOUNT_ROOT]"
if [ "${GITHUB_ACTIONS:-}" != true ] ||
   [ "${RUNNER_ENVIRONMENT:-}" != github-hosted ] ||
   [ "${RUNNER_OS:-}" != Linux ]; then
    fail "cleanup requires a GitHub-hosted Linux Actions runner"
fi

sdk_root="${1:-/ferric-host-sdks}"
[[ "$sdk_root" = /* && -d "$sdk_root" && ! -L "$sdk_root" ]] ||
    fail "SDK mount root must be an absolute directory, not a symlink"

sdks=(android dotnet ghc swift toolcache)
paths=()
# Validate every binding before deleting anything: a missing binding must never
# turn this into cleanup of a similarly named container or developer directory.
for sdk in "${sdks[@]}"; do
    path="$sdk_root/$sdk"
    [ -d "$path" ] && [ ! -L "$path" ] && mountpoint -q "$path" ||
        fail "expected an SDK bind mount: $path"
    paths+=("$path")
done

echo "prepare-image-runner: disk before SDK cleanup"
df -h "${GITHUB_WORKSPACE:-$sdk_root}"
du -sh "${paths[@]}"
for path in "${paths[@]}"; do
    # -P does not follow SDK symlinks; -xdev does not descend into nested
    # filesystems. Leave the bind-mount root itself in place.
    find -P "$path" -xdev -mindepth 1 -delete
done
echo "prepare-image-runner: disk after SDK cleanup"
df -h "${GITHUB_WORKSPACE:-$sdk_root}"
