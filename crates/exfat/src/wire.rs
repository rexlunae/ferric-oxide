//! Disk records from Microsoft's exFAT specification, revision 1.00.
//!
//! Endianize defines each native/wire pair once. simple_endian 0.4's no_std
//! slice API reads individual fields, so the adapter below uses the generated
//! wire offsets to assemble records safely, including from unaligned slices.
//! No references are cast to disk data and no allocation is needed.

use simple_endian::{Endianize, FromSlice, LittleEndian};

trait Field: Sized {
    fn read(bytes: &[u8]) -> Option<Self>;
}

impl Field for u8 {
    fn read(bytes: &[u8]) -> Option<Self> {
        bytes.first().copied()
    }
}

impl<T: simple_endian::SpecificEndian<T>> Field for LittleEndian<T>
where
    Self: FromSlice,
{
    fn read(bytes: &[u8]) -> Option<Self> {
        Self::read_from_slice(bytes).ok()
    }
}

impl<T: Field + Copy + Default, const N: usize> Field for [T; N] {
    fn read(bytes: &[u8]) -> Option<Self> {
        let mut fields = [T::default(); N];
        for (i, field) in fields.iter_mut().enumerate() {
            *field = T::read(bytes.get(i * size_of::<T>()..)?)?;
        }
        Some(fields)
    }
}

// Explicit reserved fields make every record padding-free; size assertions below
// guard this assumption. Byte order and field conversion belong to Endianize.
macro_rules! record {
    ($name:ident, $wire:ident, $size:expr, { $($field:ident: $ty:tt),* $(,)? }) => {
        #[derive(Endianize)]
        #[endian(le)]
        #[repr(C)]
        pub(crate) struct $name { $(pub(crate) $field: $ty,)* }

        impl $name {
            pub(crate) fn read(bytes: &[u8]) -> Option<Self> {
                let bytes = bytes.get(..size_of::<$wire>())?;
                let wire = $wire {
                    $($field: Field::read(&bytes[core::mem::offset_of!($wire, $field)..])?,)*
                };
                Some(wire.into())
            }
        }

        const _: () = {
            assert!(size_of::<$wire>() == $size);
            assert!(size_of::<$wire>() == 0 $(+ size_of::<$ty>())*);
        };
    };
}

record!(BootRecord, BootRecordWire, 512, {
    jump: [u8; 3], name: [u8; 8], must_zero: [u8; 53],
    partition_offset: u64, volume_sectors: u64,
    fat_offset: u32, fat_length: u32, heap_offset: u32, cluster_count: u32,
    root_cluster: u32, serial: u32, revision: u16, flags: u16,
    sector_shift: u8, cluster_shift: u8, fat_count: u8, drive: u8, percent: u8,
    reserved: [u8; 7], boot_code: [u8; 390], signature: u16,
});
record!(FileRecord, FileRecordWire, 32, {
    kind: u8, secondaries: u8, checksum: u16, attributes: u16, reserved: u16,
    created: u32, modified: u32, accessed: u32,
    created_ten_ms: u8, modified_ten_ms: u8,
    created_utc: u8, modified_utc: u8, accessed_utc: u8, reserved_end: [u8; 7],
});
record!(StreamRecord, StreamRecordWire, 32, {
    kind: u8, flags: u8, reserved: u8, name_len: u8, name_hash: u16,
    reserved2: u16, valid_length: u64, reserved3: u32,
    first_cluster: u32, length: u64,
});
record!(NameRecord, NameRecordWire, 32, {
    kind: u8, flags: u8, units: [u16; 15],
});
record!(BitmapRecord, BitmapRecordWire, 32, {
    kind: u8, flags: u8, reserved: [u8; 18], first_cluster: u32, length: u64,
});
record!(UpcaseRecord, UpcaseRecordWire, 32, {
    kind: u8, reserved: [u8; 3], checksum: u32, reserved2: [u8; 12],
    first_cluster: u32, length: u64,
});
record!(LabelRecord, LabelRecordWire, 32, {
    kind: u8, count: u8, units: [u16; 11], reserved: [u8; 8],
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_records_decode_literal_bytes_at_specified_offsets() {
        let bytes = [
            0xc0, 3, 0, 15, 0x34, 0x12, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 0, 0, 0x78, 0x56, 0x34,
            0x12, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
        ];
        let record = StreamRecord::read(&bytes).unwrap();
        assert_eq!(record.kind, 0xc0);
        assert_eq!(
            (record.flags, record.name_len, record.name_hash),
            (3, 15, 0x1234)
        );
        assert_eq!(record.valid_length, 0x0102_0304_0506_0708);
        assert_eq!(record.first_cluster, 0x1234_5678);
        assert_eq!(record.length, 0x1122_3344_5566_7788);
        // Disk buffers need no native alignment, and short buffers fail safely.
        let mut unaligned = [0; 33];
        unaligned[1..].copy_from_slice(&bytes);
        assert_eq!(
            StreamRecord::read(&unaligned[1..]).unwrap().length,
            record.length
        );
        assert!(StreamRecord::read(&bytes[..31]).is_none());

        let record = UpcaseRecord::read(&bytes).unwrap();
        assert_eq!(record.checksum, 0x1234);
        assert_eq!(
            (record.first_cluster, record.length),
            (0x1234_5678, 0x1122_3344_5566_7788)
        );
        let mut name = [0; 32];
        name[..8].copy_from_slice(&[0xc1, 0, 0x41, 0, 0xac, 0x20, 0xa9, 3]);
        assert_eq!(
            &NameRecord::read(&name).unwrap().units[..3],
            &[0x41, 0x20ac, 0x03a9]
        );
    }
}
