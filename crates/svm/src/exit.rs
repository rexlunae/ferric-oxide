//! `#VMEXIT` codes, their EXITINFO decodings, and the event-injection format (AMD64 APM Vol. 2,
//! "SVM Intercept Exit Codes" appendix and the SVM chapter's intercept and event-injection
//! sections).
//!
//! Exit codes 60h–7Fh correspond one to one to the bits of intercept vector 3, and 80h–9Fh to
//! vector 4; [`crate::vmcb::Vmcb::set_intercept`] relies on that.

/// A `#VMEXIT` code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExitCode(u64);

macro_rules! codes {
    ($($(#[$doc:meta])* $name:ident = $value:literal,)*) => {
        impl ExitCode {
            $($(#[$doc])* pub const $name: Self = Self($value);)*
        }
    };
}

codes! {
    /// A physical interrupt.
    INTR = 0x60,
    /// A physical NMI.
    NMI = 0x61,
    /// A physical SMI.
    SMI = 0x62,
    /// A physical INIT.
    INIT = 0x63,
    /// A virtual interrupt became deliverable.
    VINTR = 0x64,
    /// A CR0 write other than to CR0.TS or CR0.MP.
    CR0_SEL_WRITE = 0x65,
    /// RDTSC.
    RDTSC = 0x6e,
    /// RDPMC.
    RDPMC = 0x6f,
    /// PUSHF.
    PUSHF = 0x70,
    /// POPF.
    POPF = 0x71,
    /// CPUID.
    CPUID = 0x72,
    /// RSM.
    RSM = 0x73,
    /// IRET.
    IRET = 0x74,
    /// INT n.
    SWINT = 0x75,
    /// INVD.
    INVD = 0x76,
    /// PAUSE.
    PAUSE = 0x77,
    /// HLT.
    HLT = 0x78,
    /// INVLPG.
    INVLPG = 0x79,
    /// INVLPGA.
    INVLPGA = 0x7a,
    /// IN or OUT (IOIO_PROT).
    IOIO = 0x7b,
    /// RDMSR or WRMSR (MSR_PROT).
    MSR = 0x7c,
    /// A task switch.
    TASK_SWITCH = 0x7d,
    /// FERR freeze.
    FERR_FREEZE = 0x7e,
    /// Shutdown (for example a triple fault).
    SHUTDOWN = 0x7f,
    /// VMRUN.
    VMRUN = 0x80,
    /// VMMCALL.
    VMMCALL = 0x81,
    /// VMLOAD.
    VMLOAD = 0x82,
    /// VMSAVE.
    VMSAVE = 0x83,
    /// STGI.
    STGI = 0x84,
    /// CLGI.
    CLGI = 0x85,
    /// SKINIT.
    SKINIT = 0x86,
    /// RDTSCP.
    RDTSCP = 0x87,
    /// ICEBP.
    ICEBP = 0x88,
    /// WBINVD or WBNOINVD.
    WBINVD = 0x89,
    /// MONITOR or MONITORX.
    MONITOR = 0x8a,
    /// MWAIT or MWAITX.
    MWAIT = 0x8b,
    /// MWAIT with the monitor armed.
    MWAIT_CONDITIONAL = 0x8c,
    /// XSETBV.
    XSETBV = 0x8d,
    /// A nested page fault.
    NPF = 0x400,
    /// AVIC: an IPI could not be completed.
    AVIC_INCOMPLETE_IPI = 0x401,
    /// AVIC: an APIC access was not accelerated.
    AVIC_NOACCEL = 0x402,
    /// VMGEXIT (SEV-ES).
    VMGEXIT = 0x403,
    /// VMRUN found invalid guest state (`VMEXIT_INVALID`, −1).
    INVALID = 0xffff_ffff_ffff_ffff,
}

impl ExitCode {
    /// The code `raw`, as read from the VMCB.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw code.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// A read of CR `n` (0–15).
    pub const fn read_cr(n: u8) -> Self {
        Self((n & 0xf) as u64)
    }

    /// A write of CR `n` (0–15).
    pub const fn write_cr(n: u8) -> Self {
        Self(0x10 + (n & 0xf) as u64)
    }

    /// An exception with vector `n` (0–31).
    pub const fn exception(n: u8) -> Self {
        Self(0x40 + (n & 0x1f) as u64)
    }

    /// What the code means.
    pub const fn kind(self) -> ExitKind {
        match self.0 {
            0x00..=0x0f => ExitKind::ReadCr(self.0 as u8),
            0x10..=0x1f => ExitKind::WriteCr((self.0 - 0x10) as u8),
            0x20..=0x2f => ExitKind::ReadDr((self.0 - 0x20) as u8),
            0x30..=0x3f => ExitKind::WriteDr((self.0 - 0x30) as u8),
            0x40..=0x5f => ExitKind::Exception((self.0 - 0x40) as u8),
            0x90..=0x9f => ExitKind::WriteCrTrap((self.0 - 0x90) as u8),
            0xffff_ffff_ffff_ffff => ExitKind::Invalid,
            _ => ExitKind::Other,
        }
    }
}

/// The families of exit codes that carry a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExitKind {
    /// A read of a control register.
    ReadCr(u8),
    /// A write of a control register.
    WriteCr(u8),
    /// A read of a debug register.
    ReadDr(u8),
    /// A write of a debug register.
    WriteDr(u8),
    /// An exception vector.
    Exception(u8),
    /// A control-register write trapped after it completed.
    WriteCrTrap(u8),
    /// `VMEXIT_INVALID`.
    Invalid,
    /// One of the named single codes; compare with the [`ExitCode`] constants.
    Other,
}

/// EXITINFO1 of an IOIO intercept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoAccess {
    /// IN (true) or OUT (false).
    pub input: bool,
    /// INS or OUTS.
    pub string: bool,
    /// With a REP prefix.
    pub rep: bool,
    /// The operand size in bytes (1, 2 or 4), or 0 when the field is malformed.
    pub size: u8,
    /// The port.
    pub port: u16,
}

impl IoAccess {
    /// Decodes EXITINFO1.
    pub const fn from_info1(info: u64) -> Self {
        Self {
            input: info & 1 != 0,
            string: info & (1 << 2) != 0,
            rep: info & (1 << 3) != 0,
            size: match (info >> 4) & 7 {
                1 => 1,
                2 => 2,
                4 => 4,
                _ => 0,
            },
            port: (info >> 16) as u16,
        }
    }
}

/// EXITINFO1 of a nested page fault (EXITINFO2 holds the guest-physical address).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestedFault {
    /// The nested page was present (a protection fault, not a missing page).
    pub present: bool,
    /// The access was a write.
    pub write: bool,
    /// The access was from user mode.
    pub user: bool,
    /// A reserved bit was set in a nested page-table entry.
    pub reserved_bit: bool,
    /// The access was an instruction fetch.
    pub fetch: bool,
    /// The fault was on the final guest-physical address.
    pub final_translation: bool,
    /// The fault was on a guest page-table access during the guest's own walk.
    pub page_table_walk: bool,
}

impl NestedFault {
    /// Decodes EXITINFO1.
    pub const fn from_info1(info: u64) -> Self {
        Self {
            present: info & 1 != 0,
            write: info & (1 << 1) != 0,
            user: info & (1 << 2) != 0,
            reserved_bit: info & (1 << 3) != 0,
            fetch: info & (1 << 4) != 0,
            final_translation: info & (1 << 32) != 0,
            page_table_walk: info & (1 << 33) != 0,
        }
    }
}

/// The type of an injected or interrupted event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventType {
    /// An external or virtual interrupt.
    Interrupt,
    /// An NMI.
    Nmi,
    /// An exception.
    Exception,
    /// A software interrupt (INT n).
    SoftwareInterrupt,
}

impl EventType {
    const fn raw(self) -> u64 {
        match self {
            Self::Interrupt => 0,
            Self::Nmi => 2,
            Self::Exception => 3,
            Self::SoftwareInterrupt => 4,
        }
    }
}

/// An event in the EVENTINJ or EXITINTINFO format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Event {
    /// The vector.
    pub vector: u8,
    /// The type.
    pub kind: EventType,
    /// The error code to push, if any.
    pub error_code: Option<u32>,
}

impl Event {
    /// The field's value, with the valid bit set.
    pub const fn to_raw(self) -> u64 {
        let (ev, code) = match self.error_code {
            Some(code) => (1u64 << 11, (code as u64) << 32),
            None => (0, 0),
        };
        self.vector as u64 | (self.kind.raw() << 8) | ev | (1 << 31) | code
    }

    /// The event a field holds, or `None` when the valid bit is clear or the type is
    /// reserved.
    pub const fn from_raw(raw: u64) -> Option<Self> {
        if raw & (1 << 31) == 0 {
            return None;
        }
        let kind = match (raw >> 8) & 7 {
            0 => EventType::Interrupt,
            2 => EventType::Nmi,
            3 => EventType::Exception,
            4 => EventType::SoftwareInterrupt,
            _ => return None,
        };
        Some(Self {
            vector: raw as u8,
            kind,
            error_code: if raw & (1 << 11) != 0 {
                Some((raw >> 32) as u32)
            } else {
                None
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_codes() {
        assert_eq!(ExitCode::read_cr(3).kind(), ExitKind::ReadCr(3));
        assert_eq!(ExitCode::write_cr(8).raw(), 0x18);
        assert_eq!(ExitCode::from_raw(0x27).kind(), ExitKind::ReadDr(7));
        assert_eq!(ExitCode::from_raw(0x37).kind(), ExitKind::WriteDr(7));
        assert_eq!(ExitCode::exception(14).kind(), ExitKind::Exception(14));
        assert_eq!(ExitCode::from_raw(0x93).kind(), ExitKind::WriteCrTrap(3));
        assert_eq!(ExitCode::INVALID.kind(), ExitKind::Invalid);
        assert_eq!(ExitCode::from_raw(0x72), ExitCode::CPUID);
        assert_eq!(ExitCode::CPUID.kind(), ExitKind::Other);
    }

    #[test]
    fn exit_info_decodes() {
        // OUT DX, AL to 0x3f8 with a 64-bit address size.
        let out = IoAccess::from_info1(0x03f8_0000 | (1 << 4) | (1 << 9));
        assert_eq!((out.input, out.size, out.port), (false, 1, 0x3f8));
        // REP INSD from 0x1f0.
        let ins = IoAccess::from_info1(0x01f0_0000 | 1 | (1 << 2) | (1 << 3) | (1 << 6));
        assert_eq!(
            (ins.input, ins.string, ins.rep, ins.size),
            (true, true, true, 4)
        );
        assert_eq!(IoAccess::from_info1(3 << 4).size, 0);
        let fault = NestedFault::from_info1((1 << 32) | 0b10);
        assert!(fault.write && !fault.present && fault.final_translation && !fault.page_table_walk);
    }

    #[test]
    fn events_round_trip() {
        let gp = Event {
            vector: 13,
            kind: EventType::Exception,
            error_code: Some(0x10),
        };
        assert_eq!(gp.to_raw(), 0x0000_0010_8000_0b0d);
        assert_eq!(Event::from_raw(gp.to_raw()), Some(gp));
        let timer = Event {
            vector: 0xec,
            kind: EventType::Interrupt,
            error_code: None,
        };
        assert_eq!(Event::from_raw(timer.to_raw()), Some(timer));
        assert_eq!(Event::from_raw(0xec), None);
        assert_eq!(Event::from_raw((1 << 31) | (5 << 8)), None);
    }
}
