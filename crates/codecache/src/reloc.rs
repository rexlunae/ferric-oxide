//! Relocations.
//!
//! Compiled code calls a few runtime functions (libcalls) whose addresses are only known
//! when the code is mapped. The entry's relocation section lists each place in the text
//! that needs one. The MAC covers the unrelocated text, so the kernel checks the MAC first,
//! then applies the relocations to its private copy before mapping it executable.
//!
//! A record is 16 bytes, little-endian:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0..4 | offset in the text (`u32`) |
//! | 4 | [`RelocKind`] (`u8`) |
//! | 5..8 | zero |
//! | 8..12 | target: the libcall index the caller resolves (`u32`) |
//! | 12..16 | addend (`i32`) |
//!
//! Records are sorted by offset and do not overlap, and each lies inside the text, so one
//! list has one encoding and applying it never writes outside the text.

use crate::{array, put};

/// Why a relocation list is refused or cannot be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RelocError {
    /// The section is not a whole number of records.
    #[error("relocation section length {len} is not a multiple of 16")]
    Truncated {
        /// The section length.
        len: usize,
    },
    /// A record has an unknown kind.
    #[error("relocation kind {value} is unknown")]
    Kind {
        /// The kind byte.
        value: u8,
    },
    /// A record's padding is not zero.
    #[error("relocation padding is not zero")]
    Padding,
    /// A record reaches past the end of the text.
    #[error("relocation at {offset} lies outside the text")]
    OutOfText {
        /// The record's offset.
        offset: u32,
    },
    /// A record starts before the previous one ends.
    #[error("relocation at {offset} is out of order or overlaps the previous one")]
    Order {
        /// The record's offset.
        offset: u32,
    },
    /// The text passed to [`Relocations::apply`] is not the text they were checked against.
    #[error("text is {found} bytes, the relocations were checked against {expected}")]
    TextLength {
        /// The length checked at parse time.
        expected: usize,
        /// The length passed.
        found: usize,
    },
    /// The resolver has no address for a target.
    #[error("relocation target {target} is not resolved")]
    Unresolved {
        /// The target index.
        target: u32,
    },
    /// The resolved value does not fit the field.
    #[error("relocation at {offset} is out of range")]
    Range {
        /// The record's offset.
        offset: u32,
    },
    /// A branch target is not instruction-aligned.
    #[error("relocation at {offset} has a misaligned target")]
    Misaligned {
        /// The record's offset.
        offset: u32,
    },
}

/// What a relocation writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RelocKind {
    /// The absolute address `target + addend`, 8 bytes.
    Abs8,
    /// x86-64: `target + addend - place` as a signed 32-bit value, 4 bytes.
    X86PcRel4,
    /// AArch64 `BL`/`B`: `(target + addend - place) / 4` in the instruction's low 26 bits;
    /// the distance must be a multiple of 4 within ±128 MiB.
    Aarch64Call26,
}

impl RelocKind {
    /// The encoded value.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Abs8 => 1,
            Self::X86PcRel4 => 2,
            Self::Aarch64Call26 => 3,
        }
    }

    /// Decodes the value.
    ///
    /// # Errors
    ///
    /// [`RelocError::Kind`] for an unknown value.
    pub const fn from_u8(value: u8) -> Result<Self, RelocError> {
        match value {
            1 => Ok(Self::Abs8),
            2 => Ok(Self::X86PcRel4),
            3 => Ok(Self::Aarch64Call26),
            _ => Err(RelocError::Kind { value }),
        }
    }

    /// How many bytes of text it writes.
    pub const fn width(self) -> usize {
        match self {
            Self::Abs8 => 8,
            Self::X86PcRel4 | Self::Aarch64Call26 => 4,
        }
    }
}

/// One relocation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Reloc {
    /// Where in the text it applies.
    pub offset: u32,
    /// What it writes.
    pub kind: RelocKind,
    /// The libcall index the caller resolves to an address.
    pub target: u32,
    /// Added to the target's address.
    pub addend: i32,
}

impl Reloc {
    /// The encoded size.
    pub const SIZE: usize = 16;

    /// The encoding (see the module documentation).
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        let at = &mut 0;
        put(&mut out, at, &self.offset.to_le_bytes());
        put(&mut out, at, &[self.kind.to_u8(), 0, 0, 0]);
        put(&mut out, at, &self.target.to_le_bytes());
        put(&mut out, at, &self.addend.to_le_bytes());
        out
    }

    /// Decodes one record.
    ///
    /// # Errors
    ///
    /// [`RelocError::Kind`] for an unknown kind and [`RelocError::Padding`] for non-zero
    /// padding.
    pub fn decode(bytes: &[u8; Self::SIZE]) -> Result<Self, RelocError> {
        if bytes[5..8] != [0; 3] {
            return Err(RelocError::Padding);
        }
        Ok(Self {
            offset: u32::from_le_bytes(array(bytes, 0)),
            kind: RelocKind::from_u8(bytes[4])?,
            target: u32::from_le_bytes(array(bytes, 8)),
            addend: i32::from_le_bytes(array(bytes, 12)),
        })
    }

    /// The text range it writes. It saturates rather than wrapping, so a record near
    /// `usize::MAX` is still seen as past the text.
    fn span(&self) -> core::ops::Range<usize> {
        let start = self.offset as usize;
        start..start.saturating_add(self.kind.width())
    }

    fn write(&self, text: &mut [u8], text_base: u64, target: u64) -> Result<(), RelocError> {
        let offset = self.offset;
        let span = self.span();
        let value = target.wrapping_add_signed(i64::from(self.addend));
        let place = text_base.wrapping_add(u64::from(offset));
        let distance = value.wrapping_sub(place) as i64;
        match self.kind {
            RelocKind::Abs8 => text[span].copy_from_slice(&value.to_le_bytes()),
            RelocKind::X86PcRel4 => {
                let distance = i32::try_from(distance).map_err(|_| RelocError::Range { offset })?;
                text[span].copy_from_slice(&distance.to_le_bytes());
            }
            RelocKind::Aarch64Call26 => {
                if distance % 4 != 0 {
                    return Err(RelocError::Misaligned { offset });
                }
                let words = distance / 4;
                if !(-(1 << 25)..(1 << 25)).contains(&words) {
                    return Err(RelocError::Range { offset });
                }
                let insn = u32::from_le_bytes(array(text, span.start));
                let insn = (insn & 0xfc00_0000) | (words as u32 & 0x03ff_ffff);
                text[span].copy_from_slice(&insn.to_le_bytes());
            }
        }
        Ok(())
    }
}

/// A checked relocation section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relocations<'a> {
    bytes: &'a [u8],
    text_len: usize,
}

impl<'a> Relocations<'a> {
    /// Checks a relocation section against a text of `text_len` bytes.
    ///
    /// # Errors
    ///
    /// [`RelocError::Truncated`] if the section is not whole records, the record errors of
    /// [`Reloc::decode`], [`RelocError::OutOfText`] for a record past the text and
    /// [`RelocError::Order`] for one that is unsorted or overlapping.
    pub fn parse(bytes: &'a [u8], text_len: usize) -> Result<Self, RelocError> {
        if !bytes.len().is_multiple_of(Reloc::SIZE) {
            return Err(RelocError::Truncated { len: bytes.len() });
        }
        let mut end = 0;
        for chunk in bytes.chunks_exact(Reloc::SIZE) {
            let reloc = Reloc::decode(&array(chunk, 0))?;
            let span = reloc.span();
            if span.end > text_len {
                return Err(RelocError::OutOfText {
                    offset: reloc.offset,
                });
            }
            if span.start < end {
                return Err(RelocError::Order {
                    offset: reloc.offset,
                });
            }
            end = span.end;
        }
        Ok(Self { bytes, text_len })
    }

    /// The records, in offset order.
    pub fn iter(&self) -> impl Iterator<Item = Reloc> + 'a {
        self.bytes
            .chunks_exact(Reloc::SIZE)
            // parse decoded every record successfully, so filter_map drops nothing.
            .filter_map(|chunk| Reloc::decode(&array(chunk, 0)).ok())
    }

    /// The number of records.
    pub fn len(&self) -> usize {
        self.bytes.len() / Reloc::SIZE
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Applies every record to `text`, which will be mapped at `text_base`. `resolve` gives
    /// the address of a libcall index.
    ///
    /// On error `text` may be partly relocated; the caller discards it.
    ///
    /// # Errors
    ///
    /// [`RelocError::TextLength`] if `text` is not the length the section was checked
    /// against, [`RelocError::Unresolved`] if `resolve` returns `None`, and
    /// [`RelocError::Range`] or [`RelocError::Misaligned`] if a value does not fit.
    pub fn apply(
        &self,
        text: &mut [u8],
        text_base: u64,
        resolve: impl Fn(u32) -> Option<u64>,
    ) -> Result<(), RelocError> {
        if text.len() != self.text_len {
            return Err(RelocError::TextLength {
                expected: self.text_len,
                found: text.len(),
            });
        }
        for reloc in self.iter() {
            let target = resolve(reloc.target).ok_or(RelocError::Unresolved {
                target: reloc.target,
            })?;
            reloc.write(text, text_base, target)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    fn section(relocs: &[Reloc]) -> Vec<u8> {
        relocs.iter().flat_map(|reloc| reloc.encode()).collect()
    }

    const fn reloc(offset: u32, kind: RelocKind, target: u32, addend: i32) -> Reloc {
        Reloc {
            offset,
            kind,
            target,
            addend,
        }
    }

    #[test]
    fn records_have_a_fixed_encoding() {
        let record = reloc(0x10, RelocKind::X86PcRel4, 3, -4);
        assert_eq!(
            record.encode(),
            [
                0x10, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xfc, 0xff, 0xff, 0xff
            ]
        );
        assert_eq!(Reloc::decode(&record.encode()), Ok(record));
        let mut padded = record.encode();
        padded[6] = 1;
        assert_eq!(Reloc::decode(&padded), Err(RelocError::Padding));
        let mut kind = record.encode();
        kind[4] = 0;
        assert_eq!(Reloc::decode(&kind), Err(RelocError::Kind { value: 0 }));
    }

    #[test]
    fn lists_must_be_in_the_text_sorted_and_disjoint() {
        let ok = section(&[
            reloc(0, RelocKind::Abs8, 0, 0),
            reloc(8, RelocKind::X86PcRel4, 1, 0),
        ]);
        assert_eq!(Relocations::parse(&ok, 12).map(|r| r.len()), Ok(2));
        assert_eq!(
            Relocations::parse(&ok, 11),
            Err(RelocError::OutOfText { offset: 8 })
        );
        let overlap = section(&[
            reloc(0, RelocKind::Abs8, 0, 0),
            reloc(7, RelocKind::X86PcRel4, 1, 0),
        ]);
        assert_eq!(
            Relocations::parse(&overlap, 64),
            Err(RelocError::Order { offset: 7 })
        );
        let unsorted = section(&[
            reloc(8, RelocKind::X86PcRel4, 1, 0),
            reloc(0, RelocKind::X86PcRel4, 0, 0),
        ]);
        assert_eq!(
            Relocations::parse(&unsorted, 64),
            Err(RelocError::Order { offset: 0 })
        );
        assert_eq!(
            Relocations::parse(&ok[..20], 64),
            Err(RelocError::Truncated { len: 20 })
        );
        assert_eq!(
            Relocations::parse(&section(&[reloc(u32::MAX, RelocKind::Abs8, 0, 0)]), 64),
            Err(RelocError::OutOfText { offset: u32::MAX })
        );
    }

    #[test]
    fn relocations_write_the_documented_values() {
        let bytes = section(&[
            reloc(0, RelocKind::Abs8, 0, 8),
            reloc(8, RelocKind::X86PcRel4, 1, -4),
            reloc(12, RelocKind::Aarch64Call26, 1, 0),
        ]);
        let relocs = Relocations::parse(&bytes, 16).unwrap();
        let mut text = [0; 16];
        // A BL with a stale immediate: the opcode bits must survive.
        text[12..].copy_from_slice(&0x97ff_ffffu32.to_le_bytes());
        let resolve = |target| {
            [Some(0x1000), Some(0x2000_0000)]
                .get(target as usize)
                .copied()?
        };
        relocs
            .apply(&mut text, 0x2000_0000 - 0x100, resolve)
            .unwrap();
        assert_eq!(&text[..8], &0x1008u64.to_le_bytes());
        // 0x2000_0000 - 4 - (base + 8) = 0xf4.
        assert_eq!(&text[8..12], &0xf4i32.to_le_bytes());
        // (0x2000_0000 - (base + 12)) / 4 = 0x3d.
        assert_eq!(&text[12..], &(0x9400_0000u32 | 0x3d).to_le_bytes());

        assert_eq!(
            relocs.apply(&mut text, 0, |_| None),
            Err(RelocError::Unresolved { target: 0 })
        );
        assert_eq!(
            relocs.apply(&mut [0; 15], 0, resolve),
            Err(RelocError::TextLength {
                expected: 16,
                found: 15
            })
        );
    }

    #[test]
    fn out_of_range_values_are_refused() {
        let far = section(&[reloc(0, RelocKind::X86PcRel4, 0, 0)]);
        let far = Relocations::parse(&far, 4).unwrap();
        assert_eq!(
            far.apply(&mut [0; 4], 0, |_| Some(1 << 32)),
            Err(RelocError::Range { offset: 0 })
        );
        let branch = section(&[reloc(0, RelocKind::Aarch64Call26, 0, 0)]);
        let branch = Relocations::parse(&branch, 4).unwrap();
        assert_eq!(
            branch.apply(&mut [0; 4], 0, |_| Some(2)),
            Err(RelocError::Misaligned { offset: 0 })
        );
        assert_eq!(
            branch.apply(&mut [0; 4], 0, |_| Some(128 << 20)),
            Err(RelocError::Range { offset: 0 })
        );
        assert!(
            branch
                .apply(&mut [0; 4], 0, |_| Some((128 << 20) - 4))
                .is_ok()
        );
    }
}
