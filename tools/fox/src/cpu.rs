//! The x86-64 level `fox` installs for: which CPU-level variant of a
//! registry's prebuilt artifacts it picks, and what level it builds git
//! sources at.
//!
//! The level is, in order of precedence:
//!
//! 1. `--level`, when given;
//! 2. on the running system (`--root /`), the highest level the CPU and the
//!    operating system support (CPUID and XCR0, through the standard
//!    library's feature detection), capped by the policy's `max-cpu-level`;
//! 3. for another root, such as an image being built, whose CPU `fox`
//!    cannot see, the policy's `max-cpu-level`, else the x86-64-v3 baseline.
//!
//! Levels only apply to x86-64 targets; other targets ignore them.

use std::path::Path;

use fox_core::CpuLevel;
use fox_core::policy::Policy;

/// The highest level this CPU supports with the state the operating system
/// enabled, or `None` off x86-64 or below the v3 baseline.
pub(crate) fn detected() -> Option<CpuLevel> {
    #[cfg(target_arch = "x86_64")]
    {
        let v3 = std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("bmi2")
            && std::is_x86_feature_detected!("fma")
            && std::is_x86_feature_detected!("f16c")
            && std::is_x86_feature_detected!("lzcnt")
            && std::is_x86_feature_detected!("movbe");
        // The standard library reports AVX-512 only when XCR0 shows the
        // kernel saves its state, so a kernel without AVX-512 state
        // support reads as v3 here.
        let v4 = v3
            && std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("avx512cd")
            && std::is_x86_feature_detected!("avx512dq")
            && std::is_x86_feature_detected!("avx512vl");
        if v4 {
            Some(CpuLevel::X86_64V4)
        } else if v3 {
            Some(CpuLevel::X86_64V3)
        } else {
            None
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// The level for `root` under `policy`, with `explicit` from
/// `--level`; see the module docs.
pub(crate) fn for_root(explicit: Option<CpuLevel>, root: &Path, policy: &Policy) -> CpuLevel {
    let live = root.canonicalize().is_ok_and(|root| root == Path::new("/"));
    resolve(explicit, policy.max_cpu_level, live, detected())
}

/// [`for_root`] with its inputs spelled out.
fn resolve(
    explicit: Option<CpuLevel>,
    max: Option<CpuLevel>,
    live: bool,
    detected: Option<CpuLevel>,
) -> CpuLevel {
    if let Some(level) = explicit {
        return level;
    }
    if live {
        // A CPU below the baseline cannot run Ferric Oxide anyway.
        let cpu = detected.unwrap_or(CpuLevel::BASELINE);
        max.map_or(cpu, |max| max.min(cpu))
    } else {
        max.unwrap_or(CpuLevel::BASELINE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3: CpuLevel = CpuLevel::X86_64V3;
    const V4: CpuLevel = CpuLevel::X86_64V4;

    #[test]
    fn the_running_system_uses_its_cpu_under_the_policy_cap() {
        assert_eq!(resolve(None, None, true, Some(V4)), V4);
        assert_eq!(resolve(None, Some(V3), true, Some(V4)), V3);
        // The cap never raises the level above what the CPU runs.
        assert_eq!(resolve(None, Some(V4), true, Some(V3)), V3);
        assert_eq!(resolve(None, None, true, None), V3);
    }

    #[test]
    fn another_root_uses_the_policy_or_the_baseline() {
        // The host's CPU says nothing about the image's machine.
        assert_eq!(resolve(None, None, false, Some(V4)), V3);
        assert_eq!(resolve(None, Some(V4), false, Some(V3)), V4);
    }

    #[test]
    fn an_explicit_level_wins() {
        assert_eq!(resolve(Some(V4), Some(V3), true, Some(V3)), V4);
        assert_eq!(resolve(Some(V3), None, false, Some(V4)), V3);
    }

    #[test]
    fn detection_agrees_with_the_standard_library() {
        let level = detected();
        #[cfg(target_arch = "x86_64")]
        if level == Some(V4) {
            assert!(std::is_x86_feature_detected!("avx512f"));
        }
        #[cfg(not(target_arch = "x86_64"))]
        assert_eq!(level, None);
        let _ = level;
    }
}
