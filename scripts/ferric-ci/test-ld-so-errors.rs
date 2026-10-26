//! Host harness for the actual loader search, fatal formatter and relibc Errno.
//! Only platform errno storage, its message table and io::Error are host adapters.
#![forbid(unsafe_code)]
#![allow(dead_code)]
extern crate alloc;
mod platform {
    pub mod types {
        #[allow(non_camel_case_types)]
        pub type c_int = i32;
    }
    std::thread_local! { pub static ERRNO: core::cell::Cell<i32> = const { core::cell::Cell::new(0) }; }
}
mod header {
    pub mod errno {
        pub const ENOENT: i32 = 2;
        pub const STR_ERROR: [&str; 1] = ["Success"];
    }
}
mod io {
    pub use std::io::Error;
}
#[path = "../../crates/relibc/src/error.rs"]
mod error;
#[path = "../../crates/relibc/src/ld_so/load_error.rs"]
mod load_error;

#[path = "../../crates/relibc/src/ld_so/object_search.rs"]
mod object_search;
