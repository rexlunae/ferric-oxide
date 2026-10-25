//! Devices and image files as block devices.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use ferric_blocklayer::BlockDevice;

/// An open disk, partition or image file, with the size it had when opened.
#[derive(Debug)]
pub struct FileDevice {
    file: File,
    size: u64,
}

impl FileDevice {
    /// Opens `path` for reading.
    ///
    /// # Errors
    ///
    /// The I/O error from opening the file or asking for its size.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl BlockDevice for FileDevice {
    type Error = io::Error;

    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_read_at_offsets() {
        let path = std::env::temp_dir().join(format!("ferric-blockd-dev-{}", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        let device = FileDevice::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(device.size(), 10);
        let mut buf = [0; 3];
        device.read_at(7, &mut buf).unwrap();
        assert_eq!(&buf, b"789");
        let err = device.read_at(8, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
