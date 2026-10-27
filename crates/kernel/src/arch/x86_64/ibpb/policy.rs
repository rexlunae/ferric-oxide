//! Ferric Oxide: when to issue an indirect branch prediction barrier (IBPB), as pure functions.
//!
//! Support comes from CPUID (Intel SDM Vol. 2A, CPUID; AMD "Software Techniques for Managing
//! Speculation on AMD Processors"): Intel reports `IBRS_IBPB` in CPUID.(EAX=7,ECX=0):EDX\[26\],
//! AMD reports `IBPB` in CPUID.80000008h:EBX\[12\]. Either one means a write of bit 0 to
//! `IA32_PRED_CMD` (MSR 0x49) is an IBPB.
//!
//! The barrier is needed when a CPU is about to run user code of a different address space than
//! the last user code it ran, so branch predictions one process trained cannot steer another
//! (Spectre v2 user-to-user, CVE-2017-5715). Kernel and idle contexts have no address space of
//! their own: switching to one runs no user code and keeps the record of the last user address
//! space, so a process that is switched out for the idle loop and back pays nothing.
//!
//! This file has no dependencies so the host tests (`crates/kernel/tests`) can include it by
//! path.

/// `IBRS_IBPB` in CPUID.(EAX=7,ECX=0):EDX (Intel, and AMD CPUs that mirror it).
pub const LEAF7_EDX_IBRS_IBPB: u32 = 1 << 26;

/// `IBPB` in CPUID.80000008h:EBX (AMD).
pub const EXT8_EBX_IBPB: u32 = 1 << 12;

/// The CPUID values that decide IBPB support.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cpuid {
    /// CPUID.0:EAX, the highest basic leaf.
    pub max_basic_leaf: u32,
    /// CPUID.(EAX=7,ECX=0):EDX, read only when `max_basic_leaf >= 7`.
    pub leaf7_edx: u32,
    /// CPUID.80000000h:EAX, the highest extended leaf.
    pub max_extended_leaf: u32,
    /// CPUID.80000008h:EBX, read only when `max_extended_leaf >= 0x8000_0008`.
    pub ext8_ebx: u32,
}

impl Cpuid {
    /// Whether the CPU implements IBPB through `IA32_PRED_CMD`. Register values from leaves the
    /// CPU does not report are ignored, since such leaves return data of some other leaf.
    pub const fn supports_ibpb(&self) -> bool {
        let intel = self.max_basic_leaf >= 7 && self.leaf7_edx & LEAF7_EDX_IBRS_IBPB != 0;
        let amd = self.max_extended_leaf >= 0x8000_0008 && self.ext8_ebx & EXT8_EBX_IBPB != 0;
        intel || amd
    }
}

/// Whether switching to `next` needs a barrier, given the last user address space this CPU ran
/// (`last_user`). `None` for `next` is a kernel or idle context; `None` for `last_user` means the
/// CPU has run no user code since boot. Identities compare with `==`, so they must not be
/// reused while the caller still holds `last_user` (the kernel keeps a weak reference to the
/// address space, which keeps its address from being handed out again).
pub fn needs_barrier<Id: PartialEq>(last_user: Option<Id>, next: Option<Id>) -> bool {
    match (last_user, next) {
        (_, None) => false,
        (Some(last), Some(next)) => last != next,
        (None, Some(_)) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_to_kernel_or_idle_needs_no_barrier() {
        assert!(!needs_barrier::<u32>(None, None));
        assert!(!needs_barrier(Some(1), None));
    }

    #[test]
    fn same_address_space_needs_no_barrier() {
        assert!(!needs_barrier(Some(1), Some(1)));
    }

    #[test]
    fn different_address_space_needs_a_barrier() {
        assert!(needs_barrier(Some(1), Some(2)));
    }

    #[test]
    fn first_user_address_space_on_a_cpu_needs_a_barrier() {
        assert!(needs_barrier(None, Some(1)));
    }

    #[test]
    fn idle_between_two_runs_of_one_process_needs_no_barrier() {
        // A -> idle -> A: the CPU keeps remembering A across the idle context.
        let mut last = None;
        let mut barriers = 0;
        for next in [
            Some(1),
            None,
            Some(1),
            None,
            Some(2),
            Some(2),
            None,
            Some(1),
        ] {
            if needs_barrier(last, next) {
                barriers += 1;
            }
            if next.is_some() {
                last = next;
            }
        }
        // First run of 1, then 1 -> 2, then 2 -> 1.
        assert_eq!(barriers, 3);
    }

    #[test]
    fn no_cpuid_bits_means_no_support() {
        assert!(!Cpuid::default().supports_ibpb());
        let cpuid = Cpuid {
            max_basic_leaf: 0x16,
            leaf7_edx: !LEAF7_EDX_IBRS_IBPB,
            max_extended_leaf: 0x8000_0008,
            ext8_ebx: !EXT8_EBX_IBPB,
        };
        assert!(!cpuid.supports_ibpb());
    }

    #[test]
    fn intel_leaf7_bit_means_support() {
        let cpuid = Cpuid {
            max_basic_leaf: 7,
            leaf7_edx: LEAF7_EDX_IBRS_IBPB,
            ..Cpuid::default()
        };
        assert!(cpuid.supports_ibpb());
    }

    #[test]
    fn amd_extended_leaf_bit_means_support() {
        let cpuid = Cpuid {
            max_basic_leaf: 0x10,
            max_extended_leaf: 0x8000_0020,
            ext8_ebx: EXT8_EBX_IBPB,
            ..Cpuid::default()
        };
        assert!(cpuid.supports_ibpb());
    }

    #[test]
    fn bits_from_unreported_leaves_are_ignored() {
        let cpuid = Cpuid {
            max_basic_leaf: 6,
            leaf7_edx: LEAF7_EDX_IBRS_IBPB,
            max_extended_leaf: 0x8000_0007,
            ext8_ebx: EXT8_EBX_IBPB,
        };
        assert!(!cpuid.supports_ibpb());
    }
}
