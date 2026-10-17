//! Image configuration and filesystem installation shared by host and Redox builds.
//!
//! Configuration parsing is always available; the `installer` feature enables
//! disk creation and package installation, and `fuse` enables mounted writes.

#[macro_use]
extern crate serde_derive;

mod config;
#[cfg(feature = "installer")]
mod disk_wrapper;
#[cfg(feature = "installer")]
mod installer;
#[cfg(feature = "installer")]
pub use crate::installer::*;

pub use crate::config::file::format_bytes;
pub use crate::config::file::FileConfig;
pub use crate::config::package::PackageConfig;
pub use crate::config::Config;
