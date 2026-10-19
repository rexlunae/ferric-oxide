#!/usr/bin/env bash
# Boot a fresh ferric-cet-image on a CET-capable Linux/KVM host (including Kleya).
# Usage: boot-cet.sh IMAGE [LOG]
# The smoke harness uses a disposable copy. A CET skip is a failure in this hardware test.
set -euo pipefail

image="${1:?usage: $0 IMAGE [LOG]}"
log="${2:-serial-cet-kleya.log}"
if [ ! -w /dev/kvm ]; then
    echo "boot-cet: writable /dev/kvm is required for CET hardware validation" >&2
    exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# Kleya's QEMU hides SHSTK with plain -cpu host. Disabling migratability exposes the host bits.
bash "$script_dir/boot-smoke.sh" "$image" "$log" host,migratable=off
grep -aF 'CET: shadow stacks supported=true policy=auto decision=on' "$log"
grep -aF 'CET: shadow-stack pool isolation self-test passed' "$log"
grep -aF 'CET: shadow stacks enabled, self-test passed' "$log"
grep -aF 'FERRIC_TEST_PASS fuzz ' "$log"
echo "boot-cet: PASS, CET enabled, pool isolation and syscall fuzzing passed"
