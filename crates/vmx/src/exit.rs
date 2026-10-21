//! VM-exit reasons, exit qualifications and interruption information (SDM Vol. 3C, appendix C
//! "VMX Basic Exit Reasons", and the exit-information sections of the VMCS chapter).

/// The exit reason field, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitReason {
    /// The basic exit reason.
    pub basic: BasicReason,
    /// The VM entry failed (bit 31): the guest never ran.
    pub entry_failure: bool,
}

impl ExitReason {
    /// Decodes the EXIT_REASON field.
    pub const fn from_raw(raw: u32) -> Self {
        Self {
            basic: BasicReason::from_raw((raw & 0xffff) as u16),
            entry_failure: raw & (1 << 31) != 0,
        }
    }
}

macro_rules! reasons {
    ($($(#[$doc:meta])* $name:ident = $value:literal,)*) => {
        /// A basic exit reason.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum BasicReason {
            $($(#[$doc])* $name,)*
            /// A reason this crate does not name.
            Other(u16),
        }

        impl BasicReason {
            /// The reason numbered `raw`.
            pub const fn from_raw(raw: u16) -> Self {
                match raw {
                    $($value => Self::$name,)*
                    other => Self::Other(other),
                }
            }

            /// The reason's number.
            pub const fn raw(self) -> u16 {
                match self {
                    $(Self::$name => $value,)*
                    Self::Other(other) => other,
                }
            }
        }
    };
}

reasons! {
    /// An exception or NMI.
    ExceptionOrNmi = 0,
    /// An external interrupt.
    ExternalInterrupt = 1,
    /// A triple fault.
    TripleFault = 2,
    /// An INIT signal.
    Init = 3,
    /// A start-up IPI.
    Sipi = 4,
    /// The guest can take interrupts (interrupt-window exiting).
    InterruptWindow = 7,
    /// The guest can take NMIs (NMI-window exiting).
    NmiWindow = 8,
    /// A task switch.
    TaskSwitch = 9,
    /// CPUID.
    Cpuid = 10,
    /// GETSEC.
    Getsec = 11,
    /// HLT.
    Hlt = 12,
    /// INVD.
    Invd = 13,
    /// INVLPG.
    Invlpg = 14,
    /// RDPMC.
    Rdpmc = 15,
    /// RDTSC.
    Rdtsc = 16,
    /// RSM in SMM.
    Rsm = 17,
    /// VMCALL.
    Vmcall = 18,
    /// VMCLEAR.
    Vmclear = 19,
    /// VMLAUNCH.
    Vmlaunch = 20,
    /// VMPTRLD.
    Vmptrld = 21,
    /// VMPTRST.
    Vmptrst = 22,
    /// VMREAD.
    Vmread = 23,
    /// VMRESUME.
    Vmresume = 24,
    /// VMWRITE.
    Vmwrite = 25,
    /// VMXOFF.
    Vmxoff = 26,
    /// VMXON.
    Vmxon = 27,
    /// A control-register access.
    CrAccess = 28,
    /// MOV DR.
    MovDr = 29,
    /// An I/O instruction.
    Io = 30,
    /// RDMSR.
    Rdmsr = 31,
    /// WRMSR.
    Wrmsr = 32,
    /// VM entry failed: invalid guest state.
    InvalidGuestState = 33,
    /// VM entry failed: MSR loading.
    MsrLoading = 34,
    /// MWAIT.
    Mwait = 36,
    /// Monitor trap flag.
    MonitorTrapFlag = 37,
    /// MONITOR.
    Monitor = 39,
    /// PAUSE.
    Pause = 40,
    /// VM entry failed: machine-check event.
    MachineCheck = 41,
    /// TPR below threshold.
    TprBelowThreshold = 43,
    /// An access to the APIC-access page.
    ApicAccess = 44,
    /// A virtualized EOI.
    VirtualizedEoi = 45,
    /// GDTR or IDTR access.
    GdtrIdtrAccess = 46,
    /// LDTR or TR access.
    LdtrTrAccess = 47,
    /// An EPT violation.
    EptViolation = 48,
    /// An EPT misconfiguration.
    EptMisconfig = 49,
    /// INVEPT.
    Invept = 50,
    /// RDTSCP.
    Rdtscp = 51,
    /// The VMX-preemption timer expired.
    PreemptionTimer = 52,
    /// INVVPID.
    Invvpid = 53,
    /// WBINVD or WBNOINVD.
    Wbinvd = 54,
    /// XSETBV.
    Xsetbv = 55,
    /// An APIC write (APIC-register virtualization).
    ApicWrite = 56,
    /// RDRAND.
    Rdrand = 57,
    /// INVPCID.
    Invpcid = 58,
    /// VMFUNC.
    Vmfunc = 59,
    /// ENCLS.
    Encls = 60,
    /// RDSEED.
    Rdseed = 61,
    /// The page-modification log is full.
    PmlFull = 62,
    /// XSAVES.
    Xsaves = 63,
    /// XRSTORS.
    Xrstors = 64,
}

/// The exit qualification of a control-register access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrAccess {
    /// The control register.
    pub cr: u8,
    /// What the instruction did.
    pub access: CrAccessType,
    /// The general-purpose register (0 RAX … 15 R15) for MOV to or from a CR.
    pub gpr: u8,
    /// The LMSW source data.
    pub lmsw_data: u16,
}

/// The kind of a control-register access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrAccessType {
    /// MOV to CR.
    MovTo,
    /// MOV from CR.
    MovFrom,
    /// CLTS.
    Clts,
    /// LMSW.
    Lmsw,
}

impl CrAccess {
    /// Decodes the exit qualification.
    pub const fn from_qualification(q: u64) -> Self {
        Self {
            cr: (q & 0xf) as u8,
            access: match (q >> 4) & 3 {
                0 => CrAccessType::MovTo,
                1 => CrAccessType::MovFrom,
                2 => CrAccessType::Clts,
                _ => CrAccessType::Lmsw,
            },
            gpr: ((q >> 8) & 0xf) as u8,
            lmsw_data: ((q >> 16) & 0xffff) as u16,
        }
    }
}

/// The exit qualification of an I/O instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoAccess {
    /// The access size in bytes: 1, 2 or 4.
    pub size: u8,
    /// IN (true) or OUT (false).
    pub input: bool,
    /// INS or OUTS.
    pub string: bool,
    /// With a REP prefix.
    pub rep: bool,
    /// The port came from an immediate operand rather than DX.
    pub immediate: bool,
    /// The port.
    pub port: u16,
}

impl IoAccess {
    /// Decodes the exit qualification.
    pub const fn from_qualification(q: u64) -> Self {
        Self {
            size: ((q & 7) + 1) as u8,
            input: q & (1 << 3) != 0,
            string: q & (1 << 4) != 0,
            rep: q & (1 << 5) != 0,
            immediate: q & (1 << 6) != 0,
            port: ((q >> 16) & 0xffff) as u16,
        }
    }
}

/// The exit qualification of an EPT violation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EptViolation {
    /// The access was a data read.
    pub read: bool,
    /// The access was a data write.
    pub write: bool,
    /// The access was an instruction fetch.
    pub fetch: bool,
    /// The guest-physical address was readable.
    pub readable: bool,
    /// The guest-physical address was writable.
    pub writable: bool,
    /// The guest-physical address was executable.
    pub executable: bool,
    /// The guest-linear address field is valid.
    pub linear_valid: bool,
    /// When `linear_valid`: the access was to the final translation of the linear address,
    /// not to a guest paging structure.
    pub final_translation: bool,
    /// NMI unblocking due to IRET.
    pub nmi_unblocking: bool,
}

impl EptViolation {
    /// Decodes the exit qualification.
    pub fn from_qualification(q: u64) -> Self {
        let bit = |n: u32| q & (1 << n) != 0;
        Self {
            read: bit(0),
            write: bit(1),
            fetch: bit(2),
            readable: bit(3),
            writable: bit(4),
            executable: bit(5),
            linear_valid: bit(7),
            final_translation: bit(7) && bit(8),
            nmi_unblocking: bit(12),
        }
    }
}

/// The type of an event in an interruption-information field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventType {
    /// An external interrupt.
    ExternalInterrupt,
    /// A non-maskable interrupt.
    Nmi,
    /// A hardware exception.
    HardwareException,
    /// A software interrupt (INT n).
    SoftwareInterrupt,
    /// A privileged software exception (INT1).
    PrivilegedSoftwareException,
    /// A software exception (INT3, INTO).
    SoftwareException,
    /// Another event (for example MTF on entry).
    Other,
}

impl EventType {
    const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::ExternalInterrupt,
            2 => Self::Nmi,
            3 => Self::HardwareException,
            4 => Self::SoftwareInterrupt,
            5 => Self::PrivilegedSoftwareException,
            6 => Self::SoftwareException,
            7 => Self::Other,
            _ => return None,
        })
    }

    const fn raw(self) -> u32 {
        match self {
            Self::ExternalInterrupt => 0,
            Self::Nmi => 2,
            Self::HardwareException => 3,
            Self::SoftwareInterrupt => 4,
            Self::PrivilegedSoftwareException => 5,
            Self::SoftwareException => 6,
            Self::Other => 7,
        }
    }
}

/// An event in an interruption-information field: the VM-entry field that injects one, or the
/// VM-exit and IDT-vectoring fields that report one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// The vector.
    pub vector: u8,
    /// The type.
    pub kind: EventType,
    /// An error code is delivered (in the matching error-code field).
    pub error_code: bool,
}

impl Event {
    /// The field's value for this event, with the valid bit set.
    pub const fn to_raw(self) -> u32 {
        self.vector as u32 | (self.kind.raw() << 8) | ((self.error_code as u32) << 11) | (1 << 31)
    }

    /// The event a field holds, or `None` when its valid bit is clear or its type is reserved.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        if raw & (1 << 31) == 0 {
            return None;
        }
        match EventType::from_raw((raw >> 8) & 7) {
            Some(kind) => Some(Self {
                vector: raw as u8,
                kind,
                error_code: raw & (1 << 11) != 0,
            }),
            None => None,
        }
    }

    /// An external interrupt with `vector`.
    pub const fn interrupt(vector: u8) -> Self {
        Self {
            vector,
            kind: EventType::ExternalInterrupt,
            error_code: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_round_trip() {
        for raw in 0..=80u16 {
            assert_eq!(BasicReason::from_raw(raw).raw(), raw);
        }
        let failed = ExitReason::from_raw(0x8000_0021);
        assert_eq!(failed.basic, BasicReason::InvalidGuestState);
        assert!(failed.entry_failure);
        assert_eq!(ExitReason::from_raw(48).basic, BasicReason::EptViolation);
        assert_eq!(BasicReason::from_raw(35), BasicReason::Other(35));
    }

    #[test]
    fn qualifications_decode() {
        // OUT DX, AL to port 0x3f8.
        let io = IoAccess::from_qualification(0x03f8_0000);
        assert_eq!(
            (io.size, io.input, io.port, io.immediate),
            (1, false, 0x3f8, false)
        );
        // IN EAX, 0x71 (immediate).
        let io = IoAccess::from_qualification(0x0071_0000 | 3 | 8 | 64);
        assert_eq!(
            (io.size, io.input, io.port, io.immediate),
            (4, true, 0x71, true)
        );
        // MOV CR3 <- RBX.
        let cr = CrAccess::from_qualification(3 | (3 << 8));
        assert_eq!((cr.cr, cr.access, cr.gpr), (3, CrAccessType::MovTo, 3));
        let cr = CrAccess::from_qualification(0x1234_0000 | (3 << 4));
        assert_eq!((cr.access, cr.lmsw_data), (CrAccessType::Lmsw, 0x1234));
        // A write to a read-only page, through a final translation.
        let ept = EptViolation::from_qualification(0b1_1000_1010);
        assert!(
            ept.write && ept.readable && !ept.writable && ept.linear_valid && ept.final_translation
        );
        assert!(!EptViolation::from_qualification(1 << 8).final_translation);
    }

    #[test]
    fn events_round_trip() {
        let page_fault = Event {
            vector: 14,
            kind: EventType::HardwareException,
            error_code: true,
        };
        assert_eq!(page_fault.to_raw(), 0x8000_0b0e);
        assert_eq!(Event::from_raw(page_fault.to_raw()), Some(page_fault));
        assert_eq!(
            Event::from_raw(Event::interrupt(0x30).to_raw()),
            Some(Event::interrupt(0x30))
        );
        assert_eq!(Event::from_raw(0x0000_0b0e), None);
        assert_eq!(Event::from_raw(0x8000_0130), None);
    }
}
