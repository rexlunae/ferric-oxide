//! Relocates an ELF image file by a slide, the way the bootloader does in
//! memory: `relocate IN SLIDE OUT`, with SLIDE in hex (`0x...`) or decimal.
//!
//! `scripts/ferric-ci/check-kernel-relocs.sh` uses it to check that a kernel
//! relocated this way matches the same kernel linked at the moved address.

use std::process::ExitCode;

fn parse(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, slide, output] = args.as_slice() else {
        eprintln!("usage: relocate IN SLIDE OUT");
        return ExitCode::from(2);
    };
    let Some(slide) = parse(slide) else {
        eprintln!("relocate: bad slide {slide:?}");
        return ExitCode::from(2);
    };
    let mut image = match std::fs::read(input) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("relocate: {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ferric_elfreloc::relocate(&mut image, slide) {
        Ok(stats) => println!(
            "relocate: {} relocations, {} fields changed",
            stats.relocations, stats.patched
        ),
        Err(e) => {
            eprintln!("relocate: {input}: {e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = std::fs::write(output, &image) {
        eprintln!("relocate: {output}: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
