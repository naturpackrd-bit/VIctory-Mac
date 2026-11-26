//! End-to-end on any host: the stand-in title of tests/fixtures/pe-probe,
//! mapped by our own loader ([`MappedImage`]), not the host's, and called
//! into (docs/macos.md, M2a). No game data.
#![cfg(target_arch = "x86_64")]

mod common;

use common::*;
use victory_loader::{MapError, MappedImage};

#[test]
fn title_code_runs_and_calls_back_both_ways() {
    let image = map_probe();
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
    let first = map_probe();
    let second = map_probe();
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

/// The TEB of ours that title code reads through `gs` where the host has
/// none for it (Windows has the real one: there the loader patches nothing).
#[cfg(not(windows))]
mod teb {
    use super::*;
    use victory_loader::teb::attach_current_thread;
    use victory_platform::{Host, Thread};

    #[test]
    fn title_code_reads_and_writes_our_teb_through_gs() {
        let image = map_probe();
        let teb = attach_current_thread();
        // SAFETY: the fixture's own signatures (tests/fixtures/pe-probe).
        unsafe {
            let self_pointer: extern "win64" fn() -> u64 = export(&image, "probe_teb");
            assert_eq!(self_pointer() as usize, teb, "NtCurrentTeb()");
            let thread_id: extern "win64" fn() -> u32 = export(&image, "probe_thread_id");
            assert_eq!(thread_id(), Host::thread_id());
            let stack_limit: extern "win64" fn() -> u64 = export(&image, "probe_stack_limit");
            let (low, high) = Host::stack_bounds();
            assert_eq!(stack_limit() as usize, low, "__chkstk's limit");
            let here = &raw const teb as usize;
            assert!(low < here && here < high);
            let set_error: extern "win64" fn(u32) = export(&image, "probe_set_last_error");
            let last_error: extern "win64" fn() -> u32 = export(&image, "probe_last_error");
            assert_eq!(last_error(), 0);
            set_error(0x57);
            assert_eq!(last_error(), 0x57, "a 32-bit store, then load");
            let set_pointer: extern "win64" fn(u64) = export(&image, "probe_set_user_pointer");
            let pointer: extern "win64" fn() -> u64 = export(&image, "probe_user_pointer");
            set_pointer(0x1234_5678_9abc_def0);
            assert_eq!(
                pointer(),
                0x1234_5678_9abc_def0,
                "a 64-bit store from rax, a load into r9"
            );
        }
    }

    #[test]
    fn static_tls_is_each_threads_own() {
        let image = map_probe();
        attach_current_thread();
        // SAFETY: the fixture's signature.
        let add: extern "win64" fn(u32) -> u32 = unsafe { export(&image, "probe_tls_add") };
        assert_eq!(add(1), 6, "the image's initial value, 5, plus one");
        assert_eq!(add(1), 7);
        let other = std::thread::spawn(move || {
            attach_current_thread();
            add(10)
        })
        .join()
        .unwrap();
        assert_eq!(other, 15, "another thread starts from the image's value");
        assert_eq!(add(0), 7, "ours is untouched");
    }

    /// Two mappings of the probe have TLS indices and blocks of their own.
    #[test]
    fn each_image_has_its_own_tls_block() {
        let first = map_probe();
        let second = map_probe();
        attach_current_thread();
        // SAFETY: the fixture's signature.
        let (one, two): (extern "win64" fn(u32) -> u32, extern "win64" fn(u32) -> u32) = unsafe {
            (
                export(&first, "probe_tls_add"),
                export(&second, "probe_tls_add"),
            )
        };
        assert_eq!(one(2), 7);
        assert_eq!(two(3), 8, "not the first image's counter");
        assert_eq!(one(0), 7);
    }
}

#[test]
fn unresolved_imports_are_named_and_refused() {
    match MappedImage::map(probe(), PROBE, &Nobody) {
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
