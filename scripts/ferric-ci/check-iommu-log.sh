#!/usr/bin/env bash
# Check the IOMMU lines of a boot-smoke.sh serial log.
#
# Usage: check-iommu-log.sh MODE LOG
#   MODE is how the boot ran:
#     none     no IOMMU in the machine: VT-d stays off, the test driver skips, no DMA faults;
#     auto     emulated VT-d, kernel built with kernel.iommu = "auto";
#     enforce  emulated VT-d, kernel built with kernel.iommu = "enforce".
#
# With VT-d, every PCI driver binds a DMA domain of its own when it connects to pcid
# (pcid_interface::pci_daemon), so the disk (NVMe at 00:04.0), the network card (e1000 at
# 00:05.0) and QEMU's AHCI controller (00:1f.2) must each be attached to a domain that their
# driver bound. ferric-iommu-test drives QEMU's edu device (00:10.0) and announces the DMA
# faults it causes; the kernel must log exactly those and no others. boot-smoke.sh places the
# devices at these addresses.
set -euo pipefail

mode="$1"
log="$2"
clean="$(tr -d '\r' < "$log")"

fail() {
    echo "::error::$*" >&2
    exit 1
}

# Lines that must be in the log.
need() {
    grep -aE "$1" <<< "$clean" || fail "missing from the log: $1"
}

# The PCI drivers whose devices must DMA through their own domain: address and driver.
drivers=(
    "00:04.0 nvmed"
    "00:05.0 e1000d"
    "00:1f.2 ahcid"
)

# Checks that `driver` bound the domain the kernel attached the device at `addr` to.
check_driver_domain() {
    local addr="$1" driver="$2" did
    did="$(grep -aoE "IOMMU: 0000:${addr//./\\.} attached to domain [0-9]+" <<< "$clean" \
        | head -n 1 | grep -oE '[0-9]+$')" || fail "$addr ($driver) was not attached to a domain"
    need "IOMMU: domain $did \(1 device\(s\)\) bound by [^ ]*$driver"
}

# Every DMA fault the kernel logged must be one ferric-iommu-test announced.
check_faults() {
    local expected logged
    expected="$(grep -ao 'FERRIC_IOMMU_EXPECT_FAULT [a-z]* 0x[0-9a-f]*' <<< "$clean" \
        | awk '{ print "00:10.0 " $2 " at " $3 ":" }' | sort -u)"
    logged="$(grep -ao 'IOMMU: DMA fault at unit 0x[0-9a-f]*: .*' <<< "$clean" \
        | sed -E 's/^IOMMU: DMA fault at unit 0x[0-9a-f]*: ([0-9a-f:.]+ [a-z]+ at 0x[0-9a-f]+:).*/\1/' \
        | sort -u || true)"
    echo "expected faults:"; echo "$expected"
    echo "logged faults:"; echo "$logged"
    # Ungranted address, write after mprotect, released IOVA.
    [ "$(wc -l <<< "$expected")" -eq 3 ] || fail "the DMA test did not announce its three faults"
    [ "$expected" = "$logged" ] || fail "the kernel's DMA faults differ from the ones the test caused"
}

case "$mode" in
    none)
        need "IOMMU: no DMAR table, VT-d not enabled"
        # Without an IOMMU pcid has no domain to give the test driver.
        for t in iommu-unbound iommu-domain iommu-dma iommu-fault iommu-readonly iommu-fork iommu-release; do
            need "FERRIC_TEST_SKIP $t"
        done
        if grep -aF "IOMMU: DMA fault" <<< "$clean"; then
            fail "a DMA fault without an IOMMU"
        fi
        exit 0
        ;;
    auto)
        need "IOMMU: VT-d unit at 0xfed90000 enabled \(identity domain\)"
        # Without a domain, translation answers with physical addresses.
        need "FERRIC_TEST_PASS iommu-unbound: translation without a domain gives a physical address"
        need "FERRIC_TEST_PASS iommu-fork: the forked child is unbound: translation gives a physical address"
        ;;
    enforce)
        need "IOMMU: VT-d unit at 0xfed90000 enabled \(unattached devices blocked\)"
        need "IOMMU: enforce mode: only drivers with a bound domain get DMA addresses"
        need "FERRIC_TEST_PASS iommu-unbound: translation without a domain is refused \(EACCES\)"
        need "FERRIC_TEST_PASS iommu-fork: the forked child is unbound: translation refused \(EACCES\)"
        ;;
    *)
        fail "unknown mode $mode (expected none, auto or enforce)"
        ;;
esac

# QEMU's intel-iommu has ECAP.QI, so the unit must use the invalidation queue.
need "IOMMU: unit 0xfed90000 uses queued invalidation"
# The boot self-test of DMA domains attaches QEMU's host bridge, which a DRHD scope lists and
# which never initiates DMA.
need "IOMMU: domain self-test passed \(attach/map/unmap/detach\)"

echo "DMA domains bound:"
grep -aoE "IOMMU: domain [0-9]+ \([0-9]+ device\(s\)\) bound by .*" <<< "$clean" || true
for entry in "${drivers[@]}"; do
    # shellcheck disable=SC2086 # two words: address and driver
    check_driver_domain $entry
done

# ferric-iommu-test asks pcid for a DMA domain itself, so pcid creates one and attaches the
# edu device; the driver binds it, and its exit closes the domain.
check_driver_domain 00:10.0 ferric-iommu-test
need "IOMMU: domain [0-9]+ closed, 1 device\(s\) blocked"
need "FERRIC_TEST_PASS iommu-domain"
# With the domain bound, memory:translation returns IOVAs, and the device copies a buffer
# through them.
need "FERRIC_TEST_PASS iommu-dma"
# Fault injection: the device writes an address the driver never translated in its domain.
need "FERRIC_TEST_PASS iommu-fault"
# mprotect taking write access away makes the device mapping read-only (IOTLB invalidated).
need "IOMMU: domain [0-9]+: 1 DMA mapping\(s\) made read-only after write access was removed"
need "FERRIC_TEST_PASS iommu-readonly"
need "FERRIC_TEST_PASS iommu-fork"
# Unmapping the buffer takes its IOVAs down (release hook, strict unmap), so the device's read
# of the old IOVA faults too.
need "IOMMU: domain [0-9]+: 1 DMA mapping\(s\) released after an unmap"
need "FERRIC_TEST_PASS iommu-release"

if grep -aE "IOMMU: cannot (attach|create a domain|map|unmap|make)" <<< "$clean"; then
    fail "the iommu scheme refused a domain, an attach or a mapping"
fi
if grep -aE "pcid_interface: .*(cannot|DMA domain:)" <<< "$clean"; then
    fail "a driver could not use its DMA domain"
fi
if grep -aE "IOMMU: .* tried to bind domain" <<< "$clean"; then
    fail "a driver tried to bind an empty domain"
fi
check_faults
if grep -aE "IOMMU: (VT-d unit .* skipped|.*lost faults|domain self-test failed|unit .*: invalid|unit .*: device-TLB)" <<< "$clean"; then
    fail "the IOMMU lost faults, failed its self-test or skipped a unit"
fi
# A notice annotation, so the result can be read without the job log.
bound="$(grep -aoE "IOMMU: domain [0-9]+ \([0-9]+ device\(s\)\) bound by [^ ]*" <<< "$clean" \
    | sed -E 's/^IOMMU: //' | paste -sd ';' -)"
echo "::notice title=check-iommu-log ($mode)::OK; $bound"
echo "check-iommu-log: $mode OK"
