//! The x86-64 macOS runtime keeps the low 4 GiB for titles (docs/macos.md):
//! the default `__PAGEZERO` segment takes all of it, so it is made one page.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os == "macos" && arch == "x86_64" {
        println!("cargo:rustc-link-arg-bins=-Wl,-pagezero_size,0x1000");
    }
}
