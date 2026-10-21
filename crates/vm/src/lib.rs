//! The `kernel.vm` scheme ABI (roadmap Phase V, design in `virt/virt-design.md` in the project
//! files): the one definition shared by the kernel's hypervisor core and the user-space VMM.
//!
//! The enclave launcher opens the scheme and creates a VM; the VMM gets the VM fd, maps guest
//! RAM from regbuf regions into it, opens its vCPUs and runs them. Exits the kernel does not
//! handle itself come back through each vCPU's run page.
//!
//! - [`call`]: the paths and the `SYS_CALL` requests with their metadata encoding.
//! - [`run`]: the run page: exits for the VMM, and the VMM's completions, each checked by the
//!   side that reads them.
//! - [`regs`]: general and system register records, with segment attributes converted to the
//!   VMX and SVM formats in one place.
//! - [`cpuid`]: the CPUID table and the lookup the kernel answers CPUID exits from.
//! - [`msi`]: decoding the MSIs a VMM asks the kernel to deliver.
//!
//! The layouts are this project's own, not KVM's. Formats from the hardware follow the Intel
//! SDM (Vol. 3A for MSI and segment descriptors, Vol. 3C for VMX access rights) and the AMD64
//! APM Vol. 2 (VMCB segment attributes).
//!
//! # Examples
//!
//! ```
//! use ferric_vm::call::{Protection, Request, SetMemory};
//! use ferric_vm::run::{AREA_BYTES, Completion, Exit};
//!
//! // Map 256 MiB of guest RAM at guest-physical 0.
//! let map = Request::SetMemory(SetMemory {
//!     slot: 0,
//!     offset: 0,
//!     len: 256 << 20,
//!     gpa: 0,
//!     prot: Protection { write: true, execute: true },
//! });
//! let (words, len) = map.encode();
//! assert_eq!(Request::decode(&words[..len]), Ok(map));
//!
//! // The kernel reports an IN from the serial port's status register...
//! let mut area = [0u8; AREA_BYTES];
//! let exit = Exit::Io { port: 0x3fd, size: 1, input: true, data: 0 };
//! exit.write(&mut area);
//! assert_eq!(Exit::read(&area), Ok(exit));
//!
//! // ...and the VMM answers "transmitter empty"; the kernel checks it fits a byte.
//! Completion { data: 0x60, fault: false }.write(&mut area);
//! assert_eq!(Completion::read(&area, &exit).map(|c| c.data), Ok(0x60));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod call;
pub mod cpuid;
pub mod msi;
pub mod regs;
pub mod run;
