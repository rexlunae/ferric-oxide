use anyhow::{anyhow, Result};
use plain::Plain;
use smallvec::SmallVec;

/// HID class descriptor types (HID 1.11, 7.1).
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum HidClassType {
    HID = 0x21,
    Report = 0x22,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HidClassDescriptor {
    /// A [`HidClassType`], kept as the raw byte because devices may report other types.
    pub desc_type: u8,
    pub desc_len: u16,
}

unsafe impl Plain for HidClassDescriptor {}

#[derive(Clone, Debug, Default)]
pub struct HidDescriptor {
    pub descriptors: SmallVec<[HidClassDescriptor; 1]>,
}

impl HidDescriptor {
    // Size of the fixed part of HidDescriptor
    const HID_DESC_FIXED_SIZE: u8 = 6;
    const HID_CLASS_DESC_SIZE: u8 = 3;

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let length = bytes[0];
        let kind = bytes[1];

        // A valid descriptor has at least one class descriptor
        if length < Self::HID_DESC_FIXED_SIZE + Self::HID_CLASS_DESC_SIZE {
            return Err(anyhow!("Invalid length"));
        }

        if kind != HidClassType::HID as u8 {
            return Err(anyhow!("This is not a hid descriptor"));
        }

        let num_descriptors = bytes[5];

        if length != Self::HID_DESC_FIXED_SIZE + num_descriptors * Self::HID_CLASS_DESC_SIZE {
            return Err(anyhow!(
                "Len doesn't match the given number of descriptors ({num_descriptors})"
            ));
        }

        let mut descriptors =
            SmallVec::<[HidClassDescriptor; 1]>::with_capacity(num_descriptors as usize);

        for i in 0..num_descriptors {
            match HidClassDescriptor::from_bytes(
                &bytes[(Self::HID_DESC_FIXED_SIZE + i * Self::HID_CLASS_DESC_SIZE) as usize
                    ..(Self::HID_DESC_FIXED_SIZE + (i + 1) * Self::HID_CLASS_DESC_SIZE) as usize],
            ) {
                Ok(desc) => descriptors.push(*desc),
                Err(e) => return Err(anyhow!("{e:?}")),
            }
        }

        Ok(Self { descriptors })
    }

    pub fn get_report_desc(&self) -> Result<HidClassDescriptor> {
        self.descriptors
            .iter()
            .find(|desc| desc.desc_type == HidClassType::Report as u8)
            .copied()
            .ok_or_else(|| anyhow!("No Report descriptor found"))
    }
}
