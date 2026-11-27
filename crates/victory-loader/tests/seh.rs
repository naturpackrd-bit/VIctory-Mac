//! Structured exception handling off Windows (docs/macos.md, M2a): the
//! stand-in title of tests/fixtures/pe-probe faults and raises inside
//! `__try` blocks, and its filters, `__except` blocks and `__finally`
//! blocks run as on Windows, from its own unwind info. One test: the fault
//! handling is installed once per process. No game data.
#![cfg(all(not(windows), target_arch = "x86_64"))]

mod common;

use common::*;
use victory_loader::seh::{
    EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, ExceptionPointers,
    STATUS_ILLEGAL_INSTRUCTION,
};

/// Steps over a `ud2` (2 bytes) and has the thread go on; anything else is
/// not ours.
extern "win64" fn skip_ud2(pointers: *mut ExceptionPointers) -> i32 {
    // SAFETY: the dispatcher's pointers, to its record and context.
    unsafe {
        let pointers = &*pointers;
        if (*pointers.record).code == STATUS_ILLEGAL_INSTRUCTION {
            (*pointers.context).rip += 2;
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// A u32 the fixture exports.
fn word(image: &victory_loader::MappedImage, name: &str) -> *mut u32 {
    image
        .export(name)
        .unwrap_or_else(|| panic!("no export {name}")) as *mut u32
}

#[test]
fn title_exceptions_reach_their_handlers() {
    assert!(victory_loader::faults::install());
    let image = map_probe();
    victory_loader::teb::attach_current_thread();
    let divisor = word(&image, "PROBE_SEH_DIVISOR");
    let finally_count = word(&image, "PROBE_SEH_FINALLY_COUNT");
    let last_code = word(&image, "PROBE_SEH_LAST_CODE");
    let last_info1 = word(&image, "PROBE_SEH_LAST_INFO1");
    let read = |p: *mut u32| {
        // SAFETY: a static of the mapped fixture's.
        unsafe { p.read_volatile() }
    };

    // SAFETY: the fixture's own signatures (tests/fixtures/pe-probe).
    unsafe {
        let divide: extern "win64" fn(u32, u32) -> u32 = export(&image, "probe_seh_divide");
        assert_eq!(divide(10, 2), 5);
        assert_eq!(divide(10, 0), 0x1234, "the __except block, rbx kept");

        divisor.write_volatile(0);
        let filtered: extern "win64" fn(u32) -> u32 = export(&image, "probe_seh_filtered");
        assert_eq!(
            filtered(100),
            50,
            "the filter fixed the divisor and resumed"
        );
        assert_eq!(read(last_code), 0xC000_0094);
        assert_eq!(read(divisor), 2);

        let fault_read: extern "win64" fn(u32) -> u32 = export(&image, "probe_seh_fault_read");
        assert_eq!(fault_read(1), 0x5678);
        assert_eq!(read(last_code), 0xC000_0005);
        assert_eq!(read(last_info1), 8, "the address read");
        assert_eq!(fault_read(0), 0x1111);

        let finally: extern "win64" fn(u32) -> u32 = export(&image, "probe_seh_finally");
        assert_eq!(finally(0), 0x1111);
        assert_eq!(
            read(finally_count),
            1,
            "the __finally block on the normal path"
        );
        assert_eq!(finally(1), 0x9999);
        assert_eq!(read(finally_count), 2, "and in the unwind");

        let raise: extern "win64" fn(u32) -> u32 = export(&image, "probe_seh_raise");
        assert_eq!(raise(0x1234_5678), 0xABCD);

        let nested: extern "win64" fn(u32) -> u32 = export(&image, "probe_seh_nested");
        assert_eq!(nested(1), 0x4321, "a fault a frame further in");
        assert_eq!(nested(0), 0x2222);

        let handle = victory_loader::seh::add_vectored_handler(true, skip_ud2);
        let ud2: extern "win64" fn() -> u32 = export(&image, "probe_seh_ud2");
        assert_eq!(ud2(), 7, "the vectored handler stepped over it");
        assert!(victory_loader::seh::remove_vectored_handler(handle));
    }
}
