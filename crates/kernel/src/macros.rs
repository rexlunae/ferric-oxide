//! Console formatting and level-labelled kernel diagnostics.

/// Print to console
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ({
        use core::fmt::Write;
        let _ = write!($crate::log::Writer::new(), $($arg)*);
    });
}

/// Print with new line to console
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => ({
        use core::fmt::Write;
        let _ = writeln!($crate::log::Writer::new(), $($arg)*);
    });
}

/// Writes an error diagnostic with its originating module.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::println!("{}:ERROR -- {}", core::module_path!(), format_args!($($arg)*));
    };
}

/// Writes a warning diagnostic with its originating module.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::println!("{}:WARN -- {}", core::module_path!(), format_args!($($arg)*));
    };
}

/// Writes an informational diagnostic with its originating module.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::println!("{}:INFO -- {}", core::module_path!(), format_args!($($arg)*));
    };
}

/// Writes a debug diagnostic on architectures that enable debug output.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if cfg!(any(target_arch = "aarch64", target_arch = "riscv64")) {
            $crate::println!("{}:DEBUG -- {}", core::module_path!(), format_args!($($arg)*));
        }
    };
}

/// Defines a trace diagnostic whose output is currently disabled.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if false {
            $crate::println!("{}:TRACE -- {}", core::module_path!(), format_args!($($arg)*));
        }
    };
}
