//! Cache keys.
//!
//! An entry is filed under a [`CacheKey`], and is valid only for exactly that key. The key
//! is stored in the entry and covered by its MAC, so an entry cannot be renamed into
//! another key's place. Its [`digest`](CacheKey::digest) names the entry's file.
//!
//! The encoding is fixed-size ([`CacheKey::SIZE`] bytes, little-endian, zero padding):
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0..32 | module BLAKE3 |
//! | 32..40 | `wasmc` version: major, minor, patch (`u16` each), 2 zero bytes |
//! | 40..72 | engine identity (a BLAKE3 of Wasmtime's precompile compatibility hash and settings) |
//! | 72..80 | CPU: architecture (`u8`), 7 zero bytes |
//! | 80..88 | CPU feature mask (`u64`) |
//! | 88..96 | options: bounds strategy, Spectre level, tier, opt level (`u8` each), 4 zero bytes |
//! | 96..104 | ABI versions: `ferric:sys`, WASI preview 1 (`u32` each) |
//! | 104..112 | verifier version (`u32`), 4 zero bytes |

use core::fmt;

use crate::{array, put};

/// Why a key could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyError {
    /// An enumerated field holds an unknown value.
    #[error("cache key field {field} has unknown value {value}")]
    Unknown {
        /// The field.
        field: &'static str,
        /// The value.
        value: u8,
    },
    /// Padding is not zero, so the encoding would not be canonical.
    #[error("cache key padding is not zero")]
    Padding,
}

macro_rules! byte_enum {
    ($(#[$doc:meta])* $name:ident ($field:literal) { $($(#[$vdoc:meta])* $variant:ident = $value:literal,)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $($(#[$vdoc])* $variant,)*
        }

        impl $name {
            /// The encoded value.
            pub const fn to_u8(self) -> u8 {
                match self {
                    $(Self::$variant => $value,)*
                }
            }

            /// Decodes the value.
            ///
            /// # Errors
            ///
            /// [`KeyError::Unknown`] for a value this version does not define.
            pub const fn from_u8(value: u8) -> Result<Self, KeyError> {
                match value {
                    $($value => Ok(Self::$variant),)*
                    _ => Err(KeyError::Unknown { field: $field, value }),
                }
            }
        }
    };
}

byte_enum! {
    /// The architecture code is compiled for.
    Arch ("arch") {
        /// x86-64.
        X86_64 = 1,
        /// AArch64.
        Aarch64 = 2,
    }
}

byte_enum! {
    /// How linear-memory accesses are bounded.
    BoundsStrategy ("bounds") {
        /// wasm32 in a 4 GiB reservation plus guard: no explicit checks.
        GuardPages = 1,
        /// An explicit compare against the memory's bound, with a Spectre-safe conditional
        /// move (wasm64, or wasm32 without signal-based traps).
        Checked = 2,
        /// wasm64 in a power-of-two memory: the index is masked.
        Masked = 3,
    }
}

byte_enum! {
    /// Spectre hardening in generated code.
    SpectreLevel ("spectre") {
        /// Cranelift's heap and table guards (isolated mode).
        Guards = 1,
        /// Guards plus Swivel-style hardening at domain transitions (colocated mode).
        Swivel = 2,
    }
}

byte_enum! {
    /// The compiler tier.
    Tier ("tier") {
        /// The fast baseline compiler (Winch).
        Baseline = 1,
        /// The optimizing compiler (Cranelift).
        Optimized = 2,
    }
}

byte_enum! {
    /// The optimizing compiler's level.
    OptLevel ("opt") {
        /// No optimization (and the baseline tier).
        None = 0,
        /// Optimize for speed.
        Speed = 1,
        /// Optimize for speed and size.
        SpeedAndSize = 2,
    }
}

/// Feature bits for [`Arch::X86_64`], named after Cranelift's x86-64 ISA flags.
pub mod x86 {
    /// SSE3.
    pub const SSE3: u64 = 1 << 0;
    /// SSSE3.
    pub const SSSE3: u64 = 1 << 1;
    /// SSE4.1.
    pub const SSE41: u64 = 1 << 2;
    /// SSE4.2.
    pub const SSE42: u64 = 1 << 3;
    /// POPCNT.
    pub const POPCNT: u64 = 1 << 4;
    /// AVX.
    pub const AVX: u64 = 1 << 5;
    /// AVX2.
    pub const AVX2: u64 = 1 << 6;
    /// BMI1.
    pub const BMI1: u64 = 1 << 7;
    /// BMI2.
    pub const BMI2: u64 = 1 << 8;
    /// FMA.
    pub const FMA: u64 = 1 << 9;
    /// LZCNT.
    pub const LZCNT: u64 = 1 << 10;
    /// AVX-512 Foundation.
    pub const AVX512F: u64 = 1 << 11;
    /// AVX-512 vector length extensions.
    pub const AVX512VL: u64 = 1 << 12;
    /// AVX-512 doubleword and quadword instructions.
    pub const AVX512DQ: u64 = 1 << 13;
    /// AVX-512 byte and word instructions.
    pub const AVX512BW: u64 = 1 << 14;
    /// AVX-512 vector byte manipulation instructions.
    pub const AVX512VBMI: u64 = 1 << 15;
    /// CMPXCHG16B.
    pub const CMPXCHG16B: u64 = 1 << 16;

    /// The x86-64-v3 feature set, the user-space baseline.
    pub const V3: u64 =
        SSE3 | SSSE3 | SSE41 | SSE42 | POPCNT | AVX | AVX2 | BMI1 | BMI2 | FMA | LZCNT | CMPXCHG16B;
    /// The x86-64-v4 feature set.
    pub const V4: u64 = V3 | AVX512F | AVX512VL | AVX512DQ | AVX512BW;
}

/// Feature bits for [`Arch::Aarch64`], named after Cranelift's AArch64 ISA flags.
pub mod aarch64 {
    /// Large System Extensions (atomics).
    pub const LSE: u64 = 1 << 0;
    /// Pointer authentication.
    pub const PAUTH: u64 = 1 << 1;
    /// Half-precision floating point.
    pub const FP16: u64 = 1 << 2;
    /// Branch target identification.
    pub const BTI: u64 = 1 << 3;
}

/// The exact CPU an entry was compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CpuFeatures {
    /// The architecture.
    pub arch: Arch,
    /// The feature bits ([`x86`] or [`aarch64`]).
    pub features: u64,
}

impl CpuFeatures {
    /// A CPU description.
    pub const fn new(arch: Arch, features: u64) -> Self {
        Self { arch, features }
    }

    /// Whether code compiled for `self` may run on `cpu`: the same architecture, and every
    /// feature the code uses present.
    pub const fn runs_on(self, cpu: Self) -> bool {
        self.arch.to_u8() == cpu.arch.to_u8() && self.features & !cpu.features == 0
    }
}

/// The codegen options that change the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CodegenOptions {
    /// Bounds checking.
    pub bounds: BoundsStrategy,
    /// Spectre hardening.
    pub spectre: SpectreLevel,
    /// The tier.
    pub tier: Tier,
    /// The optimization level.
    pub opt: OptLevel,
}

/// Who compiled the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompilerId {
    /// The `wasmc` version: major, minor, patch.
    pub wasmc: [u16; 3],
    /// The engine identity: a BLAKE3 over Wasmtime's precompile compatibility hash and the
    /// engine settings `wasmc` uses.
    pub engine: [u8; 32],
}

/// Everything an entry is valid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// The module's BLAKE3, the same value as its pkgar entry hash.
    pub module: [u8; 32],
    /// The compiler.
    pub compiler: CompilerId,
    /// The CPU.
    pub cpu: CpuFeatures,
    /// The codegen options.
    pub options: CodegenOptions,
    /// The `ferric:sys` ABI version.
    pub ferric_sys: u32,
    /// The WASI preview 1 implementation version.
    pub wasi_p1: u32,
    /// The verifier version (0 when the kernel does not verify).
    pub verifier: u32,
}

impl CacheKey {
    /// The encoded size.
    pub const SIZE: usize = 112;

    /// A key with fixed placeholder values, for examples and tests.
    pub const fn example() -> Self {
        Self {
            module: [1; 32],
            compiler: CompilerId {
                wasmc: [0, 1, 0],
                engine: [2; 32],
            },
            cpu: CpuFeatures::new(Arch::X86_64, x86::V3),
            options: CodegenOptions {
                bounds: BoundsStrategy::GuardPages,
                spectre: SpectreLevel::Guards,
                tier: Tier::Optimized,
                opt: OptLevel::Speed,
            },
            ferric_sys: 1,
            wasi_p1: 1,
            verifier: 0,
        }
    }

    /// The canonical encoding (see the module documentation).
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        let at = &mut 0;
        put(&mut out, at, &self.module);
        for part in self.compiler.wasmc {
            put(&mut out, at, &part.to_le_bytes());
        }
        put(&mut out, at, &[0; 2]);
        put(&mut out, at, &self.compiler.engine);
        put(&mut out, at, &[self.cpu.arch.to_u8(), 0, 0, 0, 0, 0, 0, 0]);
        put(&mut out, at, &self.cpu.features.to_le_bytes());
        let options = &self.options;
        put(
            &mut out,
            at,
            &[
                options.bounds.to_u8(),
                options.spectre.to_u8(),
                options.tier.to_u8(),
                options.opt.to_u8(),
                0,
                0,
                0,
                0,
            ],
        );
        put(&mut out, at, &self.ferric_sys.to_le_bytes());
        put(&mut out, at, &self.wasi_p1.to_le_bytes());
        put(&mut out, at, &self.verifier.to_le_bytes());
        out
    }

    /// Decodes a key.
    ///
    /// # Errors
    ///
    /// [`KeyError::Unknown`] for an enumerated field this version does not define, and
    /// [`KeyError::Padding`] if any padding byte is not zero.
    pub fn decode(bytes: &[u8; Self::SIZE]) -> Result<Self, KeyError> {
        let zero = |range: core::ops::Range<usize>| bytes[range].iter().all(|byte| *byte == 0);
        if !zero(38..40) || !zero(73..80) || !zero(92..96) || !zero(108..112) {
            return Err(KeyError::Padding);
        }
        let half = |at: usize| u16::from_le_bytes(array(bytes, at));
        let word = |at: usize| u32::from_le_bytes(array(bytes, at));
        Ok(Self {
            module: array(bytes, 0),
            compiler: CompilerId {
                wasmc: [half(32), half(34), half(36)],
                engine: array(bytes, 40),
            },
            cpu: CpuFeatures::new(
                Arch::from_u8(bytes[72])?,
                u64::from_le_bytes(array(bytes, 80)),
            ),
            options: CodegenOptions {
                bounds: BoundsStrategy::from_u8(bytes[88])?,
                spectre: SpectreLevel::from_u8(bytes[89])?,
                tier: Tier::from_u8(bytes[90])?,
                opt: OptLevel::from_u8(bytes[91])?,
            },
            ferric_sys: word(96),
            wasi_p1: word(100),
            verifier: word(104),
        })
    }

    /// The key's digest, which names its entry in the cache.
    pub fn digest(&self) -> KeyDigest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"ferric-codecache-key/1\0");
        hasher.update(&self.encode());
        KeyDigest(*hasher.finalize().as_bytes())
    }
}

/// The BLAKE3 digest of a [`CacheKey`]; its lowercase hex form is the entry's file name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyDigest([u8; 32]);

impl KeyDigest {
    /// The digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for KeyDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for KeyDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyDigest({self})")
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::ToString;

    #[test]
    fn keys_round_trip_and_reject_noise() {
        let key = CacheKey::example();
        let bytes = key.encode();
        assert_eq!(CacheKey::decode(&bytes), Ok(key));
        assert_eq!(&bytes[32..38], &[0, 0, 1, 0, 0, 0]);
        assert_eq!(bytes[72], 1);
        assert_eq!(&bytes[80..88], &x86::V3.to_le_bytes());
        assert_eq!(&bytes[88..92], &[1, 1, 2, 1]);

        for at in [38, 79, 95, 111] {
            let mut padded = bytes;
            padded[at] = 1;
            assert_eq!(CacheKey::decode(&padded), Err(KeyError::Padding), "{at}");
        }
        let mut arch = bytes;
        arch[72] = 9;
        assert_eq!(
            CacheKey::decode(&arch),
            Err(KeyError::Unknown {
                field: "arch",
                value: 9
            })
        );
    }

    #[test]
    fn every_field_changes_the_digest() {
        let base = CacheKey::example();
        let mut variants = [base; 7];
        variants[0].module[0] ^= 1;
        variants[1].compiler.wasmc[2] += 1;
        variants[2].compiler.engine[31] ^= 1;
        variants[3].cpu.features |= x86::AVX512F;
        variants[4].options.tier = Tier::Baseline;
        variants[5].ferric_sys = 2;
        variants[6].verifier = 1;
        for variant in variants {
            assert_ne!(variant.digest(), base.digest(), "{variant:?}");
        }
        // A fixed vector: changing the encoding or the domain string changes this.
        assert_eq!(
            base.digest().to_string(),
            "3480ae0cd99a56892afc121825bafcf8ceb3890b3e3e694f16a2b8723c874f19"
        );
    }

    #[test]
    fn code_runs_only_where_its_features_are() {
        let v3 = CpuFeatures::new(Arch::X86_64, x86::V3);
        let v4 = CpuFeatures::new(Arch::X86_64, x86::V4);
        assert!(v3.runs_on(v4));
        assert!(!v4.runs_on(v3));
        assert!(!v3.runs_on(CpuFeatures::new(Arch::Aarch64, u64::MAX)));
    }
}
