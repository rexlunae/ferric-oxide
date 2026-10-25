use crate::{sync::CleanLockToken, syscall::error::Result};
use alloc::vec::Vec;

pub fn resource(_token: &mut CleanLockToken) -> Result<Vec<u8>> {
    Ok(alloc::format!(
        "Redox\n{}\n{}\n{}\n",
        env!("CARGO_PKG_VERSION"),
        env!("TARGET").split('-').next().unwrap_or("unknown"),
        option_env!("COOKBOOK_SOURCE_IDENT").unwrap_or("")
    )
    .into_bytes())
}
