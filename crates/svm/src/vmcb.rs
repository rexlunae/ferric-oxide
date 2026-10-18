//! The VMCB: the 4 KiB control block `VMRUN` takes (AMD64 APM Vol. 2, appendix B).
//!
//! The control area is at offsets 0–3FFh and the state-save area from 400h. [`Vmcb`] wraps the
//! page as bytes with typed fields, so no layout is written by hand twice. Writing a field
//! through [`Vmcb::write`] also clears its *clean bit* (offset C0h): the hint that tells the CPU
//! which cached parts of the VMCB it may keep. Fields with no clean bit are reloaded on every
//! `VMRUN` anyway.

use thiserror::Error;

use crate::exit::ExitCode;

/// The size of a VMCB.
pub const BYTES: usize = 4096;

/// The offset of the state-save area.
pub const SAVE_AREA: u16 = 0x400;

/// A VMCB clean bit: a group of fields the CPU may cache between `VMRUN`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Clean {
    /// Intercept vectors, TSC offset and pause filter.
    Intercepts = 0,
    /// IOPM and MSRPM base addresses.
    Iopm = 1,
    /// The ASID.
    Asid = 2,
    /// The virtual TPR and virtual interrupt controls.
    Tpr = 3,
    /// Nested paging: NP enable, nested CR3 and guest PAT.
    NestedPaging = 4,
    /// CR0, CR3, CR4 and EFER.
    ControlRegisters = 5,
    /// DR6 and DR7.
    DebugRegisters = 6,
    /// GDTR and IDTR.
    DescriptorTables = 7,
    /// CS, DS, SS, ES and CPL.
    Segments = 8,
    /// CR2.
    Cr2 = 9,
    /// Last-branch-record state.
    Lbr = 10,
    /// AVIC pointers.
    Avic = 11,
}

/// A VMCB field: its offset, width in bytes and clean bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    offset: u16,
    width: u8,
    clean: Option<Clean>,
}

impl Field {
    const fn new(offset: u16, width: u8, clean: Option<Clean>) -> Self {
        Self {
            offset,
            width,
            clean,
        }
    }

    /// The byte offset in the VMCB.
    pub const fn offset(self) -> u16 {
        self.offset
    }

    /// The width in bytes: 1, 2, 4 or 8.
    pub const fn width(self) -> u8 {
        self.width
    }

    /// The clean bit covering the field, if any.
    pub const fn clean(self) -> Option<Clean> {
        self.clean
    }
}

/// Whether a field of `width` bytes at `offset` is naturally aligned and inside the VMCB.
const fn fits(offset: usize, width: usize) -> bool {
    offset.is_multiple_of(width) && offset + width <= BYTES
}

macro_rules! fields {
    ($($(#[$doc:meta])* $name:ident = ($offset:expr, $width:literal, $clean:expr),)*) => {
        $(
            $(#[$doc])*
            pub const $name: Field = Field::new($offset, $width, $clean);
            const _: () = assert!(fits($offset as usize, $width));
        )*
    };
}

const I: Option<Clean> = Some(Clean::Intercepts);
const S: u16 = SAVE_AREA;

fields! {
    /// Intercepts of CR reads (bits 15:0) and writes (bits 31:16).
    INTERCEPT_CR = (0x000, 4, I),
    /// Intercepts of DR reads (bits 15:0) and writes (bits 31:16).
    INTERCEPT_DR = (0x004, 4, I),
    /// Intercepts of exception vectors 0–31.
    INTERCEPT_EXCEPTIONS = (0x008, 4, I),
    /// Intercept vector 3 (exit codes 60h–7Fh).
    INTERCEPT_3 = (0x00c, 4, I),
    /// Intercept vector 4 (exit codes 80h–9Fh).
    INTERCEPT_4 = (0x010, 4, I),
    /// Pause filter threshold.
    PAUSE_FILTER_THRESHOLD = (0x03c, 2, I),
    /// Pause filter count.
    PAUSE_FILTER_COUNT = (0x03e, 2, I),
    /// Physical address of the I/O permission map.
    IOPM_BASE = (0x040, 8, Some(Clean::Iopm)),
    /// Physical address of the MSR permission map.
    MSRPM_BASE = (0x048, 8, Some(Clean::Iopm)),
    /// TSC offset.
    TSC_OFFSET = (0x050, 8, I),
    /// Guest ASID.
    ASID = (0x058, 4, Some(Clean::Asid)),
    /// TLB control (see [`TlbControl`]).
    TLB_CONTROL = (0x05c, 1, None),
    /// Virtual interrupt controls (see [`VirtualInterrupt`]).
    VIRTUAL_INTERRUPT = (0x060, 8, Some(Clean::Tpr)),
    /// Interrupt shadow (bit 0) and guest interrupt mask (bit 1).
    INTERRUPT_SHADOW = (0x068, 8, None),
    /// Exit code.
    EXIT_CODE = (0x070, 8, None),
    /// EXITINFO1.
    EXIT_INFO1 = (0x078, 8, None),
    /// EXITINFO2.
    EXIT_INFO2 = (0x080, 8, None),
    /// EXITINTINFO: an event that was being delivered at the exit.
    EXIT_INT_INFO = (0x088, 8, None),
    /// Nested paging enable (bit 0) and other virtualization enables.
    NP_ENABLE = (0x090, 8, Some(Clean::NestedPaging)),
    /// AVIC APIC base address.
    AVIC_APIC_BAR = (0x098, 8, Some(Clean::Avic)),
    /// Event injection (see [`crate::exit::Event`]).
    EVENT_INJECT = (0x0a8, 8, None),
    /// Nested page table root (nCR3).
    NESTED_CR3 = (0x0b0, 8, Some(Clean::NestedPaging)),
    /// LBR virtualization enable (bit 0), virtualized VMSAVE/VMLOAD (bit 1).
    LBR_VIRTUALIZATION = (0x0b8, 8, Some(Clean::Lbr)),
    /// VMCB clean bits.
    CLEAN_BITS = (0x0c0, 4, None),
    /// Next sequential instruction pointer (with NRIP save).
    NEXT_RIP = (0x0c8, 8, None),
    /// Number of guest instruction bytes fetched (with decode assists).
    GUEST_INSN_LEN = (0x0d0, 1, None),

    /// Guest CPL.
    CPL = (S + 0x0cb, 1, Some(Clean::Segments)),
    /// Guest EFER.
    EFER = (S + 0x0d0, 8, Some(Clean::ControlRegisters)),
    /// Guest CR4.
    CR4 = (S + 0x148, 8, Some(Clean::ControlRegisters)),
    /// Guest CR3.
    CR3 = (S + 0x150, 8, Some(Clean::ControlRegisters)),
    /// Guest CR0.
    CR0 = (S + 0x158, 8, Some(Clean::ControlRegisters)),
    /// Guest DR7.
    DR7 = (S + 0x160, 8, Some(Clean::DebugRegisters)),
    /// Guest DR6.
    DR6 = (S + 0x168, 8, Some(Clean::DebugRegisters)),
    /// Guest RFLAGS.
    RFLAGS = (S + 0x170, 8, None),
    /// Guest RIP.
    RIP = (S + 0x178, 8, None),
    /// Guest RSP.
    RSP = (S + 0x1d8, 8, None),
    /// Guest RAX.
    RAX = (S + 0x1f8, 8, None),
    /// Guest STAR.
    STAR = (S + 0x200, 8, None),
    /// Guest LSTAR.
    LSTAR = (S + 0x208, 8, None),
    /// Guest CSTAR.
    CSTAR = (S + 0x210, 8, None),
    /// Guest SFMASK.
    SFMASK = (S + 0x218, 8, None),
    /// Guest KernelGSbase.
    KERNEL_GS_BASE = (S + 0x220, 8, None),
    /// Guest SYSENTER_CS.
    SYSENTER_CS = (S + 0x228, 8, None),
    /// Guest SYSENTER_ESP.
    SYSENTER_ESP = (S + 0x230, 8, None),
    /// Guest SYSENTER_EIP.
    SYSENTER_EIP = (S + 0x238, 8, None),
    /// Guest CR2.
    CR2 = (S + 0x240, 8, Some(Clean::Cr2)),
    /// Guest PAT, used with nested paging.
    G_PAT = (S + 0x268, 8, Some(Clean::NestedPaging)),
    /// Guest DBGCTL.
    DBGCTL = (S + 0x270, 8, Some(Clean::Lbr)),
}

/// A segment register in the state-save area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentRegister {
    /// ES.
    Es,
    /// CS.
    Cs,
    /// SS.
    Ss,
    /// DS.
    Ds,
    /// FS.
    Fs,
    /// GS.
    Gs,
    /// GDTR (selector and attributes unused).
    Gdtr,
    /// LDTR.
    Ldtr,
    /// IDTR (selector and attributes unused).
    Idtr,
    /// TR.
    Tr,
}

impl SegmentRegister {
    const fn offset(self) -> u16 {
        SAVE_AREA
            + 16 * match self {
                Self::Es => 0,
                Self::Cs => 1,
                Self::Ss => 2,
                Self::Ds => 3,
                Self::Fs => 4,
                Self::Gs => 5,
                Self::Gdtr => 6,
                Self::Ldtr => 7,
                Self::Idtr => 8,
                Self::Tr => 9,
            }
    }

    const fn clean(self) -> Option<Clean> {
        match self {
            Self::Es | Self::Cs | Self::Ss | Self::Ds => Some(Clean::Segments),
            Self::Gdtr | Self::Idtr => Some(Clean::DescriptorTables),
            Self::Fs | Self::Gs | Self::Ldtr | Self::Tr => None,
        }
    }
}

/// A segment register's state: selector, attributes in the VMCB's packed form (type 3:0, S 4,
/// DPL 6:5, P 7, AVL 8, L 9, D/B 10, G 11), limit and base.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Segment {
    /// The selector.
    pub selector: u16,
    /// The packed attributes.
    pub attributes: u16,
    /// The limit.
    pub limit: u32,
    /// The base.
    pub base: u64,
}

impl Segment {
    /// Whether the L (64-bit code) attribute is set.
    pub const fn long(self) -> bool {
        self.attributes & (1 << 9) != 0
    }

    /// Whether the D/B attribute is set.
    pub const fn default_big(self) -> bool {
        self.attributes & (1 << 10) != 0
    }
}

/// TLB control values for [`TLB_CONTROL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TlbControl {
    /// Flush nothing.
    Nothing = 0,
    /// Flush the entire TLB, all ASIDs.
    All = 1,
    /// Flush this guest's ASID.
    Guest = 3,
    /// Flush this guest's non-global entries.
    GuestNonGlobal = 7,
}

/// The virtual interrupt controls at 60h.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VirtualInterrupt {
    /// The virtual TPR.
    pub tpr: u8,
    /// A virtual interrupt is pending.
    pub irq: bool,
    /// The pending virtual interrupt's priority.
    pub priority: u8,
    /// Ignore the virtual TPR for the pending interrupt.
    pub ignore_tpr: bool,
    /// Virtualize interrupt masking (the guest's RFLAGS.IF controls only virtual interrupts).
    pub masking: bool,
    /// The pending virtual interrupt's vector.
    pub vector: u8,
}

impl VirtualInterrupt {
    /// The field's value.
    pub const fn to_raw(self) -> u64 {
        self.tpr as u64
            | ((self.irq as u64) << 8)
            | (((self.priority & 0xf) as u64) << 16)
            | ((self.ignore_tpr as u64) << 20)
            | ((self.masking as u64) << 24)
            | ((self.vector as u64) << 32)
    }

    /// Decodes the field.
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            tpr: raw as u8,
            irq: raw & (1 << 8) != 0,
            priority: ((raw >> 16) & 0xf) as u8,
            ignore_tpr: raw & (1 << 20) != 0,
            masking: raw & (1 << 24) != 0,
            vector: (raw >> 32) as u8,
        }
    }
}

/// A VMCB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vmcb([u8; BYTES]);

impl Default for Vmcb {
    fn default() -> Self {
        Self::new()
    }
}

impl Vmcb {
    /// An all-zero VMCB.
    pub const fn new() -> Self {
        Self([0; BYTES])
    }

    /// The page's bytes.
    pub const fn as_bytes(&self) -> &[u8; BYTES] {
        &self.0
    }

    /// The VMCB in `bytes`, as read back after a `VMRUN`.
    pub const fn from_bytes(bytes: [u8; BYTES]) -> Self {
        Self(bytes)
    }

    /// Reads a field.
    pub fn read(&self, field: Field) -> u64 {
        let start = usize::from(field.offset);
        let mut le = [0u8; 8];
        le[..usize::from(field.width)]
            .copy_from_slice(&self.0[start..start + usize::from(field.width)]);
        u64::from_le_bytes(le)
    }

    /// Writes a field and clears its clean bit.
    ///
    /// # Errors
    ///
    /// [`VmcbError::TooWide`] when `value` does not fit the field.
    pub fn write(&mut self, field: Field, value: u64) -> Result<(), VmcbError> {
        let width = usize::from(field.width);
        if width < 8 && value >> (8 * width) != 0 {
            return Err(VmcbError::TooWide {
                offset: field.offset,
                value,
            });
        }
        let start = usize::from(field.offset);
        self.0[start..start + width].copy_from_slice(&value.to_le_bytes()[..width]);
        if let Some(clean) = field.clean {
            self.dirty(clean);
        }
        Ok(())
    }

    /// Reads a segment register.
    pub fn segment(&self, register: SegmentRegister) -> Segment {
        let at =
            |offset: u16, width: u8| self.read(Field::new(register.offset() + offset, width, None));
        Segment {
            selector: at(0, 2) as u16,
            attributes: at(2, 2) as u16,
            limit: at(4, 4) as u32,
            base: at(8, 8),
        }
    }

    /// Writes a segment register and clears its clean bit.
    pub fn set_segment(&mut self, register: SegmentRegister, segment: Segment) {
        let base = register.offset();
        let mut put = |offset: u16, bytes: &[u8]| {
            let start = usize::from(base + offset);
            self.0[start..start + bytes.len()].copy_from_slice(bytes);
        };
        put(0, &segment.selector.to_le_bytes());
        put(2, &segment.attributes.to_le_bytes());
        put(4, &segment.limit.to_le_bytes());
        put(8, &segment.base.to_le_bytes());
        if let Some(clean) = register.clean() {
            self.dirty(clean);
        }
    }

    /// Sets or clears the intercept that produces exit code `code`.
    ///
    /// # Errors
    ///
    /// [`VmcbError::NotInterceptable`] for an exit code with no intercept bit (nested page
    /// faults, AVIC exits, `VMEXIT_INVALID`).
    pub fn set_intercept(&mut self, code: ExitCode, on: bool) -> Result<(), VmcbError> {
        let raw = code.raw();
        let (field, bit) = match raw {
            0x00..=0x0f => (INTERCEPT_CR, raw),
            0x10..=0x1f => (INTERCEPT_CR, raw),
            0x20..=0x2f => (INTERCEPT_DR, raw - 0x20),
            0x30..=0x3f => (INTERCEPT_DR, raw - 0x20),
            0x40..=0x5f => (INTERCEPT_EXCEPTIONS, raw - 0x40),
            0x60..=0x7f => (INTERCEPT_3, raw - 0x60),
            0x80..=0x9f => (INTERCEPT_4, raw - 0x80),
            _ => return Err(VmcbError::NotInterceptable { code: raw }),
        };
        let old = self.read(field);
        let new = if on {
            old | (1 << bit)
        } else {
            old & !(1 << bit)
        };
        self.write(field, new)
    }

    /// Whether the intercept for `code` is set (false for codes with no intercept bit).
    pub fn intercepts(&self, code: ExitCode) -> bool {
        let raw = code.raw();
        let (field, bit) = match raw {
            0x00..=0x1f => (INTERCEPT_CR, raw),
            0x20..=0x3f => (INTERCEPT_DR, raw - 0x20),
            0x40..=0x5f => (INTERCEPT_EXCEPTIONS, raw - 0x40),
            0x60..=0x7f => (INTERCEPT_3, raw - 0x60),
            0x80..=0x9f => (INTERCEPT_4, raw - 0x80),
            _ => return false,
        };
        self.read(field) & (1 << bit) != 0
    }

    /// The clean bits.
    pub fn clean_bits(&self) -> u32 {
        self.read(CLEAN_BITS) as u32
    }

    /// Marks everything clean, after a `VMRUN` on the CPU that will run this VMCB next.
    pub fn mark_all_clean(&mut self) {
        self.0[0xc0..0xc4].copy_from_slice(&0x0fffu32.to_le_bytes());
    }

    /// Marks everything dirty: the VMCB moved to another CPU, or this CPU ran another VMCB.
    pub fn mark_all_dirty(&mut self) {
        self.0[0xc0..0xc4].copy_from_slice(&0u32.to_le_bytes());
    }

    fn dirty(&mut self, clean: Clean) {
        let bits = self.clean_bits() & !(1 << clean as u32);
        self.0[0xc0..0xc4].copy_from_slice(&bits.to_le_bytes());
    }
}

/// A VMCB access that cannot be done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum VmcbError {
    /// The value does not fit the field.
    #[error("value {value:#x} does not fit the VMCB field at {offset:#x}")]
    TooWide {
        /// The field's offset.
        offset: u16,
        /// The value.
        value: u64,
    },
    /// The exit code has no intercept bit.
    #[error("exit code {code:#x} has no intercept bit")]
    NotInterceptable {
        /// The exit code.
        code: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip_and_dirty_their_clean_bit() {
        let mut vmcb = Vmcb::new();
        vmcb.mark_all_clean();
        vmcb.write(CR3, 0x1234_5000).expect("fits");
        assert_eq!(vmcb.read(CR3), 0x1234_5000);
        assert_eq!(vmcb.clean_bits(), 0x0fff & !(1 << 5));
        vmcb.write(RIP, 0xffff_8000_0000_1000).expect("fits");
        assert_eq!(vmcb.clean_bits(), 0x0fff & !(1 << 5));
        vmcb.write(ASID, 7).expect("fits");
        assert_eq!(vmcb.read(ASID), 7);
        assert_eq!(vmcb.clean_bits() & (1 << 2), 0);
        assert_eq!(
            vmcb.write(TLB_CONTROL, 0x100),
            Err(VmcbError::TooWide {
                offset: 0x5c,
                value: 0x100
            })
        );
        vmcb.mark_all_dirty();
        assert_eq!(vmcb.clean_bits(), 0);
        assert_eq!(Vmcb::from_bytes(*vmcb.as_bytes()), vmcb);
    }

    #[test]
    fn offsets_match_the_layout() {
        assert_eq!(EFER.offset(), 0x4d0);
        assert_eq!(RIP.offset(), 0x578);
        assert_eq!(RAX.offset(), 0x5f8);
        assert_eq!(CR2.offset(), 0x640);
        assert_eq!(SegmentRegister::Tr.offset(), 0x490);
        assert_eq!(CPL.width(), 1);
        assert_eq!(NESTED_CR3.clean(), Some(Clean::NestedPaging));
    }

    #[test]
    fn segments_round_trip() {
        let mut vmcb = Vmcb::new();
        vmcb.mark_all_clean();
        let code = Segment {
            selector: 0x08,
            attributes: 0x029b,
            limit: 0xffff_ffff,
            base: 0,
        };
        vmcb.set_segment(SegmentRegister::Cs, code);
        assert_eq!(vmcb.segment(SegmentRegister::Cs), code);
        assert!(code.long() && !code.default_big());
        assert_eq!(vmcb.clean_bits() & (1 << 8), 0);
        let before = vmcb.clean_bits();
        vmcb.set_segment(SegmentRegister::Fs, Segment::default());
        assert_eq!(vmcb.clean_bits(), before);
    }

    #[test]
    fn intercepts_follow_exit_codes() {
        let mut vmcb = Vmcb::new();
        for code in [
            ExitCode::CPUID,
            ExitCode::HLT,
            ExitCode::VMRUN,
            ExitCode::IOIO,
            ExitCode::MSR,
        ] {
            vmcb.set_intercept(code, true).expect("interceptable");
            assert!(vmcb.intercepts(code));
        }
        assert_eq!(
            vmcb.read(INTERCEPT_3),
            (1 << 18) | (1 << 24) | (1 << 27) | (1 << 28)
        );
        assert_eq!(vmcb.read(INTERCEPT_4), 1);
        vmcb.set_intercept(ExitCode::write_cr(8), true)
            .expect("interceptable");
        assert_eq!(vmcb.read(INTERCEPT_CR), 1 << 24);
        vmcb.set_intercept(ExitCode::exception(14), true)
            .expect("interceptable");
        assert_eq!(vmcb.read(INTERCEPT_EXCEPTIONS), 1 << 14);
        vmcb.set_intercept(ExitCode::HLT, false)
            .expect("interceptable");
        assert!(!vmcb.intercepts(ExitCode::HLT));
        assert_eq!(
            vmcb.set_intercept(ExitCode::NPF, true),
            Err(VmcbError::NotInterceptable { code: 0x400 })
        );
        assert!(!vmcb.intercepts(ExitCode::NPF));
    }

    #[test]
    fn virtual_interrupts_round_trip() {
        let pending = VirtualInterrupt {
            tpr: 2,
            irq: true,
            priority: 3,
            ignore_tpr: true,
            masking: true,
            vector: 0x30,
        };
        assert_eq!(pending.to_raw(), 0x30_0113_0102);
        assert_eq!(VirtualInterrupt::from_raw(pending.to_raw()), pending);
    }
}
