//! The checksum of LVM2 labels, metadata area headers and metadata text: CRC-32 with the
//! reflected polynomial 0xedb88320, initial value 0xf597a6cf, and no final XOR ("weak CRC-32"
//! in the libvslvm format description).

const POLY: u32 = 0xedb8_8320;
const INIT: u32 = 0xf597_a6cf;

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = i as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ POLY
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
};

/// The checksum of `data`.
pub fn checksum(data: &[u8]) -> u32 {
    update(INIT, data)
}

/// Continues a checksum over more data, for text split across the end of a circular buffer.
pub fn update(crc: u32, data: &[u8]) -> u32 {
    data.iter().fold(crc, |crc, &byte| {
        TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_standard_crc_with_other_parameters() {
        // With init 0xffffffff and a final XOR, this table gives the standard CRC-32, whose
        // check value for "123456789" is 0xcbf43926.
        assert_eq!(update(0xffff_ffff, b"123456789") ^ 0xffff_ffff, 0xcbf4_3926);
    }

    #[test]
    fn split_updates_match_one_pass() {
        let data = b"physical_volumes { pv0 { id = \"x\" } }";
        let (a, b) = data.split_at(11);
        assert_eq!(update(checksum(a), b), checksum(data));
        assert_eq!(checksum(b""), INIT);
    }
}
