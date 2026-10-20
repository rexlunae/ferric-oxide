#!/usr/bin/env bash
# Check the IOMMU lines of a boot-smoke.sh serial log.
#
# Usage: check-iommu-log.sh MODE LOG
#   MODE is how the boot ran:
#     none           no IOMMU in the machine: it stays off, the test driver skips, no DMA
#                    faults;
#     auto           emulated VT-d, kernel built with kernel.iommu = "auto";
#     enforce        emulated VT-d, kernel built with kernel.iommu = "enforce";
#     amdvi          emulated AMD-Vi (QEMU 10 or later), kernel.iommu = "auto";
#     amdvi-enforce  emulated AMD-Vi, kernel.iommu = "enforce".
#
# With either IOMMU, every PCI driver binds a DMA domain of its own when it connects to pcid
# (pcid_interface::pci_daemon), so the disk (NVMe at 00:04.0), the network card (e1000 at
# 00:05.0) and QEMU's AHCI controller (00:1f.2) must each be attached to a domain that their
# driver bound. ferric-iommu-test drives QEMU's edu device (00:10.0) and announces the DMA
# faults it causes; the kernel must log exactly those and no others. boot-smoke.sh places the
# devices at these addresses. It also maps a forked application's registered-buffer grant and
# sub-grant into its domain (MapGrant), and after the application revokes the grant, the same
# DMA to their IOVAs must fault: two of the five announced faults.
#
# AMD-Vi does not remap interrupts yet, so there pcid hands out compatibility-format MSI
# messages, and the DMA faults are checked by device and kind only: QEMU's AMD IOMMU leaves the
# address out of its I/O page fault events.
#
# With VT-d the unit also remaps interrupts: the I/O APIC's timer pin is checked through a
# remappable entry, the NVMe controller's MSI-X and edu's MSI go through entries pcid had the
# kernel make for them, and ferric-iommu-test forges an MSI naming the I/O APIC's entry, which
# must fault (source-id check) and be the only interrupt remapping fault.
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
    # Ungranted address, write after mprotect, revoked grant, revoked sub-grant, released IOVA.
    [ "$(wc -l <<< "$expected")" -eq 5 ] || fail "the DMA test did not announce its five faults"
    [ "$expected" = "$logged" ] || fail "the kernel's DMA faults differ from the ones the test caused"
}

# AMD-Vi: ferric-iommu-test announced its three faults, the kernel logged at least one I/O
# page fault from the edu device, and every DMA fault it logged is one. QEMU 10.2 writes its
# I/O page fault events without the address (and with its own flag bits), so they cannot be
# matched to the announced addresses the way VT-d's fault records are.
check_amd_faults() {
    local expected logged others
    expected="$(grep -ao 'FERRIC_IOMMU_EXPECT_FAULT [a-z]* 0x[0-9a-f]*' <<< "$clean" | sort -u)"
    logged="$(grep -ao 'IOMMU: DMA fault at unit 0x[0-9a-f]*: .*' <<< "$clean" || true)"
    echo "expected faults:"; echo "$expected"
    echo "logged faults:"; echo "$logged"
    [ "$(wc -l <<< "$expected")" -eq 5 ] || fail "the DMA test did not announce its five faults"
    local edu=': 00:10\.0 (read|write) at 0x[0-9a-f]+: I/O page fault'
    grep -qE "$edu" <<< "$logged" || fail "no I/O page fault from the edu device (00:10.0) was logged"
    others="$(grep -vE "$edu" <<< "$logged" || true)"
    [ -z "$others" ] || fail "DMA faults the test did not cause: $others"
}

# Every interrupt remapping fault the kernel logged must be the one ferric-iommu-test
# announced: its forged MSI naming the I/O APIC's entry, refused by the source-id check.
check_irq_faults() {
    local expected logged
    expected="$(grep -ao 'FERRIC_IOMMU_EXPECT_IRQ_FAULT [0-9]*' <<< "$clean" \
        | awk '{ print "00:10.0 interrupt index " $2 ": source-id check failed" }' | sort -u)"
    logged="$(grep -ao 'IOMMU: interrupt remapping fault at unit 0x[0-9a-f]*: .*' <<< "$clean" \
        | sed -E 's/^IOMMU: interrupt remapping fault at unit 0x[0-9a-f]*: ([0-9a-f:.]+ interrupt index [0-9]+: [^:]*):.*/\1/' \
        | sort -u || true)"
    echo "expected interrupt remapping faults:"; echo "$expected"
    echo "logged interrupt remapping faults:"; echo "$logged"
    [ "$(wc -l <<< "$expected")" -eq 1 ] && [ -n "$expected" ] \
        || fail "the interrupt test did not announce its fault"
    [ "$expected" = "$logged" ] \
        || fail "the kernel's interrupt remapping faults differ from the one the test caused"
}

# Every CI image has kernel.irq_percpu on: edu's MSI, sent to a CPU other than the BSP while
# the same vector number is held on the BSP, must be counted on that CPU only, with or without
# interrupt remapping.
need "FERRIC_TEST_PASS iommu-msi-percpu: vector 0x[0-9a-f]+ reached CPU [0-9]+, and the same vector held on CPU [0-9]+ counted nothing; low vector 0x[0-9a-f]+ reached CPU [0-9]+"
# Root moves edu's vector to another CPU while the device fires, and the descriptor follows it.
need "FERRIC_TEST_PASS iommu-msi-steer: vector 0x[0-9a-f]+ on CPU [0-9]+ moved to vector 0x[0-9a-f]+ on CPU [0-9]+ while the device fired"

case "$mode" in
    none)
        need "IOMMU: no DMAR or IVRS table, IOMMU not enabled"
        # Without an IOMMU pcid has no domain to give the test driver.
        for t in iommu-unbound iommu-domain iommu-dma iommu-fault iommu-readonly iommu-fork iommu-release \
            iommu-grant-nodomain iommu-grant-dma iommu-grant-unmap iommu-grant-revoke iommu-subgrant-revoke; do
            need "FERRIC_TEST_SKIP $t"
        done
        if grep -aF "IOMMU: DMA fault" <<< "$clean"; then
            fail "a DMA fault without an IOMMU"
        fi
        # pcid answers MsiMessage with the compatibility-format message, which still works.
        need "FERRIC_TEST_PASS iommu-msi: .*\(compatibility format, no interrupt remapping\)"
        need "FERRIC_TEST_SKIP iommu-irq-fault"
        need "FERRIC_TEST_PASS iommu-msi-steer: .*the test reprogrammed the device's message"
        if grep -aF "IOMMU: interrupt remapping" <<< "$clean"; then
            fail "interrupt remapping without an IOMMU"
        fi
        # Nothing faults, so there is nothing for iommu.fault to record.
        need "FERRIC_TEST_SKIP iommu-trace"
        if grep -aF "FERRIC_TEST_FAIL iommu-trace" <<< "$clean"; then
            fail "iommu-trace failed without an IOMMU"
        fi
        echo "::notice title=check-iommu-log ($mode)::OK; no IOMMU, compatibility-format MSI delivered"
        exit 0
        ;;
    auto)
        vendor=vtd
        need "IOMMU: VT-d unit at 0xfed90000 enabled \(identity domain\)"
        # Without a domain, translation answers with physical addresses.
        need "FERRIC_TEST_PASS iommu-unbound: translation without a domain gives a physical address"
        need "FERRIC_TEST_PASS iommu-fork: the forked child is unbound: translation gives a physical address"
        ;;
    enforce)
        vendor=vtd
        need "IOMMU: VT-d unit at 0xfed90000 enabled \(unattached devices blocked\)"
        need "IOMMU: enforce mode: only drivers with a bound domain get DMA addresses"
        need "FERRIC_TEST_PASS iommu-unbound: translation without a domain is refused \(EACCES\)"
        need "FERRIC_TEST_PASS iommu-fork: the forked child is unbound: translation refused \(EACCES\)"
        ;;
    amdvi)
        vendor=amdvi
        need "IOMMU: AMD-Vi unit at 0xfed80000 \(00:01\.0\) enabled \(identity domain\)"
        need "FERRIC_TEST_PASS iommu-unbound: translation without a domain gives a physical address"
        need "FERRIC_TEST_PASS iommu-fork: the forked child is unbound: translation gives a physical address"
        ;;
    amdvi-enforce)
        vendor=amdvi
        need "IOMMU: AMD-Vi unit at 0xfed80000 \(00:01\.0\) enabled \(unattached devices blocked\)"
        need "IOMMU: enforce mode: only drivers with a bound domain get DMA addresses"
        need "IOMMU: AMD-Vi interrupt remapping is not implemented yet"
        need "FERRIC_TEST_PASS iommu-unbound: translation without a domain is refused \(EACCES\)"
        need "FERRIC_TEST_PASS iommu-fork: the forked child is unbound: translation refused \(EACCES\)"
        ;;
    *)
        fail "unknown mode $mode (expected none, auto, enforce, amdvi or amdvi-enforce)"
        ;;
esac

# The boot self-test of DMA domains attaches an idle device: an empty slot or a host bridge,
# neither of which initiates DMA.
need "IOMMU: domain self-test passed \(attach/map/unmap/detach\)"

# Every CI image has kernel.trace (and so kernel.trace_iommu): the kernel's iommu.fault
# records must cover every fault ferric-iommu-test announced.
need "FERRIC_TEST_PASS iommu-trace: [0-9]+ announced fault\(s\) have iommu.fault records"

if [ "$vendor" = amdvi ]; then
    # QEMU's amd-iommu has IASup and an MSI capability, so the unit invalidates with
    # INVALIDATE_IOMMU_ALL and its event log raises an interrupt.
    need "IOMMU: unit 0xfed80000: 8192-entry event log \(MSI\), INVALIDATE_IOMMU_ALL invalidation"
    # No interrupt remapping: pcid hands out compatibility-format MSI, which the IOMMU passes
    # through, and the forged-MSI test skips.
    need "FERRIC_TEST_PASS iommu-msi: .*\(compatibility format, no interrupt remapping\)"
    need "FERRIC_TEST_SKIP iommu-irq-fault"
    need "FERRIC_TEST_PASS iommu-msi-steer: .*the test reprogrammed the device's message"
    if grep -aF "IOMMU: interrupt remapping" <<< "$clean"; then
        fail "interrupt remapping lines on AMD-Vi, which does not remap interrupts yet"
    fi
else
# QEMU's intel-iommu has ECAP.QI, so the unit must use the invalidation queue.
need "IOMMU: unit 0xfed90000 uses queued invalidation"

# Interrupt remapping: the table is set up and compatibility-format interrupts are blocked
# (CFI clear) under both auto and enforce.
need "IOMMU: interrupt remapping enabled at unit 0xfed90000: 4096 entries, .* destinations, compatibility-format interrupts blocked"
# The I/O APIC the DMAR scope names is remapped, and its timer pin delivers through entry 0.
need "IOMMU: I/O APIC [0-9]+ \(source-id [0-9a-f:.]+\) remapped by unit 0xfed90000"
need "IOMMU: I/O APIC [0-9]+ pin [0-9]+ \(ISA IRQ 0\) routed through interrupt remapping entry 0 for a check"
need "IOMMU: I/O APIC [0-9]+ pin [0-9]+ delivered [0-9]+ timer interrupt\(s\) through interrupt remapping entry 0"
# MSI-X (nvmed, which must work for the boot to reach the marker) and MSI (edu) go through
# entries made for their own requester IDs.
need "IOMMU: interrupt remapping entry [0-9]+ for 0000:00:04\.0: vector"
need "IOMMU: interrupt remapping entry [0-9]+ for 0000:00:10\.0: vector"
need "FERRIC_TEST_PASS iommu-msi: .*\(remappable format\)"
need "FERRIC_TEST_PASS iommu-irq-fault"
# Steering rewrote edu's remapping entry in place and logged the move.
need "FERRIC_TEST_PASS iommu-msi-steer: .*the kernel rewrote [1-9][0-9]* interrupt remapping entry"
need "IOMMU: [1-9][0-9]* interrupt remapping entr(y|ies) moved from vector 0x[0-9a-f]+ on CPU [0-9]+ to vector 0x[0-9a-f]+ on CPU [0-9]+"
if grep -aE "IOMMU: (interrupt remapping not enabled|enforce mode: unit .* cannot remap interrupts|cannot check I/O APIC)" <<< "$clean"; then
    fail "interrupt remapping was not set up"
fi
if grep -aE "pcid: no interrupt remapping entry" <<< "$clean"; then
    fail "pcid could not get an interrupt remapping entry"
fi
fi

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

# Registered-buffer grants in the domain (MapGrant): a forked application's two-page grant and
# its one-page sub-grant are mapped, the driver's own UnmapGrant works, and revoking the grant
# takes both IOMMU mappings down (IOTLB invalidated) before Revoke returns, so the device's
# DMA to their old IOVAs faults (two of the announced faults) and the requests fail with EIO.
need "FERRIC_TEST_PASS iommu-grant-nodomain"
need "IOMMU: domain [0-9]+: grant [0-9]+ mapped at IOVA 0x[0-9a-f]+ \(2 page\(s\), read-write\)"
need "IOMMU: domain [0-9]+: grant [0-9]+ mapped at IOVA 0x[0-9a-f]+ \(1 page\(s\), read-write\)"
need "FERRIC_TEST_PASS iommu-grant-dma"
need "IOMMU: domain [0-9]+: grant mapping at IOVA 0x[0-9a-f]+ unmapped by the driver"
need "FERRIC_TEST_PASS iommu-grant-unmap"
need "regbuf: grant [0-9]+ revoked with 1 grant\(s\) under it, unmapping 2 mapping\(s\)"
# Only the edu device's domain counts: ferric-blkgrant-test revokes grants in nvmed's too.
edu_did="$(grep -aoE "IOMMU: 0000:00:10\.0 attached to domain [0-9]+" <<< "$clean" \
    | head -n 1 | grep -oE '[0-9]+$')"
taken_down="$(grep -acE "IOMMU: domain $edu_did: grant mapping at IOVA 0x[0-9a-f]+ taken down with its grant, IOTLB invalidated" <<< "$clean" || true)"
[ "$taken_down" -eq 2 ] || fail "revoking the grant took down $taken_down IOMMU mapping(s), not 2"
need "FERRIC_TEST_PASS iommu-grant-revoke"
need "FERRIC_TEST_PASS iommu-subgrant-revoke"

if grep -aE "IOMMU: cannot (attach|create a domain|map|unmap|make)" <<< "$clean"; then
    fail "the iommu scheme refused a domain, an attach or a mapping"
fi
if grep -aE "pcid_interface: .*(cannot|DMA domain:)" <<< "$clean"; then
    fail "a driver could not use its DMA domain"
fi
if grep -aE "IOMMU: .* tried to bind domain" <<< "$clean"; then
    fail "a driver tried to bind an empty domain"
fi
if [ "$vendor" = amdvi ]; then
    check_amd_faults
else
    check_faults
    check_irq_faults
fi
if grep -aE "IOMMU: (VT-d unit .* skipped|AMD-Vi unit .* skipped|.*lost faults|domain self-test failed|unit .*: invalid|unit .*: device-TLB|unit .*: illegal|unit .*: hardware error)" <<< "$clean"; then
    fail "the IOMMU lost faults, failed its self-test, skipped a unit or rejected a command"
fi
# A notice annotation, so the result can be read without the job log.
bound="$(grep -aoE "IOMMU: domain [0-9]+ \([0-9]+ device\(s\)\) bound by [^ ]*" <<< "$clean" \
    | sed -E 's/^IOMMU: //' | paste -sd ';' -)"
if [ "$vendor" = amdvi ]; then
    faults="$(grep -acE 'IOMMU: DMA fault at unit 0x[0-9a-f]*: 00:10\.0' <<< "$clean" || true)"
    echo "::notice title=check-iommu-log ($mode)::OK; $bound; $faults I/O page fault line(s) from 00:10.0; compatibility-format MSI (no interrupt remapping on AMD-Vi yet)"
    echo "check-iommu-log: $mode OK"
    exit 0
fi
irtes="$(grep -aoE "IOMMU: interrupt remapping entry [0-9]+ for [0-9a-f:.]+: vector 0x[0-9a-f]+" <<< "$clean" \
    | sed -E 's/^IOMMU: interrupt remapping entry ([0-9]+) for 0000:([0-9a-f:.]+): vector (0x[0-9a-f]+)/\2 entry \1 vector \3/' \
    | paste -sd ',' -)"
allocated="$(grep -aoE "[0-9]+ IRTE\(s\) allocated" <<< "$clean" | tail -n 1)"
echo "::notice title=check-iommu-log ($mode)::OK; $bound; interrupt remapping: $allocated at most recent allocation ($irtes); I/O APIC timer pin and forged-MSI fault checked"
echo "check-iommu-log: $mode OK"
