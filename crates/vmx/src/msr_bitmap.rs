//! The VMX MSR bitmaps: which `RDMSR` and `WRMSR` instructions exit (SDM Vol. 3C, "MSR-Bitmap
//! Address").
//!
//! The 4 KiB page holds four 1 KiB bitmaps, one bit per MSR: reads of MSRs 0–1FFFh at offset 0,
//! reads of C000_0000h–C000_1FFFh at 400h, writes of the low range at 800h and writes of the high
//! range at C00h. A set bit makes the access exit; MSRs outside both ranges always exit. A page
//! of all ones (the default here) makes every access exit, so the hypervisor opts MSRs *in* to
//! passing through, never out.

/// The size of the MSR bitmap page.
pub const BYTES: usize = 4096;

/// A direction of MSR access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// RDMSR.
    Read,
    /// WRMSR.
    Write,
}

/// The bit of `msr` for `access`, as (byte offset, bit), or `None` when the MSR is outside the
/// bitmaps and always exits.
pub const fn position(msr: u32, access: Access) -> Option<(usize, u8)> {
    let (range_offset, index) = match msr {
        0..=0x1fff => (0, msr),
        0xc000_0000..=0xc000_1fff => (0x400, msr - 0xc000_0000),
        _ => return None,
    };
    let base = match access {
        Access::Read => 0,
        Access::Write => 0x800,
    };
    Some((
        base + range_offset + (index / 8) as usize,
        (index % 8) as u8,
    ))
}

/// An MSR bitmap page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MsrBitmap([u8; BYTES]);

impl Default for MsrBitmap {
    fn default() -> Self {
        Self::new()
    }
}

impl MsrBitmap {
    /// A bitmap in which every access exits.
    pub const fn new() -> Self {
        Self([0xff; BYTES])
    }

    /// The page's bytes, to copy into the page the VMCS points at.
    pub const fn as_bytes(&self) -> &[u8; BYTES] {
        &self.0
    }

    /// Lets `access` to `msr` run without an exit; returns false when the MSR is outside the
    /// bitmaps, so it exits anyway.
    pub fn pass_through(&mut self, msr: u32, access: Access) -> bool {
        match position(msr, access) {
            Some((byte, bit)) => {
                self.0[byte] &= !(1 << bit);
                true
            }
            None => false,
        }
    }

    /// Makes `access` to `msr` exit again.
    pub fn intercept(&mut self, msr: u32, access: Access) {
        if let Some((byte, bit)) = position(msr, access) {
            self.0[byte] |= 1 << bit;
        }
    }

    /// Whether `access` to `msr` exits.
    pub const fn exits(&self, msr: u32, access: Access) -> bool {
        match position(msr, access) {
            Some((byte, bit)) => self.0[byte] & (1 << bit) != 0,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_follow_the_layout() {
        assert_eq!(position(0, Access::Read), Some((0, 0)));
        assert_eq!(position(0x1fff, Access::Read), Some((0x3ff, 7)));
        assert_eq!(position(0xc000_0100, Access::Read), Some((0x420, 0)));
        assert_eq!(position(0x10, Access::Write), Some((0x802, 0)));
        assert_eq!(position(0xc000_0102, Access::Write), Some((0xc20, 2)));
        assert_eq!(position(0x2000, Access::Read), None);
        assert_eq!(position(0xc001_0000, Access::Read), None);
    }

    #[test]
    fn pass_through_is_opt_in() {
        let mut bitmap = MsrBitmap::new();
        // IA32_FS_BASE.
        assert!(bitmap.exits(0xc000_0100, Access::Read));
        assert!(bitmap.pass_through(0xc000_0100, Access::Read));
        assert!(!bitmap.exits(0xc000_0100, Access::Read));
        assert!(bitmap.exits(0xc000_0100, Access::Write));
        bitmap.intercept(0xc000_0100, Access::Read);
        assert_eq!(bitmap, MsrBitmap::new());
        assert!(!bitmap.pass_through(0x4000_0000, Access::Read));
        assert!(bitmap.exits(0x4000_0000, Access::Read));
        assert_eq!(bitmap.as_bytes().len(), BYTES);
    }
}
