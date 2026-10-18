//! Image configuration and filesystem installation shared by host and Redox builds.
//!
//! Configuration parsing is always available; the `installer` feature enables
//! disk creation and package installation, and `fuse` enables mounted writes.

#![deny(missing_docs, unsafe_op_in_unsafe_fn)]

#[macro_use]
extern crate serde_derive;

mod image_path;
pub use image_path::{ImagePathError, image_path, image_relative_path};

mod config;
mod fox;
pub use fox::{FoxError, FoxImage, FoxStep};
#[cfg(feature = "installer")]
mod disk_wrapper;
#[cfg(feature = "installer")]
mod installer;
#[cfg(feature = "installer")]
pub use crate::installer::*;

pub use crate::config::Config;
pub use crate::config::file::FileConfig;
pub use crate::config::file::format_bytes;
pub use crate::config::package::PackageConfig;
