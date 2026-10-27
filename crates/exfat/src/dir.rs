//! Directory entries: 32-byte records, grouped into entry sets.
//!
//! From the "Directory Structure" and "Directory Entry Definitions" sections of the exFAT
//! specification. Each entry starts with an `EntryType` byte: bit 7 says the entry is in
//! use, bit 6 that it is a secondary entry (belonging to the primary before it), bit 5 that
//! it is benign (a reader may skip it) rather than critical, and bits 4..0 are the type
//! code. `00` ends the directory. A file or directory is a File entry (`85`), its Stream
//! Extension (`C0`, the data's location and size) and File Name entries (`C1`, 15 UTF-16
//! code units each), checked by the set checksum in the File entry. The root directory also
//! holds the Allocation Bitmap (`81`), the Up-case Table (`82`) and the Volume Label (`83`).

use alloc::string::String;
use alloc::vec::Vec;

use thiserror::Error;

use crate::time::Timestamp;
use crate::wire::{BitmapRecord, FileRecord, LabelRecord, NameRecord, StreamRecord, UpcaseRecord};

/// The size of a directory entry.
pub const ENTRY_SIZE: usize = 32;

const END: u8 = 0x00;
const IN_USE: u8 = 0x80;
const SECONDARY: u8 = 0x40;
const BENIGN: u8 = 0x20;
const BITMAP: u8 = 0x81;
const UPCASE: u8 = 0x82;
const LABEL: u8 = 0x83;
const FILE: u8 = 0x85;
const STREAM: u8 = 0xC0;
const NAME: u8 = 0xC1;

/// `FileAttributes` bit 4: the entry is a directory.
pub const ATTR_DIRECTORY: u16 = 0x10;

/// Why a directory's entries were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DirError {
    /// A file's entry set does not match its checksum.
    #[error("the entry set at {offset} does not match its checksum")]
    SetChecksum {
        /// The set's byte offset in the directory.
        offset: usize,
    },
    /// A file's entry set is cut short, or its entries are not a Stream Extension and
    /// enough File Name entries for the name's length.
    #[error("the entry set at {offset} is malformed")]
    Malformed {
        /// The set's byte offset in the directory.
        offset: usize,
    },
    /// A critical entry of a type the specification does not define, which a reader may not
    /// skip.
    #[error("unknown critical entry type {entry_type:#04x} at {offset}")]
    UnknownCritical {
        /// The `EntryType` byte.
        entry_type: u8,
        /// The entry's byte offset in the directory.
        offset: usize,
    },
}

/// Where a file's data is and how long it is: its Stream Extension entry, or the root
/// directory's or a system file's location.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    /// The first cluster; 0 when nothing is allocated.
    pub first_cluster: u32,
    /// The bytes written; reads between this and `length` return zeros.
    pub valid_length: u64,
    /// The size in bytes.
    pub length: u64,
    /// `NoFatChain`: the clusters are consecutive and the FAT does not describe them.
    pub contiguous: bool,
}

/// A file or directory in a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The name, as UTF-16 code units.
    pub name: Vec<u16>,
    /// `FileAttributes` (see [`ATTR_DIRECTORY`]).
    pub attributes: u16,
    /// When it was created.
    pub created: Timestamp,
    /// When it was last written.
    pub modified: Timestamp,
    /// When it was last read; exFAT keeps no 10 ms increment for this one.
    pub accessed: Timestamp,
    /// Its data.
    pub stream: Stream,
    /// The `NameHash` its Stream Extension entry records.
    pub name_hash: u16,
}

impl Entry {
    /// Whether it is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }

    /// The name, with any code unit that is not valid UTF-16 replaced by U+FFFD.
    pub fn name_lossy(&self) -> String {
        String::from_utf16_lossy(&self.name)
    }
}

/// The root directory's system entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct System {
    /// The Allocation Bitmaps, by their `BitmapFlags` bit 0 (which FAT they go with).
    pub bitmaps: [Option<Stream>; 2],
    /// The Up-case Table, and its `TableChecksum`.
    pub upcase: Option<(Stream, u32)>,
    /// The volume label, as UTF-16 code units.
    pub label: Vec<u16>,
}

/// A directory's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// Its files and directories, in the order the directory stores them.
    pub entries: Vec<Entry>,
    /// The system entries (found only in the root directory).
    pub system: System,
}

/// The `SetChecksum` of an entry set: the 16-bit rotate-and-add over its bytes, skipping
/// the checksum field itself (bytes 2 and 3 of the File entry).
pub fn set_checksum(set: &[u8]) -> u16 {
    set.iter().enumerate().fold(0u16, |sum, (i, &b)| {
        if i == 2 || i == 3 {
            sum
        } else {
            sum.rotate_right(1).wrapping_add(u16::from(b))
        }
    })
}

/// Reads the entries of a directory's bytes, up to the end-of-directory entry or the end of
/// the bytes.
///
/// # Errors
///
/// The [`DirError`] for the first entry set that is wrong.
pub fn parse(bytes: &[u8]) -> Result<Listing, DirError> {
    let mut listing = Listing::default();
    let count = bytes.len() / ENTRY_SIZE;
    let entry = |i: usize| &bytes[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE];
    let mut i = 0;
    while i < count {
        let e = entry(i);
        let offset = i * ENTRY_SIZE;
        let kind = e[0];
        i += 1;
        match kind {
            END => break,
            // Deleted entries, and secondaries whose primary was skipped.
            _ if kind & IN_USE == 0 || kind & SECONDARY != 0 => {}
            BITMAP => {
                let record = BitmapRecord::read(e).ok_or(DirError::Malformed { offset })?;
                listing.system.bitmaps[usize::from(record.flags & 1)] = Some(Stream {
                    first_cluster: record.first_cluster,
                    valid_length: record.length,
                    length: record.length,
                    contiguous: false,
                });
            }
            UPCASE => {
                let record = UpcaseRecord::read(e).ok_or(DirError::Malformed { offset })?;
                listing.system.upcase = Some((
                    Stream {
                        first_cluster: record.first_cluster,
                        valid_length: record.length,
                        length: record.length,
                        contiguous: false,
                    },
                    record.checksum,
                ));
            }
            LABEL => {
                let record = LabelRecord::read(e).ok_or(DirError::Malformed { offset })?;
                let n = usize::from(record.count).min(record.units.len());
                listing.system.label = record.units[..n].to_vec();
            }
            FILE => {
                let record = FileRecord::read(e).ok_or(DirError::Malformed { offset })?;
                let secondaries = usize::from(record.secondaries);
                let end = i + secondaries;
                if secondaries < 2 || end > count {
                    return Err(DirError::Malformed { offset });
                }
                let set = &bytes[offset..end * ENTRY_SIZE];
                if set_checksum(set) != record.checksum {
                    return Err(DirError::SetChecksum { offset });
                }
                listing.entries.push(file(set, offset)?);
                i = end;
            }
            // Other benign primaries (the Volume GUID, TexFAT padding, vendor entries):
            // skip them with their secondaries.
            _ if kind & BENIGN != 0 => i += usize::from(e[1]),
            _ => {
                return Err(DirError::UnknownCritical {
                    entry_type: kind,
                    offset,
                });
            }
        }
    }
    Ok(listing)
}

/// A File entry set, already checked against its checksum.
fn file(set: &[u8], offset: usize) -> Result<Entry, DirError> {
    let malformed = DirError::Malformed { offset };
    let f = FileRecord::read(&set[..ENTRY_SIZE]).ok_or(malformed)?;
    let stream = StreamRecord::read(&set[ENTRY_SIZE..2 * ENTRY_SIZE]).ok_or(malformed)?;
    if stream.kind != STREAM {
        return Err(malformed);
    }
    let name_len = usize::from(stream.name_len);
    let name_entries = name_len.div_ceil(15);
    let names = &set[2 * ENTRY_SIZE..];
    if name_len == 0 || names.len() < name_entries * ENTRY_SIZE {
        return Err(malformed);
    }
    let mut name = Vec::with_capacity(name_len);
    for (k, n) in names
        .chunks_exact(ENTRY_SIZE)
        .take(name_entries)
        .enumerate()
    {
        let n = NameRecord::read(n).ok_or(malformed)?;
        if n.kind != NAME {
            return Err(malformed);
        }
        let units = (name_len - k * 15).min(15);
        name.extend_from_slice(&n.units[..units]);
    }
    // Secondaries after the names must be benign (vendor extensions); a critical one this
    // reader does not know means it cannot read the file correctly.
    for extra in names.chunks_exact(ENTRY_SIZE).skip(name_entries) {
        if extra[0] & IN_USE != 0 && extra[0] & BENIGN == 0 {
            return Err(DirError::UnknownCritical {
                entry_type: extra[0],
                offset,
            });
        }
    }
    Ok(Entry {
        name,
        attributes: f.attributes,
        created: Timestamp {
            packed: f.created,
            ten_ms: f.created_ten_ms,
            utc_offset: f.created_utc,
        },
        modified: Timestamp {
            packed: f.modified,
            ten_ms: f.modified_ten_ms,
            utc_offset: f.modified_utc,
        },
        accessed: Timestamp {
            packed: f.accessed,
            ten_ms: 0,
            utc_offset: f.accessed_utc,
        },
        stream: Stream {
            first_cluster: stream.first_cluster,
            valid_length: stream.valid_length,
            length: stream.length,
            contiguous: stream.flags & 2 != 0,
        },
        name_hash: stream.name_hash,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::encode;
    use simple_endian::LittleEndian;

    /// The entry set of a file `name` with the given stream, checksummed.
    pub(crate) fn file_set(name: &str, attributes: u16, stream: Stream) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let names = units.len().div_ceil(15);
        let mut set = alloc::vec![0u8; (2 + names) * ENTRY_SIZE];
        set[0] = FILE;
        set[1] = (1 + names) as u8;
        set[4..6].copy_from_slice(&encode(LittleEndian::from(attributes)));
        set[12..16].copy_from_slice(&encode(LittleEndian::from(0x5C21_6000u32)));
        let s = &mut set[ENTRY_SIZE..2 * ENTRY_SIZE];
        s[0] = STREAM;
        s[1] = 1 | if stream.contiguous { 2 } else { 0 };
        s[3] = units.len() as u8;
        s[8..16].copy_from_slice(&encode(LittleEndian::from(stream.valid_length)));
        s[20..24].copy_from_slice(&encode(LittleEndian::from(stream.first_cluster)));
        s[24..32].copy_from_slice(&encode(LittleEndian::from(stream.length)));
        for (k, chunk) in units.chunks(15).enumerate() {
            let n = &mut set[(2 + k) * ENTRY_SIZE..(3 + k) * ENTRY_SIZE];
            n[0] = NAME;
            for (u, unit) in chunk.iter().enumerate() {
                n[2 + 2 * u..4 + 2 * u].copy_from_slice(&encode(LittleEndian::from(*unit)));
            }
        }
        let sum = set_checksum(&set);
        set[2..4].copy_from_slice(&encode(LittleEndian::from(sum)));
        set
    }

    fn stream(first_cluster: u32, length: u64) -> Stream {
        Stream {
            first_cluster,
            valid_length: length,
            length,
            contiguous: true,
        }
    }

    #[test]
    fn files_and_system_entries_parse() {
        let mut dir = alloc::vec![0u8; ENTRY_SIZE * 3];
        dir[0] = LABEL;
        dir[1] = 2;
        dir[2..6].copy_from_slice(&[b'O', 0, b'K', 0]);
        dir[ENTRY_SIZE] = BITMAP;
        dir[ENTRY_SIZE + 20] = 2;
        dir[ENTRY_SIZE + 24] = 10;
        dir[2 * ENTRY_SIZE] = 0x05; // a deleted File entry
        dir.extend(file_set(
            "A rather long file name.txt",
            0x20,
            stream(9, 100),
        ));
        dir.extend(file_set("sub", ATTR_DIRECTORY, stream(12, 4096)));
        dir.extend([0u8; ENTRY_SIZE]);
        dir.extend(file_set("after the end", 0, stream(1, 1)));
        let listing = parse(&dir).unwrap();
        assert_eq!(
            listing.system.label,
            "OK".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(listing.system.bitmaps[0].unwrap().first_cluster, 2);
        assert_eq!(listing.entries.len(), 2);
        let long = &listing.entries[0];
        assert_eq!(long.name_lossy(), "A rather long file name.txt");
        assert_eq!((long.stream, long.is_dir()), (stream(9, 100), false));
        assert_eq!(long.modified.packed, 0x5C21_6000);
        assert!(listing.entries[1].is_dir());
    }

    #[test]
    fn damaged_sets_are_refused() {
        let mut set = file_set("x", 0, stream(5, 1));
        set[ENTRY_SIZE + 24] ^= 1;
        assert_eq!(parse(&set), Err(DirError::SetChecksum { offset: 0 }));
        let set = file_set("x", 0, stream(5, 1));
        assert_eq!(
            parse(&set[..2 * ENTRY_SIZE]),
            Err(DirError::Malformed { offset: 0 })
        );
        let mut critical = [0u8; ENTRY_SIZE];
        critical[0] = 0x84;
        assert_eq!(
            parse(&critical),
            Err(DirError::UnknownCritical {
                entry_type: 0x84,
                offset: 0
            })
        );
        // A benign primary (the Volume GUID) is skipped.
        let mut guid = [0u8; ENTRY_SIZE];
        guid[0] = 0xA0;
        assert_eq!(parse(&guid).unwrap(), Listing::default());
    }
}
