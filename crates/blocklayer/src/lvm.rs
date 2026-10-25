//! The LVM logical-volume layer.
//!
//! [`Lvm::open`] reads the label and metadata of every device given, through `ferric-lvm`,
//! takes the newest copy of the group's metadata (the highest `seqno`, as LVM does), matches
//! each physical volume the metadata lists to a device by its label's PV UUID, and finds the
//! logical volume. A read is split wherever the volume's mapping moves to another extent run,
//! stripe chunk or physical volume, and each piece is read from its device.

use alloc::vec::Vec;

use ferric_lvm::{LogicalVolume, MapError, Pv, ReadError, Uuid, VolumeGroup};
use thiserror::Error;

use crate::{BlockDevice, OutOfRange};

/// A logical volume of an LVM2 volume group, over the group's physical volumes.
#[derive(Debug)]
pub struct Lvm<D> {
    /// The devices, in the order of [`VolumeGroup::physical_volumes`].
    pvs: Vec<D>,
    vg: VolumeGroup,
    lv: usize,
    size: u64,
}

impl<D: BlockDevice> Lvm<D> {
    /// Opens logical volume `name` of the group on `devices`, which must be all of the
    /// group's physical volumes, in any order.
    ///
    /// # Errors
    ///
    /// An [`LvmOpenError`] when a device is not a readable PV, the PVs are not of one group or
    /// none holds its metadata, a PV of the group is missing, or the group has no such volume.
    pub fn open(devices: Vec<D>, name: &str) -> Result<Self, LvmOpenError<D::Error>> {
        let mut labelled = Vec::with_capacity(devices.len());
        let mut newest: Option<VolumeGroup> = None;
        for (index, device) in devices.into_iter().enumerate() {
            let pv = Pv::read(&mut |offset, buf: &mut [u8]| device.read_at(offset, buf))
                .map_err(|source| LvmOpenError::Pv { index, source })?;
            if let Some(vg) = pv.vg {
                if newest.as_ref().is_some_and(|n| n.id != vg.id) {
                    return Err(LvmOpenError::SeveralGroups);
                }
                if newest.as_ref().is_none_or(|n| vg.seqno > n.seqno) {
                    newest = Some(vg);
                }
            }
            labelled.push((pv.label.pv_uuid, Some(device)));
        }
        let vg = newest.ok_or(LvmOpenError::NoMetadata)?;
        let pvs = order_pvs(&vg, labelled)?;
        let lv = vg
            .logical_volumes
            .iter()
            .position(|lv| lv.name == name)
            .ok_or(LvmOpenError::NoVolume)?;
        let size = vg.lv_bytes(&vg.logical_volumes[lv]);
        Ok(Self { pvs, vg, lv, size })
    }

    /// The volume group.
    pub fn group(&self) -> &VolumeGroup {
        &self.vg
    }

    /// The logical volume.
    pub fn volume(&self) -> &LogicalVolume {
        &self.vg.logical_volumes[self.lv]
    }
}

/// Puts the devices in the order the group lists its PVs, checking every PV has exactly one.
fn order_pvs<D, E>(
    vg: &VolumeGroup,
    mut labelled: Vec<(Uuid, Option<D>)>,
) -> Result<Vec<D>, LvmOpenError<E>> {
    if let Some(index) = labelled
        .iter()
        .position(|(id, _)| vg.pv_index(id).is_none())
    {
        return Err(LvmOpenError::NotInGroup { index });
    }
    vg.physical_volumes
        .iter()
        .map(|pv| {
            let mut found = labelled.iter_mut().filter(|(id, _)| *id == pv.id);
            match (found.next(), found.next()) {
                (Some((_, device)), None) => Ok(device.take().expect("each PV is taken once")),
                (Some(_), Some(_)) => Err(LvmOpenError::DuplicatePv { id: pv.id }),
                (None, _) => Err(LvmOpenError::MissingPv { id: pv.id }),
            }
        })
        .collect()
}

impl<D: BlockDevice> BlockDevice for Lvm<D> {
    type Error = LvmError<D::Error>;

    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> Result<(), Self::Error> {
        OutOfRange::check(offset, buf.len(), self.size)?;
        while !buf.is_empty() {
            let run = self.vg.map(self.volume(), offset)?;
            let n = usize::try_from(run.len).map_or(buf.len(), |len| len.min(buf.len()));
            let (piece, rest) = buf.split_at_mut(n);
            self.pvs[run.pv]
                .read_at(run.offset, piece)
                .map_err(|source| LvmError::Inner { pv: run.pv, source })?;
            buf = rest;
            offset += n as u64;
        }
        Ok(())
    }
}

/// A logical volume that could not be opened.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LvmOpenError<E> {
    /// A device's PV label or metadata cannot be read.
    #[error("device {index} is not a readable LVM physical volume")]
    Pv {
        /// The index of the device, in the order given.
        index: usize,
        /// Why.
        #[source]
        source: ReadError<E>,
    },
    /// No device holds the group's metadata.
    #[error("no device holds LVM metadata")]
    NoMetadata,
    /// The devices hold metadata of more than one group.
    #[error("the devices are physical volumes of more than one volume group")]
    SeveralGroups,
    /// A device is not a PV of the group.
    #[error("device {index} is not a physical volume of the group")]
    NotInGroup {
        /// The index of the device, in the order given.
        index: usize,
    },
    /// No device is this PV of the group.
    #[error("physical volume {id} of the group is missing")]
    MissingPv {
        /// The PV's UUID.
        id: Uuid,
    },
    /// More than one device has this PV's UUID.
    #[error("more than one device is physical volume {id}")]
    DuplicatePv {
        /// The PV's UUID.
        id: Uuid,
    },
    /// The group has no logical volume of that name.
    #[error("the volume group has no logical volume of that name")]
    NoVolume,
}

/// A read from an [`Lvm`] volume that failed.
#[derive(Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LvmError<E> {
    /// The read is not inside the volume.
    #[error(transparent)]
    OutOfRange(#[from] OutOfRange),
    /// Part of the volume is in a segment type that is not mapped (mirror, RAID, thin, ...).
    #[error("part of the logical volume cannot be mapped")]
    Map(#[from] MapError),
    /// A physical volume's device failed.
    #[error("reading physical volume {pv} failed")]
    Inner {
        /// The PV's index in the group.
        pv: usize,
        /// The device error.
        #[source]
        source: E,
    },
}
