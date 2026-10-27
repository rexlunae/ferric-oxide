//! The 40-byte kernel.pcache queue record, with fixed little-endian fields.
//!
//! The native layout is defined once; Endianize supplies the wire type and
//! conversions. Its no_std slice API reads individual fields, so offsets come
//! from that generated type. Both directions avoid casts and allocation.

use simple_endian::{Endianize, FromSlice};

#[derive(Endianize)]
#[endian(le)]
#[repr(C)]
pub(crate) struct QueueRecord {
    pub(crate) kind: u16,
    pub(crate) count: u16,
    pub(crate) reserved: u32,
    pub(crate) object: u64,
    pub(crate) first: u64,
    pub(crate) generation: u64,
    pub(crate) fill: u64,
}

/// The queue record's wire size; checked against the published ABI below.
pub(crate) const BYTES: usize = size_of::<QueueRecordWire>();
const _: () = assert!(BYTES == 40);

impl QueueRecord {
    pub(crate) fn read(bytes: &[u8; BYTES]) -> Option<Self> {
        macro_rules! field {
            ($name:ident) => {
                FromSlice::read_from_slice(&bytes[core::mem::offset_of!(QueueRecordWire, $name)..])
                    .ok()?
            };
        }
        Some(
            QueueRecordWire {
                kind: field!(kind),
                count: field!(count),
                reserved: field!(reserved),
                object: field!(object),
                first: field!(first),
                generation: field!(generation),
                fill: field!(fill),
            }
            .into(),
        )
    }

    pub(crate) fn bytes(self) -> [u8; BYTES] {
        let wire = QueueRecordWire::from(self);
        let mut bytes = [0; BYTES];
        macro_rules! field {
            ($name:ident) => {
                put(
                    wire.$name,
                    &mut bytes[core::mem::offset_of!(QueueRecordWire, $name)..],
                );
            };
        }
        field!(kind);
        field!(count);
        field!(reserved);
        field!(object);
        field!(first);
        field!(generation);
        field!(fill);
        bytes
    }
}

/// Adapts a fixed record buffer to simple_endian's allocation-free writer.
struct Sink<'a> {
    bytes: &'a mut [u8],
    written: usize,
}

impl Extend<u8> for Sink<'_> {
    fn extend<T: IntoIterator<Item = u8>>(&mut self, bytes: T) {
        for byte in bytes {
            self.bytes[self.written] = byte;
            self.written += 1;
        }
    }
}

fn put<T: FromSlice>(field: T, bytes: &mut [u8]) {
    field
        .write_to_extend(&mut Sink { bytes, written: 0 })
        .expect("queue fields are supported integer widths, within the fixed wire layout");
}
