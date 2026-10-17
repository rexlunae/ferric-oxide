//! AVX-512 register state test for the Ferric Oxide kernel.
//!
//! Two properties are checked:
//!
//! 1. **Preservation.** Worker threads load all 32 zmm registers and the opmask
//!    registers k1-k7 with a per-thread pattern, spin without making system
//!    calls so the scheduler has to preempt them, then check every register still
//!    holds its pattern. This exercises the XSAVE/XRSTOR path in `switch_to`.
//! 2. **No leaks.** "Writer" threads put a marker in zmm16-zmm31 and k1-k7 and
//!    then yield (a system call). "Probe" threads yield and then read those
//!    registers. Seeing a writer's marker means the kernel's syscall-return fast
//!    path let one context see another's registers.
//!
//! Prints one result line, `FERRIC_TEST_PASS xstate ...`, `FERRIC_TEST_SKIP
//! xstate ...` (no AVX-512, or the OS has not enabled its state) or
//! `FERRIC_TEST_FAIL xstate ...`, and exits non-zero on failure.

use std::arch::asm;
use std::arch::x86_64::{__cpuid_count, _xgetbv};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const ZMM_BYTES: usize = 64;
/// Opmask k1-k7 as u16 (AVX512F only guarantees 16 bits); slot 0 is unused.
const K_SLOTS: usize = 8;
const LEAK_MAGIC: u64 = 0xfe44_1c0a_1de5_0000;

#[repr(C, align(64))]
struct Zmm<const N: usize>([u8; N]);

#[allow(unused_unsafe)] // __cpuid_count became safe in newer Rust
fn cpu_has_avx512f() -> bool {
    // CPUID.(EAX=7,ECX=0):EBX bit 16
    let leaf7 = unsafe { __cpuid_count(7, 0) };
    leaf7.ebx & (1 << 16) != 0
}

#[allow(unused_unsafe)]
fn os_enabled_avx512() -> bool {
    // CPUID.1:ECX.OSXSAVE, then XCR0 bits 5-7 (opmask, ZMM_Hi256, Hi16_ZMM)
    let leaf1 = unsafe { __cpuid_count(1, 0) };
    if leaf1.ecx & (1 << 27) == 0 {
        return false;
    }
    let xcr0 = unsafe { _xgetbv(0) };
    xcr0 & 0xe6 == 0xe6
}

fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            // xorshift64
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Load every zmm and k1-k7, spin for `iters` iterations, store them back.
#[target_feature(enable = "avx512f")]
unsafe fn hold_registers(
    src: &Zmm<{32 * ZMM_BYTES}>,
    ksrc: &[u16; K_SLOTS],
    dst: &mut Zmm<{32 * ZMM_BYTES}>,
    kdst: &mut [u16; K_SLOTS],
    iters: u64,
) {
    unsafe {
        asm!(
            "vmovdqu64 zmm0, [{src} + 0]",
            "vmovdqu64 zmm1, [{src} + 64]",
            "vmovdqu64 zmm2, [{src} + 128]",
            "vmovdqu64 zmm3, [{src} + 192]",
            "vmovdqu64 zmm4, [{src} + 256]",
            "vmovdqu64 zmm5, [{src} + 320]",
            "vmovdqu64 zmm6, [{src} + 384]",
            "vmovdqu64 zmm7, [{src} + 448]",
            "vmovdqu64 zmm8, [{src} + 512]",
            "vmovdqu64 zmm9, [{src} + 576]",
            "vmovdqu64 zmm10, [{src} + 640]",
            "vmovdqu64 zmm11, [{src} + 704]",
            "vmovdqu64 zmm12, [{src} + 768]",
            "vmovdqu64 zmm13, [{src} + 832]",
            "vmovdqu64 zmm14, [{src} + 896]",
            "vmovdqu64 zmm15, [{src} + 960]",
            "vmovdqu64 zmm16, [{src} + 1024]",
            "vmovdqu64 zmm17, [{src} + 1088]",
            "vmovdqu64 zmm18, [{src} + 1152]",
            "vmovdqu64 zmm19, [{src} + 1216]",
            "vmovdqu64 zmm20, [{src} + 1280]",
            "vmovdqu64 zmm21, [{src} + 1344]",
            "vmovdqu64 zmm22, [{src} + 1408]",
            "vmovdqu64 zmm23, [{src} + 1472]",
            "vmovdqu64 zmm24, [{src} + 1536]",
            "vmovdqu64 zmm25, [{src} + 1600]",
            "vmovdqu64 zmm26, [{src} + 1664]",
            "vmovdqu64 zmm27, [{src} + 1728]",
            "vmovdqu64 zmm28, [{src} + 1792]",
            "vmovdqu64 zmm29, [{src} + 1856]",
            "vmovdqu64 zmm30, [{src} + 1920]",
            "vmovdqu64 zmm31, [{src} + 1984]",
            "kmovw k1, word ptr [{ksrc} + 2]",
            "kmovw k2, word ptr [{ksrc} + 4]",
            "kmovw k3, word ptr [{ksrc} + 6]",
            "kmovw k4, word ptr [{ksrc} + 8]",
            "kmovw k5, word ptr [{ksrc} + 10]",
            "kmovw k6, word ptr [{ksrc} + 12]",
            "kmovw k7, word ptr [{ksrc} + 14]",
            "2:",
            "dec {iters}",
            "jnz 2b",
            "vmovdqu64 [{dst} + 0], zmm0",
            "vmovdqu64 [{dst} + 64], zmm1",
            "vmovdqu64 [{dst} + 128], zmm2",
            "vmovdqu64 [{dst} + 192], zmm3",
            "vmovdqu64 [{dst} + 256], zmm4",
            "vmovdqu64 [{dst} + 320], zmm5",
            "vmovdqu64 [{dst} + 384], zmm6",
            "vmovdqu64 [{dst} + 448], zmm7",
            "vmovdqu64 [{dst} + 512], zmm8",
            "vmovdqu64 [{dst} + 576], zmm9",
            "vmovdqu64 [{dst} + 640], zmm10",
            "vmovdqu64 [{dst} + 704], zmm11",
            "vmovdqu64 [{dst} + 768], zmm12",
            "vmovdqu64 [{dst} + 832], zmm13",
            "vmovdqu64 [{dst} + 896], zmm14",
            "vmovdqu64 [{dst} + 960], zmm15",
            "vmovdqu64 [{dst} + 1024], zmm16",
            "vmovdqu64 [{dst} + 1088], zmm17",
            "vmovdqu64 [{dst} + 1152], zmm18",
            "vmovdqu64 [{dst} + 1216], zmm19",
            "vmovdqu64 [{dst} + 1280], zmm20",
            "vmovdqu64 [{dst} + 1344], zmm21",
            "vmovdqu64 [{dst} + 1408], zmm22",
            "vmovdqu64 [{dst} + 1472], zmm23",
            "vmovdqu64 [{dst} + 1536], zmm24",
            "vmovdqu64 [{dst} + 1600], zmm25",
            "vmovdqu64 [{dst} + 1664], zmm26",
            "vmovdqu64 [{dst} + 1728], zmm27",
            "vmovdqu64 [{dst} + 1792], zmm28",
            "vmovdqu64 [{dst} + 1856], zmm29",
            "vmovdqu64 [{dst} + 1920], zmm30",
            "vmovdqu64 [{dst} + 1984], zmm31",
            "kmovw word ptr [{kdst} + 2], k1",
            "kmovw word ptr [{kdst} + 4], k2",
            "kmovw word ptr [{kdst} + 6], k3",
            "kmovw word ptr [{kdst} + 8], k4",
            "kmovw word ptr [{kdst} + 10], k5",
            "kmovw word ptr [{kdst} + 12], k6",
            "kmovw word ptr [{kdst} + 14], k7",
            src = in(reg) src.0.as_ptr(),
            ksrc = in(reg) ksrc.as_ptr(),
            dst = in(reg) dst.0.as_mut_ptr(),
            kdst = in(reg) kdst.as_mut_ptr(),
            iters = inout(reg) iters => _,
            out("zmm0") _,
            out("zmm1") _,
            out("zmm2") _,
            out("zmm3") _,
            out("zmm4") _,
            out("zmm5") _,
            out("zmm6") _,
            out("zmm7") _,
            out("zmm8") _,
            out("zmm9") _,
            out("zmm10") _,
            out("zmm11") _,
            out("zmm12") _,
            out("zmm13") _,
            out("zmm14") _,
            out("zmm15") _,
            out("zmm16") _,
            out("zmm17") _,
            out("zmm18") _,
            out("zmm19") _,
            out("zmm20") _,
            out("zmm21") _,
            out("zmm22") _,
            out("zmm23") _,
            out("zmm24") _,
            out("zmm25") _,
            out("zmm26") _,
            out("zmm27") _,
            out("zmm28") _,
            out("zmm29") _,
            out("zmm30") _,
            out("zmm31") _,
            out("k1") _,
            out("k2") _,
            out("k3") _,
            out("k4") _,
            out("k5") _,
            out("k6") _,
            out("k7") _,
            options(nostack),
        );
    }
}

/// Put the leak marker in zmm16-31 and k1-k7.
#[target_feature(enable = "avx512f")]
unsafe fn plant_marker(src: &Zmm<{16 * ZMM_BYTES}>, ksrc: &[u16; K_SLOTS]) {
    unsafe {
        asm!(
            "vmovdqu64 zmm16, [{src} + 0]",
            "vmovdqu64 zmm17, [{src} + 64]",
            "vmovdqu64 zmm18, [{src} + 128]",
            "vmovdqu64 zmm19, [{src} + 192]",
            "vmovdqu64 zmm20, [{src} + 256]",
            "vmovdqu64 zmm21, [{src} + 320]",
            "vmovdqu64 zmm22, [{src} + 384]",
            "vmovdqu64 zmm23, [{src} + 448]",
            "vmovdqu64 zmm24, [{src} + 512]",
            "vmovdqu64 zmm25, [{src} + 576]",
            "vmovdqu64 zmm26, [{src} + 640]",
            "vmovdqu64 zmm27, [{src} + 704]",
            "vmovdqu64 zmm28, [{src} + 768]",
            "vmovdqu64 zmm29, [{src} + 832]",
            "vmovdqu64 zmm30, [{src} + 896]",
            "vmovdqu64 zmm31, [{src} + 960]",
            "kmovw k1, word ptr [{ksrc} + 2]",
            "kmovw k2, word ptr [{ksrc} + 4]",
            "kmovw k3, word ptr [{ksrc} + 6]",
            "kmovw k4, word ptr [{ksrc} + 8]",
            "kmovw k5, word ptr [{ksrc} + 10]",
            "kmovw k6, word ptr [{ksrc} + 12]",
            "kmovw k7, word ptr [{ksrc} + 14]",
            src = in(reg) src.0.as_ptr(),
            ksrc = in(reg) ksrc.as_ptr(),
            out("zmm16") _,
            out("zmm17") _,
            out("zmm18") _,
            out("zmm19") _,
            out("zmm20") _,
            out("zmm21") _,
            out("zmm22") _,
            out("zmm23") _,
            out("zmm24") _,
            out("zmm25") _,
            out("zmm26") _,
            out("zmm27") _,
            out("zmm28") _,
            out("zmm29") _,
            out("zmm30") _,
            out("zmm31") _,
            out("k1") _,
            out("k2") _,
            out("k3") _,
            out("k4") _,
            out("k5") _,
            out("k6") _,
            out("k7") _,
            options(nostack, readonly),
        );
    }
}

/// Read zmm16-31 and k1-k7 as they are right now.
#[target_feature(enable = "avx512f")]
unsafe fn read_upper(dst: &mut Zmm<{16 * ZMM_BYTES}>, kdst: &mut [u16; K_SLOTS]) {
    unsafe {
        asm!(
            "vmovdqu64 [{dst} + 0], zmm16",
            "vmovdqu64 [{dst} + 64], zmm17",
            "vmovdqu64 [{dst} + 128], zmm18",
            "vmovdqu64 [{dst} + 192], zmm19",
            "vmovdqu64 [{dst} + 256], zmm20",
            "vmovdqu64 [{dst} + 320], zmm21",
            "vmovdqu64 [{dst} + 384], zmm22",
            "vmovdqu64 [{dst} + 448], zmm23",
            "vmovdqu64 [{dst} + 512], zmm24",
            "vmovdqu64 [{dst} + 576], zmm25",
            "vmovdqu64 [{dst} + 640], zmm26",
            "vmovdqu64 [{dst} + 704], zmm27",
            "vmovdqu64 [{dst} + 768], zmm28",
            "vmovdqu64 [{dst} + 832], zmm29",
            "vmovdqu64 [{dst} + 896], zmm30",
            "vmovdqu64 [{dst} + 960], zmm31",
            "kmovw word ptr [{kdst} + 2], k1",
            "kmovw word ptr [{kdst} + 4], k2",
            "kmovw word ptr [{kdst} + 6], k3",
            "kmovw word ptr [{kdst} + 8], k4",
            "kmovw word ptr [{kdst} + 10], k5",
            "kmovw word ptr [{kdst} + 12], k6",
            "kmovw word ptr [{kdst} + 14], k7",
            dst = in(reg) dst.0.as_mut_ptr(),
            kdst = in(reg) kdst.as_mut_ptr(),
            options(nostack),
        );
    }
}

fn preservation_test(threads: usize, rounds: u64, iters: u64) -> Result<u64, String> {
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            thread::spawn(move || -> Result<(), String> {
                for round in 0..rounds {
                    let seed = (t as u64 + 1) * 0x9e37_79b9_7f4a_7c15 ^ round;
                    let mut src = Zmm([0u8; 32 * ZMM_BYTES]);
                    src.0.copy_from_slice(&pattern(seed, 32 * ZMM_BYTES));
                    let kp = pattern(seed ^ 0x55, 2 * K_SLOTS);
                    let mut ksrc = [0u16; K_SLOTS];
                    for i in 1..K_SLOTS {
                        ksrc[i] = u16::from_le_bytes([kp[2 * i], kp[2 * i + 1]]);
                    }
                    let mut dst = Zmm([0u8; 32 * ZMM_BYTES]);
                    let mut kdst = [0u16; K_SLOTS];
                    unsafe { hold_registers(&src, &ksrc, &mut dst, &mut kdst, iters) };
                    if let Some(i) = (0..32 * ZMM_BYTES).find(|&i| src.0[i] != dst.0[i]) {
                        return Err(format!(
                            "thread {t} round {round}: zmm{} byte {} changed",
                            i / ZMM_BYTES,
                            i % ZMM_BYTES
                        ));
                    }
                    if let Some(i) = (1..K_SLOTS).find(|&i| ksrc[i] != kdst[i]) {
                        return Err(format!("thread {t} round {round}: k{i} changed"));
                    }
                }
                Ok(())
            })
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| "worker panicked".to_string())??;
    }
    Ok(threads as u64 * rounds)
}

fn leak_test(writers: usize, probes: usize, duration: Duration) -> Result<(u64, u64), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let leaks = Arc::new(AtomicU64::new(0));
    let samples = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    for w in 0..writers {
        let stop = stop.clone();
        handles.push(thread::spawn(move || {
            let mut src = Zmm([0u8; 16 * ZMM_BYTES]);
            for (i, chunk) in src.0.chunks_mut(8).enumerate() {
                chunk.copy_from_slice(&(LEAK_MAGIC | (w as u64) << 8 | (i as u64 & 0xff)).to_le_bytes());
            }
            let ksrc = [0xfe44u16; K_SLOTS];
            while !stop.load(Ordering::Relaxed) {
                unsafe { plant_marker(&src, &ksrc) };
                thread::yield_now();
            }
        }));
    }
    for _ in 0..probes {
        let stop = stop.clone();
        let leaks = leaks.clone();
        let samples = samples.clone();
        handles.push(thread::spawn(move || {
            let mut dst = Zmm([0u8; 16 * ZMM_BYTES]);
            let mut kdst = [0u16; K_SLOTS];
            while !stop.load(Ordering::Relaxed) {
                thread::yield_now();
                unsafe { read_upper(&mut dst, &mut kdst) };
                let zmm_leak = dst
                    .0
                    .chunks(8)
                    .any(|c| u64::from_le_bytes(c.try_into().unwrap()) & !0xffff == LEAK_MAGIC);
                let k_leak = kdst[1..].iter().any(|&k| k == 0xfe44);
                if zmm_leak || k_leak {
                    leaks.fetch_add(1, Ordering::Relaxed);
                }
                samples.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    thread::sleep(duration);
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().map_err(|_| "leak thread panicked".to_string())?;
    }
    Ok((samples.load(Ordering::Relaxed), leaks.load(Ordering::Relaxed)))
}

fn main() {
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let cpu = cpu_has_avx512f();
    let os = os_enabled_avx512();
    if !cpu || !os {
        println!("FERRIC_TEST_SKIP xstate avx512 cpu={} os={}", cpu as u8, os as u8);
        return;
    }

    let start = Instant::now();
    let threads = (cpus * 3).max(4);
    let rounds = match preservation_test(threads, 20, 100_000_000) {
        Ok(n) => n,
        Err(e) => {
            println!("FERRIC_TEST_FAIL xstate preservation: {e}");
            std::process::exit(1);
        }
    };
    let (samples, leaks) = match leak_test(cpus.max(2), cpus.max(2), Duration::from_secs(5)) {
        Ok(r) => r,
        Err(e) => {
            println!("FERRIC_TEST_FAIL xstate leak test: {e}");
            std::process::exit(1);
        }
    };
    if leaks != 0 {
        println!("FERRIC_TEST_FAIL xstate leak: {leaks} of {samples} probes saw another context's AVX-512 registers");
        std::process::exit(1);
    }
    println!(
        "FERRIC_TEST_PASS xstate avx512 cpus={cpus} threads={threads} rounds={rounds} probes={samples} time={:.1}s",
        start.elapsed().as_secs_f64()
    );
}
