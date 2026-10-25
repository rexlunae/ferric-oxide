#![no_std]
#![no_main]
#![feature(abort_immediate, never_type)]

#[cfg(target_arch = "aarch64")]
#[path = "aarch64.rs"]
pub mod arch;

#[cfg(target_arch = "x86")]
#[path = "i686.rs"]
pub mod arch;

#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
pub mod arch;

#[cfg(target_arch = "riscv64")]
#[path = "riscv64.rs"]
pub mod arch;

pub mod exec;
pub mod initfs;
// Ferric Oxide: the initfs trace configuration (lib/trace.toml).
pub mod initfs_trace;
pub mod initnsmgr;
pub mod procmgr;
// Ferric Oxide: procmgr's process lifecycle tracepoints.
pub mod proc_trace;
pub mod start;

extern crate alloc;

use alloc::collections::btree_map::BTreeMap;
use core::cell::UnsafeCell;

use redox_rt::proc::FdGuard;
use syscall::data::{GlobalSchemes, KernelSchemeInfo, Map};
use syscall::flag::MapFlags;

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;

    struct Writer;

    impl Write for Writer {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            syscall::write(1, s.as_bytes())
                .map_err(|_| core::fmt::Error)
                .map(|_| ())
        }
    }

    let _ = writeln!(&mut Writer, "{}", info);
    core::process::abort_immediate();
}

const HEAP_OFF: usize = arch::USERMODE_END / 2;

struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

struct AllocStateInner {
    heap: Option<linked_list_allocator::Heap>,
    heap_top: usize,
}
struct AllocState(UnsafeCell<AllocStateInner>);
unsafe impl Send for AllocState {}
unsafe impl Sync for AllocState {}
static ALLOC_STATE: AllocState = AllocState(UnsafeCell::new(AllocStateInner {
    heap: None,
    heap_top: HEAP_OFF + SIZE,
}));

const SIZE: usize = 1024 * 1024;
const HEAP_INCREASE_BY: usize = SIZE;

unsafe impl alloc::alloc::GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let state = unsafe { &mut (*ALLOC_STATE.0.get()) };
        let heap = state.heap.get_or_insert_with(|| {
            state.heap_top = HEAP_OFF + SIZE;
            let _ = unsafe {
                syscall::fmap(
                    !0,
                    &Map {
                        offset: 0,
                        size: SIZE,
                        address: HEAP_OFF,
                        flags: MapFlags::PROT_WRITE
                            | MapFlags::PROT_READ
                            | MapFlags::MAP_PRIVATE
                            | MapFlags::MAP_FIXED_NOREPLACE,
                    },
                )
            }
            .expect("failed to map initial heap");
            unsafe { linked_list_allocator::Heap::new(HEAP_OFF as *mut u8, SIZE) }
        });

        match heap.allocate_first_fit(layout) {
            Ok(p) => p.as_ptr(),
            Err(_) => {
                if layout.size() > HEAP_INCREASE_BY || layout.align() > 4096 {
                    return core::ptr::null_mut();
                }

                let _ = unsafe {
                    syscall::fmap(
                        !0,
                        &Map {
                            offset: 0,
                            size: HEAP_INCREASE_BY,
                            address: state.heap_top,
                            flags: MapFlags::PROT_WRITE
                                | MapFlags::PROT_READ
                                | MapFlags::MAP_PRIVATE
                                | MapFlags::MAP_FIXED_NOREPLACE,
                        },
                    )
                }
                .expect("failed to extend heap");
                unsafe { heap.extend(HEAP_INCREASE_BY) };
                state.heap_top += HEAP_INCREASE_BY;

                unsafe { self.alloc(layout) }
            }
        }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        unsafe {
            (&mut *ALLOC_STATE.0.get())
                .heap
                .as_mut()
                .unwrap()
                .deallocate(core::ptr::NonNull::new(ptr).unwrap(), layout)
        }
    }
}

/// The kernel schemes by number, and (Ferric Oxide) the kernel schemes this crate's
/// `redox_syscall` has no `GlobalSchemes` variant for, by name.
pub struct KernelSchemeMap(
    BTreeMap<GlobalSchemes, FdGuard>,
    alloc::vec::Vec<(&'static str, FdGuard)>,
);
impl KernelSchemeMap {
    fn new(kernel_scheme_infos: &[KernelSchemeInfo]) -> Self {
        let mut map = BTreeMap::new();
        let mut ferric = alloc::vec::Vec::new();
        for info in kernel_scheme_infos {
            if let Some(scheme_id) = GlobalSchemes::try_from_raw(info.scheme_id) {
                map.insert(scheme_id, FdGuard::new(info.fd));
            } else if info.scheme_id == ferric_iommu::abi::SCHEME_ID {
                // Ferric Oxide: the kernel's iommu scheme (kernel.iommu = "auto"/"enforce").
                ferric.push((ferric_iommu::abi::SCHEME_NAME, FdGuard::new(info.fd)));
            } else if info.scheme_id == ferric_regbuf::abi::SCHEME_ID {
                // Ferric Oxide: the kernel's regbuf scheme (kernel.regbuf).
                ferric.push((ferric_regbuf::abi::SCHEME_NAME, FdGuard::new(info.fd)));
            } else if info.scheme_id == ferric_trace::abi::SCHEME_ID {
                // Ferric Oxide: the kernel's trace scheme (kernel.trace = true).
                ferric.push((ferric_trace::abi::SCHEME_NAME, FdGuard::new(info.fd)));
            } else if info.scheme_id == ferric_ring::abi::SCHEME_ID {
                // Ferric Oxide: the kernel's ring scheme (kernel.rings = true).
                ferric.push((ferric_ring::abi::SCHEME_NAME, FdGuard::new(info.fd)));
            } else if info.scheme_id == ferric_pcache::abi::SCHEME_ID {
                // Ferric Oxide: the kernel's page cache scheme (kernel.page_cache = true).
                ferric.push((ferric_pcache::abi::SCHEME_NAME, FdGuard::new(info.fd)));
            }
        }
        Self(map, ferric)
    }
    fn get(&self, scheme: GlobalSchemes) -> Option<&FdGuard> {
        self.0.get(&scheme)
    }
}
