//! The MSR and I/O permission maps (AMD64 APM Vol. 2, "MSR Intercepts" and "I/O Intercepts").
//!
//! The MSR permission map is 8 KiB with two bits per MSR, read then write, for three ranges:
//! MSRs 0–1FFFh at offset 0, C000_0000h–C000_1FFFh at 800h, and C001_0000h–C001_1FFFh at 1000h.
//! The I/O permission map is 12 KiB with one bit per port; an access of several bytes checks
//! the bit of every port it touches, which is why the map runs past port FFFFh. A set bit
//! intercepts. Both maps start with every bit set here, so the hypervisor opts in to passing
//! accesses through.

/// The size of the MSR permission map.
pub const MSRPM_BYTES: usize = 8192;

/// The size of the I/O permission map.
pub const IOPM_BYTES: usize = 12288;

/// A direction of access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// RDMSR or IN.
    Read,
    /// WRMSR or OUT.
    Write,
}

/// The bit of `msr` for `access` in the MSR permission map, as (byte, bit), or `None` when the
/// MSR is outside the map's ranges and always intercepted.
pub const fn msr_position(msr: u32, access: Access) -> Option<(usize, u8)> {
    let (base, index) = match msr {
        0..=0x1fff => (0, msr),
        0xc000_0000..=0xc000_1fff => (0x800, msr - 0xc000_0000),
        0xc001_0000..=0xc001_1fff => (0x1000, msr - 0xc001_0000),
        _ => return None,
    };
    let bit = 2 * index
        + match access {
            Access::Read => 0,
            Access::Write => 1,
        };
    Some((base + (bit / 8) as usize, (bit % 8) as u8))
}

/// An MSR permission map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MsrPermissionMap([u8; MSRPM_BYTES]);

impl Default for MsrPermissionMap {
    fn default() -> Self {
        Self::new()
    }
}

impl MsrPermissionMap {
    /// A map that intercepts every access.
    pub const fn new() -> Self {
        Self([0xff; MSRPM_BYTES])
    }

    /// The map's bytes.
    pub const fn as_bytes(&self) -> &[u8; MSRPM_BYTES] {
        &self.0
    }

    /// Lets `access` to `msr` through; returns false when the MSR is outside the map.
    pub fn pass_through(&mut self, msr: u32, access: Access) -> bool {
        match msr_position(msr, access) {
            Some((byte, bit)) => {
                self.0[byte] &= !(1 << bit);
                true
            }
            None => false,
        }
    }

    /// Whether `access` to `msr` is intercepted.
    pub const fn intercepts(&self, msr: u32, access: Access) -> bool {
        match msr_position(msr, access) {
            Some((byte, bit)) => self.0[byte] & (1 << bit) != 0,
            None => true,
        }
    }
}

/// An I/O permission map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoPermissionMap([u8; IOPM_BYTES]);

impl Default for IoPermissionMap {
    fn default() -> Self {
        Self::new()
    }
}

impl IoPermissionMap {
    /// A map that intercepts every port.
    pub const fn new() -> Self {
        Self([0xff; IOPM_BYTES])
    }

    /// The map's bytes.
    pub const fn as_bytes(&self) -> &[u8; IOPM_BYTES] {
        &self.0
    }

    /// Lets accesses to `port` through.
    pub fn pass_through(&mut self, port: u16) {
        self.0[usize::from(port / 8)] &= !(1 << (port % 8));
    }

    /// Whether an access of `size` bytes at `port` is intercepted: it is if any port it
    /// touches is.
    pub fn intercepts(&self, port: u16, size: u8) -> bool {
        (0..u32::from(size.max(1))).any(|i| {
            let p = u32::from(port) + i;
            self.0[(p / 8) as usize] & (1 << (p % 8)) != 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msr_positions() {
        assert_eq!(msr_position(0, Access::Read), Some((0, 0)));
        assert_eq!(msr_position(0, Access::Write), Some((0, 1)));
        assert_eq!(msr_position(0x1fff, Access::Write), Some((0x7ff, 7)));
        // IA32_EFER (C000_0080h).
        assert_eq!(msr_position(0xc000_0080, Access::Read), Some((0x820, 0)));
        assert_eq!(msr_position(0xc001_0114, Access::Write), Some((0x1045, 1)));
        assert_eq!(msr_position(0x4000_0000, Access::Read), None);
    }

    #[test]
    fn maps_are_deny_by_default() {
        let mut msrs = MsrPermissionMap::new();
        assert!(msrs.intercepts(0xc000_0100, Access::Read));
        assert!(msrs.pass_through(0xc000_0100, Access::Read));
        assert!(!msrs.intercepts(0xc000_0100, Access::Read));
        assert!(msrs.intercepts(0xc000_0100, Access::Write));
        assert!(!msrs.pass_through(0x4000_0000, Access::Read));
        assert_eq!(msrs.as_bytes().len(), MSRPM_BYTES);

        let mut ports = IoPermissionMap::new();
        ports.pass_through(0x80);
        assert!(!ports.intercepts(0x80, 1));
        // A word access at 0x80 also touches 0x81, which is still intercepted.
        assert!(ports.intercepts(0x80, 2));
        ports.pass_through(0xffff);
        // A dword at FFFFh runs into the extra bits past the 64 Ki ports.
        assert!(ports.intercepts(0xffff, 4));
        assert!(!ports.intercepts(0xffff, 1));
        assert_eq!(ports.as_bytes().len(), IOPM_BYTES);
    }
}
