//! Ferric Oxide: kernel shadow stacks (Intel CET SS) for x86_64, detection and policy only.
//!
//! With supervisor shadow stacks on, every `call` also pushes the return address to a separate,
//! write-protected stack, and every `ret` checks the two copies match, raising a
//! control-protection exception (#CP) when they do not. That stops return-oriented gadget
//! chains.
//!
//! This is the first step: the `shstk` CPU feature (`arch.x86_64.features.shstk` in the kernel
//! config, `kernel.cpu.shstk` in `ferric.toml`) is either `never` (the default, which builds this
//! out and logs nothing) or `auto`. With `auto`, [`report`] logs on the BSP whether
//! `CPUID.(EAX=7,ECX=0):ECX[7]` reports shadow stacks and what the policy decided. Nothing
//! enables them yet: `IA32_S_CET.SH_STK_EN`, `IA32_PL0_SSP`, CR4 and the stacks themselves are
//! left alone. `always` is not offered until the kernel can actually turn them on.
//!
//! The CPUID and policy logic lives in the host-tested `ferric-cet` crate.

use ferric_cet::{Feature, Policy};

/// The configured policy. build.rs refuses `always`.
const POLICY: Policy = if cfg!(cpu_feature_auto = "shstk") {
    Policy::Auto
} else {
    Policy::Never
};

/// Whether this CPU supports shadow stacks.
fn cpu_has_shstk() -> bool {
    super::ibt::cpuid_7_0().is_some_and(|leaf| ferric_cet::has_shstk(leaf.ecx))
}

/// Logs whether the CPU supports shadow stacks and what the policy decided. Called once, on the
/// BSP. Does nothing when the policy is `never`.
pub fn report() {
    if POLICY == Policy::Never {
        return;
    }
    let supported = cpu_has_shstk();
    // `decide` only fails for `always`, which this build cannot have.
    let decision = match ferric_cet::decide(Feature::ShadowStack, POLICY, supported) {
        Ok(true) => "on",
        Ok(false) => "off",
        Err(err) => {
            warn!("CET: {err}");
            "off"
        }
    };
    info!(
        "CET: shadow stacks supported={supported} policy={POLICY} decision={decision} (not yet enabled)"
    );
}
