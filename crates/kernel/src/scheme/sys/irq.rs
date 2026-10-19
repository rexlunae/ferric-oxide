use alloc::{string::String, vec::Vec};
use core::fmt::Write;

use crate::{sync::CleanLockToken, syscall::error::Result};

pub fn resource(_token: &mut CleanLockToken) -> Result<Vec<u8>> {
    let mut string = String::new();

    // Ferric Oxide: through `total_count`, which adds up the per-CPU counts with `irq-percpu`.
    for i in 0..224_u8 {
        let _ = writeln!(string, "{}: {}", i, crate::scheme::irq::total_count(i));
    }

    Ok(string.into_bytes())
}
