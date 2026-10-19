#!/usr/bin/env bash
# Check ferric-blkgrant-test's lines in a boot-smoke.sh serial log: storage DMA into
# registered buffers (drivers.grant_dma, which the CI image turns on).
#
# Usage: check-blkgrant-log.sh MODE LOG
#   MODE is how the boot ran, as for check-iommu-log.sh: none (no IOMMU), auto, enforce,
#   amdvi or amdvi-enforce.
#
# With an IOMMU, nvmed (the NVMe controller at 00:04.0) must map the test's grants into the
# DMA domain it bound (a 4 MiB read-write grant on two partition descriptors and a one-page
# read-only one), the test must pass every step, the driver's own unmap of the read-only
# grant and the kernel's take-down of the revoked one must be logged, and detaching the
# revoked grant must give its reserved IOVA back. Without one, nvmed has no DMA domain, so the
# grant steps skip. Either way the benchmark lines must be there; a notice annotation repeats
# them, so the numbers can be read without the job log.
set -euo pipefail

mode="$1"
log="$2"
clean="$(tr -d '\r' < "$log")"

fail() {
    echo "::error::$*" >&2
    exit 1
}

need() {
    grep -aE "$1" <<< "$clean" || fail "missing from the log: $1"
}

if grep -aE "FERRIC_TEST_FAIL nvme-grant" <<< "$clean"; then
    fail "ferric-blkgrant-test failed"
fi

# nvmed's own lines (its setup and any error) as one notice, so a failed check below can be
# read without the job log.
nvmed_lines="$(grep -aE 'nvmed: ' <<< "$clean" | head -n 20 || true)"
if [ -n "$nvmed_lines" ]; then
    echo "::notice title=nvmed lines::$(sed 's/%/%25/g' <<< "$nvmed_lines" | sed ':a;N;$!ba;s/\n/%0A/g')"
fi

# drivers.nvme_percore is on in every CI image: nvmed spreads its I/O completion queues' MSI-X
# entries over the CPUs in two batches, checks them against irq:cpus, and serves each pair from
# a worker thread on its CPU.
need "nvmed: MSI-X entries 1\.\.=[0-9]+ spread over CPUs \[[0-9, ]+\] in batches of [0-9]+ and [0-9]+, as irq:cpus and ferric_irq::spread give"
need "nvmed: [2-9][0-9]* per-CPU I/O queue pairs \(controller grants [0-9]+ pairs\), workers on CPUs \[[0-9, ]+\]"
# A watchdog line means a job's completion or report was not delivered by its event, and was
# only picked up by the once-a-second tick.
if grep -aE "nvmed: (MSI-X spread check failed|queue pair [0-9]+ worker not pinned|no worker for I/O queue pair|watchdog)" <<< "$clean"; then
    fail "nvmed's per-CPU queue pairs were not set up as expected"
fi

# 4 KiB preads from 1, 2 and 4 threads at once, in every mode (per-core NVMe queues), as a
# same-boot A/B of nvmed's queue policies. The default policy must reach the single queue's
# IOPS within the rounds' noise (ferric_blkqueue::ab); the numbers go out as a notice first.
for threads in 1 2 4; do
    need "FERRIC_BENCH nvme-read-4k-t$threads .*copy_iops=.* ab=(pass|fail)"
done
if grep -aE "FERRIC_BENCH nvme-read-4k-t[0-9]+ .* ab=fail" <<< "$clean"; then
    ab="$(grep -aoE "FERRIC_BENCH nvme-read-4k-t[0-9]+ .*" <<< "$clean" | sed 's/^FERRIC_BENCH //' | paste -sd ';' -)"
    echo "::notice title=queue policy A/B ($mode)::$ab"
    fail "nvmed's default queue policy is slower than its single queue in the same boot"
fi

case "$mode" in
    none)
        need "FERRIC_TEST_SKIP nvme-grant: nvmed has no DMA domain \(no IOMMU\)"
        need "FERRIC_BENCH nvme-read-128k .*copy_mib_s="
        need "FERRIC_BENCH nvme-read-4k .*copy_iops="
        ;;
    auto | enforce | amdvi | amdvi-enforce)
        for step in attach read write checks bench revoke detach; do
            need "FERRIC_TEST_PASS nvme-grant-$step"
        done
        did="$(grep -aoE "IOMMU: 0000:00:04\.0 attached to domain [0-9]+" <<< "$clean" \
            | head -n 1 | grep -oE '[0-9]+$')" || fail "00:04.0 (nvmed) was not attached to a domain"
        need "IOMMU: domain $did \(1 device\(s\)\) bound by [^ ]*nvmed"
        mapped="$(grep -acE "IOMMU: domain $did: grant [0-9]+ mapped at IOVA 0x[0-9a-f]+ \(1024 page\(s\), read-write\)" <<< "$clean" || true)"
        [ "$mapped" -eq 2 ] || fail "nvmed mapped $mapped 4 MiB read-write grant(s), not 2"
        need "IOMMU: domain $did: grant [0-9]+ mapped at IOVA 0x[0-9a-f]+ \(1 page\(s\), read-only\)"
        need "IOMMU: domain $did: grant mapping at IOVA 0x[0-9a-f]+ unmapped by the driver"
        need "IOMMU: domain $did: grant mapping at IOVA 0x[0-9a-f]+ taken down with its grant, IOTLB invalidated"
        need "IOMMU: domain $did: IOVA 0x[0-9a-f]+ of a revoked grant released by the driver"
        need "FERRIC_BENCH nvme-read-128k .*grant_mib_s="
        need "FERRIC_BENCH nvme-read-4k .*grant_iops="
        ;;
    *)
        fail "unknown mode $mode (expected none, auto, enforce, amdvi or amdvi-enforce)"
        ;;
esac

bench="$(grep -aoE "FERRIC_BENCH nvme-read-[0-9a-z-]+ .*" <<< "$clean" | sed 's/^FERRIC_BENCH //' | paste -sd ';' -)"
echo "::notice title=check-blkgrant-log ($mode)::OK; $bench"
echo "check-blkgrant-log: $mode OK"
