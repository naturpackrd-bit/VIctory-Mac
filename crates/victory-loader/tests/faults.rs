//! Faults in title code off Windows (docs/macos.md, M2a): the stand-in
//! title of tests/fixtures/pe-probe faults, and the loader's handling
//! either hands the fault to a handler of ours or does the Zen 2
//! instruction the processor lacks. One test: the handling is installed
//! once per process.
#![cfg(all(not(windows), target_arch = "x86_64"))]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use common::*;
use victory_platform::faults::{Fault, FaultKind, Handled};

/// Where [`resume_null_read`] sends the thread.
static RESUME: AtomicUsize = AtomicUsize::new(0);

/// A read of address 0 resumes at [`RESUME`]; anything else is not ours.
fn resume_null_read(fault: &mut Fault) -> Handled {
    if fault.kind == FaultKind::Access && fault.address == 0 {
        fault.registers.rip = RESUME.load(SeqCst) as u64;
        return Handled::Resume;
    }
    Handled::Pass
}

/// A cache line's worth either side of the one `clzero` zeroes.
#[repr(align(64))]
struct Lines([u8; 192]);

/// The three bytes at an export.
fn bytes_at(address: usize) -> [u8; 3] {
    let p = address as *const u8;
    // SAFETY: the title's code, mapped read-execute.
    unsafe { [0, 1, 2].map(|i| std::ptr::read_volatile(p.add(i))) }
}

#[test]
fn title_faults_are_emulated_or_handed_to_our_handler() {
    assert!(victory_loader::faults::install());
    assert!(victory_loader::faults::register(resume_null_read));
    let image = map_probe();
    victory_loader::teb::attach_current_thread();
    RESUME.store(image.export("probe_fault_resume").unwrap(), SeqCst);

    // SAFETY: the fixture's own signatures (tests/fixtures/pe-probe).
    unsafe {
        let fault: extern "win64" fn() -> u32 = export(&image, "probe_fault");
        assert_eq!(fault(), 0x5E, "resumed by our handler");

        let clzero: extern "win64" fn(*mut u8) = export(&image, "probe_clzero");
        let mut lines = Lines([0xAA; 192]);
        let before = victory_loader::faults::emulated();
        clzero(lines.0.as_mut_ptr().add(64));
        // A processor with the instructions (AMD's) runs them itself: then
        // nothing is emulated or patched.
        let emulating = victory_loader::faults::emulated() > before;
        assert!(lines.0[64..128].iter().all(|&b| b == 0), "the line");
        assert!(lines.0[..64].iter().all(|&b| b == 0xAA), "the line before");
        assert!(lines.0[128..].iter().all(|&b| b == 0xAA), "the line after");
        if emulating {
            assert!(victory_loader::faults::emulated() >= 1);
        } else {
            println!("this processor runs clzero, monitorx and mwaitx itself: nothing to emulate");
        }

        let monitorx: extern "win64" fn() -> u32 = export(&image, "probe_monitorx");
        assert_eq!(monitorx(), 1);
        let mwaitx: extern "win64" fn() -> u32 = export(&image, "probe_mwaitx");
        assert_eq!(mwaitx(), 2);
        if emulating {
            assert_eq!(
                bytes_at(image.export("probe_monitorx").unwrap()),
                [0x0F, 0x1F, 0x00],
                "monitorx patched to a NOP"
            );
            assert_eq!(
                bytes_at(image.export("probe_mwaitx").unwrap()),
                [0xF3, 0x90, 0x90],
                "mwaitx patched to pause"
            );
        }
        assert_eq!(monitorx(), 1, "and runs as patched");
    }
}
