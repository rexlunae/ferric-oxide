//! Ferric Oxide: the parts of an ELF object that loading reads, and how much of the file that
//! is.
//!
//! The dynamic linker maps only the start of an object (see [`loaded_len`]), so it parses the
//! ELF header and program headers alone, without `object`'s `ElfFile`, which also needs the
//! section headers at the end of the file. This module depends only on `object` and `core`, so
//! the host harness (`scripts/ferric-ci/test-ld-so-errors.sh`) tests it on real ELF files.
//!
//! See the ELF specification's "ELF Header" and "Program Header" chapters
//! (<https://refspecs.linuxfoundation.org/elf/elf.pdf>).

use object::{
    NativeEndian,
    read::elf::{FileHeader as _, ProgramHeader as _},
};

#[cfg(target_pointer_width = "32")]
mod shim {
    use object::{NativeEndian, elf};
    pub type FileHeader = elf::FileHeader32<NativeEndian>;
    pub type ProgramHeader = elf::ProgramHeader32<NativeEndian>;
}

#[cfg(target_pointer_width = "64")]
mod shim {
    use object::{NativeEndian, elf};
    pub type FileHeader = elf::FileHeader64<NativeEndian>;
    pub type ProgramHeader = elf::ProgramHeader64<NativeEndian>;
}

pub use shim::*;

/// The parts of an ELF object that loading uses, its header and program headers. Unlike `object`'s `ElfFile`, parsing them does not read the section headers,
/// which come after the part of the file [`loaded_len`] maps.
pub(super) struct Image<'a> {
    pub(super) header: &'a FileHeader,
    pub(super) segments: &'a [ProgramHeader],
}

impl<'a> Image<'a> {
    /// Parses the header and program headers at the start of `data`.
    pub(super) fn parse(data: &'a [u8]) -> object::Result<Self> {
        let header = FileHeader::parse(data)?;
        let segments = header.program_headers(header.endian()?, data)?;
        Ok(Self { header, segments })
    }

    /// The entry point, as the header gives it.
    pub(super) fn entry(&self) -> u64 {
        self.header.e_entry(NativeEndian).into()
    }

    /// The end of the program header table or of the last part of the file a program
    /// header covers, whichever is later.
    fn loaded_end(&self) -> u64 {
        let table_start: u64 = self.header.e_phoff(NativeEndian).into();
        let table_end = table_start.saturating_add(size_of_val(self.segments) as u64);
        self.segments
            .iter()
            .map(|ph| {
                let (offset, size) = ph.file_range(NativeEndian);
                offset.saturating_add(size)
            })
            .fold(table_end, u64::max)
    }
}

/// How many bytes from the start of an object loading reads, given its first
/// bytes `probe` and its length `size`: up to the end of its program header table or of the
/// last part of the file a program header covers. The section headers and the sections no
/// segment holds (debug info, the full symbol table) usually come after, and are not mapped.
/// When `probe` does not hold the header and program headers, all of `size`, so that parsing
/// the mapped object reports what is wrong with it.
pub(super) fn loaded_len(probe: &[u8], size: usize) -> usize {
    Image::parse(probe)
        .ok()
        .and_then(|image| usize::try_from(image.loaded_end()).ok())
        .map_or(size, |end| end.min(size))
}
