//! The Up-case Table, which says how to compare file names without regard to case.
//!
//! From the "Up-case Table Directory Entry" section of the exFAT specification. The table
//! maps each UTF-16 code unit to its upper-case form, starting at U+0000. It may be
//! compressed: the value `FFFF` followed by a count means that many code units map to
//! themselves. Code units past the table's end map to themselves too. Names are compared,
//! and their hashes computed, after mapping every code unit through the table.

use alloc::vec::Vec;

use thiserror::Error;

/// Why an up-case table was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum UpcaseError {
    /// The table's length is odd, or above the 128 KiB that 65536 entries take.
    #[error("an up-case table of {len} bytes")]
    Length {
        /// The length in bytes.
        len: u64,
    },
    /// The table's checksum does not match its directory entry's `TableChecksum`.
    #[error("the up-case table's checksum is {computed:#010x}, its entry says {stored:#010x}")]
    Checksum {
        /// The checksum in the directory entry.
        stored: u32,
        /// The checksum of the table's bytes.
        computed: u32,
    },
    /// The table, uncompressed, maps more than 65536 code units.
    #[error("the up-case table maps more than 65536 code units")]
    TooLong,
}

/// An up-case table, uncompressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upcase {
    map: Vec<u16>,
}

/// The `TableChecksum` of an up-case table's bytes: the boot checksum's rotate-and-add, over
/// every byte.
pub fn table_checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |sum, &b| {
        sum.rotate_right(1).wrapping_add(u32::from(b))
    })
}

impl Upcase {
    /// Reads a table's bytes, checking them against the `stored` checksum.
    ///
    /// # Errors
    ///
    /// [`UpcaseError::Length`], [`UpcaseError::Checksum`] and [`UpcaseError::TooLong`].
    pub fn parse(bytes: &[u8], stored: u32) -> Result<Self, UpcaseError> {
        let len = bytes.len() as u64;
        if !len.is_multiple_of(2) || len > 2 * 65_536 {
            return Err(UpcaseError::Length { len });
        }
        let computed = table_checksum(bytes);
        if computed != stored {
            return Err(UpcaseError::Checksum { stored, computed });
        }
        let mut units = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]));
        let mut map = Vec::with_capacity(bytes.len() / 2);
        while let Some(unit) = units.next() {
            if unit == 0xFFFF {
                // A run of identity mappings; a trailing FFFF with no count maps U+FFFF.
                let Some(count) = units.next() else {
                    map.push(0xFFFF);
                    break;
                };
                for _ in 0..count {
                    map.push(map.len() as u16);
                    if map.len() > 65_536 {
                        return Err(UpcaseError::TooLong);
                    }
                }
            } else {
                map.push(unit);
            }
            if map.len() > 65_536 {
                return Err(UpcaseError::TooLong);
            }
        }
        Ok(Self { map })
    }

    /// The upper-case form of `unit`.
    pub fn up(&self, unit: u16) -> u16 {
        self.map.get(usize::from(unit)).copied().unwrap_or(unit)
    }

    /// Whether two names, as UTF-16, are equal once up-cased.
    pub fn same(&self, a: &[u16], b: &[u16]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| self.up(x) == self.up(y))
    }

    /// The `NameHash` of a name: the 16-bit rotate-and-add over its up-cased code units,
    /// low byte first ("NameHash Field" in the specification).
    pub fn name_hash(&self, name: &[u16]) -> u16 {
        name.iter().fold(0u16, |hash, &unit| {
            let [lo, hi] = self.up(unit).to_le_bytes();
            let hash = hash.rotate_right(1).wrapping_add(u16::from(lo));
            hash.rotate_right(1).wrapping_add(u16::from(hi))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compressed table: identity for 0..0x61, then a..z to A..Z, then identity for the
    /// rest of the first 256.
    fn table() -> Vec<u8> {
        let mut units = alloc::vec![0xFFFF, 0x61];
        units.extend((0x41..=0x5A).map(|u| u as u16));
        units.extend([0xFFFF, 256 - 0x7B]);
        units.iter().flat_map(|u: &u16| u.to_le_bytes()).collect()
    }

    #[test]
    fn a_compressed_table_upcases() {
        let bytes = table();
        let up = Upcase::parse(&bytes, table_checksum(&bytes)).unwrap();
        assert_eq!(up.up(u16::from(b'a')), u16::from(b'A'));
        assert_eq!(up.up(u16::from(b'z')), u16::from(b'Z'));
        assert_eq!(up.up(u16::from(b'{')), u16::from(b'{'));
        assert_eq!(up.up(u16::from(b'A')), u16::from(b'A'));
        // Past the table: unchanged.
        assert_eq!(up.up(0x3B1), 0x3B1);
        let name = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert!(up.same(&name("Hello.TXT"), &name("hello.txt")));
        assert!(!up.same(&name("hello.txt"), &name("hello.txt2")));
        assert_eq!(up.name_hash(&name("abc")), up.name_hash(&name("ABC")));
    }

    #[test]
    fn bad_tables_are_refused() {
        let bytes = table();
        assert!(matches!(
            Upcase::parse(&bytes, 1),
            Err(UpcaseError::Checksum { stored: 1, .. })
        ));
        assert_eq!(
            Upcase::parse(&bytes[..3], table_checksum(&bytes[..3])),
            Err(UpcaseError::Length { len: 3 })
        );
        let long: Vec<u8> = [0xFFFFu16, 0xFFFF, 0xFFFF, 0xFFFF]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(
            Upcase::parse(&long, table_checksum(&long)),
            Err(UpcaseError::TooLong)
        );
    }
}
