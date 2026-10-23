//! Checks of guest state before VM entry, so the kernel reports a named reason instead of an
//! opaque "invalid guest state" entry failure.
//!
//! These are a subset of the SDM's "Checks on Guest Control Registers, Debug Registers, and
//! MSRs" and "Checks on Guest RIP, RFLAGS, and SSP" (Vol. 3C, VM entries chapter): the ones that
//! depend on values a VMM sets through `SetRegs` and `SetSregs`. Segment, descriptor-table and
//! interruptibility checks follow with the kernel backend; until then the CPU's own checks
//! still catch them, with exit reason 33.

use thiserror::Error;

use crate::caps::{CR0_PE, CR0_PG, Capabilities};
use crate::controls::{entry, secondary};

const CR0_NW: u64 = 1 << 29;
const CR0_CD: u64 = 1 << 30;
const CR4_PAE: u64 = 1 << 5;
const CR4_PCIDE: u64 = 1 << 17;
const EFER_SCE: u64 = 1 << 0;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;
const EFER_NXE: u64 = 1 << 11;
const RFLAGS_FIXED_ONE: u64 = 1 << 1;
const RFLAGS_RESERVED: u64 = !0x3f_ffff | (1 << 15) | (1 << 5) | (1 << 3);
const RFLAGS_VM: u64 = 1 << 17;

/// The guest state and controls the checks look at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestState {
    /// Guest CR0.
    pub cr0: u64,
    /// Guest CR4.
    pub cr4: u64,
    /// Guest IA32_EFER (checked when the entry controls load it).
    pub efer: u64,
    /// Guest RFLAGS.
    pub rflags: u64,
    /// The VM-entry controls.
    pub entry_controls: u32,
    /// The secondary processor-based controls (0 when not activated).
    pub secondary_controls: u32,
    /// The activity state: 0 active, 1 HLT, 2 shutdown, 3 wait-for-SIPI.
    pub activity_state: u32,
    /// The VMCS link pointer.
    pub vmcs_link_pointer: u64,
}

/// Runs the checks against `caps`.
///
/// # Errors
///
/// The first [`GuestStateError`] found.
pub fn check(state: &GuestState, caps: &Capabilities) -> Result<(), GuestStateError> {
    let unrestricted = state.secondary_controls & secondary::UNRESTRICTED_GUEST != 0;
    let ia32e = state.entry_controls & entry::IA32E_MODE_GUEST != 0;
    if !caps.guest_cr0(unrestricted).accepts(state.cr0) {
        return Err(GuestStateError::Cr0Fixed);
    }
    if state.cr0 & CR0_PG != 0 && state.cr0 & CR0_PE == 0 {
        return Err(GuestStateError::PagingWithoutProtection);
    }
    if state.cr0 & CR0_NW != 0 && state.cr0 & CR0_CD == 0 {
        return Err(GuestStateError::NotWriteThroughWithoutCacheDisable);
    }
    if !caps.cr4.accepts(state.cr4) {
        return Err(GuestStateError::Cr4Fixed);
    }
    if ia32e && (state.cr0 & CR0_PG == 0 || state.cr4 & CR4_PAE == 0) {
        return Err(GuestStateError::LongModeWithoutPaging);
    }
    if !ia32e && state.cr4 & CR4_PCIDE != 0 {
        return Err(GuestStateError::PcidOutsideLongMode);
    }
    if state.entry_controls & entry::LOAD_EFER != 0 {
        check_efer(state, ia32e)?;
    }
    check_rflags(state, ia32e)?;
    if state.activity_state > 3 {
        return Err(GuestStateError::ActivityState {
            state: state.activity_state,
        });
    }
    if state.vmcs_link_pointer != u64::MAX {
        return Err(GuestStateError::VmcsLinkPointer);
    }
    Ok(())
}

fn check_efer(state: &GuestState, ia32e: bool) -> Result<(), GuestStateError> {
    if state.efer & !(EFER_SCE | EFER_LME | EFER_LMA | EFER_NXE) != 0 {
        return Err(GuestStateError::EferReserved);
    }
    let lma = state.efer & EFER_LMA != 0;
    let lme = state.efer & EFER_LME != 0;
    if lma != ia32e || (state.cr0 & CR0_PG != 0 && lme != lma) {
        return Err(GuestStateError::EferMode);
    }
    Ok(())
}

fn check_rflags(state: &GuestState, ia32e: bool) -> Result<(), GuestStateError> {
    if state.rflags & RFLAGS_RESERVED != 0 || state.rflags & RFLAGS_FIXED_ONE == 0 {
        return Err(GuestStateError::RflagsReserved);
    }
    if state.rflags & RFLAGS_VM != 0 && (ia32e || state.cr0 & CR0_PE == 0) {
        return Err(GuestStateError::Virtual8086);
    }
    Ok(())
}

/// A guest state VM entry would refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum GuestStateError {
    /// CR0 breaks the bits VMX fixes.
    #[error("guest CR0 breaks the bits VMX operation fixes")]
    Cr0Fixed,
    /// CR0.PG without CR0.PE.
    #[error("guest CR0 enables paging without protected mode")]
    PagingWithoutProtection,
    /// CR0.NW without CR0.CD.
    #[error("guest CR0 sets NW without CD")]
    NotWriteThroughWithoutCacheDisable,
    /// CR4 breaks the bits VMX fixes.
    #[error("guest CR4 breaks the bits VMX operation fixes")]
    Cr4Fixed,
    /// An IA-32e mode guest without CR0.PG and CR4.PAE.
    #[error("an IA-32e mode guest needs CR0.PG and CR4.PAE")]
    LongModeWithoutPaging,
    /// CR4.PCIDE outside IA-32e mode.
    #[error("guest CR4.PCIDE is set outside IA-32e mode")]
    PcidOutsideLongMode,
    /// A reserved IA32_EFER bit is set.
    #[error("guest IA32_EFER sets a reserved bit")]
    EferReserved,
    /// EFER.LMA does not match the IA-32e mode control, or EFER.LME does not match LMA with
    /// paging on.
    #[error("guest IA32_EFER.LMA/LME do not match the IA-32e mode guest control")]
    EferMode,
    /// A reserved RFLAGS bit is set, or bit 1 is clear.
    #[error("guest RFLAGS has reserved bits wrong")]
    RflagsReserved,
    /// RFLAGS.VM in IA-32e mode or with CR0.PE clear.
    #[error("guest RFLAGS.VM is set in IA-32e mode or real mode")]
    Virtual8086,
    /// An activity state that does not exist.
    #[error("guest activity state {state} is not valid")]
    ActivityState {
        /// The state.
        state: u32,
    },
    /// The VMCS link pointer is not all ones (VMCS shadowing is not used).
    #[error("the VMCS link pointer must be all ones")]
    VmcsLinkPointer,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::tests::modern;

    fn long_mode() -> GuestState {
        GuestState {
            cr0: 0x8000_0031,
            cr4: 0x2000 | CR4_PAE,
            efer: EFER_LME | EFER_LMA | EFER_NXE | EFER_SCE,
            rflags: 0x2,
            entry_controls: entry::IA32E_MODE_GUEST | entry::LOAD_EFER,
            secondary_controls: secondary::ENABLE_EPT | secondary::UNRESTRICTED_GUEST,
            activity_state: 0,
            vmcs_link_pointer: u64::MAX,
        }
    }

    fn caps() -> Capabilities {
        Capabilities::new(&modern()).expect("usable")
    }

    #[test]
    fn a_long_mode_guest_passes() {
        assert_eq!(check(&long_mode(), &caps()), Ok(()));
    }

    #[test]
    fn a_real_mode_guest_needs_unrestricted() {
        let real = GuestState {
            cr0: 0x0000_0030,
            cr4: 0x2000,
            efer: 0,
            entry_controls: entry::LOAD_EFER,
            ..long_mode()
        };
        assert_eq!(check(&real, &caps()), Ok(()));
        let restricted = GuestState {
            secondary_controls: secondary::ENABLE_EPT,
            ..real
        };
        assert_eq!(check(&restricted, &caps()), Err(GuestStateError::Cr0Fixed));
    }

    #[test]
    fn each_check_names_its_failure() {
        let caps = caps();
        let cases: [(GuestState, GuestStateError); 10] = [
            (
                GuestState {
                    cr0: 0x8000_0030,
                    ..long_mode()
                },
                GuestStateError::PagingWithoutProtection,
            ),
            (
                GuestState {
                    cr0: 0xa000_0031,
                    ..long_mode()
                },
                GuestStateError::NotWriteThroughWithoutCacheDisable,
            ),
            (
                GuestState {
                    cr4: CR4_PAE,
                    ..long_mode()
                },
                GuestStateError::Cr4Fixed,
            ),
            (
                GuestState {
                    cr4: 0x2000,
                    ..long_mode()
                },
                GuestStateError::LongModeWithoutPaging,
            ),
            (
                GuestState {
                    entry_controls: entry::LOAD_EFER,
                    efer: 0,
                    cr4: 0x2000 | CR4_PAE | CR4_PCIDE,
                    ..long_mode()
                },
                GuestStateError::PcidOutsideLongMode,
            ),
            (
                GuestState {
                    efer: EFER_LMA | EFER_LME | (1 << 20),
                    ..long_mode()
                },
                GuestStateError::EferReserved,
            ),
            (
                GuestState {
                    efer: EFER_LMA,
                    ..long_mode()
                },
                GuestStateError::EferMode,
            ),
            (
                GuestState {
                    rflags: 0,
                    ..long_mode()
                },
                GuestStateError::RflagsReserved,
            ),
            (
                GuestState {
                    rflags: 0x2 | RFLAGS_VM,
                    ..long_mode()
                },
                GuestStateError::Virtual8086,
            ),
            (
                GuestState {
                    vmcs_link_pointer: 0,
                    ..long_mode()
                },
                GuestStateError::VmcsLinkPointer,
            ),
        ];
        for (state, error) in cases {
            assert_eq!(check(&state, &caps), Err(error), "{state:?}");
        }
        assert_eq!(
            check(
                &GuestState {
                    activity_state: 4,
                    ..long_mode()
                },
                &caps
            ),
            Err(GuestStateError::ActivityState { state: 4 })
        );
    }
}
