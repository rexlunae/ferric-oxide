//! AMD SVM encodings for the Ferric Oxide hypervisor (roadmap Phase V, design in
//! `virt/virt-design.md` in the project files).
//!
//! The kernel's SVM backend executes `VMRUN`, `VMLOAD` and `VMSAVE`; this crate holds everything
//! about them that is plain data, so it is tested on the host:
//!
//! - [`vmcb`]: the VMCB as typed fields over its 4 KiB page, segment registers, intercepts set by
//!   exit code, the virtual interrupt controls, and clean bits that are cleared automatically
//!   when a cached field is written.
//! - [`exit`]: `#VMEXIT` codes, IOIO and nested-page-fault EXITINFO decodings, and the
//!   EVENTINJ/EXITINTINFO event format.
//! - [`permission`]: the MSR and I/O permission maps, deny by default.
//! - [`caps`]: CPUID Fn8000_000A features, VM_CR and EFER.SVME, and the `VMRUN` consistency
//!   checks that turn `VMEXIT_INVALID` into a named reason.
//!
//! Nested page tables use the ordinary x86-64 page-table format, so no encoder is needed here.
//!
//! Source: *AMD64 Architecture Programmer's Manual*, Volume 2, chapter 15 "Secure Virtual
//! Machine" and appendices B (VMCB layout) and C (intercept exit codes). No hypervisor source
//! was used.
//!
//! # Examples
//!
//! ```
//! use ferric_svm::exit::{ExitCode, IoAccess};
//! use ferric_svm::vmcb::{self, Vmcb};
//!
//! let mut vmcb = Vmcb::new();
//! vmcb.set_intercept(ExitCode::VMRUN, true).expect("interceptable");
//! vmcb.set_intercept(ExitCode::IOIO, true).expect("interceptable");
//! vmcb.write(vmcb::ASID, 1).expect("fits");
//! vmcb.mark_all_clean();
//!
//! // A new CR3 invalidates the cached control registers, and only those.
//! vmcb.write(vmcb::CR3, 0x10_0000).expect("fits");
//! assert_eq!(vmcb.clean_bits(), 0x0fff & !(1 << 5));
//!
//! // After an OUT to the serial port, EXITINFO1 says which port and size.
//! let io = IoAccess::from_info1(0x03f8_0000 | (1 << 4));
//! assert_eq!((io.port, io.size, io.input), (0x3f8, 1, false));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod caps;
pub mod exit;
pub mod permission;
pub mod vmcb;
