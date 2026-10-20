//! Big-endian integer and C-string fields of the binary headers.

use alloc::string::String;

/// The big-endian `u16` at `offset`.
pub(crate) fn be16(buf: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([buf[offset], buf[offset + 1]])
}

/// The big-endian `u32` at `offset`.
pub(crate) fn be32(buf: &[u8], offset: usize) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_be_bytes(bytes)
}

/// The big-endian `u64` at `offset`.
pub(crate) fn be64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_be_bytes(bytes)
}

/// The NUL-terminated ASCII string filling the field `buf[offset..offset + len]`, or `None` when
/// the field has no terminator or holds non-ASCII bytes.
pub(crate) fn c_string(buf: &[u8], offset: usize, len: usize) -> Option<String> {
    let field = &buf[offset..offset + len];
    let end = field.iter().position(|&b| b == 0)?;
    let text = &field[..end];
    text.is_ascii()
        .then(|| text.iter().map(|&b| char::from(b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_decode() {
        let buf = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, b'h', b'i', 0, 0xff,
        ];
        assert_eq!(be16(&buf, 0), 0x1234);
        assert_eq!(be32(&buf, 0), 0x1234_5678);
        assert_eq!(be64(&buf, 0), 0x1234_5678_9abc_def0);
        assert_eq!(c_string(&buf, 8, 3).as_deref(), Some("hi"));
        // No terminator inside the field.
        assert_eq!(c_string(&buf, 8, 2), None);
        // Not ASCII.
        assert_eq!(c_string(&[0xff, 0], 0, 2), None);
    }
}
