//! Mapping a byte offset in a logical volume to a byte offset on one of the group's physical
//! volumes (lvcreate(8) and lvmraid(7) for the striped layout).
//!
//! A linear segment places its extents one after another from the stripe's first physical
//! extent. A striped segment with `n` stripes splits its range into chunks of `stripe_size`
//! bytes and deals them out round robin: chunk `c` goes to stripe `c % n`, as that stripe's
//! chunk number `c / n`. Each answer covers the bytes up to the end of the extent run or chunk,
//! so a reader can loop over a request without mapping every sector.

use thiserror::Error;

use crate::label::Uuid;
use crate::vg::{LogicalVolume, SegmentKind, VolumeGroup};

/// Where a run of logical-volume bytes is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mapping {
    /// The index of the PV in [`VolumeGroup::physical_volumes`].
    pub pv: usize,
    /// That PV's UUID, to find its device.
    pub pv_id: Uuid,
    /// The byte offset on the PV's device.
    pub offset: u64,
    /// How many bytes from `offset` continue the run, never zero.
    pub len: u64,
}

impl VolumeGroup {
    /// The size of `lv` in bytes.
    pub fn lv_bytes(&self, lv: &LogicalVolume) -> u64 {
        // Validation bounds every extent by a PV's size, so this cannot overflow for an LV
        // of this group.
        lv.extents().saturating_mul(self.extent_bytes)
    }

    /// Maps byte `offset` of `lv`, which must be a volume of this group.
    ///
    /// # Errors
    ///
    /// [`MapError::OutOfRange`] for an offset at or past the end of the volume, and
    /// [`MapError::Unsupported`] inside a segment that is not linear or striped.
    ///
    /// # Panics
    ///
    /// When `lv` is from another group and names a PV index this group does not have.
    pub fn map(&self, lv: &LogicalVolume, offset: u64) -> Result<Mapping, MapError> {
        let extent = offset / self.extent_bytes;
        let segment = lv
            .segments
            .iter()
            .find(|s| (s.start_extent..s.start_extent + s.extent_count).contains(&extent))
            .ok_or(MapError::OutOfRange)?;
        let SegmentKind::Striped {
            stripe_bytes,
            stripes,
        } = &segment.kind
        else {
            return Err(MapError::Unsupported);
        };
        let in_segment = offset - segment.start_extent * self.extent_bytes;
        let segment_bytes = segment.extent_count * self.extent_bytes;
        let (stripe, on_stripe, len) = match stripe_bytes {
            None => (stripes[0], in_segment, segment_bytes - in_segment),
            Some(chunk) => {
                let n = stripes.len() as u64;
                let index = in_segment / chunk;
                let within = in_segment % chunk;
                let stripe = stripes[(index % n) as usize];
                (stripe, (index / n) * chunk + within, chunk - within)
            }
        };
        let pv = &self.physical_volumes[stripe.pv];
        Ok(Mapping {
            pv: stripe.pv,
            pv_id: pv.id,
            offset: pv.pe_start + stripe.start_pe * self.extent_bytes + on_stripe,
            len,
        })
    }
}

/// A logical-volume offset that cannot be mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum MapError {
    /// The offset is at or past the end of the volume.
    #[error("the offset is past the end of the logical volume")]
    OutOfRange,
    /// The offset is in a segment type this crate does not map.
    #[error("the logical volume segment type is not supported")]
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vg::tests::demo;

    const MIB: u64 = 1 << 20;

    #[test]
    fn linear_segments_map_extent_by_extent() {
        let vg = demo();
        let lv = vg.logical_volume("linear").unwrap();
        assert_eq!(vg.lv_bytes(lv), 20 * MIB);
        let at = |offset| vg.map(lv, offset).map(|m| (m.pv, m.offset, m.len));
        assert_eq!(at(0), Ok((0, MIB, 8 * MIB)));
        assert_eq!(at(8 * MIB - 1), Ok((0, 9 * MIB - 1, 1)));
        // Segment 2 starts at PE 10 of pv1: 1 MiB + 40 MiB.
        assert_eq!(at(8 * MIB), Ok((1, 41 * MIB, 12 * MIB)));
        assert_eq!(at(20 * MIB - 1), Ok((1, 53 * MIB - 1, 1)));
        assert_eq!(at(20 * MIB), Err(MapError::OutOfRange));
    }

    #[test]
    fn striped_segments_deal_chunks_round_robin() {
        let vg = demo();
        let lv = vg.logical_volume("wide").unwrap();
        let chunk = 64 << 10;
        let at = |offset| vg.map(lv, offset).map(|m| (m.pv, m.offset, m.len));
        // pv0's stripe starts at PE 2 (1 MiB + 8 MiB), pv1's at PE 0 (1 MiB).
        assert_eq!(at(0), Ok((0, 9 * MIB, chunk)));
        assert_eq!(at(chunk + 5), Ok((1, MIB + 5, chunk - 5)));
        assert_eq!(at(2 * chunk), Ok((0, 9 * MIB + chunk, chunk)));
        assert_eq!(at(3 * chunk), Ok((1, MIB + chunk, chunk)));
        // The last chunk of 16 MiB is the 128th of pv1's stripe.
        assert_eq!(at(16 * MIB - 1), Ok((1, MIB + 8 * MIB - 1, 1)));
        assert_eq!(vg.map(lv, 0).unwrap().pv_id, vg.physical_volumes[0].id);
    }

    #[test]
    fn other_segment_types_are_refused() {
        let vg = demo();
        let lv = vg.logical_volume("copy").unwrap();
        assert_eq!(vg.map(lv, 0), Err(MapError::Unsupported));
    }
}
