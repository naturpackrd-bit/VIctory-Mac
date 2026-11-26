//! What the loader's tests share: the stand-in title (tests/fixtures/
//! pe-probe), built once, and a resolver for the imports it makes.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use victory_loader::{ImportResolver, MappedImage, Provenance, Resolution};
use victory_pe::ImportSymbol;

/// The fixture's name, as the loader's reports call it.
pub const PROBE: &str = "pe_probe";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// The fixture's DLL, built once: for `x86_64-pc-windows-msvc`, linked by
/// the rust-lld that comes with Rust (no Windows SDK), into the outer
/// build's scratch folder for tests.
pub fn probe() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let manifest = workspace_root().join("tests/fixtures/pe-probe/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pe-probe");
        let status = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", "x86_64-pc-windows-msvc", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            // Flags of the outer build would replace the fixture's own.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", "rust-lld")
            .env(
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
                "-C linker-flavor=lld-link -C link-arg=/NODEFAULTLIB",
            )
            .status()
            .expect("running cargo");
        assert!(status.success(), "building pe-probe failed");
        let dll = target.join("x86_64-pc-windows-msvc/release/pe_probe.dll");
        std::fs::read(&dll).unwrap_or_else(|e| panic!("{}: {e}", dll.display()))
    })
}

/// The fixture, mapped with [`Probe`] resolving its imports.
pub fn map_probe() -> MappedImage {
    MappedImage::map(probe(), PROBE, &Probe).expect("mapping the probe")
}

pub const VALUE: u32 = 0x5EED;

pub extern "win64" fn probe_value() -> u32 {
    VALUE
}

/// Each argument at its own decimal place, so that any argument in the
/// wrong place (register or stack slot) shows.
pub extern "win64" fn probe_sum6(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64 {
    a + 10 * b + 100 * c + 1000 * d + 10_000 * e + 100_000 * f
}

pub extern "win64" fn twice_plus_one(x: u64) -> u64 {
    2 * x + 1
}

/// `victory-probe.dll`, as the fixture imports it: what it resolves is in
/// `resolve`; the rest is missing.
pub struct Probe;

impl ImportResolver for Probe {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let found = |f: usize| Resolution::Found {
            address: f,
            provenance: Provenance::Hle,
        };
        match symbol {
            ImportSymbol::Name { name, .. } if dll.eq_ignore_ascii_case("victory-probe.dll") => {
                match name.as_str() {
                    "ProbeValue" => found(probe_value as *const () as usize),
                    "ProbeSum6" => found(probe_sum6 as *const () as usize),
                    _ => Resolution::Missing,
                }
            }
            _ => Resolution::Missing,
        }
    }
}

/// Nobody: every import is missing.
pub struct Nobody;

impl ImportResolver for Nobody {
    fn resolve(&self, _: &str, _: &ImportSymbol) -> Resolution {
        Resolution::Missing
    }
}

/// The export `name` of `image` as a function of type `F`.
///
/// # Safety
/// `F` is the export's type.
pub unsafe fn export<F: Copy>(image: &MappedImage, name: &str) -> F {
    let address = image
        .export(name)
        .unwrap_or_else(|| panic!("no export {name}"));
    assert_eq!(size_of::<F>(), size_of::<usize>());
    // SAFETY: the caller's type, of the same size as an address.
    unsafe { std::mem::transmute_copy(&address) }
}
