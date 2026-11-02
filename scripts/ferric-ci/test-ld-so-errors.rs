//! Host harness for the actual loader search, fatal formatter and relibc Errno.
//! Only platform errno storage, its message table and io::Error are host adapters.
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]
extern crate alloc;
mod platform {
    pub mod types {
        #[allow(non_camel_case_types)]
        pub type c_int = i32;
        #[allow(non_camel_case_types)]
        pub type off_t = i64;
        pub use core::ffi::c_void;
    }
    use crate::{c_str::CStr, error::Errno, header::sys_stat::stat, out::Out};
    pub struct Sys;
    pub trait Pal {
        fn open(path: CStr, flags: i32, mode: u32) -> Result<i32, Errno>;
        fn fstat(fd: i32, out: Out<stat>) -> Result<(), Errno>;
        fn pread(fd: i32, buf: &mut [u8], offset: i64) -> Result<usize, Errno>;
        unsafe fn mmap(
            addr: *mut types::c_void,
            size: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            off: i64,
        ) -> Result<*mut types::c_void, Errno>;
        unsafe fn munmap(addr: *mut types::c_void, size: usize) -> Result<(), Errno>;
    }
    impl Pal for Sys {
        fn open(_: CStr, _: i32, _: u32) -> Result<i32, Errno> {
            crate::mapping_tests::step("open")?;
            Ok(42)
        }
        fn fstat(_: i32, out: Out<stat>) -> Result<(), Errno> {
            crate::mapping_tests::step("stat")?;
            out.0.st_size = crate::mapping_tests::STATE.with(|s| s.borrow().size);
            Ok(())
        }
        fn pread(_: i32, buf: &mut [u8], offset: i64) -> Result<usize, Errno> {
            crate::mapping_tests::step("read")?;
            crate::mapping_tests::STATE.with(|s| {
                let mut s = s.borrow_mut();
                if s.interrupts > 0 {
                    s.interrupts -= 1;
                    return Err(Errno(crate::header::errno::EINTR));
                }
                let offset = usize::try_from(offset).unwrap();
                assert_eq!(
                    offset,
                    s.probed.unwrap_or(0),
                    "reads continue where the last ended"
                );
                let size = usize::try_from(s.size).unwrap();
                let count = buf.len().min(s.chunk).min(size.saturating_sub(offset));
                buf[..count].fill(5);
                s.probed = Some(offset + count);
                Ok(count)
            })
        }
        unsafe fn mmap(
            _: *mut types::c_void,
            size: usize,
            _: i32,
            _: i32,
            _: i32,
            _: i64,
        ) -> Result<*mut types::c_void, Errno> {
            crate::mapping_tests::step("map")?;
            crate::mapping_tests::STATE.with(|s| {
                let mut s = s.borrow_mut();
                let mut bytes = vec![7; size].into_boxed_slice();
                let ptr = bytes.as_mut_ptr().cast();
                s.mapping = Some(bytes);
                Ok(ptr)
            })
        }
        unsafe fn munmap(_: *mut types::c_void, _: usize) -> Result<(), Errno> {
            crate::mapping_tests::step("unmap")?;
            crate::mapping_tests::STATE.with(|s| s.borrow_mut().mapping = None);
            Ok(())
        }
    }
    std::thread_local! { pub static ERRNO: core::cell::Cell<i32> = const { core::cell::Cell::new(0) }; }
}
mod header {
    pub mod errno {
        pub const ENOENT: i32 = 2;
        pub const EINTR: i32 = 4;
        pub const EOVERFLOW: i32 = 75;
        pub const STR_ERROR: [&str; 1] = ["Success"];
    }
    pub mod sys_stat {
        #[derive(Default)]
        #[allow(non_camel_case_types)]
        pub struct stat {
            pub st_size: i64,
        }
    }
    pub mod sys_mman {
        pub const PROT_READ: i32 = 1;
        pub const MAP_PRIVATE: i32 = 2;
    }
}
mod c_str {
    pub type CStr<'a> = &'a str;
}
mod out {
    pub struct Out<'a, T>(pub &'a mut T);
    impl<'a, T> Out<'a, T> {
        pub fn from_mut(value: &'a mut T) -> Self {
            Self(value)
        }
    }
}
mod fs {
    pub struct File {
        pub fd: i32,
    }
    impl File {
        pub fn new(fd: i32) -> Self {
            Self { fd }
        }
    }
    impl Drop for File {
        fn drop(&mut self) {
            crate::mapping_tests::step("close").unwrap();
        }
    }
}
mod io {
    pub use std::io::Error;
}
#[path = "../../crates/relibc/src/error.rs"]
mod error;
#[path = "../../crates/relibc/src/ld_so/load_error.rs"]
mod load_error;

#[path = "../../crates/relibc/src/ld_so/object_search.rs"]
mod object_search;

#[path = "../../crates/relibc/src/ld_so/mmap_file.rs"]
mod mmap_file;

#[path = "../../crates/relibc/src/ld_so/image.rs"]
mod image;

/// How much of an ELF object the loader maps (`image::loaded_len`), on hand-built objects and
/// on this test binary, a real ELF file with section headers after its segments.
mod image_tests {
    use super::{image::loaded_len, mmap_file::PROBE_LEN};
    use object::{
        NativeEndian as E, U16, U32, U64,
        elf::{self, FileHeader64, Ident, ProgramHeader64},
        pod::bytes_of,
        read::elf::{FileHeader as _, ProgramHeader as _},
    };

    const HEADER_SIZE: usize = size_of::<FileHeader64<E>>();
    const SEGMENT_SIZE: usize = size_of::<ProgramHeader64<E>>();

    /// An ELF header at the start of a `len`-byte object, with its program header table at
    /// `phoff` holding `segments` as (type, offset, file size), and four section headers at
    /// the end of the file.
    fn object(phoff: usize, phnum: u16, segments: &[(u32, u64, u64)], len: usize) -> Vec<u8> {
        let u64_of = |n: usize| U64::new(E, u64::try_from(n).unwrap());
        let mut bytes = vec![0; len];
        let header = FileHeader64::<E> {
            e_ident: Ident {
                magic: elf::ELFMAG,
                class: elf::ELFCLASS64,
                data: elf::ELFDATA2LSB,
                version: elf::EV_CURRENT,
                os_abi: elf::ELFOSABI_SYSV,
                abi_version: 0,
                padding: [0; 7],
            },
            e_type: U16::new(E, elf::ET_DYN),
            e_machine: U16::new(E, elf::EM_X86_64),
            e_version: U32::new(E, elf::EV_CURRENT.into()),
            e_entry: U64::new(E, 0),
            e_phoff: u64_of(phoff),
            e_shoff: u64_of(len - 4 * 64),
            e_flags: U32::new(E, 0),
            e_ehsize: U16::new(E, HEADER_SIZE as u16),
            e_phentsize: U16::new(E, SEGMENT_SIZE as u16),
            e_phnum: U16::new(E, phnum),
            e_shentsize: U16::new(E, 64),
            e_shnum: U16::new(E, 4),
            e_shstrndx: U16::new(E, 0),
        };
        bytes[..HEADER_SIZE].copy_from_slice(bytes_of(&header));
        for (i, &(kind, offset, size)) in segments.iter().enumerate() {
            let segment = ProgramHeader64::<E> {
                p_type: U32::new(E, kind),
                p_flags: U32::new(E, elf::PF_R),
                p_offset: U64::new(E, offset),
                p_vaddr: U64::new(E, offset),
                p_paddr: U64::new(E, offset),
                p_filesz: U64::new(E, size),
                p_memsz: U64::new(E, size),
                p_align: U64::new(E, 0x1000),
            };
            let at = phoff + i * SEGMENT_SIZE;
            bytes[at..at + SEGMENT_SIZE].copy_from_slice(bytes_of(&segment));
        }
        bytes
    }

    fn probe(bytes: &[u8]) -> &[u8] {
        &bytes[..bytes.len().min(PROBE_LEN)]
    }

    fn table(segments: &[(u32, u64, u64)]) -> u16 {
        u16::try_from(segments.len()).unwrap()
    }

    #[test]
    fn the_mapping_ends_with_the_last_segment_contents() {
        let segments = [
            (elf::PT_LOAD, 0, 0x2000),
            (elf::PT_LOAD, 0x2000, 0x1800),
            (elf::PT_DYNAMIC, 0x3000, 0x100),
        ];
        let bytes = object(HEADER_SIZE, table(&segments), &segments, 1 << 20);
        assert_eq!(loaded_len(probe(&bytes), bytes.len()), 0x3800);
    }

    #[test]
    fn every_program_header_counts_not_only_load_segments() {
        let segments = [(elf::PT_LOAD, 0, 0x2000), (elf::PT_NOTE, 0x5000, 0x20)];
        let bytes = object(HEADER_SIZE, table(&segments), &segments, 1 << 20);
        assert_eq!(loaded_len(probe(&bytes), bytes.len()), 0x5020);
    }

    #[test]
    fn the_program_header_table_counts_when_it_ends_last() {
        let segments = [(elf::PT_LOAD, 0, 0x100)];
        let bytes = object(0x800, table(&segments), &segments, 1 << 20);
        assert_eq!(loaded_len(probe(&bytes), bytes.len()), 0x800 + SEGMENT_SIZE);
    }

    #[test]
    fn a_segment_past_the_end_of_the_file_maps_only_the_file() {
        let segments = [(elf::PT_LOAD, 0, 0x3800)];
        let bytes = object(HEADER_SIZE, table(&segments), &segments, 0x2000);
        assert_eq!(loaded_len(probe(&bytes), bytes.len()), 0x2000);
    }

    /// Whatever the probe cannot parse maps the whole file, so the full parse reports it.
    #[test]
    fn unparsable_headers_map_the_whole_file() {
        let segments = [(elf::PT_LOAD, 0, 0x2000)];
        let size = 1 << 20;
        let not_elf = vec![0x55; size];
        assert_eq!(loaded_len(probe(&not_elf), size), size);
        let truncated = object(HEADER_SIZE, table(&segments), &segments, size);
        assert_eq!(loaded_len(&truncated[..32], size), size);
        let past_probe = object(PROBE_LEN + HEADER_SIZE, table(&segments), &segments, size);
        assert_eq!(loaded_len(probe(&past_probe), size), size);
        // PN_XNUM: the real count is in section header 0, at the end of the file.
        let extended = object(HEADER_SIZE, elf::PN_XNUM, &segments, size);
        assert_eq!(loaded_len(probe(&extended), size), size);
    }

    /// This test binary is a real ELF object: the mapping ends exactly where its last
    /// program header range or the program header table ends, and before the section
    /// header table.
    #[test]
    fn a_real_object_maps_its_segments_and_not_its_section_headers() {
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let header = FileHeader64::<E>::parse(&*bytes).unwrap();
        let segments = header.program_headers(E, &*bytes).unwrap();
        let table_end = header.e_phoff(E) + size_of_val(segments) as u64;
        let expected = segments
            .iter()
            .map(|segment| {
                let (offset, size) = segment.file_range(E);
                offset + size
            })
            .fold(table_end, u64::max);
        let len = u64::try_from(loaded_len(probe(&bytes), bytes.len())).unwrap();
        assert_eq!(len, expected);
        let shoff = header.e_shoff(E);
        assert!(
            len <= shoff && shoff < bytes.len() as u64,
            "{len:#x} {shoff:#x}"
        );
    }
}

mod mapping_tests {
    use super::{
        error::Errno,
        load_error::ObjectIoOperation,
        mmap_file::{MmapFile, PROBE_LEN},
    };
    use std::cell::RefCell;
    pub struct State {
        pub fail: Option<&'static str>,
        pub size: i64,
        pub calls: Vec<&'static str>,
        pub mapping: Option<Box<[u8]>>,
        /// How far into the file the probe has read.
        pub probed: Option<usize>,
        /// How many reads fail with `EINTR` before one succeeds.
        pub interrupts: usize,
        /// The most one read returns.
        pub chunk: usize,
    }
    std::thread_local! { pub static STATE: RefCell<State> = const { RefCell::new(State { fail: None, size: 16, calls: Vec::new(), mapping: None, probed: None, interrupts: 0, chunk: usize::MAX }) }; }
    pub fn step(operation: &'static str) -> Result<(), Errno> {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.calls.push(operation);
            if s.fail == Some(operation) {
                Err(Errno(12))
            } else {
                Ok(())
            }
        })
    }
    fn reset(fail: Option<&'static str>, size: i64) {
        STATE.with(|s| {
            *s.borrow_mut() = State {
                fail,
                size,
                calls: vec![],
                mapping: None,
                probed: None,
                interrupts: 0,
                chunk: usize::MAX,
            }
        });
    }
    /// Maps the whole file, as for an object whose headers could not be parsed.
    fn whole(_: &[u8], size: usize) -> usize {
        size
    }
    #[test]
    fn failed_operations_keep_errno_and_close_only_owned_descriptors() {
        for (fail, operation, calls) in [
            ("open", ObjectIoOperation::Open, vec!["open"]),
            (
                "stat",
                ObjectIoOperation::Stat,
                vec!["open", "stat", "close"],
            ),
            (
                "read",
                ObjectIoOperation::Read,
                vec!["open", "stat", "read", "close"],
            ),
            (
                "map",
                ObjectIoOperation::Map { size: 16 },
                vec!["open", "stat", "read", "map", "close"],
            ),
        ] {
            reset(Some(fail), 16);
            let error = MmapFile::open("/lib/test.so", 0, whole).err().unwrap();
            assert_eq!(error.operation, operation);
            assert_eq!(error.source.0, 12);
            STATE.with(|s| assert_eq!(s.borrow().calls, calls));
        }
    }
    #[test]
    fn invalid_file_size_is_typed_and_closes_the_descriptor() {
        reset(None, -1);
        let error = MmapFile::open("/lib/test.so", 0, whole).err().unwrap();
        assert_eq!(error.operation, ObjectIoOperation::Stat);
        assert_eq!(error.source.0, 75);
        STATE.with(|s| assert_eq!(s.borrow().calls, ["open", "stat", "close"]));
    }
    #[test]
    fn successful_mapping_keeps_descriptor_until_after_unmap() {
        reset(None, 16);
        let map = MmapFile::open("/lib/test.so", 0, whole).unwrap();
        assert_eq!(map.data(), [7; 16]);
        STATE.with(|s| assert_eq!(s.borrow().calls, ["open", "stat", "read", "map"]));
        drop(map);
        STATE.with(|s| {
            assert_eq!(
                s.borrow().calls,
                ["open", "stat", "read", "map", "unmap", "close"]
            )
        });
    }
    /// Only the start of the file the caller asks for is mapped, decided from the
    /// file's first `PROBE_LEN` bytes, so a large tail (debug info) is never mapped.
    #[test]
    fn only_the_needed_start_of_the_file_is_mapped() {
        reset(None, 1 << 30);
        let map = MmapFile::open("/lib/test.so", 0, |probe, size| {
            assert_eq!(probe, [5; PROBE_LEN]);
            assert_eq!(size, 1 << 30);
            100
        })
        .unwrap();
        assert_eq!(map.data().len(), 100);
        STATE.with(|s| assert_eq!(s.borrow().probed, Some(PROBE_LEN)));
    }
    /// An interrupted read (`EINTR`, which a scheme returns when the kernel cancels a call
    /// whose context was woken) is retried, and short reads continue until the probe is
    /// full, so neither makes a library fail to load.
    #[test]
    fn interrupted_and_short_reads_still_fill_the_probe() {
        reset(None, 1 << 20);
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.interrupts = 2;
            s.chunk = 1000;
        });
        let map = MmapFile::open("/lib/test.so", 0, |probe, _| {
            assert_eq!(probe, [5; PROBE_LEN]);
            64
        })
        .unwrap();
        assert_eq!(map.data().len(), 64);
        STATE.with(|s| {
            let s = s.borrow();
            assert_eq!(s.probed, Some(PROBE_LEN));
            // Two interrupted reads, then five short ones (4 x 1000 + 96 bytes).
            assert_eq!(s.calls.iter().filter(|&&c| c == "read").count(), 7);
        });
    }
    /// A file shorter than the probe is read only to its end, and an answer beyond
    /// the file's length maps the whole file rather than past its end.
    #[test]
    fn short_files_are_probed_and_mapped_to_their_end() {
        reset(None, 16);
        let map = MmapFile::open("/lib/test.so", 0, |probe, _| {
            assert_eq!(probe.len(), 16);
            usize::MAX
        })
        .unwrap();
        assert_eq!(map.data().len(), 16);
        STATE.with(|s| assert_eq!(s.borrow().probed, Some(16)));
    }
}
