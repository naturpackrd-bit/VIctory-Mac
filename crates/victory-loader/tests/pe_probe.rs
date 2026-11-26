//! End-to-end on any host: the stand-in title of tests/fixtures/pe-probe,
//! mapped by our own loader ([`MappedImage`]), not the host's, and called
//! into (docs/macos.md, M2a). No game data.
#![cfg(target_arch = "x86_64")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use victory_loader::{ImportResolver, MapError, MappedImage, Provenance, Resolution};
use victory_pe::ImportSymbol;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// The fixture's DLL, built once: for `x86_64-pc-windows-msvc`, linked by
/// the rust-lld that comes with Rust (no Windows SDK), into the outer
/// build's scratch folder for tests.
fn probe() -> &'static [u8] {
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

const VALUE: u32 = 0x5EED;

extern "win64" fn probe_value() -> u32 {
    VALUE
}

/// Each argument at its own decimal place, so that any argument in the
/// wrong place (register or stack slot) shows.
extern "win64" fn probe_sum6(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64 {
    a + 10 * b + 100 * c + 1000 * d + 10_000 * e + 100_000 * f
}

extern "win64" fn twice_plus_one(x: u64) -> u64 {
    2 * x + 1
}

/// `victory-probe.dll`, as the fixture imports it.
struct Probe;

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
struct Nobody;

impl ImportResolver for Nobody {
    fn resolve(&self, _: &str, _: &ImportSymbol) -> Resolution {
        Resolution::Missing
    }
}

/// The export `name` of `image` as a function of type `F`.
///
/// # Safety
/// `F` is the export's type.
unsafe fn export<F: Copy>(image: &MappedImage, name: &str) -> F {
    let address = image
        .export(name)
        .unwrap_or_else(|| panic!("no export {name}"));
    assert_eq!(size_of::<F>(), size_of::<usize>());
    // SAFETY: the caller's type, of the same size as an address.
    unsafe { std::mem::transmute_copy(&address) }
}

#[test]
fn title_code_runs_and_calls_back_both_ways() {
    let image = MappedImage::map(probe(), &Probe).expect("mapping the probe");
    // SAFETY: the fixture's own signatures (tests/fixtures/pe-probe).
    unsafe {
        let add: extern "win64" fn(u64, u64) -> u64 = export(&image, "probe_add");
        assert_eq!(add(40, 2), 42);
        let import: extern "win64" fn() -> u32 = export(&image, "probe_import");
        assert_eq!(import(), VALUE, "an import bound by the loader");
        let sum6: extern "win64" fn(u64, u64, u64, u64, u64, u64) -> u64 =
            export(&image, "probe_sum6");
        // The fixture passes them on reversed: f, e, d, c, b, a.
        assert_eq!(
            sum6(1, 2, 3, 4, 5, 6),
            123_456,
            "six arguments, two on the stack"
        );
        let callback: extern "win64" fn(extern "win64" fn(u64) -> u64, u64) -> u64 =
            export(&image, "probe_callback");
        assert_eq!(callback(twice_plus_one, 20), 82, "title code calling ours");
    }
}

/// Two copies of the probe cannot both have its preferred base: at least one
/// is moved, and each reads its own data through its relocated pointer.
#[test]
fn a_moved_image_uses_its_relocated_addresses() {
    let first = MappedImage::map(probe(), &Probe).expect("mapping the probe");
    let second = MappedImage::map(probe(), &Probe).expect("mapping it again");
    assert_ne!(first.base(), second.base());
    let preferred = first.info().image_base as usize;
    assert!(
        first.base() != preferred || second.base() != preferred,
        "one of them is moved"
    );
    // SAFETY: as above.
    unsafe {
        let one: extern "win64" fn() -> u32 = export(&first, "probe_reloc");
        let two: extern "win64" fn() -> u32 = export(&second, "probe_reloc");
        assert_eq!(one(), 0x0D15_EA5E);
        assert_eq!(one(), 0x0D15_EA5F, "the first one's value went up");
        assert_eq!(two(), 0x0D15_EA5E, "the second one has a value of its own");
    }
}

#[test]
fn unresolved_imports_are_named_and_refused() {
    match MappedImage::map(probe(), &Nobody) {
        Err(MapError::Unresolved(missing)) => {
            assert!(
                missing.contains(&"victory-probe.dll!ProbeValue".to_owned()),
                "{missing:?}"
            );
            assert!(
                missing.contains(&"victory-probe.dll!ProbeSum6".to_owned()),
                "{missing:?}"
            );
        }
        other => panic!("expected the unresolved imports, got {other:?}"),
    }
}
