//! The VMX capability MSRs (SDM Vol. 3C, appendix A), decoded.
//!
//! The kernel reads the raw values with `RDMSR` once per CPU and hands them to
//! [`Capabilities::new`]; everything after that is computation on these values, so it is tested
//! on the host.

use thiserror::Error;

use crate::controls::Allowed;

/// IA32_VMX_BASIC.
pub const MSR_BASIC: u32 = 0x480;
/// IA32_VMX_PINBASED_CTLS.
pub const MSR_PINBASED_CTLS: u32 = 0x481;
/// IA32_VMX_PROCBASED_CTLS.
pub const MSR_PROCBASED_CTLS: u32 = 0x482;
/// IA32_VMX_EXIT_CTLS.
pub const MSR_EXIT_CTLS: u32 = 0x483;
/// IA32_VMX_ENTRY_CTLS.
pub const MSR_ENTRY_CTLS: u32 = 0x484;
/// IA32_VMX_MISC.
pub const MSR_MISC: u32 = 0x485;
/// IA32_VMX_CR0_FIXED0.
pub const MSR_CR0_FIXED0: u32 = 0x486;
/// IA32_VMX_CR0_FIXED1.
pub const MSR_CR0_FIXED1: u32 = 0x487;
/// IA32_VMX_CR4_FIXED0.
pub const MSR_CR4_FIXED0: u32 = 0x488;
/// IA32_VMX_CR4_FIXED1.
pub const MSR_CR4_FIXED1: u32 = 0x489;
/// IA32_VMX_PROCBASED_CTLS2.
pub const MSR_PROCBASED_CTLS2: u32 = 0x48b;
/// IA32_VMX_EPT_VPID_CAP.
pub const MSR_EPT_VPID_CAP: u32 = 0x48c;
/// IA32_VMX_TRUE_PINBASED_CTLS.
pub const MSR_TRUE_PINBASED_CTLS: u32 = 0x48d;
/// IA32_VMX_TRUE_PROCBASED_CTLS.
pub const MSR_TRUE_PROCBASED_CTLS: u32 = 0x48e;
/// IA32_VMX_TRUE_EXIT_CTLS.
pub const MSR_TRUE_EXIT_CTLS: u32 = 0x48f;
/// IA32_VMX_TRUE_ENTRY_CTLS.
pub const MSR_TRUE_ENTRY_CTLS: u32 = 0x490;

/// IA32_FEATURE_CONTROL.
pub const MSR_FEATURE_CONTROL: u32 = 0x3a;

/// IA32_FEATURE_CONTROL, decoded: whether firmware left VMXON usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureControl(u64);

impl FeatureControl {
    /// The lock bit: once set, the MSR cannot be written until reset.
    pub const LOCK: u64 = 1 << 0;
    /// VMXON allowed outside SMX operation.
    pub const VMX_OUTSIDE_SMX: u64 = 1 << 2;

    /// The MSR's value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// What the kernel may do about VMX on this CPU.
    pub const fn state(self) -> VmxFirmwareState {
        match (
            self.0 & Self::LOCK != 0,
            self.0 & Self::VMX_OUTSIDE_SMX != 0,
        ) {
            (true, true) => VmxFirmwareState::Enabled,
            (true, false) => VmxFirmwareState::LockedOff,
            (false, _) => VmxFirmwareState::Unlocked,
        }
    }

    /// The value to write to enable VMX and lock the MSR, when it is unlocked.
    pub const fn enabled_and_locked(self) -> u64 {
        self.0 | Self::LOCK | Self::VMX_OUTSIDE_SMX
    }
}

/// What firmware did with IA32_FEATURE_CONTROL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmxFirmwareState {
    /// Locked with VMX allowed: the kernel may use VMXON.
    Enabled,
    /// Locked with VMX disallowed: VMX cannot be used until firmware changes it.
    LockedOff,
    /// Unlocked: the kernel enables VMX and locks the MSR itself.
    Unlocked,
}

/// The raw capability MSR values of one CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawCapabilities {
    /// IA32_VMX_BASIC.
    pub basic: u64,
    /// IA32_VMX_PINBASED_CTLS.
    pub pinbased: u64,
    /// IA32_VMX_PROCBASED_CTLS.
    pub procbased: u64,
    /// IA32_VMX_EXIT_CTLS.
    pub exit: u64,
    /// IA32_VMX_ENTRY_CTLS.
    pub entry: u64,
    /// IA32_VMX_TRUE_*_CTLS, in the order pin, proc, exit, entry; read only when
    /// [`Basic::true_controls`] is set.
    pub true_controls: [u64; 4],
    /// IA32_VMX_MISC.
    pub misc: u64,
    /// IA32_VMX_CR0_FIXED0 and FIXED1.
    pub cr0_fixed: [u64; 2],
    /// IA32_VMX_CR4_FIXED0 and FIXED1.
    pub cr4_fixed: [u64; 2],
    /// IA32_VMX_PROCBASED_CTLS2; read only when the primary controls allow
    /// [`crate::controls::primary::ACTIVATE_SECONDARY`].
    pub procbased2: u64,
    /// IA32_VMX_EPT_VPID_CAP; read only when the secondary controls allow EPT or VPID.
    pub ept_vpid: u64,
}

/// IA32_VMX_BASIC, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Basic(u64);

impl Basic {
    /// The VMCS revision identifier, written at the start of the VMXON region and every VMCS.
    pub const fn revision_id(self) -> u32 {
        (self.0 & 0x7fff_ffff) as u32
    }

    /// The size of the VMXON region and of a VMCS, in bytes (at most 4096).
    pub const fn region_size(self) -> u32 {
        ((self.0 >> 32) & 0x1fff) as u32
    }

    /// Whether VMXON, VMCS and related addresses are limited to 32 bits.
    pub const fn addresses_32bit(self) -> bool {
        self.0 & (1 << 48) != 0
    }

    /// The memory type the CPU uses for the VMCS: 6 is write-back.
    pub const fn memory_type(self) -> u8 {
        ((self.0 >> 50) & 0xf) as u8
    }

    /// Whether VM exits for INS and OUTS report instruction information.
    pub const fn ins_outs_info(self) -> bool {
        self.0 & (1 << 54) != 0
    }

    /// Whether the TRUE capability MSRs exist and govern the default-1 controls.
    pub const fn true_controls(self) -> bool {
        self.0 & (1 << 55) != 0
    }
}

/// IA32_VMX_EPT_VPID_CAP, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EptVpidCap(u64);

impl EptVpidCap {
    const fn bit(self, n: u32) -> bool {
        self.0 & (1 << n) != 0
    }

    /// Execute-only EPT translations are allowed.
    pub const fn execute_only(self) -> bool {
        self.bit(0)
    }

    /// A page-walk length of 4 is supported.
    pub const fn walk_length_4(self) -> bool {
        self.bit(6)
    }

    /// A page-walk length of 5 is supported.
    pub const fn walk_length_5(self) -> bool {
        self.bit(7)
    }

    /// Uncacheable EPT paging structures are supported.
    pub const fn memory_type_uc(self) -> bool {
        self.bit(8)
    }

    /// Write-back EPT paging structures are supported.
    pub const fn memory_type_wb(self) -> bool {
        self.bit(14)
    }

    /// 2 MiB EPT pages.
    pub const fn pages_2m(self) -> bool {
        self.bit(16)
    }

    /// 1 GiB EPT pages.
    pub const fn pages_1g(self) -> bool {
        self.bit(17)
    }

    /// INVEPT is supported.
    pub const fn invept(self) -> bool {
        self.bit(20)
    }

    /// EPT accessed and dirty flags.
    pub const fn accessed_dirty(self) -> bool {
        self.bit(21)
    }

    /// Single-context INVEPT.
    pub const fn invept_single_context(self) -> bool {
        self.bit(25)
    }

    /// All-context INVEPT.
    pub const fn invept_all_context(self) -> bool {
        self.bit(26)
    }

    /// INVVPID is supported.
    pub const fn invvpid(self) -> bool {
        self.bit(32)
    }

    /// Single-context INVVPID.
    pub const fn invvpid_single_context(self) -> bool {
        self.bit(41)
    }

    /// All-context INVVPID.
    pub const fn invvpid_all_context(self) -> bool {
        self.bit(42)
    }
}

/// The CR0 or CR4 bits VMX operation fixes, from the FIXED0 and FIXED1 MSRs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedBits {
    /// Bits that must be 1 (FIXED0).
    pub must_be_one: u64,
    /// Bits that may be 1 (FIXED1).
    pub may_be_one: u64,
}

impl FixedBits {
    /// `value` with the bits that must be 1 set and the bits that must be 0 cleared.
    pub const fn adjust(self, value: u64) -> u64 {
        (value | self.must_be_one) & self.may_be_one
    }

    /// Whether `value` meets the constraint.
    pub const fn accepts(self, value: u64) -> bool {
        value & self.must_be_one == self.must_be_one && value & !self.may_be_one == 0
    }
}

/// CR0.PE.
pub const CR0_PE: u64 = 1 << 0;
/// CR0.PG.
pub const CR0_PG: u64 = 1 << 31;

/// One CPU's VMX capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// IA32_VMX_BASIC.
    pub basic: Basic,
    /// The pin-based controls.
    pub pin: Allowed,
    /// The primary processor-based controls.
    pub primary: Allowed,
    /// The secondary processor-based controls (all zero when there are none).
    pub secondary: Allowed,
    /// The VM-exit controls.
    pub exit: Allowed,
    /// The VM-entry controls.
    pub entry: Allowed,
    /// The fixed CR0 bits in VMX operation.
    pub cr0: FixedBits,
    /// The fixed CR4 bits in VMX operation.
    pub cr4: FixedBits,
    /// EPT and VPID capabilities (all zero when neither exists).
    pub ept_vpid: EptVpidCap,
    /// IA32_VMX_MISC.
    pub misc: u64,
}

impl Capabilities {
    /// Decodes one CPU's raw values, using the TRUE MSRs for the default-1 controls when
    /// IA32_VMX_BASIC says they exist.
    ///
    /// # Errors
    ///
    /// [`CapsError::RegionSize`] when the VMCS does not fit a 4 KiB page, and
    /// [`CapsError::MemoryType`] when the VMCS memory type is not write-back; the hypervisor
    /// supports neither.
    pub const fn new(raw: &RawCapabilities) -> Result<Self, CapsError> {
        let basic = Basic(raw.basic);
        if basic.region_size() == 0 || basic.region_size() > 4096 {
            return Err(CapsError::RegionSize {
                size: basic.region_size(),
            });
        }
        if basic.memory_type() != 6 {
            return Err(CapsError::MemoryType {
                memory_type: basic.memory_type(),
            });
        }
        let [pin, procbased, exit, entry] = if basic.true_controls() {
            raw.true_controls
        } else {
            [raw.pinbased, raw.procbased, raw.exit, raw.entry]
        };
        let primary = Allowed::from_msr(procbased);
        let secondary = if primary.supports(crate::controls::primary::ACTIVATE_SECONDARY) {
            Allowed::from_msr(raw.procbased2)
        } else {
            Allowed::from_msr(0)
        };
        let has_ept_or_vpid = secondary.supports(crate::controls::secondary::ENABLE_EPT)
            || secondary.supports(crate::controls::secondary::ENABLE_VPID);
        Ok(Self {
            basic,
            pin: Allowed::from_msr(pin),
            primary,
            secondary,
            exit: Allowed::from_msr(exit),
            entry: Allowed::from_msr(entry),
            cr0: FixedBits {
                must_be_one: raw.cr0_fixed[0],
                may_be_one: raw.cr0_fixed[1],
            },
            cr4: FixedBits {
                must_be_one: raw.cr4_fixed[0],
                may_be_one: raw.cr4_fixed[1],
            },
            ept_vpid: EptVpidCap(if has_ept_or_vpid { raw.ept_vpid } else { 0 }),
            misc: raw.misc,
        })
    }

    /// The fixed CR0 bits for a guest; with unrestricted guest, PE and PG are free.
    pub const fn guest_cr0(&self, unrestricted: bool) -> FixedBits {
        if unrestricted {
            FixedBits {
                must_be_one: self.cr0.must_be_one & !(CR0_PE | CR0_PG),
                may_be_one: self.cr0.may_be_one,
            }
        } else {
            self.cr0
        }
    }

    /// The rate of the VMX-preemption timer: it counts down once every 2^n TSC ticks.
    pub const fn preemption_timer_shift(&self) -> u32 {
        (self.misc & 0x1f) as u32
    }

    /// Whether VMWRITE may write the VM-exit information fields.
    pub const fn exit_info_writable(&self) -> bool {
        self.misc & (1 << 29) != 0
    }

    /// Whether the CPU can run guests the hypervisor needs: EPT with 4-level walks and
    /// write-back tables, VPID, unrestricted guest and MSR bitmaps.
    pub const fn sufficient(&self) -> Result<(), CapsError> {
        use crate::controls::{primary, secondary};
        if !self
            .primary
            .supports(primary::ACTIVATE_SECONDARY | primary::USE_MSR_BITMAPS)
        {
            return Err(CapsError::Missing {
                what: Missing::MsrBitmaps,
            });
        }
        if !self
            .secondary
            .supports(secondary::ENABLE_EPT | secondary::UNRESTRICTED_GUEST)
        {
            return Err(CapsError::Missing { what: Missing::Ept });
        }
        if !self.ept_vpid.walk_length_4()
            || !self.ept_vpid.memory_type_wb()
            || !self.ept_vpid.invept()
        {
            return Err(CapsError::Missing { what: Missing::Ept });
        }
        if !self.secondary.supports(secondary::ENABLE_VPID) || !self.ept_vpid.invvpid() {
            return Err(CapsError::Missing {
                what: Missing::Vpid,
            });
        }
        Ok(())
    }
}

/// A VMX feature the hypervisor needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Missing {
    /// Secondary controls and MSR bitmaps.
    MsrBitmaps,
    /// EPT with 4-level, write-back tables and INVEPT, and unrestricted guest.
    Ept,
    /// VPID and INVVPID.
    Vpid,
}

/// Why a CPU's VMX capabilities cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CapsError {
    /// The VMCS region is larger than a page, or zero.
    #[error("the VMCS region size {size} is not supported")]
    RegionSize {
        /// The reported size.
        size: u32,
    },
    /// The VMCS memory type is not write-back.
    #[error("the VMCS memory type {memory_type} is not write-back")]
    MemoryType {
        /// The reported type.
        memory_type: u8,
    },
    /// A feature the hypervisor needs is missing.
    #[error("the CPU lacks a VMX feature the hypervisor needs: {what:?}")]
    Missing {
        /// What is missing.
        what: Missing,
    },
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::controls::{primary, secondary};

    /// A plausible modern CPU: TRUE controls, every control allowed except where noted.
    pub(crate) fn modern() -> RawCapabilities {
        // Revision 4, 4096-byte regions, write-back, INS/OUTS info, TRUE controls.
        let basic = 4 | (4096 << 32) | (6 << 50) | (1 << 54) | (1 << 55);
        let all = 0xffff_ffff_0000_0000;
        RawCapabilities {
            basic,
            // The non-TRUE MSRs claim more must-be-one bits; they must be ignored.
            pinbased: all | 0x16,
            procbased: all | 0x0401_e172,
            exit: all | 0x0003_6dff,
            entry: all | 0x0000_11ff,
            true_controls: [
                all | 0x16,
                all | 0x0400_6172,
                all | 0x0003_6dfb,
                all | 0x0000_11fb,
            ],
            misc: 5 | (1 << 29),
            cr0_fixed: [0x8000_0021, 0xffff_ffff],
            cr4_fixed: [0x2000, 0x0037_27ff],
            procbased2: all,
            ept_vpid: (1 << 0)
                | (1 << 6)
                | (1 << 14)
                | (1 << 16)
                | (1 << 17)
                | (1 << 20)
                | (1 << 21)
                | (1 << 25)
                | (1 << 26)
                | (1 << 32)
                | (1 << 41)
                | (1 << 42),
        }
    }

    #[test]
    fn true_controls_replace_the_defaults() {
        let caps = Capabilities::new(&modern()).expect("usable");
        assert_eq!(caps.basic.revision_id(), 4);
        assert_eq!(caps.primary.must_be_one(), 0x0400_6172);
        assert_eq!(caps.preemption_timer_shift(), 5);
        assert!(caps.exit_info_writable());
        assert_eq!(caps.sufficient(), Ok(()));
        let mut old = modern();
        old.basic &= !(1 << 55);
        let caps = Capabilities::new(&old).expect("usable");
        assert_eq!(caps.primary.must_be_one(), 0x0401_e172);
    }

    #[test]
    fn missing_features_are_named() {
        let mut raw = modern();
        raw.procbased2 &= !(u64::from(secondary::ENABLE_VPID) << 32);
        let caps = Capabilities::new(&raw).expect("decodes");
        assert_eq!(
            caps.sufficient(),
            Err(CapsError::Missing {
                what: Missing::Vpid
            })
        );
        let mut raw = modern();
        raw.true_controls[1] &= !(u64::from(primary::ACTIVATE_SECONDARY) << 32);
        let caps = Capabilities::new(&raw).expect("decodes");
        // Without secondary controls there is no EPT, and the EPT MSR is not read.
        assert_eq!(caps.secondary.may_be_one(), 0);
        assert_eq!(caps.ept_vpid, EptVpidCap(0));
        assert_eq!(
            caps.sufficient(),
            Err(CapsError::Missing {
                what: Missing::MsrBitmaps
            })
        );
        let mut raw = modern();
        // Memory type 0 (uncacheable) in bits 53:50.
        raw.basic &= !(0xf << 50);
        assert_eq!(
            Capabilities::new(&raw),
            Err(CapsError::MemoryType { memory_type: 0 })
        );
        let mut raw = modern();
        raw.basic = (raw.basic & !(0x1fff << 32)) | (8192 << 32);
        assert!(matches!(
            Capabilities::new(&raw),
            Err(CapsError::RegionSize { .. })
        ));
    }

    #[test]
    fn unrestricted_guests_may_clear_pe_and_pg() {
        let caps = Capabilities::new(&modern()).expect("usable");
        let real_mode = 0x0000_0020;
        assert!(!caps.guest_cr0(false).accepts(real_mode));
        assert!(caps.guest_cr0(true).accepts(real_mode));
        assert_eq!(caps.cr4.adjust(0), 0x2000);
        assert_eq!(caps.cr4.adjust(1 << 40), 0x2000);
    }

    #[test]
    fn feature_control_states() {
        assert_eq!(FeatureControl::new(0x5).state(), VmxFirmwareState::Enabled);
        assert_eq!(
            FeatureControl::new(0x1).state(),
            VmxFirmwareState::LockedOff
        );
        assert_eq!(FeatureControl::new(0x0).state(), VmxFirmwareState::Unlocked);
        assert_eq!(FeatureControl::new(0).enabled_and_locked(), 0x5);
    }
}
