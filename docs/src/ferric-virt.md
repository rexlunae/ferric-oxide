# Virtualization (prepared)

Roadmap Phase V adds a hypervisor core to the kernel and a user-space VMM, so
that programs built for other systems run in lightweight VMs (enclaves). This
chapter describes what exists so far: three host-tested crates that hold the
hardware formats and the `kernel.vm` scheme ABI. **No kernel backend, scheme,
VMM or image integration exists yet.** The kernel does not execute VMX or SVM
instructions, `/scheme/kernel.vm` does not exist, and there is no
`features.toml` option until the kernel core lands.

All three crates are `no_std`, `forbid(unsafe_code)` and `deny(missing_docs)`,
and are written from the vendors' manuals only (no hypervisor source was used).

## `ferric-vmx` (`crates/vmx`)

Intel VMX as plain data, from the *Intel SDM* Volume 3C (the VMX chapters and
appendices A–C):

- `field`: VMCS field encodings, checked at compile time against their width.
- `controls`: control bits, and `Allowed::resolve`, which combines the controls
  the hypervisor needs with what the capability MSRs allow.
- `caps`: the capability MSRs (with the TRUE-control rule), CR0/CR4 fixed bits,
  EPT/VPID capabilities and IA32_FEATURE_CONTROL. `Capabilities::sufficient`
  requires EPT with 4-level write-back tables, unrestricted guest, MSR bitmaps,
  VPID, and a single-context or all-context type of both INVEPT and INVVPID.
- `exit`, `ept`, `msr_bitmap` (deny by default) and `check` (guest-state checks
  that name the reason for an entry failure).

## `ferric-svm` (`crates/svm`)

AMD SVM as plain data, from the *AMD64 APM* Volume 2, chapter 15 and
appendices B and C: the VMCB as typed fields with automatic clean bits, exit
codes and EXITINFO decodings, the MSR and I/O permission maps (deny by
default), and the CPUID, VM_CR and VMRUN consistency checks. Nested page tables
use the ordinary x86-64 page-table format.

## `ferric-vm` (`crates/vm`)

The one definition of the `kernel.vm` ABI, to be shared by the kernel and the
VMM: the scheme's paths and `SYS_CALL` requests (`call`), the per-vCPU run page
with exits for the VMM and the VMM's completions (`run`), register records
(`regs`), the CPUID table the kernel will answer CPUID exits from (`cpuid`),
and MSI decoding, which keeps the level bit so a level-triggered INIT
de-assert is not mistaken for an INIT (`msi`). The layouts are this project's
own, not KVM's.

## Validation

`cargo test -p ferric-vmx -p ferric-svm -p ferric-vm` on the host, including
capability fixtures for CPUs that lack required features. Nothing runs on the
target yet.
