//! Moves a statically linked x86_64 ELF image to a new base address, using the
//! relocations the linker kept with `--emit-relocs`.
//!
//! The Ferric Oxide bootloader uses this to load the kernel at a random
//! address (kernel image KASLR). The kernel is linked at a fixed address
//! (`-mcmodel=kernel`, not position independent), so its code holds absolute
//! addresses. Linking with `--emit-relocs` keeps every relocation the linker
//! resolved in `.rela.*` sections of the final file, and this crate replays
//! them for a slide: every allocated section moves by the same amount, so
//!
//! - an absolute reference (`R_X86_64_64`, `R_X86_64_32S`, `R_X86_64_32`) to
//!   something inside the image gains the slide, and one to an absolute
//!   symbol stays;
//! - a PC-relative reference (`R_X86_64_PC32`, `R_X86_64_PLT32`,
//!   `R_X86_64_PC64`) to something inside the image stays, and one to an
//!   absolute symbol loses the slide.
//!
//! Relocations that patch sections which are not loaded (debug information)
//! are ignored. Any other relocation type is an error, and the image is left
//! untouched: every relocation is checked before the first one is applied.
//!
//! The image is the ELF file as loaded into memory, with each section at its
//! file offset (the layout the Redox bootloader uses for the kernel).
//!
//! Formats: the ELF-64 Object File Format (version 1.5) and the System V
//! AMD64 psABI (relocation types and their calculations).
//!
//! # Examples
//!
//! ```
//! // Not an ELF file: nothing to relocate.
//! let mut image = [0_u8; 64];
//! assert!(ferric_elfreloc::relocate(&mut image, 0x20_0000).is_err());
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use core::fmt;

const EHDR_SIZE: usize = 64;
const SHDR_SIZE: usize = 64;
const SYM_SIZE: usize = 24;
const RELA_SIZE: usize = 24;

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_X86_64: u16 = 62;

const SHT_SYMTAB: u32 = 2;
const SHT_RELA: u32 = 4;
const SHF_ALLOC: u64 = 2;

const SHN_UNDEF: u16 = 0;
const SHN_LORESERVE: u16 = 0xff00;

const R_X86_64_NONE: u32 = 0;
const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;
const R_X86_64_PC64: u32 = 24;

/// Why an image could not be relocated. The image is unchanged in every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Not a little-endian ELF-64 file.
    NotElf64,
    /// An ELF file for a machine other than x86_64.
    WrongMachine(u16),
    /// A header, section or table runs past the end of the image.
    Truncated,
    /// A section header names a section index that does not exist.
    BadSectionIndex(u32),
    /// A relocation names a symbol past the end of its symbol table.
    BadSymbol(u32),
    /// The image has no relocations for its loaded sections, so it was not
    /// linked with `--emit-relocs` and cannot move.
    NoRelocations,
    /// A relocation of a type this crate does not handle, at this address.
    UnsupportedType {
        /// The relocation type (`R_X86_64_*`).
        kind: u32,
        /// The address it patches.
        offset: u64,
    },
    /// A relocation patches bytes outside its section.
    OutOfSection {
        /// The address it patches.
        offset: u64,
    },
    /// The relocated value no longer fits its field (a 32-bit reference
    /// moved out of range).
    Overflow {
        /// The address it patches.
        offset: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotElf64 => write!(f, "not a little-endian ELF-64 image"),
            Self::WrongMachine(m) => write!(f, "ELF machine {m} is not x86_64"),
            Self::Truncated => write!(f, "image is truncated"),
            Self::BadSectionIndex(i) => write!(f, "section index {i} does not exist"),
            Self::BadSymbol(i) => write!(f, "symbol index {i} does not exist"),
            Self::NoRelocations => write!(f, "image has no relocations (link with --emit-relocs)"),
            Self::UnsupportedType { kind, offset } => {
                write!(f, "unsupported relocation type {kind} at {offset:#x}")
            }
            Self::OutOfSection { offset } => {
                write!(f, "relocation at {offset:#x} is outside its section")
            }
            Self::Overflow { offset } => {
                write!(f, "relocated value at {offset:#x} does not fit its field")
            }
        }
    }
}

impl core::error::Error for Error {}

/// What [`relocate`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Relocations read for loaded sections.
    pub relocations: usize,
    /// Fields that changed.
    pub patched: usize,
}

/// Moves `image` by `slide` bytes: after this, the image is correct when
/// loaded `slide` bytes above its link address. The entry point moves by the
/// same amount (see [`entry`]).
///
/// A slide of 0 checks the relocations without changing anything.
///
/// # Errors
///
/// Returns an [`Error`] and leaves the image unchanged if it is not an x86_64
/// ELF-64 file, has no relocations for its loaded sections, or has a
/// relocation this crate cannot replay for this slide.
pub fn relocate(image: &mut [u8], slide: u64) -> Result<Stats, Error> {
    let stats = walk(image, slide, false)?;
    if stats.relocations == 0 {
        return Err(Error::NoRelocations);
    }
    walk(image, slide, true)?;
    Ok(stats)
}

/// The image's entry point at its link address.
///
/// # Errors
///
/// Fails if the image is not an x86_64 ELF-64 file.
pub fn entry(image: &[u8]) -> Result<u64, Error> {
    header(image)?;
    read_u64(image, 24)
}

/// The fields of a section header this crate uses.
struct Section {
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    entsize: u64,
}

impl Section {
    fn loaded(&self) -> bool {
        self.flags & SHF_ALLOC != 0
    }
}

/// Where a relocation's target lives, which decides how it moves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    /// In a loaded section: moves with the image.
    Image,
    /// An absolute or undefined symbol: stays put.
    Fixed,
}

/// Header fields: section header table offset, count and string table index.
fn header(image: &[u8]) -> Result<(u64, u16), Error> {
    if image.len() < EHDR_SIZE || image[..4] != *b"\x7fELF" {
        return Err(Error::NotElf64);
    }
    if image[4] != ELFCLASS64 || image[5] != ELFDATA2LSB {
        return Err(Error::NotElf64);
    }
    let machine = read_u16(image, 18)?;
    if machine != EM_X86_64 {
        return Err(Error::WrongMachine(machine));
    }
    if read_u16(image, 58)? as usize != SHDR_SIZE {
        return Err(Error::NotElf64);
    }
    Ok((read_u64(image, 40)?, read_u16(image, 60)?))
}

fn section(image: &[u8], shoff: u64, shnum: u16, index: u32) -> Result<Section, Error> {
    if index >= u32::from(shnum) {
        return Err(Error::BadSectionIndex(index));
    }
    let at = to_usize(shoff)?
        .checked_add(index as usize * SHDR_SIZE)
        .ok_or(Error::Truncated)?;
    Ok(Section {
        kind: read_u32(image, at + 4)?,
        flags: read_u64(image, at + 8)?,
        addr: read_u64(image, at + 16)?,
        offset: read_u64(image, at + 24)?,
        size: read_u64(image, at + 32)?,
        link: read_u32(image, at + 40)?,
        info: read_u32(image, at + 44)?,
        entsize: read_u64(image, at + 56)?,
    })
}

/// Goes through every relocation of every loaded section. Without `apply`
/// it only checks them; with it, it writes the new values.
fn walk(image: &mut [u8], slide: u64, apply: bool) -> Result<Stats, Error> {
    let (shoff, shnum) = header(image)?;
    let mut stats = Stats::default();
    for index in 0..u32::from(shnum) {
        let rela = section(image, shoff, shnum, index)?;
        if rela.kind != SHT_RELA {
            continue;
        }
        let place = section(image, shoff, shnum, rela.info)?;
        if !place.loaded() {
            continue;
        }
        let symtab = section(image, shoff, shnum, rela.link)?;
        if symtab.kind != SHT_SYMTAB || symtab.entsize as usize != SYM_SIZE {
            return Err(Error::BadSectionIndex(rela.link));
        }
        if rela.entsize as usize != RELA_SIZE {
            return Err(Error::Truncated);
        }
        let start = to_usize(rela.offset)?;
        let count = to_usize(rela.size)? / RELA_SIZE;
        for i in 0..count {
            let at = start + i * RELA_SIZE;
            let offset = read_u64(image, at)?;
            let info = read_u64(image, at + 8)?;
            let (sym, kind) = ((info >> 32) as u32, info as u32);
            stats.relocations += 1;
            let target = symbol_target(image, shoff, shnum, &symtab, sym)?;
            if patch(image, &place, offset, kind, target, slide, apply)? {
                stats.patched += 1;
            }
        }
    }
    Ok(stats)
}

fn symbol_target(
    image: &[u8],
    shoff: u64,
    shnum: u16,
    symtab: &Section,
    sym: u32,
) -> Result<Target, Error> {
    // Symbol 0 is "no symbol": the addend alone is the (absolute) value.
    if sym == 0 {
        return Ok(Target::Fixed);
    }
    if u64::from(sym) >= symtab.size / SYM_SIZE as u64 {
        return Err(Error::BadSymbol(sym));
    }
    let at = to_usize(symtab.offset)? + sym as usize * SYM_SIZE;
    let shndx = read_u16(image, at + 6)?;
    if shndx == SHN_UNDEF || shndx >= SHN_LORESERVE {
        // Undefined (a weak symbol resolved to 0) or absolute (SHN_ABS), or
        // another reserved index; none of them move with the image.
        return Ok(Target::Fixed);
    }
    let target = section(image, shoff, shnum, u32::from(shndx))?;
    Ok(if target.loaded() {
        Target::Image
    } else {
        Target::Fixed
    })
}

/// Checks one relocation and, with `apply`, writes its new value. Returns
/// whether the field changes.
fn patch(
    image: &mut [u8],
    place: &Section,
    offset: u64,
    kind: u32,
    target: Target,
    slide: u64,
    apply: bool,
) -> Result<bool, Error> {
    // How the field's value changes: absolute references to the image gain
    // the slide; PC-relative references to fixed targets lose it, since the
    // place moved and the target did not.
    let delta = match (kind, target) {
        (R_X86_64_NONE, _) => return Ok(false),
        (R_X86_64_64 | R_X86_64_32S | R_X86_64_32, Target::Image) => slide as i128,
        (R_X86_64_PC32 | R_X86_64_PLT32 | R_X86_64_PC64, Target::Fixed) => -(slide as i128),
        (R_X86_64_64 | R_X86_64_32S | R_X86_64_32, Target::Fixed)
        | (R_X86_64_PC32 | R_X86_64_PLT32 | R_X86_64_PC64, Target::Image) => 0,
        _ => return Err(Error::UnsupportedType { kind, offset }),
    };
    let width = match kind {
        R_X86_64_64 | R_X86_64_PC64 => 8,
        _ => 4,
    };
    let rel = offset
        .checked_sub(place.addr)
        .filter(|rel| rel.checked_add(width).is_some_and(|end| end <= place.size))
        .ok_or(Error::OutOfSection { offset })?;
    let at = to_usize(place.offset)?
        .checked_add(to_usize(rel)?)
        .ok_or(Error::Truncated)?;
    if delta == 0 {
        // Still check the field is inside the image.
        read_field(image, at, width)?;
        return Ok(false);
    }
    let old = read_field(image, at, width)?;
    let old = match kind {
        R_X86_64_32 => i128::from(old as u32),
        R_X86_64_64 | R_X86_64_PC64 => i128::from(old as i64),
        _ => i128::from(old as u32 as i32),
    };
    let new = old + delta;
    let fits = match kind {
        R_X86_64_64 | R_X86_64_PC64 => i64::try_from(new).is_ok() || u64::try_from(new).is_ok(),
        R_X86_64_32 => u32::try_from(new).is_ok(),
        _ => i32::try_from(new).is_ok(),
    };
    if !fits {
        return Err(Error::Overflow { offset });
    }
    if apply {
        let bytes = (new as u64).to_le_bytes();
        image[at..at + width as usize].copy_from_slice(&bytes[..width as usize]);
    }
    Ok(true)
}

fn to_usize(v: u64) -> Result<usize, Error> {
    usize::try_from(v).map_err(|_| Error::Truncated)
}

fn bytes<const N: usize>(image: &[u8], at: usize) -> Result<[u8; N], Error> {
    image
        .get(at..at.checked_add(N).ok_or(Error::Truncated)?)
        .and_then(|b| b.try_into().ok())
        .ok_or(Error::Truncated)
}

fn read_u16(image: &[u8], at: usize) -> Result<u16, Error> {
    bytes(image, at).map(u16::from_le_bytes)
}

fn read_u32(image: &[u8], at: usize) -> Result<u32, Error> {
    bytes(image, at).map(u32::from_le_bytes)
}

fn read_u64(image: &[u8], at: usize) -> Result<u64, Error> {
    bytes(image, at).map(u64::from_le_bytes)
}

fn read_field(image: &[u8], at: usize, width: u64) -> Result<u64, Error> {
    match width {
        8 => read_u64(image, at),
        _ => read_u32(image, at).map(u64::from),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const BASE: u64 = 0xffff_ffff_8000_0000;
    const TEXT: u64 = BASE + 0x1000;
    const ABS: u64 = 0x1234_5678;

    /// One relocation in the test image's `.text`: (offset in .text, type,
    /// symbol index, the value the linker wrote).
    type Rel = (u64, u32, u32, u64);

    /// Builds a minimal linked image: `.text` (16 words of 8 bytes) at
    /// `TEXT`, a non-loaded `.debug` section, symbols for `.text` (section
    /// symbol, index 1) and an absolute symbol (index 2), and the given
    /// relocations against `.text`, plus one against `.debug`.
    fn image(rels: &[Rel]) -> Vec<u8> {
        let text_off = 0x1000_usize;
        let text_len = 128_usize;
        let debug_off = text_off + text_len;
        let debug_len = 8;
        let sym_off = debug_off + debug_len;
        let syms = 3;
        let rela_off = sym_off + syms * SYM_SIZE;
        let rela_debug_off = rela_off + rels.len() * RELA_SIZE;
        let sh_off = rela_debug_off + RELA_SIZE;
        // null, .text, .debug, .symtab, .rela.text, .rela.debug
        let shnum = 6;
        let mut out = std::vec![0_u8; sh_off + shnum * SHDR_SIZE];

        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = ELFCLASS64;
        out[5] = ELFDATA2LSB;
        out[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        out[24..32].copy_from_slice(&TEXT.to_le_bytes());
        out[40..48].copy_from_slice(&(sh_off as u64).to_le_bytes());
        out[58..60].copy_from_slice(&(SHDR_SIZE as u16).to_le_bytes());
        out[60..62].copy_from_slice(&(shnum as u16).to_le_bytes());

        let mut shdr = |i: usize,
                        kind: u32,
                        flags: u64,
                        addr: u64,
                        off: usize,
                        size: usize,
                        link: u32,
                        info: u32,
                        entsize: usize| {
            let at = sh_off + i * SHDR_SIZE;
            out[at + 4..at + 8].copy_from_slice(&kind.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&flags.to_le_bytes());
            out[at + 16..at + 24].copy_from_slice(&addr.to_le_bytes());
            out[at + 24..at + 32].copy_from_slice(&(off as u64).to_le_bytes());
            out[at + 32..at + 40].copy_from_slice(&(size as u64).to_le_bytes());
            out[at + 40..at + 44].copy_from_slice(&link.to_le_bytes());
            out[at + 44..at + 48].copy_from_slice(&info.to_le_bytes());
            out[at + 56..at + 64].copy_from_slice(&(entsize as u64).to_le_bytes());
        };
        shdr(1, 1, SHF_ALLOC | 4, TEXT, text_off, text_len, 0, 0, 0);
        shdr(2, 1, 0, 0, debug_off, debug_len, 0, 0, 0);
        shdr(
            3,
            SHT_SYMTAB,
            0,
            0,
            sym_off,
            syms * SYM_SIZE,
            0,
            0,
            SYM_SIZE,
        );
        shdr(
            4,
            SHT_RELA,
            0,
            0,
            rela_off,
            rels.len() * RELA_SIZE,
            3,
            1,
            RELA_SIZE,
        );
        shdr(
            5,
            SHT_RELA,
            0,
            0,
            rela_debug_off,
            RELA_SIZE,
            3,
            2,
            RELA_SIZE,
        );

        // Symbol 1: section symbol of .text. Symbol 2: absolute.
        let s1 = sym_off + SYM_SIZE;
        out[s1 + 6..s1 + 8].copy_from_slice(&1_u16.to_le_bytes());
        let s2 = sym_off + 2 * SYM_SIZE;
        out[s2 + 6..s2 + 8].copy_from_slice(&0xfff1_u16.to_le_bytes());

        for (i, &(off, kind, sym, value)) in rels.iter().enumerate() {
            let at = rela_off + i * RELA_SIZE;
            out[at..at + 8].copy_from_slice(&(TEXT + off).to_le_bytes());
            let info = (u64::from(sym) << 32) | u64::from(kind);
            out[at + 8..at + 16].copy_from_slice(&info.to_le_bytes());
            let field = text_off + off as usize;
            let width = if matches!(kind, R_X86_64_64 | R_X86_64_PC64) {
                8
            } else {
                4
            };
            out[field..field + width].copy_from_slice(&value.to_le_bytes()[..width]);
        }
        // A debug relocation that must be ignored.
        let at = rela_debug_off;
        out[at..at + 8].copy_from_slice(&0_u64.to_le_bytes());
        out[at + 8..at + 16]
            .copy_from_slice(&((1_u64 << 32) | u64::from(R_X86_64_64)).to_le_bytes());
        out[debug_off..debug_off + 8].copy_from_slice(&0x55_u64.to_le_bytes());
        out
    }

    fn word(image: &[u8], off: u64, width: usize) -> u64 {
        let at = 0x1000 + off as usize;
        let mut b = [0_u8; 8];
        b[..width].copy_from_slice(&image[at..at + width]);
        u64::from_le_bytes(b)
    }

    #[test]
    fn absolute_references_into_the_image_move() {
        let mut img = image(&[
            (0, R_X86_64_64, 1, TEXT + 0x40),
            (8, R_X86_64_32S, 1, (TEXT + 0x10) & 0xffff_ffff),
        ]);
        let stats = relocate(&mut img, 0x20_0000).unwrap();
        assert_eq!(
            stats,
            Stats {
                relocations: 2,
                patched: 2
            }
        );
        assert_eq!(word(&img, 0, 8), TEXT + 0x40 + 0x20_0000);
        assert_eq!(
            word(&img, 8, 4) as u32 as i32 as i64 as u64,
            TEXT + 0x10 + 0x20_0000
        );
    }

    #[test]
    fn absolute_symbols_stay_and_pc_relative_ones_compensate() {
        let mut img = image(&[
            (0, R_X86_64_64, 2, ABS),
            (8, R_X86_64_PC32, 2, 0x100),
            (16, R_X86_64_PLT32, 1, 0x20),
        ]);
        let stats = relocate(&mut img, 0x3000).unwrap();
        assert_eq!(
            stats,
            Stats {
                relocations: 3,
                patched: 1
            }
        );
        assert_eq!(word(&img, 0, 8), ABS);
        assert_eq!(word(&img, 8, 4) as u32, 0x100_u32.wrapping_sub(0x3000));
        assert_eq!(word(&img, 16, 4), 0x20);
    }

    #[test]
    fn debug_sections_are_ignored() {
        let mut img = image(&[(0, R_X86_64_NONE, 0, 0)]);
        relocate(&mut img, 0x1000).unwrap();
        assert_eq!(&img[0x1080..0x1088], &0x55_u64.to_le_bytes());
    }

    #[test]
    fn unsupported_types_leave_the_image_unchanged() {
        const R_X86_64_GOTPCREL: u32 = 9;
        let mut img = image(&[(0, R_X86_64_64, 1, TEXT), (8, R_X86_64_GOTPCREL, 1, 0)]);
        let before = img.clone();
        assert_eq!(
            relocate(&mut img, 0x1000),
            Err(Error::UnsupportedType {
                kind: R_X86_64_GOTPCREL,
                offset: TEXT + 8
            })
        );
        assert_eq!(img, before);
    }

    #[test]
    fn overflowing_32_bit_references_are_refused() {
        // 0x7fff_f000 as a sign-extended 32-bit field cannot move up 1 MiB.
        let mut img = image(&[(0, R_X86_64_32S, 1, 0x7fff_f000)]);
        assert_eq!(
            relocate(&mut img, 0x10_0000),
            Err(Error::Overflow { offset: TEXT })
        );
    }

    #[test]
    fn relocations_outside_their_section_are_refused() {
        let mut img = image(&[(126, R_X86_64_32S, 1, 0)]);
        assert_eq!(
            relocate(&mut img, 0x1000),
            Err(Error::OutOfSection { offset: TEXT + 126 })
        );
    }

    #[test]
    fn images_without_relocations_are_refused() {
        let mut img = image(&[]);
        assert_eq!(relocate(&mut img, 0x1000), Err(Error::NoRelocations));
        assert_eq!(entry(&img), Ok(TEXT));
    }

    #[test]
    fn non_elf_input_is_refused() {
        let mut junk = [0_u8; 100];
        assert_eq!(relocate(&mut junk, 0), Err(Error::NotElf64));
        let mut short = *b"\x7fELF";
        assert_eq!(relocate(&mut short, 0), Err(Error::NotElf64));
    }
}
