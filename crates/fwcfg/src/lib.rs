//! Reads QEMU's firmware configuration device, fw_cfg, through its traditional (non-DMA)
//! interface.
//!
//! QEMU passes data to the guest's firmware as numbered items: a few fixed ones (the
//! signature, the feature bitmap, the file directory) and named files, such as the ones
//! `-fw_cfg name=opt/...,file=...` adds. A guest selects an item by writing its key to the
//! selector register, then reads the item one byte at a time from the data register; each
//! read advances through the item and reads past its end return 0. On x86 the selector is
//! the 16-bit I/O port [`SELECTOR_PORT`] and the data register the 8-bit port [`DATA_PORT`].
//!
//! This crate implements that protocol over [`Transport`], which the caller implements with
//! real port I/O (the bootloader's `fw_cfg_env` feature does) or with a fake in tests, so it
//! has no `unsafe` and runs on the host. [`FwCfg::probe`] checks the signature;
//! [`FwCfg::find`] looks a file up in the directory and [`FwCfg::read_file`] reads it.
//!
//! Writing a selector port on a machine that has no fw_cfg device is not harmless in
//! general, so callers must first establish that they run under QEMU (the bootloader checks
//! the CPUID hypervisor bit) and only then call [`FwCfg::probe`].
//!
//! The layouts and keys come from QEMU's published specification of the device, "QEMU
//! Firmware Configuration (fw_cfg) Device" (`docs/specs/fw_cfg.rst` in QEMU's documentation).
//! The file directory is big-endian; its entry is defined once with `simple_endian`.
//!
//! # Examples
//!
//! ```
//! use ferric_fwcfg::{FwCfg, Transport, key};
//!
//! /// A device with the signature and an empty file directory.
//! struct Empty {
//!     item: Vec<u8>,
//! }
//!
//! impl Transport for Empty {
//!     fn select(&mut self, selected: u16) {
//!         self.item = match selected {
//!             key::SIGNATURE => b"QEMU".to_vec(),
//!             key::FILE_DIR => vec![0; 4],
//!             _ => Vec::new(),
//!         };
//!     }
//!     fn read(&mut self, buf: &mut [u8]) {
//!         for byte in buf {
//!             *byte = if self.item.is_empty() { 0 } else { self.item.remove(0) };
//!         }
//!     }
//! }
//!
//! let mut fw_cfg = FwCfg::probe(Empty { item: Vec::new() }).expect("the signature matches");
//! assert_eq!(fw_cfg.find("opt/org.example/config"), Ok(None));
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

use core::fmt;

use simple_endian::{BigEndian, Endianize, FromSlice};

/// The x86 I/O port of the 16-bit selector register.
pub const SELECTOR_PORT: u16 = 0x510;
/// The x86 I/O port of the 8-bit data register.
pub const DATA_PORT: u16 = 0x511;

/// The content of the [`key::SIGNATURE`] item on a QEMU fw_cfg device.
pub const SIGNATURE: [u8; 4] = *b"QEMU";

/// The bytes of a file name in the directory, its terminating NUL included: a name has at
/// most 55 bytes.
pub const FILE_NAME_BYTES: usize = 56;

/// The most directory entries [`FwCfg::find`] reads. QEMU's machines have a few dozen; a
/// larger count means the device is not answering as fw_cfg does.
pub const MAX_FILES: u32 = 4096;

/// The keys of the fixed items this crate reads.
pub mod key {
    /// The signature, [`SIGNATURE`](super::SIGNATURE).
    pub const SIGNATURE: u16 = 0x0000;
    /// The feature bitmap: bit 0 is the traditional interface, bit 1 the DMA interface.
    pub const ID: u16 = 0x0001;
    /// The file directory: a big-endian `u32` count, then that many entries.
    pub const FILE_DIR: u16 = 0x0019;
}

/// Access to a fw_cfg device's selector and data registers.
pub trait Transport {
    /// Writes `key` to the selector register, which selects that item and rewinds reading to
    /// its start.
    fn select(&mut self, key: u16);

    /// Reads `buf.len()` bytes of the selected item from the data register; bytes past the
    /// item's end read as 0.
    fn read(&mut self, buf: &mut [u8]);
}

/// A directory entry as the device stores it.
#[derive(Endianize)]
#[endian(be)]
#[repr(C)]
struct DirEntry {
    size: u32,
    select: u16,
    reserved: u16,
    name: [u8; FILE_NAME_BYTES],
}

/// The bytes of a directory entry.
const DIR_ENTRY_BYTES: usize = size_of::<DirEntryWire>();
const _: () = assert!(DIR_ENTRY_BYTES == 64);

impl DirEntry {
    /// Decodes an entry from its bytes.
    fn read(bytes: &[u8; DIR_ENTRY_BYTES]) -> Option<Self> {
        macro_rules! field {
            ($name:ident) => {
                FromSlice::read_from_slice(&bytes[core::mem::offset_of!(DirEntryWire, $name)..])
                    .ok()?
            };
        }
        let at = core::mem::offset_of!(DirEntryWire, name);
        let mut name = [0; FILE_NAME_BYTES];
        name.copy_from_slice(&bytes[at..at + FILE_NAME_BYTES]);
        Some(
            DirEntryWire {
                size: field!(size),
                select: field!(select),
                reserved: field!(reserved),
                name,
            }
            .into(),
        )
    }
}

/// A file in the device's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File {
    size: u32,
    selector: u16,
    name: [u8; FILE_NAME_BYTES],
}

impl File {
    /// The file's size in bytes.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// The key that selects the file's item.
    pub fn selector(&self) -> u16 {
        self.selector
    }

    /// The file's name, without the NUL that ends it.
    pub fn name(&self) -> &[u8] {
        let len = self
            .name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(FILE_NAME_BYTES);
        &self.name[..len]
    }
}

/// Why a fw_cfg read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The directory claims more than [`MAX_FILES`] entries.
    TooManyFiles {
        /// The count the directory gave.
        count: u32,
    },
    /// A file is larger than the buffer it was to be read into.
    FileTooLarge {
        /// The file's size in bytes.
        size: u32,
        /// The buffer's size in bytes.
        capacity: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyFiles { count } => {
                write!(
                    f,
                    "the file directory claims {count} files, over {MAX_FILES}"
                )
            }
            Self::FileTooLarge { size, capacity } => {
                write!(
                    f,
                    "the file has {size} bytes, over the {capacity} it may have"
                )
            }
        }
    }
}

impl core::error::Error for Error {}

/// A fw_cfg device whose signature matched.
#[derive(Debug)]
pub struct FwCfg<T> {
    transport: T,
}

impl<T: Transport> FwCfg<T> {
    /// Reads the signature item through `transport`, and returns the device when it is
    /// [`SIGNATURE`]. Writes the selector register once and reads the data register four
    /// times.
    pub fn probe(mut transport: T) -> Option<Self> {
        let mut signature = [0; SIGNATURE.len()];
        transport.select(key::SIGNATURE);
        transport.read(&mut signature);
        (signature == SIGNATURE).then_some(Self { transport })
    }

    /// Looks the file `name` up in the directory, reading entries until it is found.
    ///
    /// # Errors
    /// [`Error::TooManyFiles`] when the directory's count is over [`MAX_FILES`].
    pub fn find(&mut self, name: &str) -> Result<Option<File>, Error> {
        let mut count = [0; 4];
        self.transport.select(key::FILE_DIR);
        self.transport.read(&mut count);
        let count = BigEndian::<u32>::read_from_slice(&count)
            .map_or(0, |count: BigEndian<u32>| count.to_native());
        if count > MAX_FILES {
            return Err(Error::TooManyFiles { count });
        }
        for _ in 0..count {
            let mut bytes = [0; DIR_ENTRY_BYTES];
            self.transport.read(&mut bytes);
            let Some(entry) = DirEntry::read(&bytes) else {
                continue;
            };
            let file = File {
                size: entry.size,
                selector: entry.select,
                name: entry.name,
            };
            if file.name() == name.as_bytes() {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Reads all of `file` into the start of `buf`, and returns that part of it.
    ///
    /// # Errors
    /// [`Error::FileTooLarge`] when the file does not fit in `buf`; nothing is read then.
    pub fn read_file<'b>(&mut self, file: &File, buf: &'b mut [u8]) -> Result<&'b [u8], Error> {
        let capacity = buf.len();
        let size = usize::try_from(file.size).unwrap_or(usize::MAX);
        let Some(contents) = buf.get_mut(..size) else {
            return Err(Error::FileTooLarge {
                size: file.size,
                capacity,
            });
        };
        self.transport.select(file.selector);
        self.transport.read(contents);
        Ok(contents)
    }

    /// Gives the transport back.
    pub fn into_inner(self) -> T {
        self.transport
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    /// A fw_cfg device holding `items`, which counts the register accesses it sees.
    #[derive(Default)]
    struct Fake {
        items: BTreeMap<u16, Vec<u8>>,
        selected: Option<u16>,
        offset: usize,
        selects: usize,
        reads: usize,
    }

    impl Transport for Fake {
        fn select(&mut self, key: u16) {
            self.selected = Some(key);
            self.offset = 0;
            self.selects += 1;
        }

        fn read(&mut self, buf: &mut [u8]) {
            let item = self
                .selected
                .and_then(|key| self.items.get(&key))
                .map_or(&[][..], Vec::as_slice);
            for byte in buf {
                *byte = item.get(self.offset).copied().unwrap_or(0);
                self.offset += 1;
                self.reads += 1;
            }
        }
    }

    /// The directory of a QEMU q35 machine with the harness's boot environment file, as the
    /// device serves it: a big-endian count, then 64-byte big-endian entries.
    fn directory() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0, 0, 0, 3]);
        for (size, select, name) in [
            ([0x00, 0x00, 0x00, 0x08], [0x00, 0x20], &b"bootorder"[..]),
            (
                [0x00, 0x00, 0x00, 0x31],
                [0x00, 0x2b],
                b"opt/org.ferric-oxide/boot-env",
            ),
            ([0x00, 0x01, 0x00, 0x00], [0x00, 0x21], b"etc/table-loader"),
        ] {
            bytes.extend_from_slice(&size);
            bytes.extend_from_slice(&select);
            bytes.extend_from_slice(&[0, 0]);
            let mut name_field = [0; FILE_NAME_BYTES];
            name_field[..name.len()].copy_from_slice(name);
            bytes.extend_from_slice(&name_field);
        }
        bytes
    }

    const BOOT_ENV: &[u8] = b"FERRIC_KERNEL=trace\nFERRIC_BOOT_RESOLUTION=auto\n\n";

    fn qemu() -> Fake {
        let mut fake = Fake::default();
        fake.items.insert(key::SIGNATURE, b"QEMU".to_vec());
        fake.items.insert(key::ID, std::vec![3, 0, 0, 0]);
        fake.items.insert(key::FILE_DIR, directory());
        fake.items.insert(0x2b, BOOT_ENV.to_vec());
        fake
    }

    #[test]
    fn the_directory_entry_layout_is_the_specifications() {
        let bytes = directory();
        let entry: &[u8; DIR_ENTRY_BYTES] = bytes[4 + 64..4 + 128].try_into().unwrap();
        let entry = DirEntry::read(entry).unwrap();
        assert_eq!(entry.size, 0x31);
        assert_eq!(entry.select, 0x2b);
        assert_eq!(entry.reserved, 0);
        assert!(entry.name.starts_with(b"opt/org.ferric-oxide/boot-env\0"));
    }

    #[test]
    fn finds_and_reads_a_file() {
        let mut fw_cfg = FwCfg::probe(qemu()).unwrap();
        let file = fw_cfg
            .find("opt/org.ferric-oxide/boot-env")
            .unwrap()
            .unwrap();
        assert_eq!(file.size(), BOOT_ENV.len() as u32);
        assert_eq!(file.selector(), 0x2b);
        assert_eq!(file.name(), b"opt/org.ferric-oxide/boot-env");
        let mut buf = [0xff; 4096];
        assert_eq!(fw_cfg.read_file(&file, &mut buf).unwrap(), BOOT_ENV);
        // The rest of the buffer is left alone.
        assert_eq!(buf[BOOT_ENV.len()], 0xff);
    }

    #[test]
    fn a_missing_file_is_none() {
        let mut fw_cfg = FwCfg::probe(qemu()).unwrap();
        assert_eq!(fw_cfg.find("opt/org.ferric-oxide/other"), Ok(None));
        // A prefix of a name is not the name.
        assert_eq!(fw_cfg.find("opt/org.ferric-oxide/boot"), Ok(None));
        assert_eq!(fw_cfg.find("boot"), Ok(None));
        // Nor is a name with the NUL the device pads with.
        assert_eq!(fw_cfg.find("bootorder\0"), Ok(None));
    }

    #[test]
    fn the_search_stops_at_the_file() {
        let mut fw_cfg = FwCfg::probe(qemu()).unwrap();
        fw_cfg.find("bootorder").unwrap().unwrap();
        let fake = fw_cfg.into_inner();
        // The signature (one select, 4 reads), then the count and the first entry only.
        assert_eq!(fake.selects, 2);
        assert_eq!(fake.reads, 4 + 4 + DIR_ENTRY_BYTES);
    }

    #[test]
    fn another_device_is_not_probed_further() {
        // Ports with nothing behind them read as all ones.
        let mut fake = Fake::default();
        fake.items.insert(key::SIGNATURE, std::vec![0xff; 4]);
        assert!(FwCfg::probe(fake).is_none());
        let mut fake = Fake::default();
        fake.items.insert(key::SIGNATURE, b"QEMx".to_vec());
        assert!(FwCfg::probe(fake).is_none());
    }

    #[test]
    fn an_oversized_file_is_not_read() {
        let mut fw_cfg = FwCfg::probe(qemu()).unwrap();
        let file = fw_cfg.find("etc/table-loader").unwrap().unwrap();
        let mut buf = [0; 4096];
        assert_eq!(
            fw_cfg.read_file(&file, &mut buf),
            Err(Error::FileTooLarge {
                size: 0x1_0000,
                capacity: 4096
            })
        );
        let fake = fw_cfg.into_inner();
        // The file was never selected.
        assert_eq!(fake.selected, Some(key::FILE_DIR));
    }

    #[test]
    fn a_file_may_fill_the_buffer_exactly() {
        let file = |size| File {
            size,
            selector: 0x2b,
            name: [0; FILE_NAME_BYTES],
        };
        let mut fake = qemu();
        fake.items.insert(0x2b, std::vec![b'x'; 4097]);
        let mut fw_cfg = FwCfg::probe(fake).unwrap();
        let mut buf = [0; 4096];
        assert_eq!(fw_cfg.read_file(&file(4096), &mut buf).unwrap().len(), 4096);
        assert_eq!(
            fw_cfg.read_file(&file(4097), &mut buf),
            Err(Error::FileTooLarge {
                size: 4097,
                capacity: 4096
            })
        );
    }

    #[test]
    fn a_nonsense_directory_count_is_refused() {
        let mut fake = qemu();
        fake.items.insert(key::FILE_DIR, std::vec![0xff; 4]);
        let mut fw_cfg = FwCfg::probe(fake).unwrap();
        assert_eq!(
            fw_cfg.find("bootorder"),
            Err(Error::TooManyFiles { count: u32::MAX })
        );
    }

    #[test]
    fn a_name_filling_the_field_has_no_nul() {
        let mut bytes = std::vec![0, 0, 0, 1, 0, 0, 0, 1, 0, 0x30, 0, 0];
        bytes.extend_from_slice(&[b'x'; FILE_NAME_BYTES]);
        let mut fake = qemu();
        fake.items.insert(key::FILE_DIR, bytes);
        let mut fw_cfg = FwCfg::probe(fake).unwrap();
        let name = core::str::from_utf8(&[b'x'; FILE_NAME_BYTES]).unwrap();
        let file = fw_cfg.find(name).unwrap().unwrap();
        assert_eq!(file.name().len(), FILE_NAME_BYTES);
        assert_eq!(file.selector(), 0x30);
    }
}
