//! C++ exceptions off Windows (docs/macos.md, M2b): the stand-in title of
//! tests/fixtures/pe-probe throws ints through `_CxxThrowException`, and
//! its `__CxxFrameHandler3` tables, written by hand, have them caught, the
//! destructors on the way run and the functions resumed at their
//! continuations, by our `vcruntime140.dll`. No game data.
#![cfg(all(not(windows), target_arch = "x86_64"))]

#[path = "../../victory-loader/tests/common/mod.rs"]
mod common;

use std::ffi::c_void;
use std::io::Write as _;
use std::process::Command;

use common::{PROBE, Probe, export, probe};
use victory_hle::{HleResolver, HostPolicy};
use victory_loader::seh::{EXCEPTION_EXECUTE_HANDLER, ExceptionPointers};
use victory_loader::stubs::UnimplementedPolicy;
use victory_loader::{ImportResolver, MappedImage, Resolution};
use victory_pe::ImportSymbol;

/// `common::Probe`, with our `vcruntime140.dll` (which `Probe` answers with
/// stand-ins, for the loader's own tests) and the rest of what it leaves
/// missing answered by the HLE modules.
struct WithHle(HleResolver);

impl ImportResolver for WithHle {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        if dll.eq_ignore_ascii_case("vcruntime140.dll") {
            return self.0.resolve(dll, symbol);
        }
        match Probe.resolve(dll, symbol) {
            Resolution::Missing => self.0.resolve(dll, symbol),
            found => found,
        }
    }
}

fn map() -> MappedImage {
    let resolver = WithHle(HleResolver::new(HostPolicy::none()));
    MappedImage::map_with(probe(), PROBE, &resolver, UnimplementedPolicy::Zero)
        .expect("mapping the probe")
}

/// A u32 the fixture exports.
fn word(image: &MappedImage, name: &str) -> u32 {
    let at = image
        .export(name)
        .unwrap_or_else(|| panic!("no export {name}"));
    // SAFETY: a static of the mapped fixture's.
    unsafe { (at as *const u32).read_volatile() }
}

#[test]
fn thrown_ints_are_caught() {
    assert!(victory_loader::faults::install());
    let image = map();
    victory_loader::teb::attach_current_thread();
    // SAFETY: the fixture's own signatures (tests/fixtures/pe-probe).
    unsafe {
        let catch_int: extern "win64" fn(u32) -> u32 = export(&image, "probe_cxx_catch_int");
        assert_eq!(catch_int(0), 7, "nothing thrown");
        assert_eq!(catch_int(5), 0x105, "caught, rbx kept");
        assert_eq!(catch_int(6), 0x106, "and again");

        let unwind: extern "win64" fn(u32) -> u32 = export(&image, "probe_cxx_unwind");
        assert_eq!(unwind(3), 0x201, "the destructor ran once, then catch(...)");
        assert_eq!(word(&image, "PROBE_CXX_DESTROYED"), 1);

        let rethrow: extern "win64" fn(u32) -> u32 = export(&image, "probe_cxx_rethrow");
        assert_eq!(rethrow(9), 0x309, "thrown again after the inner catch");
        assert_eq!(word(&image, "PROBE_CXX_INNER"), 9);
    }
    assert_eq!(
        victory_hle::modules::ucrt::cxx::__uncaught_exceptions(),
        0,
        "every throw caught"
    );
    // SAFETY: the thread's slot.
    let current = unsafe { *victory_hle::modules::ucrt::cxx::__current_exception() };
    assert!(current.is_null(), "no exception left current");
}

/// Set in the child process that throws with nothing to catch.
const CHILD: &str = "VICTORY_CXX_UNCAUGHT_CHILD";

/// The unhandled-exception filter of the child: says the code, ends the
/// process (`EXCEPTION_EXECUTE_HANDLER`).
extern "system" fn report(pointers: *const c_void) -> i32 {
    // SAFETY: the dispatcher's pointers.
    let code = unsafe { (*(*pointers.cast::<ExceptionPointers>()).record).code };
    let _ = writeln!(std::io::stderr(), "unhandled exception {code:#x}");
    EXCEPTION_EXECUTE_HANDLER
}

#[test]
fn an_uncaught_exception_reaches_the_unhandled_filter() {
    if std::env::var_os(CHILD).is_some() {
        assert!(victory_loader::faults::install());
        let image = map();
        victory_loader::teb::attach_current_thread();
        victory_loader::seh::set_unhandled_filter(Some(report));
        // SAFETY: the fixture's own signature.
        let uncaught: extern "win64" fn(u32) = unsafe { export(&image, "probe_cxx_uncaught") };
        uncaught(4);
        unreachable!("the throw returned");
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "an_uncaught_exception_reaches_the_unhandled_filter",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .expect("running the child");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("0xe06d7363"), "{stderr}");
    // The process ends with the exception's code (its low byte here).
    assert_eq!(output.status.code(), Some(0x63), "{stderr}");
}
