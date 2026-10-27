//! FAT-family identification and timestamps, shared by FAT and exFAT readers.
//!
//! These definitions follow Microsoft's FAT specification (version 1.03) and
//! exFAT specification (revision 1.00); they require neither allocation nor a parser.
//!
//! # Examples
//!
//! ```
//! use ferric_fat_types::time::from_fields;
//! assert_eq!(from_fields(1980, 1, 1, 0, 0, 0), Some(315_532_800));
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

pub mod time;

/// The exFAT boot sector's `FileSystemName` field (bytes 3..11).
pub const EXFAT_NAME: &[u8; 8] = b"EXFAT   ";

/// The prefix containing FAT-family identification and the boot signature.
pub const BOOT_SECTOR_PREFIX: usize = 512;
