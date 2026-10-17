//! Ferric Oxide: hardware random numbers for memory layout randomization (KASLR, ASLR).
//!
//! These seed address choices, not cryptographic keys. On x86_64 they come from RDRAND
//! (Intel Digital Random Number Generator guide); other architectures have none yet, and callers
//! keep their fixed layout there.

#[cfg(target_arch = "x86_64")]
/// Reads a random number from RDRAND, or `None` if the CPU has no RDRAND or it keeps failing.
pub fn hw_u64() -> Option<u64> {
    let has_rdrand = raw_cpuid::CpuId::new()
        .get_feature_info()
        .is_some_and(|f| f.has_rdrand());
    if !has_rdrand {
        return None;
    }
    // Intel's DRNG guide recommends retrying up to 10 times before giving up.
    for _ in 0..10 {
        let value: u64;
        let ok: u8;
        // SAFETY: RDRAND is present (checked above) and only writes the two output registers
        // and the flags.
        unsafe {
            core::arch::asm!(
                "rdrand {value}",
                "setc {ok}",
                value = out(reg) value,
                ok = out(reg_byte) ok,
                options(nomem, nostack),
            );
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

/// No hardware random source on this architecture yet.
#[cfg(not(target_arch = "x86_64"))]
pub fn hw_u64() -> Option<u64> {
    None
}
