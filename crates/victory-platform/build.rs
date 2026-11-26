//! The crate's tests see the low 4 GiB as the x86-64 macOS runtime does
//! (docs/macos.md): the default `__PAGEZERO` segment takes all of it, so the
//! test binaries get one of one page, as apps/victory's build script gives
//! the runtime.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os == "macos" && arch == "x86_64" {
        println!("cargo:rustc-link-arg-tests=-Wl,-pagezero_size,0x1000");
    }
}
