//! VM-execution, VM-exit and VM-entry control bits, and choosing their values from what the CPU
//! allows.
//!
//! Each 32-bit control field has a capability MSR (SDM Vol. 3C, appendix A.3–A.5): bits 31:0
//! are the *allowed 0-settings* (a 1 there means the control must be 1) and bits 63:32 the
//! *allowed 1-settings* (a 0 there means the control must be 0). [`Allowed::resolve`] combines
//! the controls the hypervisor needs, the ones it would like, and those constraints into the
//! value to write, or says which needed controls the CPU lacks.

use thiserror::Error;

/// Pin-based VM-execution controls.
pub mod pin {
    /// External interrupts cause VM exits.
    pub const EXTERNAL_INTERRUPT_EXITING: u32 = 1 << 0;
    /// NMIs cause VM exits.
    pub const NMI_EXITING: u32 = 1 << 3;
    /// Virtual NMIs.
    pub const VIRTUAL_NMIS: u32 = 1 << 5;
    /// The VMX-preemption timer counts down in the guest.
    pub const PREEMPTION_TIMER: u32 = 1 << 6;
    /// Posted interrupts are processed.
    pub const POSTED_INTERRUPTS: u32 = 1 << 7;
}

/// Primary processor-based VM-execution controls.
pub mod primary {
    /// Exit at the start of any instruction when the guest can take interrupts.
    pub const INTERRUPT_WINDOW_EXITING: u32 = 1 << 2;
    /// The TSC offset applies to the guest's TSC reads.
    pub const USE_TSC_OFFSETTING: u32 = 1 << 3;
    /// HLT exits.
    pub const HLT_EXITING: u32 = 1 << 7;
    /// INVLPG exits.
    pub const INVLPG_EXITING: u32 = 1 << 9;
    /// MWAIT exits.
    pub const MWAIT_EXITING: u32 = 1 << 10;
    /// RDPMC exits.
    pub const RDPMC_EXITING: u32 = 1 << 11;
    /// RDTSC exits.
    pub const RDTSC_EXITING: u32 = 1 << 12;
    /// MOV to CR3 exits.
    pub const CR3_LOAD_EXITING: u32 = 1 << 15;
    /// MOV from CR3 exits.
    pub const CR3_STORE_EXITING: u32 = 1 << 16;
    /// MOV to CR8 exits.
    pub const CR8_LOAD_EXITING: u32 = 1 << 19;
    /// MOV from CR8 exits.
    pub const CR8_STORE_EXITING: u32 = 1 << 20;
    /// TPR shadow through the virtual-APIC page.
    pub const USE_TPR_SHADOW: u32 = 1 << 21;
    /// Exit when the guest can take NMIs.
    pub const NMI_WINDOW_EXITING: u32 = 1 << 22;
    /// MOV DR exits.
    pub const MOV_DR_EXITING: u32 = 1 << 23;
    /// Every I/O instruction exits.
    pub const UNCONDITIONAL_IO_EXITING: u32 = 1 << 24;
    /// The I/O bitmaps decide which ports exit.
    pub const USE_IO_BITMAPS: u32 = 1 << 25;
    /// Monitor trap flag.
    pub const MONITOR_TRAP_FLAG: u32 = 1 << 27;
    /// The MSR bitmaps decide which MSR accesses exit.
    pub const USE_MSR_BITMAPS: u32 = 1 << 28;
    /// MONITOR exits.
    pub const MONITOR_EXITING: u32 = 1 << 29;
    /// PAUSE exits.
    pub const PAUSE_EXITING: u32 = 1 << 30;
    /// The secondary controls apply.
    pub const ACTIVATE_SECONDARY: u32 = 1 << 31;
}

/// Secondary processor-based VM-execution controls.
pub mod secondary {
    /// Accesses to the APIC-access page are virtualized.
    pub const VIRTUALIZE_APIC_ACCESSES: u32 = 1 << 0;
    /// EPT.
    pub const ENABLE_EPT: u32 = 1 << 1;
    /// Descriptor-table instructions exit.
    pub const DESCRIPTOR_TABLE_EXITING: u32 = 1 << 2;
    /// RDTSCP does not cause #UD.
    pub const ENABLE_RDTSCP: u32 = 1 << 3;
    /// x2APIC MSR accesses are virtualized.
    pub const VIRTUALIZE_X2APIC: u32 = 1 << 4;
    /// VPID tags the guest's TLB entries.
    pub const ENABLE_VPID: u32 = 1 << 5;
    /// WBINVD exits.
    pub const WBINVD_EXITING: u32 = 1 << 6;
    /// The guest may run with paging off or in real mode.
    pub const UNRESTRICTED_GUEST: u32 = 1 << 7;
    /// APIC-register virtualization.
    pub const APIC_REGISTER_VIRTUALIZATION: u32 = 1 << 8;
    /// Virtual-interrupt delivery.
    pub const VIRTUAL_INTERRUPT_DELIVERY: u32 = 1 << 9;
    /// PAUSE-loop exiting.
    pub const PAUSE_LOOP_EXITING: u32 = 1 << 10;
    /// RDRAND exits.
    pub const RDRAND_EXITING: u32 = 1 << 11;
    /// INVPCID does not cause #UD.
    pub const ENABLE_INVPCID: u32 = 1 << 12;
    /// RDSEED exits.
    pub const RDSEED_EXITING: u32 = 1 << 16;
    /// XSAVES and XRSTORS do not cause #UD.
    pub const ENABLE_XSAVES: u32 = 1 << 20;
    /// TSC scaling.
    pub const USE_TSC_SCALING: u32 = 1 << 25;
}

/// Primary VM-exit controls.
pub mod exit {
    /// DR7 and IA32_DEBUGCTL are saved on exit.
    pub const SAVE_DEBUG_CONTROLS: u32 = 1 << 2;
    /// The host runs in 64-bit mode after the exit.
    pub const HOST_ADDRESS_SPACE_SIZE: u32 = 1 << 9;
    /// IA32_PERF_GLOBAL_CTRL is loaded on exit.
    pub const LOAD_PERF_GLOBAL_CTRL: u32 = 1 << 12;
    /// The interrupt that caused an exit is acknowledged, and its vector saved.
    pub const ACK_INTERRUPT_ON_EXIT: u32 = 1 << 15;
    /// The guest's IA32_PAT is saved on exit.
    pub const SAVE_PAT: u32 = 1 << 18;
    /// The host's IA32_PAT is loaded on exit.
    pub const LOAD_PAT: u32 = 1 << 19;
    /// The guest's IA32_EFER is saved on exit.
    pub const SAVE_EFER: u32 = 1 << 20;
    /// The host's IA32_EFER is loaded on exit.
    pub const LOAD_EFER: u32 = 1 << 21;
    /// The VMX-preemption timer value is saved on exit.
    pub const SAVE_PREEMPTION_TIMER: u32 = 1 << 22;
    /// The host's CET state is loaded on exit.
    pub const LOAD_CET_STATE: u32 = 1 << 28;
}

/// VM-entry controls.
pub mod entry {
    /// DR7 and IA32_DEBUGCTL are loaded on entry.
    pub const LOAD_DEBUG_CONTROLS: u32 = 1 << 2;
    /// The guest runs in IA-32e mode.
    pub const IA32E_MODE_GUEST: u32 = 1 << 9;
    /// IA32_PERF_GLOBAL_CTRL is loaded on entry.
    pub const LOAD_PERF_GLOBAL_CTRL: u32 = 1 << 13;
    /// The guest's IA32_PAT is loaded on entry.
    pub const LOAD_PAT: u32 = 1 << 14;
    /// The guest's IA32_EFER is loaded on entry.
    pub const LOAD_EFER: u32 = 1 << 15;
    /// The guest's CET state is loaded on entry.
    pub const LOAD_CET_STATE: u32 = 1 << 20;
}

/// The allowed settings of one 32-bit control field, from its capability MSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Allowed {
    /// Controls that must be 1.
    must_be_one: u32,
    /// Controls that may be 1.
    may_be_one: u32,
}

impl Allowed {
    /// The settings a capability MSR's value reports.
    pub const fn from_msr(value: u64) -> Self {
        let must_be_one = value as u32;
        // A control the CPU requires to be 1 is also allowed to be 1, whatever the MSR says.
        let may_be_one = (value >> 32) as u32 | must_be_one;
        Self {
            must_be_one,
            may_be_one,
        }
    }

    /// Controls that must be 1.
    pub const fn must_be_one(self) -> u32 {
        self.must_be_one
    }

    /// Controls that may be 1.
    pub const fn may_be_one(self) -> u32 {
        self.may_be_one
    }

    /// Whether every control in `controls` may be 1.
    pub const fn supports(self, controls: u32) -> bool {
        controls & !self.may_be_one == 0
    }

    /// The value to write: the `required` controls, those of the `optional` ones the CPU
    /// allows, and those the CPU requires.
    ///
    /// # Errors
    ///
    /// [`ControlError`] naming the required controls the CPU does not allow.
    pub const fn resolve(self, required: u32, optional: u32) -> Result<u32, ControlError> {
        let missing = required & !self.may_be_one;
        if missing != 0 {
            return Err(ControlError { missing });
        }
        Ok(required | (optional & self.may_be_one) | self.must_be_one)
    }

    /// Whether `value` is a setting the CPU accepts.
    pub const fn accepts(self, value: u32) -> bool {
        value & self.must_be_one == self.must_be_one && self.supports(value)
    }
}

/// Required controls the CPU does not support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("the CPU does not allow required VMX controls {missing:#010x}")]
pub struct ControlError {
    /// The unsupported controls.
    pub missing: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_adds_required_optional_and_fixed_bits() {
        // Bits 1, 2, 4 must be one; bits 0..=7 and 31 may be one.
        let allowed = Allowed::from_msr(0x8000_00ff_0000_0016);
        assert_eq!(allowed.must_be_one(), 0x16);
        let value = allowed
            .resolve(
                primary::HLT_EXITING | primary::ACTIVATE_SECONDARY,
                1 << 0 | 1 << 20,
            )
            .expect("supported");
        assert_eq!(value, 0x8000_0097);
        assert!(allowed.accepts(value));
        assert!(!allowed.accepts(0x80));
        assert_eq!(
            allowed.resolve(primary::USE_MSR_BITMAPS, 0),
            Err(ControlError {
                missing: primary::USE_MSR_BITMAPS
            })
        );
    }

    #[test]
    fn a_required_one_is_always_allowed() {
        // An MSR whose allowed-1 half leaves out a must-be-one bit still allows it.
        let allowed = Allowed::from_msr(0x0000_0000_0000_0002);
        assert!(allowed.supports(0x2));
        assert_eq!(allowed.resolve(0, 0), Ok(0x2));
    }
}
