//! Records the target triple `fox` is built for, which is the default
//! target of the prebuilt registry artifacts it installs.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=FOX_HOST_TARGET={target}");
}
