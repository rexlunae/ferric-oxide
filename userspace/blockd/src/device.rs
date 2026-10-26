//! Devices and image files as block devices: [`ferric_blocklayer::file`], which the file
//! system daemons share, under the name this crate has always used for it.

pub use ferric_blocklayer::file::{Aligned, FileDevice, ReadAt};
