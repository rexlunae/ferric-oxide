//! Intel VMX encodings for the Ferric Oxide hypervisor (roadmap Phase V, design in
//! `virt/virt-design.md` in the project files).
//!
//! The kernel's VMX backend executes `VMXON`, `VMREAD`, `VMWRITE`, `VMLAUNCH` and friends; this
//! crate holds everything about those operations that is plain data, so it is tested on the
//! host:
//!
//! - [`field`]: VMCS field encodings, checked at compile time against their width and type.
//! - [`controls`]: the control bits, and [`controls::Allowed::resolve`], which combines the
//!   controls the hypervisor needs with what the capability MSRs allow.
//! - [`caps`]: the capability MSRs decoded, the TRUE-control rule, CR0/CR4 fixed bits, the
//!   EPT/VPID capabilities, IA32_FEATURE_CONTROL, and whether a CPU is good enough.
//! - [`exit`]: exit reasons, exit qualifications and interruption information.
//! - [`ept`]: EPT entries and the EPT pointer, refusing misconfigurations.
//! - [`msr_bitmap`]: the MSR bitmap page, deny by default.
//! - [`check`]: guest-state checks that turn an opaque entry failure into a named one.
//!
//! Source: *Intel 64 and IA-32 Architectures Software Developer's Manual*, Volume 3C (the VMX
//! chapters and appendices A–C). No hypervisor source was used.
//!
//! # Examples
//!
//! ```
//! use ferric_vmx::controls::{Allowed, primary};
//! use ferric_vmx::exit::{BasicReason, ExitReason, IoAccess};
//!
//! // IA32_VMX_TRUE_PROCBASED_CTLS: bits 1, 4–6 and 8 must be one; everything may be one.
//! let allowed = Allowed::from_msr(0xffff_ffff_0000_0172);
//! let controls = allowed
//!     .resolve(primary::HLT_EXITING | primary::USE_MSR_BITMAPS, primary::PAUSE_EXITING)
//!     .expect("supported");
//! assert!(allowed.accepts(controls));
//!
//! // An OUT to the serial port exits with reason 30.
//! let reason = ExitReason::from_raw(30);
//! assert_eq!(reason.basic, BasicReason::Io);
//! let io = IoAccess::from_qualification(0x03f8_0000);
//! assert_eq!((io.port, io.size, io.input), (0x3f8, 1, false));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod caps;
pub mod check;
pub mod controls;
pub mod ept;
pub mod exit;
pub mod field;
pub mod msr_bitmap;
