//! Hardware random numbers for memory layout randomization (KASLR and ASLR).
//!
//! The kernel and the user-space program loader both need a few random bits
//! early, before any entropy pool exists: the kernel while it builds its first
//! page tables, and `exec` before the new program has opened a single file.
//! This crate reads them straight from the CPU. On x86_64 that is the RDRAND
//! instruction, as described in Intel's "Digital Random Number Generator
//! (DRNG) Software Implementation Guide"; other architectures have no source
//! yet, and every function returns `None` there so callers keep their fixed
//! layout.
//!
//! The values pick addresses. They are not meant for keys or anything else
//! that needs a vetted cryptographic generator.
//!
//! # Examples
//!
//! ```
//! // A random 1 GiB-aligned offset below 16 TiB, or 0 without RDRAND.
//! let offset = ferric_hwrand::bits(14).map_or(0, |slot| slot << 30);
//! assert!(offset < 1 << 44);
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

/// Intel's DRNG guide recommends retrying up to 10 times before treating
/// RDRAND as broken.
#[cfg(target_arch = "x86_64")]
const RETRIES: usize = 10;

/// Returns a random 64-bit number, or `None` if the CPU has no hardware
/// random number generator or it keeps failing.
#[cfg(target_arch = "x86_64")]
pub fn u64() -> Option<u64> {
    if !has_rdrand() {
        return None;
    }
    (0..RETRIES).find_map(|_| {
        let mut value = 0;
        // SAFETY: the CPU supports RDRAND (checked above), and the intrinsic
        // only writes `value`.
        let ok = unsafe { rdrand(&mut value) };
        (ok == 1).then_some(value)
    })
}

/// Returns a random 64-bit number, or `None` if the CPU has no hardware
/// random number generator or it keeps failing.
#[cfg(not(target_arch = "x86_64"))]
pub fn u64() -> Option<u64> {
    None
}

/// Returns a random number below `1 << n`, or `None` without a hardware
/// source. `n` of 64 or more gives the full 64 bits.
pub fn bits(n: u32) -> Option<u64> {
    let value = u64()?;
    Some(match 1_u64.checked_shl(n) {
        Some(bound) => value & (bound - 1),
        None => value,
    })
}

/// Whether CPUID reports RDRAND (leaf 1, ECX bit 30).
#[cfg(target_arch = "x86_64")]
fn has_rdrand() -> bool {
    // CPUID leaf 1 exists on every x86_64 CPU. Older toolchains declare the
    // intrinsic unsafe, newer ones safe.
    #[allow(unused_unsafe)]
    // SAFETY: CPUID is available on every x86_64 CPU.
    let leaf1 = unsafe { core::arch::x86_64::__cpuid(1) };
    leaf1.ecx & (1 << 30) != 0
}

/// Wrapper that enables the `rdrand` target feature for the intrinsic only.
/// Calling it is unsafe unless the CPU supports RDRAND.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "rdrand")]
fn rdrand(value: &mut u64) -> i32 {
    // Older toolchains declare the intrinsic unsafe, newer ones safe inside a
    // function with the feature enabled.
    #[allow(unused_unsafe)]
    // SAFETY: this function only runs with RDRAND enabled.
    unsafe {
        core::arch::x86_64::_rdrand64_step(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_stay_below_the_bound() {
        for n in [0, 1, 12, 22, 63] {
            if let Some(value) = bits(n) {
                assert!(value < 1 << n, "{value:#x} has more than {n} bits");
            }
        }
        assert_eq!(bits(0).unwrap_or(0), 0);
    }

    #[test]
    fn values_vary() {
        // Without RDRAND (some emulators) there is nothing to compare.
        let Some(first) = u64() else { return };
        // 16 identical 64-bit draws from a working generator do not happen.
        assert!((0..16).any(|_| u64() != Some(first)));
    }
}
