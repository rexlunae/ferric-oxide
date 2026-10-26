//! The CRC32C that every JBD2 version 2 and 3 checksum (and ext4's own) uses.
//!
//! JBD2 starts the CRC at `0xFFFF_FFFF` and does not invert the result, then feeds the
//! journal UUID followed by the covered bytes. That is the standard CRC32C (which also starts
//! at `0xFFFF_FFFF` but inverts at the end) of the concatenation, inverted once more. The
//! convention was checked against volumes made by e2fsprogs 1.47 (see the crate docs).

use crc::{CRC_32_ISCSI, Crc};

static CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// CRC32C over `parts`, one after another, in the form JBD2 and ext4 store: started at
/// `0xFFFF_FFFF` and not inverted at the end.
///
/// A JBD2 checksum is this over the journal UUID and then the covered bytes; ext4's
/// superblock checksum (`s_checksum`) is this over the superblock's first 1020 bytes.
///
/// # Examples
///
/// ```
/// // The standard CRC32C of "123456789" is 0xE3069283; the stored form is its inverse.
/// assert_eq!(ferric_jbd2::crc32c(&[b"1234", b"56789"]), !0xE306_9283);
/// ```
pub fn crc32c(parts: &[&[u8]]) -> u32 {
    let mut digest = CRC32C.digest();
    for part in parts {
        digest.update(part);
    }
    !digest.finalize()
}

/// [`crc32c`] over `block` with the four bytes at `at` read as zero, after `prefix`: how
/// every checksum stored inside the block it covers is computed.
pub(crate) fn crc32c_without(prefix: &[&[u8]], block: &[u8], at: usize) -> u32 {
    let mut digest = CRC32C.digest();
    for part in prefix {
        digest.update(part);
    }
    digest.update(&block[..at]);
    digest.update(&[0; 4]);
    digest.update(&block[at + 4..]);
    !digest.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_unfinalized_castagnoli_crc() {
        // CRC32C("123456789") is 0xE3069283; JBD2's form is its inverse.
        assert_eq!(crc32c(&[b"123456789"]), !0xE306_9283);
        assert_eq!(crc32c(&[b"1234", b"56789"]), crc32c(&[b"123456789"]));
    }

    #[test]
    fn a_stored_checksum_reads_as_zero() {
        let mut block = *b"abcdWXYZefgh";
        let expected = {
            block[4..8].copy_from_slice(&[0; 4]);
            crc32c(&[b"uuid", &block])
        };
        block[4..8].copy_from_slice(b"WXYZ");
        assert_eq!(crc32c_without(&[b"uuid"], &block, 4), expected);
    }
}
