//! Recovery-flag editing in the ext4 superblock ("The Super Block", Ext4 Disk Layout).
//! Unchanged fields remain opaque; generated wire fields encode both feature words and
//! the checksum, preserving every other byte of the 1024-byte superblock.

use simple_endian::{Endianize, read_native};

use crate::journal::map::{CHECKSUM_AT, RO_COMPAT_METADATA_CSUM};
use crate::superblock::INCOMPAT_RECOVER;

#[derive(Endianize)]
#[endian(le)]
#[repr(C)]
struct Recovery {
    before_features: [u8; 96],
    incompat: u32,
    ro_compat: u32,
    before_checksum: [u8; 916],
    checksum: u32,
}

/// Set the recovery flag and, when enabled, reseal the checksum.
///
/// # Errors
/// Returns an I/O error when `sb` is shorter than the superblock layout.
pub(crate) fn put_recover_flag(sb: &mut [u8], recover: bool) -> std::io::Result<()> {
    let mut native: Recovery = read_native::<_, RecoveryWire, _>(&mut &*sb)?;
    native.incompat &= !INCOMPAT_RECOVER;
    if recover {
        native.incompat |= INCOMPAT_RECOVER;
    }
    let checksum = native.ro_compat & RO_COMPAT_METADATA_CSUM != 0;
    let mut wire: RecoveryWire = native.into();
    // CRC32C covers the updated feature word but excludes s_checksum itself.
    simple_endian::write_specific(&mut &mut *sb, &wire)?;
    if checksum {
        wire.checksum = ferric_jbd2::crc32c(&[&sb[..CHECKSUM_AT]]).into();
        simple_endian::write_specific(&mut &mut *sb, &wire)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_good_superblock_round_trips_and_reseals() {
        let mut sb: [u8; 1024] = include_bytes!("../../tests/fixtures/basic.ext4")[1024..2048]
            .try_into()
            .unwrap();
        let original = sb;
        put_recover_flag(&mut sb, false).unwrap();
        assert_eq!(sb, original);
        put_recover_flag(&mut sb, true).unwrap();
        assert_eq!(sb[0x60], original[0x60] | 4);
        assert_eq!(&sb[..0x60], &original[..0x60]);
        assert_eq!(&sb[0x61..CHECKSUM_AT], &original[0x61..CHECKSUM_AT]);
        crate::journal::map::Volume::parse(&sb).unwrap();
        put_recover_flag(&mut sb, false).unwrap();
        assert_eq!(sb, original);
        assert_eq!(size_of::<RecoveryWire>(), 1024);
        assert_eq!(core::mem::offset_of!(RecoveryWire, incompat), 0x60);
        assert_eq!(core::mem::offset_of!(RecoveryWire, checksum), CHECKSUM_AT);
    }

    #[test]
    fn volumes_without_metadata_checksums_preserve_the_checksum_bytes() {
        let mut sb = [0xA5; 1024];
        sb[0x60..0x68].fill(0);
        let mut expected = sb;
        expected[0x60] = 4;
        put_recover_flag(&mut sb, true).unwrap();
        assert_eq!(sb, expected);
    }
}
