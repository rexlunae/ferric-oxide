//! Fixture serialization through the same endian boundary as disk records.
extern crate alloc;
use alloc::vec::Vec;

use simple_endian::{FromSlice, LittleEndian, SpecificEndian};

pub fn encode<T: SpecificEndian<T>>(value: LittleEndian<T>) -> Vec<u8>
where
    LittleEndian<T>: FromSlice,
{
    let mut bytes = Vec::new();
    value
        .write_to_extend(&mut bytes)
        .expect("a supported integer fixture field");
    bytes
}
