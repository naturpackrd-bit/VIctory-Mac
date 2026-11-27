//! The stand-in title's TLS callbacks and entry point as the owner of a
//! mapping runs them ([`MappedImage::notify`]), its unresolved imports bound
//! to stubs, and its delay imports bound ahead (docs/macos.md, M2a). No
//! game data.
#![cfg(target_arch = "x86_64")]

mod common;

use common::*;
use victory_loader::stubs::UnimplementedPolicy;
use victory_loader::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH, DLL_THREAD_ATTACH, MapError, MappedImage,
};

/// Off Windows, the TEB title code reaches through `gs`.
fn attach() {
    #[cfg(not(windows))]
    victory_loader::teb::attach_current_thread();
}

type Probe0<T> = extern "win64" fn() -> T;

#[test]
fn attaching_runs_tls_callbacks_then_dllmain() {
    let image = map_probe();
    attach();
    // SAFETY: the fixture's own signatures (tests/fixtures/pe-probe).
    let (tls_reasons, dllmain_reasons, dllmain_base): (Probe0<u64>, Probe0<u64>, Probe0<u64>) = unsafe {
        (
            export(&image, "probe_tls_reasons"),
            export(&image, "probe_dllmain_reasons"),
            export(&image, "probe_dllmain_base"),
        )
    };
    image.notify(DLL_PROCESS_ATTACH).unwrap();
    assert_eq!(tls_reasons(), 1 << 1, "the TLS callback, attaching");
    assert_eq!(dllmain_reasons(), 1 << 1, "DllMain, attaching");
    assert_eq!(dllmain_base(), image.base() as u64, "DllMain's base");
    image.notify(DLL_THREAD_ATTACH).unwrap();
    assert_eq!(tls_reasons(), 1 << 1 | 1 << 2, "a thread attaching");
    assert_eq!(dllmain_reasons(), 1 << 1 | 1 << 2);
    image.notify(DLL_PROCESS_DETACH).unwrap();
    assert_eq!(tls_reasons(), 1 | 1 << 1 | 1 << 2, "detaching");
    assert_eq!(dllmain_reasons(), 1 | 1 << 1 | 1 << 2);
    assert_eq!(image.entry_point(), image.export("_DllMainCRTStartup"));
    assert_eq!(image.tls_callbacks().len(), 1);
}

#[test]
fn a_missing_import_is_stubbed_counted_and_returns_zero() {
    let image = MappedImage::map_with(probe(), PROBE, &Probe, UnimplementedPolicy::Zero)
        .expect("mapping the probe");
    attach();
    // SAFETY: the fixture's signature.
    let missing: Probe0<u32> = unsafe { export(&image, "probe_missing") };
    assert_eq!(missing(), 0);
    assert_eq!(missing(), 0);
    assert_eq!(
        image.unimplemented(),
        vec![("victory-probe.dll", "ProbeMissing", 2)]
    );
    assert!(victory_loader::stubs::unimplemented_calls() >= 2);
}

#[test]
fn refusing_names_the_missing_import() {
    match MappedImage::map_with(probe(), PROBE, &Probe, UnimplementedPolicy::Refuse) {
        Err(MapError::Unresolved(missing)) => assert_eq!(
            missing,
            vec!["victory-probe.dll!ProbeMissing".to_owned()],
            "only the import Probe leaves missing"
        ),
        other => panic!("expected the unresolved import, got {other:?}"),
    }
}

#[test]
fn a_delay_import_is_bound_ahead() {
    let image = map_probe();
    attach();
    // SAFETY: the fixture's signature.
    let delayed: Probe0<u32> = unsafe { export(&image, "probe_delayed") };
    assert_eq!(delayed(), 4242);
}
