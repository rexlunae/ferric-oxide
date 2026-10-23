//! SVM capabilities (CPUID Fn8000_000A and Fn8000_0001) and the MSRs that enable SVM, and
//! the `VMRUN` consistency checks that depend on state a VMM sets.

use thiserror::Error;

use crate::exit::ExitCode;
use crate::vmcb::{self, SegmentRegister, Vmcb};

/// VM_CR: bit 4 (SVMDIS) set means firmware disabled SVM.
pub const MSR_VM_CR: u32 = 0xc001_0114;
/// VM_HSAVE_PA: the physical address of the host save area.
pub const MSR_VM_HSAVE_PA: u32 = 0xc001_0117;
/// EFER.SVME.
pub const EFER_SVME: u64 = 1 << 12;
/// VM_CR.SVMDIS.
pub const VM_CR_SVMDIS: u64 = 1 << 4;

/// CPUID Fn8000_0001 ECX bit 2: SVM.
pub const fn has_svm(cpuid_8000_0001_ecx: u32) -> bool {
    cpuid_8000_0001_ecx & (1 << 2) != 0
}

/// CPUID Fn8000_000A, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    /// The SVM revision (EAX bits 7:0).
    pub revision: u8,
    /// The number of ASIDs (EBX); ASID 0 is the host's.
    pub asids: u32,
    edx: u32,
}

impl Features {
    /// Decodes the leaf's EAX, EBX and EDX.
    pub const fn new(eax: u32, ebx: u32, edx: u32) -> Self {
        Self {
            revision: eax as u8,
            asids: ebx,
            edx,
        }
    }

    const fn bit(self, n: u32) -> bool {
        self.edx & (1 << n) != 0
    }

    /// Nested paging.
    pub const fn nested_paging(self) -> bool {
        self.bit(0)
    }

    /// LBR virtualization.
    pub const fn lbr_virtualization(self) -> bool {
        self.bit(1)
    }

    /// SVM lock.
    pub const fn svm_lock(self) -> bool {
        self.bit(2)
    }

    /// The next RIP is saved on intercepts (NRIP save).
    pub const fn nrip_save(self) -> bool {
        self.bit(3)
    }

    /// The TSC ratio MSR.
    pub const fn tsc_rate(self) -> bool {
        self.bit(4)
    }

    /// VMCB clean bits.
    pub const fn vmcb_clean(self) -> bool {
        self.bit(5)
    }

    /// TLB flush by ASID.
    pub const fn flush_by_asid(self) -> bool {
        self.bit(6)
    }

    /// Decode assists.
    pub const fn decode_assists(self) -> bool {
        self.bit(7)
    }

    /// Pause filter.
    pub const fn pause_filter(self) -> bool {
        self.bit(10)
    }

    /// Pause filter threshold.
    pub const fn pause_filter_threshold(self) -> bool {
        self.bit(12)
    }

    /// AVIC.
    pub const fn avic(self) -> bool {
        self.bit(13)
    }

    /// Virtualized VMSAVE/VMLOAD.
    pub const fn vmsave_virtualization(self) -> bool {
        self.bit(15)
    }

    /// Virtual GIF.
    pub const fn vgif(self) -> bool {
        self.bit(16)
    }

    /// Whether the CPU has what the hypervisor needs: nested paging, NRIP save, flush by
    /// ASID, and at least two ASIDs.
    ///
    /// # Errors
    ///
    /// [`SvmError::Missing`] naming the first missing feature.
    pub const fn sufficient(self) -> Result<(), SvmError> {
        if !self.nested_paging() {
            return Err(SvmError::Missing {
                what: SvmFeature::NestedPaging,
            });
        }
        if !self.nrip_save() {
            return Err(SvmError::Missing {
                what: SvmFeature::NripSave,
            });
        }
        if !self.flush_by_asid() {
            return Err(SvmError::Missing {
                what: SvmFeature::FlushByAsid,
            });
        }
        if self.asids < 2 {
            return Err(SvmError::Missing {
                what: SvmFeature::GuestAsids,
            });
        }
        Ok(())
    }
}

/// Whether firmware left SVM usable, from VM_CR.
pub const fn firmware_allows(vm_cr: u64) -> bool {
    vm_cr & VM_CR_SVMDIS == 0
}

const CR0_PE: u64 = 1 << 0;
const CR0_NW: u64 = 1 << 29;
const CR0_CD: u64 = 1 << 30;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;

/// Runs the `VMRUN` consistency checks (APM Vol. 2, "Canonicalization and Consistency Checks")
/// that depend on guest state and controls a VMM sets, so the kernel reports a named reason
/// instead of `VMEXIT_INVALID`. `asids` is [`Features::asids`].
///
/// # Errors
///
/// The first [`SvmError`] found.
pub fn check(vmcb: &Vmcb, asids: u32) -> Result<(), SvmError> {
    let efer = vmcb.read(vmcb::EFER);
    let cr0 = vmcb.read(vmcb::CR0);
    let cr4 = vmcb.read(vmcb::CR4);
    if efer & EFER_SVME == 0 {
        return Err(SvmError::SvmeClear);
    }
    if cr0 & CR0_NW != 0 && cr0 & CR0_CD == 0 {
        return Err(SvmError::Cr0CacheBits);
    }
    if cr0 >> 32 != 0 {
        return Err(SvmError::Cr0High);
    }
    if vmcb.read(vmcb::DR6) >> 32 != 0 || vmcb.read(vmcb::DR7) >> 32 != 0 {
        return Err(SvmError::DebugHigh);
    }
    if efer & EFER_LME != 0 && cr0 & CR0_PG != 0 {
        if cr4 & CR4_PAE == 0 || cr0 & CR0_PE == 0 {
            return Err(SvmError::LongModePaging);
        }
        let cs = vmcb.segment(SegmentRegister::Cs);
        if cs.long() && cs.default_big() {
            return Err(SvmError::CodeSegmentLAndD);
        }
    }
    if !vmcb.intercepts(ExitCode::VMRUN) {
        return Err(SvmError::VmrunNotIntercepted);
    }
    let asid = vmcb.read(vmcb::ASID) as u32;
    if asid == 0 || asid >= asids {
        return Err(SvmError::Asid { asid });
    }
    Ok(())
}

/// An SVM feature the hypervisor needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SvmFeature {
    /// Nested paging.
    NestedPaging,
    /// NRIP save.
    NripSave,
    /// TLB flush by ASID.
    FlushByAsid,
    /// At least one ASID besides the host's.
    GuestAsids,
}

/// Why SVM cannot be used, or why `VMRUN` would fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SvmError {
    /// A feature the hypervisor needs is missing.
    #[error("the CPU lacks an SVM feature the hypervisor needs: {what:?}")]
    Missing {
        /// The feature.
        what: SvmFeature,
    },
    /// Guest EFER.SVME is clear.
    #[error("guest EFER.SVME must be set")]
    SvmeClear,
    /// CR0.NW without CR0.CD.
    #[error("guest CR0 sets NW without CD")]
    Cr0CacheBits,
    /// Bits 63:32 of CR0 are set.
    #[error("guest CR0 bits 63:32 must be zero")]
    Cr0High,
    /// Bits 63:32 of DR6 or DR7 are set.
    #[error("guest DR6 and DR7 bits 63:32 must be zero")]
    DebugHigh,
    /// Long mode with paging but without CR4.PAE or CR0.PE.
    #[error("guest long mode with paging needs CR4.PAE and CR0.PE")]
    LongModePaging,
    /// Long mode with CS.L and CS.D both set.
    #[error("guest CS has both L and D set in long mode")]
    CodeSegmentLAndD,
    /// The VMRUN intercept is clear.
    #[error("the VMRUN intercept must be set")]
    VmrunNotIntercepted,
    /// The ASID is 0 (the host's) or beyond the CPU's count.
    #[error("guest ASID {asid} is not valid")]
    Asid {
        /// The ASID.
        asid: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmcb::Segment;

    fn long_mode() -> Vmcb {
        let mut vmcb = Vmcb::new();
        vmcb.write(vmcb::EFER, EFER_SVME | EFER_LME | (1 << 10))
            .expect("fits");
        vmcb.write(vmcb::CR0, 0x8000_0011).expect("fits");
        vmcb.write(vmcb::CR4, CR4_PAE).expect("fits");
        vmcb.write(vmcb::ASID, 1).expect("fits");
        vmcb.set_intercept(ExitCode::VMRUN, true)
            .expect("interceptable");
        vmcb.set_segment(
            SegmentRegister::Cs,
            Segment {
                selector: 8,
                attributes: 0x029b,
                limit: u32::MAX,
                base: 0,
            },
        );
        vmcb
    }

    #[test]
    fn features_decode() {
        let features = Features::new(1, 0x8000, 0b1_0100_1111_1111);
        assert!(features.nested_paging() && features.nrip_save() && features.flush_by_asid());
        assert!(features.vmcb_clean() && features.decode_assists() && features.pause_filter());
        assert!(!features.avic());
        assert_eq!(features.sufficient(), Ok(()));
        assert_eq!(
            Features::new(1, 1, 0xff).sufficient(),
            Err(SvmError::Missing {
                what: SvmFeature::GuestAsids
            })
        );
        assert_eq!(
            Features::new(1, 16, 0b110).sufficient(),
            Err(SvmError::Missing {
                what: SvmFeature::NestedPaging
            })
        );
        assert!(has_svm(1 << 2));
        assert!(firmware_allows(0));
        assert!(!firmware_allows(VM_CR_SVMDIS));
    }

    #[test]
    fn consistency_checks() {
        assert_eq!(check(&long_mode(), 16), Ok(()));
        let mut bad = long_mode();
        bad.write(vmcb::EFER, EFER_LME).expect("fits");
        assert_eq!(check(&bad, 16), Err(SvmError::SvmeClear));
        let mut bad = long_mode();
        bad.write(vmcb::CR0, 0xa000_0011).expect("fits");
        assert_eq!(check(&bad, 16), Err(SvmError::Cr0CacheBits));
        let mut bad = long_mode();
        bad.write(vmcb::CR0, (1 << 40) | 0x8000_0011).expect("fits");
        assert_eq!(check(&bad, 16), Err(SvmError::Cr0High));
        let mut bad = long_mode();
        bad.write(vmcb::DR7, 1 << 33).expect("fits");
        assert_eq!(check(&bad, 16), Err(SvmError::DebugHigh));
        let mut bad = long_mode();
        bad.write(vmcb::CR4, 0).expect("fits");
        assert_eq!(check(&bad, 16), Err(SvmError::LongModePaging));
        let mut bad = long_mode();
        bad.set_segment(
            SegmentRegister::Cs,
            Segment {
                attributes: 0x069b,
                ..bad.segment(SegmentRegister::Cs)
            },
        );
        assert_eq!(check(&bad, 16), Err(SvmError::CodeSegmentLAndD));
        let mut bad = long_mode();
        bad.set_intercept(ExitCode::VMRUN, false)
            .expect("interceptable");
        assert_eq!(check(&bad, 16), Err(SvmError::VmrunNotIntercepted));
        let mut bad = long_mode();
        bad.write(vmcb::ASID, 0).expect("fits");
        assert_eq!(check(&bad, 16), Err(SvmError::Asid { asid: 0 }));
        assert_eq!(check(&long_mode(), 1), Err(SvmError::Asid { asid: 1 }));
    }
}
