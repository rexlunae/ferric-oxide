//! Records the target triple `fox-registry` is built for, which is the
//! target `build` builds for and checks artifacts of when no `--target` is
//! given.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=FOX_REGISTRY_HOST_TARGET={target}");
}
