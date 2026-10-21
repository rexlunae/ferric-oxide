//! vCPU register records for `GetRegs`/`SetRegs` and `GetSregs`/`SetSregs`.
//!
//! Both are fixed-size little-endian records. Segment attributes use one vendor-neutral
//! packed form, and [`Segment::vmx_access_rights`] and [`Segment::svm_attributes`] convert it
//! into the two backends' formats, so the conversion is written once and tested here.

use thiserror::Error;

/// The general registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Regs {
    /// RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, then R8–R15, in that order.
    pub gprs: [u64; 16],
    /// RIP.
    pub rip: u64,
    /// RFLAGS.
    pub rflags: u64,
}

impl Regs {
    /// The record's size.
    pub const BYTES: usize = 18 * 8;

    /// The record.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut out = [0; Self::BYTES];
        let words = self.gprs.iter().chain([&self.rip, &self.rflags]);
        for (chunk, word) in out.chunks_exact_mut(8).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// The registers a record holds.
    ///
    /// # Errors
    ///
    /// [`RegsError::Length`] when `bytes` is not [`Regs::BYTES`] long.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RegsError> {
        if bytes.len() != Self::BYTES {
            return Err(RegsError::Length);
        }
        let mut words = [0u64; 18];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
            *word = le_u64(chunk);
        }
        let mut gprs = [0; 16];
        gprs.copy_from_slice(&words[..16]);
        Ok(Self {
            gprs,
            rip: words[16],
            rflags: words[17],
        })
    }
}

/// A segment register: base, limit, selector and packed attributes.
///
/// The attributes are: type in bits 3:0, S 4, DPL 6:5, P 7, AVL 8, L 9, D/B 10, G 11 and
/// *unusable* in bit 12 (a null segment loaded in 64-bit mode). Bits 15:13 are reserved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Segment {
    /// The base.
    pub base: u64,
    /// The limit.
    pub limit: u32,
    /// The selector.
    pub selector: u16,
    /// The packed attributes.
    pub attributes: u16,
}

const UNUSABLE: u16 = 1 << 12;

impl Segment {
    /// The access-rights value for a VMCS: type 3:0, S 4, DPL 6:5, P 7, AVL 12, L 13, D/B 14,
    /// G 15, unusable 16.
    pub const fn vmx_access_rights(self) -> u32 {
        let low = (self.attributes & 0xff) as u32;
        let high = ((self.attributes >> 8) & 0xf) as u32;
        let unusable = ((self.attributes & UNUSABLE) != 0) as u32;
        low | (high << 12) | (unusable << 16)
    }

    /// The attributes for a VMCB: the same packed form without the unusable bit, which SVM
    /// expresses by a clear P bit.
    pub const fn svm_attributes(self) -> u16 {
        if self.attributes & UNUSABLE != 0 {
            self.attributes & 0x0f7f
        } else {
            self.attributes & 0x0fff
        }
    }

    /// The segment from VMCS access rights, with the other fields given.
    pub const fn from_vmx(base: u64, limit: u32, selector: u16, access_rights: u32) -> Self {
        let low = (access_rights & 0xff) as u16;
        let high = ((access_rights >> 12) & 0xf) as u16;
        let unusable = if access_rights & (1 << 16) != 0 {
            UNUSABLE
        } else {
            0
        };
        Self {
            base,
            limit,
            selector,
            attributes: low | (high << 8) | unusable,
        }
    }

    fn write(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.base.to_le_bytes());
        out[8..12].copy_from_slice(&self.limit.to_le_bytes());
        out[12..14].copy_from_slice(&self.selector.to_le_bytes());
        out[14..16].copy_from_slice(&self.attributes.to_le_bytes());
    }

    fn read(bytes: &[u8]) -> Result<Self, RegsError> {
        let attributes = u16::from_le_bytes([bytes[14], bytes[15]]);
        if attributes & 0xe000 != 0 {
            return Err(RegsError::Reserved);
        }
        Ok(Self {
            base: le_u64(&bytes[0..8]),
            limit: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            selector: u16::from_le_bytes([bytes[12], bytes[13]]),
            attributes,
        })
    }
}

/// A descriptor table register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Table {
    /// The base.
    pub base: u64,
    /// The limit.
    pub limit: u16,
}

/// The system registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Sregs {
    /// CS, DS, ES, FS, GS, SS, TR, LDTR, in that order.
    pub segments: [Segment; 8],
    /// GDTR.
    pub gdt: Table,
    /// IDTR.
    pub idt: Table,
    /// CR0.
    pub cr0: u64,
    /// CR2.
    pub cr2: u64,
    /// CR3.
    pub cr3: u64,
    /// CR4.
    pub cr4: u64,
    /// IA32_EFER.
    pub efer: u64,
    /// IA32_APIC_BASE.
    pub apic_base: u64,
}

impl Sregs {
    /// The record's size.
    pub const BYTES: usize = 8 * 16 + 2 * 16 + 6 * 8;

    /// The record.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut out = [0; Self::BYTES];
        for (chunk, segment) in out[..128].chunks_exact_mut(16).zip(&self.segments) {
            segment.write(chunk);
        }
        for (chunk, table) in out[128..160].chunks_exact_mut(16).zip([self.gdt, self.idt]) {
            chunk[0..8].copy_from_slice(&table.base.to_le_bytes());
            chunk[8..10].copy_from_slice(&table.limit.to_le_bytes());
        }
        let words = [
            self.cr0,
            self.cr2,
            self.cr3,
            self.cr4,
            self.efer,
            self.apic_base,
        ];
        for (chunk, word) in out[160..].chunks_exact_mut(8).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// The registers a record holds.
    ///
    /// # Errors
    ///
    /// [`RegsError::Length`] when `bytes` is not [`Sregs::BYTES`] long, and
    /// [`RegsError::Reserved`] when a reserved attribute or padding bit is set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RegsError> {
        if bytes.len() != Self::BYTES {
            return Err(RegsError::Length);
        }
        let mut segments = [Segment::default(); 8];
        for (segment, chunk) in segments.iter_mut().zip(bytes[..128].chunks_exact(16)) {
            *segment = Segment::read(chunk)?;
        }
        let table = |chunk: &[u8]| -> Result<Table, RegsError> {
            if chunk[10..16].iter().any(|&b| b != 0) {
                return Err(RegsError::Reserved);
            }
            Ok(Table {
                base: le_u64(&chunk[0..8]),
                limit: u16::from_le_bytes([chunk[8], chunk[9]]),
            })
        };
        let word = |i: usize| le_u64(&bytes[160 + 8 * i..168 + 8 * i]);
        Ok(Self {
            segments,
            gdt: table(&bytes[128..144])?,
            idt: table(&bytes[144..160])?,
            cr0: word(0),
            cr2: word(1),
            cr3: word(2),
            cr4: word(3),
            efer: word(4),
            apic_base: word(5),
        })
    }
}

fn le_u64(chunk: &[u8]) -> u64 {
    let mut le = [0; 8];
    le.copy_from_slice(chunk);
    u64::from_le_bytes(le)
}

/// Why bytes are not a register record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RegsError {
    /// The record has the wrong length.
    #[error("a register record has the wrong length")]
    Length,
    /// A reserved bit is set.
    #[error("a register record sets a reserved bit")]
    Reserved,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code64() -> Segment {
        // Type 0xb (execute/read, accessed), S, P, L.
        Segment {
            base: 0,
            limit: u32::MAX,
            selector: 0x08,
            attributes: 0x029b,
        }
    }

    #[test]
    fn regs_round_trip() {
        let regs = Regs {
            gprs: core::array::from_fn(|i| i as u64 * 0x1111),
            rip: 0xffff_ffff_8100_0000,
            rflags: 0x2,
        };
        assert_eq!(Regs::from_bytes(&regs.to_bytes()), Ok(regs));
        assert_eq!(Regs::from_bytes(&[0; 10]), Err(RegsError::Length));
    }

    #[test]
    fn sregs_round_trip_and_check_reserved_bits() {
        let mut sregs = Sregs {
            cr0: 0x8000_0011,
            cr3: 0x1000,
            cr4: 0x20,
            efer: 0x500,
            apic_base: 0xfee0_0900,
            gdt: Table {
                base: 0x5000,
                limit: 0x27,
            },
            ..Sregs::default()
        };
        sregs.segments[0] = code64();
        let bytes = sregs.to_bytes();
        assert_eq!(Sregs::from_bytes(&bytes), Ok(sregs));
        let mut bad = bytes;
        bad[15] = 0x80;
        assert_eq!(Sregs::from_bytes(&bad), Err(RegsError::Reserved));
        let mut bad = bytes;
        bad[128 + 12] = 1;
        assert_eq!(Sregs::from_bytes(&bad), Err(RegsError::Reserved));
        assert_eq!(Sregs::from_bytes(&bytes[1..]), Err(RegsError::Length));
    }

    #[test]
    fn attribute_conversions() {
        let cs = code64();
        assert_eq!(cs.vmx_access_rights(), 0x209b);
        assert_eq!(cs.svm_attributes(), 0x029b);
        assert_eq!(Segment::from_vmx(0, u32::MAX, 0x08, 0x209b), cs);
        let null = Segment {
            attributes: UNUSABLE | 0x0093,
            ..Segment::default()
        };
        assert_eq!(null.vmx_access_rights(), 0x1_0093);
        assert_eq!(null.svm_attributes() & 0x80, 0);
        assert_eq!(Segment::from_vmx(0, 0, 0, 0x1_0093), null);
    }
}
