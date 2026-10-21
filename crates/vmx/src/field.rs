//! VMCS field encodings: the 32-bit values `VMREAD` and `VMWRITE` take.
//!
//! An encoding (SDM Vol. 3C, "Field Encoding in VMCS", and appendix B) packs:
//!
//! | Bits | Meaning |
//! | --- | --- |
//! | 0 | access type: 0 full, 1 high (the upper 32 bits of a 64-bit field) |
//! | 9:1 | index |
//! | 11:10 | type: 0 control, 1 VM-exit information (read only), 2 guest state, 3 host state |
//! | 12 | reserved, 0 |
//! | 14:13 | width: 0 16-bit, 1 64-bit, 2 32-bit, 3 natural width |
//! | 31:15 | reserved, 0 |
//!
//! The constants below are the fields the hypervisor uses, grouped by width and type as in
//! appendix B. Each group is checked at compile time: a constant whose encoding does not decode
//! to its group's width and type does not build.

use thiserror::Error;

/// The width of a VMCS field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Width {
    /// 16 bits.
    Bits16,
    /// 64 bits.
    Bits64,
    /// 32 bits.
    Bits32,
    /// The width of the processor's natural word (64 bits on Intel 64).
    Natural,
}

/// What a VMCS field holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// VM-execution, VM-exit and VM-entry controls.
    Control,
    /// Information about the last VM exit; read only.
    ExitInfo,
    /// Guest state, loaded on entry and saved on exit.
    Guest,
    /// Host state, loaded on exit.
    Host,
}

/// A VMCS field encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Field(u32);

impl Field {
    /// The encoding `raw`.
    ///
    /// # Errors
    ///
    /// [`FieldError`] when a reserved bit is set, or the access type is "high" on a field that
    /// is not 64 bits wide.
    pub const fn from_raw(raw: u32) -> Result<Self, FieldError> {
        if raw & !0x6fff != 0 {
            return Err(FieldError::Reserved { raw });
        }
        let field = Self(raw);
        if field.is_high() && !matches!(field.width(), Width::Bits64) {
            return Err(FieldError::HighNot64 { raw });
        }
        Ok(field)
    }

    /// The encoding.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The field's width.
    pub const fn width(self) -> Width {
        match (self.0 >> 13) & 3 {
            0 => Width::Bits16,
            1 => Width::Bits64,
            2 => Width::Bits32,
            _ => Width::Natural,
        }
    }

    /// What the field holds.
    pub const fn kind(self) -> Kind {
        match (self.0 >> 10) & 3 {
            0 => Kind::Control,
            1 => Kind::ExitInfo,
            2 => Kind::Guest,
            _ => Kind::Host,
        }
    }

    /// The field's index within its width and type.
    pub const fn index(self) -> u32 {
        (self.0 >> 1) & 0x1ff
    }

    /// Whether this encoding names the upper half of a 64-bit field.
    pub const fn is_high(self) -> bool {
        self.0 & 1 != 0
    }

    /// The encoding of the upper 32 bits of this 64-bit field, for 32-bit access, or `None`
    /// when the field is not 64 bits wide or already names the upper half.
    pub const fn high(self) -> Option<Self> {
        if matches!(self.width(), Width::Bits64) && !self.is_high() {
            Some(Self(self.0 | 1))
        } else {
            None
        }
    }

    /// Whether a guest or the hypervisor may write the field with `VMWRITE`: every field but
    /// the VM-exit information fields, unless the CPU allows writing them (IA32_VMX_MISC bit
    /// 29).
    pub const fn writable(self, exit_info_writable: bool) -> bool {
        !matches!(self.kind(), Kind::ExitInfo) || exit_info_writable
    }
}

/// An invalid VMCS field encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FieldError {
    /// A reserved bit is set.
    #[error("VMCS field encoding {raw:#x} sets a reserved bit")]
    Reserved {
        /// The encoding.
        raw: u32,
    },
    /// The access type is "high" on a field that is not 64 bits wide.
    #[error("VMCS field encoding {raw:#x} asks for the high half of a field that has none")]
    HighNot64 {
        /// The encoding.
        raw: u32,
    },
}

/// Defines a group of fields of one width and type, and checks each at compile time.
macro_rules! fields {
    ($width:ident, $kind:ident; $($(#[$doc:meta])* $name:ident = $raw:literal,)*) => {
        $(
            $(#[$doc])*
            pub const $name: Field = Field($raw);
            const _: () = assert!(
                matches!($name.width(), Width::$width)
                    && matches!($name.kind(), Kind::$kind)
                    && !$name.is_high()
                    && $raw & !0x6fff == 0
            );
        )*
    };
}

fields! { Bits16, Control;
    /// Virtual-processor identifier.
    VPID = 0x0000,
    /// Posted-interrupt notification vector.
    POSTED_INTERRUPT_VECTOR = 0x0002,
    /// EPTP index.
    EPTP_INDEX = 0x0004,
}

fields! { Bits16, Guest;
    /// Guest ES selector.
    GUEST_ES_SELECTOR = 0x0800,
    /// Guest CS selector.
    GUEST_CS_SELECTOR = 0x0802,
    /// Guest SS selector.
    GUEST_SS_SELECTOR = 0x0804,
    /// Guest DS selector.
    GUEST_DS_SELECTOR = 0x0806,
    /// Guest FS selector.
    GUEST_FS_SELECTOR = 0x0808,
    /// Guest GS selector.
    GUEST_GS_SELECTOR = 0x080a,
    /// Guest LDTR selector.
    GUEST_LDTR_SELECTOR = 0x080c,
    /// Guest TR selector.
    GUEST_TR_SELECTOR = 0x080e,
    /// Guest interrupt status (virtual-interrupt delivery).
    GUEST_INTERRUPT_STATUS = 0x0810,
}

fields! { Bits16, Host;
    /// Host ES selector.
    HOST_ES_SELECTOR = 0x0c00,
    /// Host CS selector.
    HOST_CS_SELECTOR = 0x0c02,
    /// Host SS selector.
    HOST_SS_SELECTOR = 0x0c04,
    /// Host DS selector.
    HOST_DS_SELECTOR = 0x0c06,
    /// Host FS selector.
    HOST_FS_SELECTOR = 0x0c08,
    /// Host GS selector.
    HOST_GS_SELECTOR = 0x0c0a,
    /// Host TR selector.
    HOST_TR_SELECTOR = 0x0c0c,
}

fields! { Bits64, Control;
    /// Address of I/O bitmap A (ports 0000h–7FFFh).
    IO_BITMAP_A = 0x2000,
    /// Address of I/O bitmap B (ports 8000h–FFFFh).
    IO_BITMAP_B = 0x2002,
    /// Address of the MSR bitmaps.
    MSR_BITMAPS = 0x2004,
    /// VM-exit MSR-store address.
    EXIT_MSR_STORE_ADDR = 0x2006,
    /// VM-exit MSR-load address.
    EXIT_MSR_LOAD_ADDR = 0x2008,
    /// VM-entry MSR-load address.
    ENTRY_MSR_LOAD_ADDR = 0x200a,
    /// TSC offset.
    TSC_OFFSET = 0x2010,
    /// Virtual-APIC address.
    VIRTUAL_APIC_ADDR = 0x2012,
    /// APIC-access address.
    APIC_ACCESS_ADDR = 0x2014,
    /// Posted-interrupt descriptor address.
    POSTED_INTERRUPT_DESC_ADDR = 0x2016,
    /// EPT pointer.
    EPT_POINTER = 0x201a,
    /// XSS-exiting bitmap.
    XSS_EXITING_BITMAP = 0x202c,
    /// TSC multiplier.
    TSC_MULTIPLIER = 0x2032,
}

fields! { Bits64, ExitInfo;
    /// Guest-physical address of an EPT violation or misconfiguration.
    GUEST_PHYSICAL_ADDR = 0x2400,
}

fields! { Bits64, Guest;
    /// VMCS link pointer; all ones when VMCS shadowing is not used.
    VMCS_LINK_POINTER = 0x2800,
    /// Guest IA32_DEBUGCTL.
    GUEST_DEBUGCTL = 0x2802,
    /// Guest IA32_PAT.
    GUEST_PAT = 0x2804,
    /// Guest IA32_EFER.
    GUEST_EFER = 0x2806,
    /// Guest IA32_PERF_GLOBAL_CTRL.
    GUEST_PERF_GLOBAL_CTRL = 0x2808,
    /// Guest PDPTE0 (PAE paging with EPT).
    GUEST_PDPTE0 = 0x280a,
    /// Guest PDPTE1.
    GUEST_PDPTE1 = 0x280c,
    /// Guest PDPTE2.
    GUEST_PDPTE2 = 0x280e,
    /// Guest PDPTE3.
    GUEST_PDPTE3 = 0x2810,
}

fields! { Bits64, Host;
    /// Host IA32_PAT.
    HOST_PAT = 0x2c00,
    /// Host IA32_EFER.
    HOST_EFER = 0x2c02,
    /// Host IA32_PERF_GLOBAL_CTRL.
    HOST_PERF_GLOBAL_CTRL = 0x2c04,
}

fields! { Bits32, Control;
    /// Pin-based VM-execution controls.
    PIN_BASED_CONTROLS = 0x4000,
    /// Primary processor-based VM-execution controls.
    PRIMARY_CONTROLS = 0x4002,
    /// Exception bitmap.
    EXCEPTION_BITMAP = 0x4004,
    /// Page-fault error-code mask.
    PAGE_FAULT_ERROR_MASK = 0x4006,
    /// Page-fault error-code match.
    PAGE_FAULT_ERROR_MATCH = 0x4008,
    /// CR3-target count.
    CR3_TARGET_COUNT = 0x400a,
    /// Primary VM-exit controls.
    EXIT_CONTROLS = 0x400c,
    /// VM-exit MSR-store count.
    EXIT_MSR_STORE_COUNT = 0x400e,
    /// VM-exit MSR-load count.
    EXIT_MSR_LOAD_COUNT = 0x4010,
    /// VM-entry controls.
    ENTRY_CONTROLS = 0x4012,
    /// VM-entry MSR-load count.
    ENTRY_MSR_LOAD_COUNT = 0x4014,
    /// VM-entry interruption-information field: the event to inject.
    ENTRY_INTERRUPTION_INFO = 0x4016,
    /// VM-entry exception error code.
    ENTRY_EXCEPTION_ERROR_CODE = 0x4018,
    /// VM-entry instruction length (software interrupts and exceptions).
    ENTRY_INSTRUCTION_LEN = 0x401a,
    /// TPR threshold.
    TPR_THRESHOLD = 0x401c,
    /// Secondary processor-based VM-execution controls.
    SECONDARY_CONTROLS = 0x401e,
    /// PAUSE-loop exiting gap.
    PLE_GAP = 0x4020,
    /// PAUSE-loop exiting window.
    PLE_WINDOW = 0x4022,
}

fields! { Bits32, ExitInfo;
    /// VM-instruction error number.
    INSTRUCTION_ERROR = 0x4400,
    /// Exit reason.
    EXIT_REASON = 0x4402,
    /// VM-exit interruption information.
    EXIT_INTERRUPTION_INFO = 0x4404,
    /// VM-exit interruption error code.
    EXIT_INTERRUPTION_ERROR_CODE = 0x4406,
    /// IDT-vectoring information: an event that was being delivered when the exit happened.
    IDT_VECTORING_INFO = 0x4408,
    /// IDT-vectoring error code.
    IDT_VECTORING_ERROR_CODE = 0x440a,
    /// VM-exit instruction length.
    EXIT_INSTRUCTION_LEN = 0x440c,
    /// VM-exit instruction information.
    EXIT_INSTRUCTION_INFO = 0x440e,
}

fields! { Bits32, Guest;
    /// Guest ES limit.
    GUEST_ES_LIMIT = 0x4800,
    /// Guest CS limit.
    GUEST_CS_LIMIT = 0x4802,
    /// Guest SS limit.
    GUEST_SS_LIMIT = 0x4804,
    /// Guest DS limit.
    GUEST_DS_LIMIT = 0x4806,
    /// Guest FS limit.
    GUEST_FS_LIMIT = 0x4808,
    /// Guest GS limit.
    GUEST_GS_LIMIT = 0x480a,
    /// Guest LDTR limit.
    GUEST_LDTR_LIMIT = 0x480c,
    /// Guest TR limit.
    GUEST_TR_LIMIT = 0x480e,
    /// Guest GDTR limit.
    GUEST_GDTR_LIMIT = 0x4810,
    /// Guest IDTR limit.
    GUEST_IDTR_LIMIT = 0x4812,
    /// Guest ES access rights.
    GUEST_ES_ACCESS_RIGHTS = 0x4814,
    /// Guest CS access rights.
    GUEST_CS_ACCESS_RIGHTS = 0x4816,
    /// Guest SS access rights.
    GUEST_SS_ACCESS_RIGHTS = 0x4818,
    /// Guest DS access rights.
    GUEST_DS_ACCESS_RIGHTS = 0x481a,
    /// Guest FS access rights.
    GUEST_FS_ACCESS_RIGHTS = 0x481c,
    /// Guest GS access rights.
    GUEST_GS_ACCESS_RIGHTS = 0x481e,
    /// Guest LDTR access rights.
    GUEST_LDTR_ACCESS_RIGHTS = 0x4820,
    /// Guest TR access rights.
    GUEST_TR_ACCESS_RIGHTS = 0x4822,
    /// Guest interruptibility state.
    GUEST_INTERRUPTIBILITY = 0x4824,
    /// Guest activity state.
    GUEST_ACTIVITY_STATE = 0x4826,
    /// Guest SMBASE.
    GUEST_SMBASE = 0x4828,
    /// Guest IA32_SYSENTER_CS.
    GUEST_SYSENTER_CS = 0x482a,
    /// VMX-preemption timer value.
    PREEMPTION_TIMER_VALUE = 0x482e,
}

fields! { Bits32, Host;
    /// Host IA32_SYSENTER_CS.
    HOST_SYSENTER_CS = 0x4c00,
}

fields! { Natural, Control;
    /// CR0 guest/host mask: bits the guest may not change without an exit.
    CR0_GUEST_HOST_MASK = 0x6000,
    /// CR4 guest/host mask.
    CR4_GUEST_HOST_MASK = 0x6002,
    /// CR0 read shadow: what the guest reads for masked bits.
    CR0_READ_SHADOW = 0x6004,
    /// CR4 read shadow.
    CR4_READ_SHADOW = 0x6006,
}

fields! { Natural, ExitInfo;
    /// Exit qualification.
    EXIT_QUALIFICATION = 0x6400,
    /// Guest-linear address.
    GUEST_LINEAR_ADDR = 0x640a,
}

fields! { Natural, Guest;
    /// Guest CR0.
    GUEST_CR0 = 0x6800,
    /// Guest CR3.
    GUEST_CR3 = 0x6802,
    /// Guest CR4.
    GUEST_CR4 = 0x6804,
    /// Guest ES base.
    GUEST_ES_BASE = 0x6806,
    /// Guest CS base.
    GUEST_CS_BASE = 0x6808,
    /// Guest SS base.
    GUEST_SS_BASE = 0x680a,
    /// Guest DS base.
    GUEST_DS_BASE = 0x680c,
    /// Guest FS base.
    GUEST_FS_BASE = 0x680e,
    /// Guest GS base.
    GUEST_GS_BASE = 0x6810,
    /// Guest LDTR base.
    GUEST_LDTR_BASE = 0x6812,
    /// Guest TR base.
    GUEST_TR_BASE = 0x6814,
    /// Guest GDTR base.
    GUEST_GDTR_BASE = 0x6816,
    /// Guest IDTR base.
    GUEST_IDTR_BASE = 0x6818,
    /// Guest DR7.
    GUEST_DR7 = 0x681a,
    /// Guest RSP.
    GUEST_RSP = 0x681c,
    /// Guest RIP.
    GUEST_RIP = 0x681e,
    /// Guest RFLAGS.
    GUEST_RFLAGS = 0x6820,
    /// Guest pending debug exceptions.
    GUEST_PENDING_DEBUG = 0x6822,
    /// Guest IA32_SYSENTER_ESP.
    GUEST_SYSENTER_ESP = 0x6824,
    /// Guest IA32_SYSENTER_EIP.
    GUEST_SYSENTER_EIP = 0x6826,
    /// Guest IA32_S_CET.
    GUEST_S_CET = 0x6828,
    /// Guest shadow-stack pointer.
    GUEST_SSP = 0x682a,
    /// Guest IA32_INTERRUPT_SSP_TABLE_ADDR.
    GUEST_INTERRUPT_SSP_TABLE = 0x682c,
}

fields! { Natural, Host;
    /// Host CR0.
    HOST_CR0 = 0x6c00,
    /// Host CR3.
    HOST_CR3 = 0x6c02,
    /// Host CR4.
    HOST_CR4 = 0x6c04,
    /// Host FS base.
    HOST_FS_BASE = 0x6c06,
    /// Host GS base.
    HOST_GS_BASE = 0x6c08,
    /// Host TR base.
    HOST_TR_BASE = 0x6c0a,
    /// Host GDTR base.
    HOST_GDTR_BASE = 0x6c0c,
    /// Host IDTR base.
    HOST_IDTR_BASE = 0x6c0e,
    /// Host IA32_SYSENTER_ESP.
    HOST_SYSENTER_ESP = 0x6c10,
    /// Host IA32_SYSENTER_EIP.
    HOST_SYSENTER_EIP = 0x6c12,
    /// Host RSP.
    HOST_RSP = 0x6c14,
    /// Host RIP: where a VM exit continues.
    HOST_RIP = 0x6c16,
    /// Host IA32_S_CET.
    HOST_S_CET = 0x6c18,
    /// Host shadow-stack pointer.
    HOST_SSP = 0x6c1a,
    /// Host IA32_INTERRUPT_SSP_TABLE_ADDR.
    HOST_INTERRUPT_SSP_TABLE = 0x6c1c,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_decode() {
        assert_eq!(GUEST_RIP.width(), Width::Natural);
        assert_eq!(GUEST_RIP.kind(), Kind::Guest);
        assert_eq!(GUEST_RIP.index(), 0x0f);
        assert_eq!(EXIT_REASON.kind(), Kind::ExitInfo);
        assert_eq!(EPT_POINTER.high().map(Field::raw), Some(0x201b));
        assert_eq!(GUEST_RIP.high(), None);
        assert!(!EXIT_QUALIFICATION.writable(false));
        assert!(EXIT_QUALIFICATION.writable(true));
        assert!(GUEST_RIP.writable(false));
    }

    #[test]
    fn raw_encodings_are_checked() {
        assert_eq!(Field::from_raw(0x681e), Ok(GUEST_RIP));
        assert_eq!(Field::from_raw(0x201b).map(Field::is_high), Ok(true));
        assert_eq!(
            Field::from_raw(0x1000),
            Err(FieldError::Reserved { raw: 0x1000 })
        );
        assert_eq!(
            Field::from_raw(0x8000),
            Err(FieldError::Reserved { raw: 0x8000 })
        );
        assert_eq!(
            Field::from_raw(0x681f),
            Err(FieldError::HighNot64 { raw: 0x681f })
        );
    }
}
