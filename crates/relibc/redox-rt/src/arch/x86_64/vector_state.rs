//! Native x86-64 vector spill areas for signal entry, following the instruction
//! entries in the [Intel SDM, Volumes 1/2](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html).
//!
//! The public signal context retains its x87/XMM/YMM layout. AVX-512's additional
//! state lives privately below that context, or in TLS while dequeuing a realtime
//! signal before a handler stack has been selected. Restore these components after
//! the VEX YMM loads, which otherwise clear zmm0-zmm15's upper 256 bits.
//!
//! Only AVX512F instructions are used except the guarded AVX512BW KMOVQ path.
//! F-only processors expose 16-bit mask operations; BW processors need all 64 bits.

/// AVX-512 state is usable by this process.
pub(super) const AVX512_ENABLED: u8 = 1;
/// The processor supports 64-bit opmask moves.
pub(super) const AVX512_WIDE_MASKS: u8 = 2;

/// Vector capabilities after both CPUID and XCR0 have been checked.
pub(super) struct Support {
    /// AVX instructions and YMM state are usable.
    pub(super) avx: bool,
    /// AVX-512 enable and wide-mask bits, consumed by assembly.
    pub(super) avx512: u8,
}

/// Derives signal-entry capabilities from the architectural feature registers.
pub(super) const fn support(leaf1_ecx: u32, leaf7_ebx: u32, xcr0: u64) -> Support {
    let avx = leaf1_ecx & ((1 << 27) | (1 << 28)) == ((1 << 27) | (1 << 28)) && xcr0 & 0x6 == 0x6;
    let enabled = avx && leaf7_ebx & (1 << 16) != 0 && xcr0 & 0xe0 == 0xe0;
    Support {
        avx,
        avx512: if enabled {
            AVX512_ENABLED
                | if leaf7_ebx & (1 << 30) != 0 {
                    AVX512_WIDE_MASKS
                } else {
                    0
                }
        } else {
            0
        },
    }
}

/// Private native register slots; neither a serialized format nor a public ABI.
#[repr(C)]
#[derive(Debug)]
pub(super) struct Avx512Regs {
    /// Upper 256 bits of zmm0-zmm15.
    pub(super) hi256: [[u8; 32]; 16],
    /// Complete zmm16-zmm31.
    pub(super) hi16: [[u8; 64]; 16],
    /// k0-k7, with only their low 16 bits accessed on F-only processors.
    pub(super) masks: [u64; 8],
}

impl Default for Avx512Regs {
    fn default() -> Self {
        Self {
            hi256: [[0; 32]; 16],
            hi16: [[0; 64]; 16],
            masks: [0; 8],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_the_instruction_and_every_enabled_state_component() {
        let cpu_avx = (1 << 27) | (1 << 28);
        let cpu_avx512 = (1 << 16) | (1 << 30);
        assert_eq!(support(cpu_avx, cpu_avx512, 0xe6).avx512, 3);
        for bit in [1, 2, 5, 6, 7] {
            assert_eq!(support(cpu_avx, cpu_avx512, 0xe6 & !(1 << bit)).avx512, 0);
        }
        for bit in [27, 28] {
            assert!(!support(cpu_avx & !(1 << bit), cpu_avx512, 0xe6).avx);
        }
        assert_eq!(support(cpu_avx, 1 << 30, 0xe6).avx512, 0);
        assert_eq!(support(cpu_avx, 1 << 16, 0xe6).avx512, AVX512_ENABLED);
    }

    #[test]
    fn private_spill_layout_holds_every_additional_register() {
        let state = Avx512Regs::default();
        assert_eq!(core::mem::size_of_val(&state), 1600);
        assert_eq!(core::mem::offset_of!(Avx512Regs, hi256), 0);
        assert_eq!(core::mem::offset_of!(Avx512Regs, hi16), 512);
        assert_eq!(core::mem::offset_of!(Avx512Regs, masks), 1536);
        assert!(state.hi256.iter().flatten().all(|&b| b == 0));
        assert!(state.hi16.iter().flatten().all(|&b| b == 0));
        assert_eq!(state.masks, [0; 8]);
    }
}
