# Shared by the boot checks that gate on a same-boot performance A/B (check-blkgrant-log.sh,
# check-iobench-log.sh). Source it; it defines perf_ab_failed.
#
# PERF_AB says what a failed A/B does:
#   enforce (the default) fails the check with an ::error annotation;
#   report  prints an ::warning annotation and lets the check go on.
# image.yml sets PERF_AB=report for the whole boot job of the boot with x2APIC hidden
# (`host,-x2apic`), which is there to check xAPIC correctness: its EOIs and IPIs are xAPIC
# MMIO accesses that exit to the host under the runners' nested KVM, so a per-CPU policy
# that takes more of them measures the APIC emulation more than the driver.

# perf_ab_failed TITLE MESSAGE: a same-boot A/B failed. Fails (exit 1) unless PERF_AB=report.
perf_ab_failed() {
    case "${PERF_AB:-enforce}" in
        report)
            echo "::warning title=$1::$2 (reported only: PERF_AB=report)"
            ;;
        enforce)
            echo "::error title=$1::$2" >&2
            exit 1
            ;;
        *)
            echo "::error title=$1::PERF_AB must be enforce or report, not '${PERF_AB}'" >&2
            exit 1
            ;;
    esac
}
