//! The entry format, version 1.
//!
//! An entry is one contiguous byte string, little-endian throughout:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0..4 | magic `FXCC` |
//! | 4..6 | format version (`u16`, 1) |
//! | 6..8 | flags (`u16`, 0) |
//! | 8..120 | [`CacheKey`] encoding |
//! | 120..152 | [`Mac`] (zero until sealed) |
//! | 152..184 | section table: offset and length (`u32` each) of text, read-only data, metadata and relocations |
//! | 184.. | the sections, in that order |
//!
//! Each section starts at the first multiple of [`ALIGN`] at or after the end of the one
//! before it (the first after the header), padding is zero, and the entry ends where the
//! relocation section ends. [`Entry::parse`] refuses anything else, so an entry has exactly
//! one encoding and every section offset in it is checked before use.
//!
//! The flow is: the compiler service writes an unsealed entry with [`EntryBuilder`], the
//! kernel [`seal`]s it, and before mapping code it copies the entry into its own memory,
//! parses it and calls [`Entry::verify`].

use core::ops::Range;

use crate::key::{CacheKey, KeyError};
use crate::mac::{Mac, MacKey};
use crate::reloc::{Reloc, RelocError, Relocations};
use crate::{array, put};

/// The magic bytes.
pub const MAGIC: [u8; 4] = *b"FXCC";

/// The format version this crate reads and writes.
pub const VERSION: u16 = 1;

/// The header size; the first section starts at the next multiple of [`ALIGN`].
pub const HEADER_SIZE: usize = 184;

/// Section alignment.
pub const ALIGN: usize = 16;

const KEY_AT: usize = 8;
const MAC_AT: usize = KEY_AT + CacheKey::SIZE;
const TABLE_AT: usize = MAC_AT + Mac::SIZE;

/// One of an entry's sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// The machine code.
    Text,
    /// Read-only data the code refers to.
    Rodata,
    /// The runtime's metadata (Wasmtime's serialized module information).
    Metadata,
    /// [`Reloc`] records.
    Relocations,
}

impl Section {
    /// The sections in their table and file order.
    pub const ALL: [Self; 4] = [Self::Text, Self::Rodata, Self::Metadata, Self::Relocations];
}

/// Why an entry is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EntryError {
    /// The bytes end before the header or a section does.
    #[error("cache entry is truncated")]
    Truncated,
    /// The bytes are not a cache entry.
    #[error("not a cache entry")]
    Magic,
    /// The entry is from another format version.
    #[error("cache entry format version {found} is not supported")]
    Version {
        /// The version found.
        found: u16,
    },
    /// The flags field is not zero.
    #[error("cache entry flags {found:#x} are not supported")]
    Flags {
        /// The flags found.
        found: u16,
    },
    /// The key does not decode.
    #[error("cache entry key is invalid")]
    Key(#[from] KeyError),
    /// A section is not where the canonical layout puts it.
    #[error("cache entry section {section:?} is misplaced")]
    Layout {
        /// The section.
        section: Section,
    },
    /// Padding between sections is not zero.
    #[error("cache entry padding is not zero")]
    Padding,
    /// Bytes follow the last section.
    #[error("cache entry has trailing bytes")]
    TrailingBytes,
    /// The relocations are invalid.
    #[error("cache entry relocations are invalid")]
    Relocations(#[from] RelocError),
    /// A section, or the whole entry, is too large for the format's `u32` offsets.
    #[error("cache entry is too large")]
    TooLarge,
    /// The output buffer is smaller than the entry.
    #[error("cache entry needs a {needed}-byte buffer")]
    BufferSize {
        /// The entry's size.
        needed: usize,
    },
    /// The MAC does not match: the entry was not sealed by this kernel, or was changed.
    #[error("cache entry MAC does not match")]
    BadMac,
}

/// The canonical start of each section, and the entry's length, for these section lengths.
fn layout(lens: [usize; 4]) -> Result<([usize; 4], usize), EntryError> {
    let mut starts = [0; 4];
    let mut end = HEADER_SIZE;
    for (start, len) in starts.iter_mut().zip(lens) {
        *start = end
            .checked_next_multiple_of(ALIGN)
            .ok_or(EntryError::TooLarge)?;
        end = start.checked_add(len).ok_or(EntryError::TooLarge)?;
    }
    if u32::try_from(end).is_err() {
        return Err(EntryError::TooLarge);
    }
    Ok((starts, end))
}

/// A parsed entry, borrowing its bytes. Parsing checks the layout but not the MAC; call
/// [`verify`](Self::verify) before trusting the contents.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    bytes: &'a [u8],
    key: CacheKey,
    starts: [usize; 4],
    lens: [usize; 4],
    relocations: Relocations<'a>,
}

impl<'a> Entry<'a> {
    /// Parses an entry.
    ///
    /// # Errors
    ///
    /// [`EntryError::Truncated`], [`Magic`](EntryError::Magic),
    /// [`Version`](EntryError::Version), [`Flags`](EntryError::Flags) and
    /// [`Key`](EntryError::Key) for a bad header; [`Layout`](EntryError::Layout),
    /// [`Padding`](EntryError::Padding) and [`TrailingBytes`](EntryError::TrailingBytes)
    /// for a non-canonical layout; [`Relocations`](EntryError::Relocations) for invalid
    /// relocation records.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, EntryError> {
        if bytes.len() < HEADER_SIZE {
            return Err(EntryError::Truncated);
        }
        if bytes[..4] != MAGIC {
            return Err(EntryError::Magic);
        }
        let half = |at: usize| u16::from_le_bytes(array(bytes, at));
        let word = |at: usize| u32::from_le_bytes(array(bytes, at)) as usize;
        if half(4) != VERSION {
            return Err(EntryError::Version { found: half(4) });
        }
        if half(6) != 0 {
            return Err(EntryError::Flags { found: half(6) });
        }
        let key = CacheKey::decode(&array(bytes, KEY_AT))?;
        let table: [(usize, usize); 4] =
            core::array::from_fn(|i| (word(TABLE_AT + 8 * i), word(TABLE_AT + 8 * i + 4)));
        let lens = table.map(|(_, len)| len);
        let (starts, end) = layout(lens)?;
        for (section, ((found, _), start)) in
            Section::ALL.into_iter().zip(table.into_iter().zip(starts))
        {
            if found != start {
                return Err(EntryError::Layout { section });
            }
        }
        if bytes.len() < end {
            return Err(EntryError::Truncated);
        }
        if bytes.len() > end {
            return Err(EntryError::TrailingBytes);
        }
        let mut gap_start = HEADER_SIZE;
        for (start, len) in starts.into_iter().zip(lens) {
            if bytes[gap_start..start].iter().any(|byte| *byte != 0) {
                return Err(EntryError::Padding);
            }
            gap_start = start + len;
        }
        let range = |i: usize| starts[i]..starts[i] + lens[i];
        let relocations = Relocations::parse(&bytes[range(3)], lens[0])?;
        Ok(Self {
            bytes,
            key,
            starts,
            lens,
            relocations,
        })
    }

    fn range(&self, section: Section) -> Range<usize> {
        let i = section as usize;
        self.starts[i]..self.starts[i] + self.lens[i]
    }

    /// The key the entry was compiled for.
    pub fn key(&self) -> &CacheKey {
        &self.key
    }

    /// A section's bytes.
    pub fn section(&self, section: Section) -> &'a [u8] {
        &self.bytes[self.range(section)]
    }

    /// The machine code, before relocation.
    pub fn text(&self) -> &'a [u8] {
        self.section(Section::Text)
    }

    /// The read-only data.
    pub fn rodata(&self) -> &'a [u8] {
        self.section(Section::Rodata)
    }

    /// The runtime's metadata.
    pub fn metadata(&self) -> &'a [u8] {
        self.section(Section::Metadata)
    }

    /// The relocations, checked against the text.
    pub fn relocations(&self) -> Relocations<'a> {
        self.relocations
    }

    /// The stored MAC.
    pub fn mac(&self) -> Mac {
        Mac::from_bytes(array(self.bytes, MAC_AT))
    }

    /// The MAC this entry should carry under `secret`.
    pub fn compute_mac(&self, secret: &MacKey) -> Mac {
        Mac::compute(
            secret,
            &self.key,
            Section::ALL.map(|section| self.section(section)),
        )
    }

    /// Checks the stored MAC.
    ///
    /// # Errors
    ///
    /// [`EntryError::BadMac`] if it does not match.
    pub fn verify(&self, secret: &MacKey) -> Result<(), EntryError> {
        if self.mac() == self.compute_mac(secret) {
            Ok(())
        } else {
            Err(EntryError::BadMac)
        }
    }
}

/// Seals an entry in place: parses it and writes its MAC under `secret`. Returns the MAC.
///
/// # Errors
///
/// The errors of [`Entry::parse`].
pub fn seal(bytes: &mut [u8], secret: &MacKey) -> Result<Mac, EntryError> {
    let mac = Entry::parse(bytes)?.compute_mac(secret);
    bytes[MAC_AT..MAC_AT + Mac::SIZE].copy_from_slice(mac.as_bytes());
    Ok(mac)
}

/// Writes an unsealed entry. Sections left unset are empty.
#[derive(Debug, Clone, Copy)]
pub struct EntryBuilder<'a> {
    key: CacheKey,
    sections: [&'a [u8]; 3],
    relocations: &'a [Reloc],
}

impl<'a> EntryBuilder<'a> {
    /// An entry for `key` with empty sections.
    pub const fn new(key: CacheKey) -> Self {
        Self {
            key,
            sections: [&[]; 3],
            relocations: &[],
        }
    }

    /// Sets the machine code.
    pub const fn text(mut self, text: &'a [u8]) -> Self {
        self.sections[0] = text;
        self
    }

    /// Sets the read-only data.
    pub const fn rodata(mut self, rodata: &'a [u8]) -> Self {
        self.sections[1] = rodata;
        self
    }

    /// Sets the runtime's metadata.
    pub const fn metadata(mut self, metadata: &'a [u8]) -> Self {
        self.sections[2] = metadata;
        self
    }

    /// Sets the relocations, sorted by offset.
    pub const fn relocations(mut self, relocations: &'a [Reloc]) -> Self {
        self.relocations = relocations;
        self
    }

    fn lens(&self) -> Result<[usize; 4], EntryError> {
        let relocs = self
            .relocations
            .len()
            .checked_mul(Reloc::SIZE)
            .ok_or(EntryError::TooLarge)?;
        Ok([
            self.sections[0].len(),
            self.sections[1].len(),
            self.sections[2].len(),
            relocs,
        ])
    }

    /// The entry's size in bytes.
    ///
    /// # Errors
    ///
    /// [`EntryError::TooLarge`] if it does not fit the format.
    pub fn encoded_len(&self) -> Result<usize, EntryError> {
        Ok(layout(self.lens()?)?.1)
    }

    /// Writes the entry, with a zero MAC, to the start of `out`, and returns its size.
    ///
    /// # Errors
    ///
    /// [`EntryError::TooLarge`] if it does not fit the format,
    /// [`EntryError::BufferSize`] if `out` is too small, and
    /// [`EntryError::Relocations`] if the relocations are unsorted, overlap or lie outside
    /// the text.
    pub fn write(&self, out: &mut [u8]) -> Result<usize, EntryError> {
        let lens = self.lens()?;
        let (starts, end) = layout(lens)?;
        let out = out
            .get_mut(..end)
            .ok_or(EntryError::BufferSize { needed: end })?;
        out.fill(0);
        let at = &mut 0;
        put(out, at, &MAGIC);
        put(out, at, &VERSION.to_le_bytes());
        put(out, at, &[0; 2]);
        put(out, at, &self.key.encode());
        *at += Mac::SIZE;
        for (start, len) in starts.into_iter().zip(lens) {
            // layout() checked that every end fits in u32.
            put(out, at, &(start as u32).to_le_bytes());
            put(out, at, &(len as u32).to_le_bytes());
        }
        for (section, start) in self.sections.iter().zip(starts) {
            put(out, &mut { start }, section);
        }
        let mut reloc_at = starts[3];
        for reloc in self.relocations {
            put(out, &mut reloc_at, &reloc.encode());
        }
        Relocations::parse(&out[starts[3]..end], lens[0])?;
        Ok(end)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::reloc::RelocKind;
    use std::vec;
    use std::vec::Vec;

    const SECRET: MacKey = MacKey::from_bytes([7; 32]);
    const RELOCS: [Reloc; 1] = [Reloc {
        offset: 1,
        kind: RelocKind::X86PcRel4,
        target: 0,
        addend: -4,
    }];

    fn build(builder: EntryBuilder<'_>) -> Vec<u8> {
        let mut out = vec![0; builder.encoded_len().unwrap()];
        assert_eq!(builder.write(&mut out), Ok(out.len()));
        out
    }

    fn sample() -> Vec<u8> {
        let text = [0xe8, 0, 0, 0, 0, 0xc3];
        let mut bytes = build(
            EntryBuilder::new(CacheKey::example())
                .text(&text)
                .rodata(b"ro")
                .metadata(b"meta")
                .relocations(&RELOCS),
        );
        seal(&mut bytes, &SECRET).unwrap();
        bytes
    }

    #[test]
    fn the_layout_is_fixed() {
        let bytes = sample();
        // Header 184 → text at 192 (6 bytes), rodata at 208 (2), metadata at 224 (4),
        // relocations at 240 (16): 256 bytes.
        assert_eq!(bytes.len(), 256);
        assert_eq!(&bytes[..8], b"FXCC\x01\0\0\0");
        let table: Vec<u32> = bytes[TABLE_AT..HEADER_SIZE]
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(table, [192, 6, 208, 2, 224, 4, 240, 16]);
        let entry = Entry::parse(&bytes).unwrap();
        assert_eq!(entry.text(), &[0xe8, 0, 0, 0, 0, 0xc3]);
        assert_eq!(entry.rodata(), b"ro");
        assert_eq!(entry.metadata(), b"meta");
        assert_eq!(entry.relocations().iter().collect::<Vec<_>>(), RELOCS);
        assert_eq!(entry.verify(&SECRET), Ok(()));
        assert_eq!(
            entry.verify(&MacKey::from_bytes([8; 32])),
            Err(EntryError::BadMac)
        );
    }

    #[test]
    fn an_empty_entry_is_valid() {
        let mut bytes = build(EntryBuilder::new(CacheKey::example()));
        assert_eq!(bytes.len(), 192);
        let unsealed = Entry::parse(&bytes).unwrap();
        assert_eq!(unsealed.verify(&SECRET), Err(EntryError::BadMac));
        seal(&mut bytes, &SECRET).unwrap();
        assert_eq!(Entry::parse(&bytes).unwrap().verify(&SECRET), Ok(()));
    }

    #[test]
    fn every_byte_is_covered() {
        // Each single-bit change is refused by the parser or by the MAC.
        let bytes = sample();
        for at in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[at] ^= 1;
            let result = Entry::parse(&changed).and_then(|entry| entry.verify(&SECRET));
            assert!(result.is_err(), "byte {at}");
        }
    }

    #[test]
    fn header_and_layout_errors() {
        let bytes = sample();
        let with = |at: usize, value: u8| {
            let mut changed = bytes.clone();
            changed[at] = value;
            Entry::parse(&changed).err()
        };
        assert_eq!(with(0, b'X'), Some(EntryError::Magic));
        assert_eq!(with(4, 2), Some(EntryError::Version { found: 2 }));
        assert_eq!(with(7, 1), Some(EntryError::Flags { found: 0x100 }));
        assert_eq!(
            with(KEY_AT + 72, 9).map(|e| matches!(e, EntryError::Key(_))),
            Some(true)
        );
        assert_eq!(
            with(TABLE_AT + 8, 209),
            Some(EntryError::Layout {
                section: Section::Rodata
            })
        );
        assert_eq!(with(200, 1), Some(EntryError::Padding));
        assert_eq!(with(186, 1), Some(EntryError::Padding));
        assert_eq!(
            Entry::parse(&bytes[..255]).err(),
            Some(EntryError::Truncated)
        );
        assert_eq!(
            Entry::parse(&bytes[..100]).err(),
            Some(EntryError::Truncated)
        );
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(Entry::parse(&longer).err(), Some(EntryError::TrailingBytes));
        assert_eq!(
            with(240, 9).map(|e| matches!(e, EntryError::Relocations(_))),
            Some(true)
        );
    }

    #[test]
    fn the_builder_checks_its_inputs() {
        let builder = EntryBuilder::new(CacheKey::example()).text(&[0; 6]);
        assert_eq!(
            builder.write(&mut [0; 100]),
            Err(EntryError::BufferSize { needed: 208 })
        );
        let outside = builder.text(&[0; 4]).relocations(&RELOCS);
        assert_eq!(
            outside.write(&mut [0; 512]),
            Err(EntryError::Relocations(RelocError::OutOfText { offset: 1 }))
        );
    }

    #[test]
    fn mutations_never_panic() {
        // A deterministic stand-in for fuzzing: random multi-byte changes and truncations
        // of a valid entry must return, not panic.
        let bytes = sample();
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut changed = bytes.clone();
            for _ in 0..1 + next() % 4 {
                let at = next() as usize % changed.len();
                changed[at] = next() as u8;
            }
            let len = if next() % 8 == 0 {
                next() as usize % changed.len()
            } else {
                changed.len()
            };
            if let Ok(entry) = Entry::parse(&changed[..len]) {
                let _ = entry.verify(&SECRET);
                let mut text = entry.text().to_vec();
                let _ = entry
                    .relocations()
                    .apply(&mut text, 0x1000, |_| Some(0x2000));
            }
        }
    }
}
