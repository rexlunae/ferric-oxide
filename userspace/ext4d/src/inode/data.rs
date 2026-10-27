//! Size and flags in the original 128-byte ext4 inode ("Inode Table", Ext4 Disk Layout).
//! Unmodified fields stay opaque so rewriting the size preserves every other byte,
//! including the creator-specific fields and any extra inode bytes after this prefix.

use simple_endian::{Endianize, read_native, write_native};

#[derive(Endianize)]
#[endian(le)]
#[repr(C)]
pub(crate) struct Data {
    before_size: [u8; 4],
    size_lo: u32,
    before_flags: [u8; 24],
    pub(crate) flags: u32,
    before_size_high: [u8; 72],
    size_high: u32,
    after_size: [u8; 16],
}

impl Data {
    /// Decode the inode's original prefix, leaving its extra fields untouched.
    pub(crate) fn read(inode: &[u8]) -> std::io::Result<Self> {
        read_native::<_, DataWire, Self>(&mut &*inode)
    }

    /// The size of a regular file, from both halves of `i_size`.
    pub(crate) fn size(&self) -> u64 {
        u64::from(self.size_high) << 32 | u64::from(self.size_lo)
    }

    /// Store both halves of a regular file's size and preserve every other field.
    pub(crate) fn put_size(mut self, inode: &mut [u8], size: u64) -> std::io::Result<()> {
        self.size_lo = size as u32;
        self.size_high = (size >> 32) as u32;
        write_native::<_, DataWire, Self>(&mut &mut *inode, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_flags_preserve_the_fixture_encoding() {
        let image = include_bytes!("../../tests/fixtures/basic.ext4").to_vec();
        let window = crate::device::Window::whole(image).unwrap();
        let number = crate::lookup::inode_of(&window, "/hello.txt").unwrap();
        let sb = crate::journal::read_volume_superblock(&window).unwrap();
        let volume = crate::journal::map::Volume::parse(&sb).unwrap();
        let mut inode = volume.read_inode(&window, number).unwrap();
        let original = inode.clone();
        let native = Data::read(&inode).unwrap();
        let size = native.size();
        native.put_size(&mut inode, size).unwrap();
        assert_eq!(inode, original);
        // Literal format bytes test both halves independently of the encoder.
        inode[4..8].copy_from_slice(&[0x89, 0x67, 0x45, 0x23]);
        inode[0x6C..0x70].copy_from_slice(&[1, 0, 0, 0]);
        inode[0x20..0x24].copy_from_slice(&[0, 0, 8, 0]);
        let native = Data::read(&inode).unwrap();
        assert_eq!(native.size(), 0x1_2345_6789);
        assert_eq!(native.flags, 0x8_0000);
        let mut expected = inode.clone();
        expected[4..8].copy_from_slice(&[4, 3, 2, 1]);
        expected[0x6C..0x70].copy_from_slice(&[2, 0, 0, 0]);
        native.put_size(&mut inode, 0x2_0102_0304).unwrap();
        assert_eq!(inode, expected);
        assert_eq!(size_of::<DataWire>(), 128);
    }
}
