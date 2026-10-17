//! Intel Control-flow Enforcement Technology (CET) for the Ferric Oxide kernel: indirect branch
//! tracking (IBT) and, in preparation, supervisor shadow stacks.
//!
//! With IBT on, every indirect `call` or `jmp`, and every entry through the IDT or `SYSCALL`,
//! must land on an `endbr64` instruction, or the CPU raises a control-protection exception
//! (#CP, vector 21). That turns most jump-oriented and call-oriented gadgets into crashes.
//!
//! This crate holds the parts that do not need the hardware, so they can be tested on the host:
//!
//! - [`has_ibt`] and [`has_shstk`] read the CPUID feature bits.
//! - [`decide`] applies the build's [`Policy`] for one [`Feature`] (`kernel.cpu.ibt` or
//!   `kernel.cpu.shstk` in `ferric.toml`) to them.
//! - [`ControlProtectionError`] decodes the #CP error code for the panic message.
//! - The constants name the CPUID bit, the MSRs and the control-register bits the kernel writes.
//! - [`shstk`] holds the shadow-stack layout arithmetic: the shadow-stack page encoding, the
//!   supervisor and restore tokens, a new context's initial shadow stack and the slot window the
//!   kernel maps shadow stacks in.
//!
//! The kernel side (`kernel/src/arch/x86_64/ibt.rs`) writes the MSR and CR4 for IBT. For shadow
//! stacks, `kernel/src/arch/x86_64/shstk.rs` allocates and maps them and keeps the SSP MSRs and
//! context switches ready, but nothing turns them on yet. User-mode CET is not covered here.
//!
//! Specification: Intel 64 and IA-32 Architectures Software Developer's Manual, Volume 1,
//! chapter 17 ("Control-flow Enforcement Technology"); Volume 2, `CPUID` leaf 07H; Volume 3A,
//! section 2.5 (CR4.CET) and section 6.15 ("Interrupt 21—Control Protection Exception");
//! Volume 4, table 2-2 (`IA32_U_CET`, `IA32_S_CET`).
//!
//! # Examples
//!
//! ```
//! use ferric_cet::{
//!     decide, has_ibt, has_shstk, Feature, Policy, CPUID_7_0_ECX_CET_SS, CPUID_7_0_EDX_CET_IBT,
//! };
//!
//! let edx = CPUID_7_0_EDX_CET_IBT;
//! assert_eq!(decide(Feature::Ibt, Policy::Auto, has_ibt(edx)), Ok(true));
//! assert_eq!(decide(Feature::Ibt, Policy::Auto, has_ibt(0)), Ok(false));
//! assert!(decide(Feature::Ibt, Policy::Always, has_ibt(0)).is_err());
//!
//! let ecx = CPUID_7_0_ECX_CET_SS;
//! assert_eq!(decide(Feature::ShadowStack, Policy::Auto, has_shstk(ecx)), Ok(true));
//! assert_eq!(decide(Feature::ShadowStack, Policy::Never, has_shstk(ecx)), Ok(false));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use core::fmt;

pub mod shstk;

/// `CPUID.(EAX=7,ECX=0):EDX[20]`: the CPU supports CET indirect branch tracking.
pub const CPUID_7_0_EDX_CET_IBT: u32 = 1 << 20;

/// `CPUID.(EAX=7,ECX=0):ECX[7]`: the CPU supports CET shadow stacks.
pub const CPUID_7_0_ECX_CET_SS: u32 = 1 << 7;

/// `IA32_U_CET`: CET configuration for user mode. The kernel leaves it at zero.
pub const IA32_U_CET: u32 = 0x6A0;

/// `IA32_S_CET`: CET configuration for supervisor mode.
pub const IA32_S_CET: u32 = 0x6A2;

/// `IA32_S_CET`/`IA32_U_CET` bit 0: shadow stacks enabled.
pub const CET_SH_STK_EN: u64 = 1 << 0;

/// `IA32_S_CET`/`IA32_U_CET` bit 1: `WRSS` instructions enabled.
pub const CET_WR_SHSTK_EN: u64 = 1 << 1;

/// `IA32_S_CET`/`IA32_U_CET` bit 2: indirect branch tracking (`endbr64` checks) enabled.
pub const CET_ENDBR_EN: u64 = 1 << 2;

/// `IA32_S_CET`/`IA32_U_CET` bit 4: the `notrack` prefix is honoured. The kernel leaves this off
/// and is built without jump tables instead, so `notrack` cannot be used to skip the check.
pub const CET_NO_TRACK_EN: u64 = 1 << 4;

/// The `IA32_S_CET` value for kernel IBT: `endbr64` checks only, no shadow stack, no `notrack`.
pub const S_CET_KERNEL_IBT: u64 = CET_ENDBR_EN;

/// CR4 bit 23 (CR4.CET): master enable for CET. It can only be set while CR0.WP is set.
pub const CR4_CET_BIT: u8 = 23;

/// CR0 bit 16 (CR0.WP): supervisor writes honour read-only pages. Required for CR4.CET.
pub const CR0_WP_BIT: u8 = 16;

/// The `endbr64` instruction encoding (`F3 0F 1E FA`).
pub const ENDBR64: [u8; 4] = [0xF3, 0x0F, 0x1E, 0xFA];

/// Whether the CPU supports indirect branch tracking, from `CPUID.(EAX=7,ECX=0):EDX`.
///
/// The caller must make sure leaf 7 exists (the maximum basic leaf is at least 7); on CPUs
/// without it, pass 0.
#[inline]
pub const fn has_ibt(cpuid_7_0_edx: u32) -> bool {
    cpuid_7_0_edx & CPUID_7_0_EDX_CET_IBT != 0
}

/// Whether the CPU supports shadow stacks, from `CPUID.(EAX=7,ECX=0):ECX`.
///
/// The caller must make sure leaf 7 exists (the maximum basic leaf is at least 7); on CPUs
/// without it, pass 0.
#[inline]
pub const fn has_shstk(cpuid_7_0_ecx: u32) -> bool {
    cpuid_7_0_ecx & CPUID_7_0_ECX_CET_SS != 0
}

/// A CET feature the kernel can be configured to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Feature {
    /// Indirect branch tracking (`kernel.cpu.ibt`, `CPUID.(EAX=7,ECX=0):EDX[20]`).
    Ibt,
    /// Supervisor shadow stacks (`kernel.cpu.shstk`, `CPUID.(EAX=7,ECX=0):ECX[7]`).
    ShadowStack,
}

impl Feature {
    /// The `ferric.toml` option that sets this feature's [`Policy`].
    pub const fn option(self) -> &'static str {
        match self {
            Self::Ibt => "kernel.cpu.ibt",
            Self::ShadowStack => "kernel.cpu.shstk",
        }
    }

    /// The feature's name in messages.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Ibt => "CET indirect branch tracking",
            Self::ShadowStack => "CET shadow stacks",
        }
    }

    /// The CPUID bit that reports the feature.
    pub const fn cpuid_bit(self) -> &'static str {
        match self {
            Self::Ibt => "CPUID.(EAX=7,ECX=0):EDX[20]",
            Self::ShadowStack => "CPUID.(EAX=7,ECX=0):ECX[7]",
        }
    }
}

/// The build-time policy for one CET [`Feature`], the value of `kernel.cpu.ibt` or
/// `kernel.cpu.shstk`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Policy {
    /// Require the feature; booting on a CPU without it is an error.
    Always,
    /// Use the feature when the CPU has it.
    Auto,
    /// Never use the feature (for IBT, the kernel is built without `endbr64`).
    Never,
}

impl Policy {
    /// The option value as written in `ferric.toml` and the kernel's `config.toml`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Auto => "auto",
            Self::Never => "never",
        }
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a CET feature could not be turned on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CetError {
    /// The policy is [`Policy::Always`] but the CPU does not report the feature.
    Unsupported(Feature),
}

impl fmt::Display for CetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(feature) => write!(
                f,
                "{} is \"always\" but this CPU does not support {} ({} is clear)",
                feature.option(),
                feature.description(),
                feature.cpuid_bit(),
            ),
        }
    }
}

impl core::error::Error for CetError {}

/// Decides whether to turn `feature` on for a CPU.
///
/// Returns `Ok(true)` to enable it and `Ok(false)` to leave it off. The rule is the same for
/// every feature; `feature` names it in the error.
///
/// # Errors
///
/// [`CetError::Unsupported`] when `policy` is [`Policy::Always`] and `cpu_has_feature` is false.
pub const fn decide(
    feature: Feature,
    policy: Policy,
    cpu_has_feature: bool,
) -> Result<bool, CetError> {
    match policy {
        Policy::Never => Ok(false),
        Policy::Auto => Ok(cpu_has_feature),
        Policy::Always if cpu_has_feature => Ok(true),
        Policy::Always => Err(CetError::Unsupported(feature)),
    }
}

/// What a control-protection exception (#CP) reports, from bits 14:0 of its error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlProtectionKind {
    /// 1: a near `RET` did not match the shadow stack.
    NearRet,
    /// 2: a far `RET` or `IRET` did not match the shadow stack.
    FarRet,
    /// 3: an indirect branch did not land on `endbr64` (an IBT violation).
    Endbranch,
    /// 4: `RSTORSSP` found an invalid shadow-stack restore token.
    Rstorssp,
    /// 5: `SETSSBSY` found an invalid supervisor shadow-stack token.
    Setssbsy,
    /// Any other value.
    Unknown(u16),
}

/// A decoded #CP error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlProtectionError {
    /// What went wrong.
    pub kind: ControlProtectionKind,
    /// Bit 15: the violation happened inside an SGX enclave.
    pub enclave: bool,
}

impl ControlProtectionError {
    /// Decodes the error code the CPU pushes for #CP.
    pub const fn from_error_code(code: u64) -> Self {
        let kind = match code & 0x7FFF {
            1 => ControlProtectionKind::NearRet,
            2 => ControlProtectionKind::FarRet,
            3 => ControlProtectionKind::Endbranch,
            4 => ControlProtectionKind::Rstorssp,
            5 => ControlProtectionKind::Setssbsy,
            // Masked to 15 bits above, so this always fits.
            other => ControlProtectionKind::Unknown(other as u16),
        };
        Self {
            kind,
            enclave: code & (1 << 15) != 0,
        }
    }
}

impl fmt::Display for ControlProtectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ControlProtectionKind::NearRet => f.write_str("near RET shadow-stack mismatch")?,
            ControlProtectionKind::FarRet => f.write_str("far RET/IRET shadow-stack mismatch")?,
            ControlProtectionKind::Endbranch => {
                f.write_str("indirect branch target is not an endbr64 instruction")?
            }
            ControlProtectionKind::Rstorssp => f.write_str("invalid RSTORSSP token")?,
            ControlProtectionKind::Setssbsy => f.write_str("invalid SETSSBSY token")?,
            ControlProtectionKind::Unknown(code) => write!(f, "unknown cause {code:#x}")?,
        }
        if self.enclave {
            f.write_str(" (in an SGX enclave)")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::*;

    #[test]
    fn ibt_bit_is_edx_20() {
        assert!(!has_ibt(0));
        assert!(has_ibt(1 << 20));
        assert!(has_ibt(u32::MAX));
        assert!(!has_ibt(!(1 << 20)));
        // CET_SS is in ECX[7], not EDX; EDX[7] alone must not count.
        assert!(!has_ibt(1 << 7));
    }

    #[test]
    fn shstk_bit_is_ecx_7() {
        assert!(!has_shstk(0));
        assert!(has_shstk(1 << 7));
        assert!(has_shstk(u32::MAX));
        assert!(!has_shstk(!(1 << 7)));
        // CET_IBT is in EDX[20]; ECX[20] alone must not count.
        assert!(!has_shstk(1 << 20));
        assert_eq!(CPUID_7_0_ECX_CET_SS, 0x80);
    }

    #[test]
    fn policy() {
        for feature in [Feature::Ibt, Feature::ShadowStack] {
            assert_eq!(decide(feature, Policy::Never, true), Ok(false));
            assert_eq!(decide(feature, Policy::Never, false), Ok(false));
            assert_eq!(decide(feature, Policy::Auto, true), Ok(true));
            assert_eq!(decide(feature, Policy::Auto, false), Ok(false));
            assert_eq!(decide(feature, Policy::Always, true), Ok(true));
            assert_eq!(
                decide(feature, Policy::Always, false),
                Err(CetError::Unsupported(feature))
            );
        }
    }

    #[test]
    fn policy_names() {
        assert_eq!(Policy::Always.to_string(), "always");
        assert_eq!(Policy::Auto.to_string(), "auto");
        assert_eq!(Policy::Never.to_string(), "never");
    }

    #[test]
    fn kernel_s_cet_has_no_shadow_stack_or_notrack() {
        assert_eq!(S_CET_KERNEL_IBT, 0b100);
        assert_eq!(S_CET_KERNEL_IBT & (CET_SH_STK_EN | CET_WR_SHSTK_EN), 0);
        assert_eq!(S_CET_KERNEL_IBT & CET_NO_TRACK_EN, 0);
        assert_eq!(IA32_S_CET, 0x6A2);
        assert_eq!(1u64 << CR4_CET_BIT, 0x80_0000);
    }

    #[test]
    fn control_protection_codes() {
        let e = ControlProtectionError::from_error_code(3);
        assert_eq!(e.kind, ControlProtectionKind::Endbranch);
        assert!(!e.enclave);
        assert_eq!(
            e.to_string(),
            "indirect branch target is not an endbr64 instruction"
        );

        let e = ControlProtectionError::from_error_code(0x8001);
        assert_eq!(e.kind, ControlProtectionKind::NearRet);
        assert!(e.enclave);

        let e = ControlProtectionError::from_error_code(0x1_0042);
        assert_eq!(e.kind, ControlProtectionKind::Unknown(0x42));
        assert_eq!(e.to_string(), "unknown cause 0x42");
    }

    #[test]
    fn unsupported_message_names_the_option() {
        // Unchanged from before shadow-stack support, so IBT boot panics read the same.
        assert_eq!(
            CetError::Unsupported(Feature::Ibt).to_string(),
            "kernel.cpu.ibt is \"always\" but this CPU does not support CET indirect branch \
             tracking (CPUID.(EAX=7,ECX=0):EDX[20] is clear)"
        );
        assert_eq!(
            CetError::Unsupported(Feature::ShadowStack).to_string(),
            "kernel.cpu.shstk is \"always\" but this CPU does not support CET shadow stacks \
             (CPUID.(EAX=7,ECX=0):ECX[7] is clear)"
        );
    }
}
