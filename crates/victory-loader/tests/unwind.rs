//! The unwinder on real unwind info: the probe's `probe_call_through` calls
//! `probe_call_middle`, which calls a function of ours that undoes the
//! frames above it from the stack and the probe's `.pdata` alone
//! (docs/macos.md, M2a). No game data.
#![cfg(target_arch = "x86_64")]

mod common;

use std::arch::naked_asm;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};

use common::*;
use victory_loader::unwind::{Context, backtrace, lookup, virtual_unwind};
use victory_platform::{Host, Memory};

/// rsp as [`capture`] found it: its return address is at it.
static mut SAVED_RSP: u64 = 0;
/// The probe as mapped: its base and its exception table.
static IMAGE: Mutex<Option<(u64, Vec<u8>)>> = Mutex::new(None);
/// rip after one unwind from `capture`'s caller, then after two.
static AFTER_ONE: AtomicU64 = AtomicU64::new(0);
static AFTER_TWO: AtomicU64 = AtomicU64::new(0);
/// The RVA of the function the first unwind undid.
static FIRST_BEGIN: AtomicU32 = AtomicU32::new(0);
/// What `backtrace` gave from `capture`'s caller.
static BACKTRACE: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The function the probe calls: records rsp at entry, where its return
/// address lies, then has [`unwind_from_here`] walk the stack; returns `x`,
/// kept on the stack across the call (40 bytes in all, as the call's
/// alignment and shadow space need).
#[unsafe(naked)]
extern "win64" fn capture(x: u64) -> u64 {
    naked_asm!(
        "mov [rip + {saved}], rsp",
        "push rcx",
        "sub rsp, 32",
        "call {inner}",
        "add rsp, 32",
        "pop rax",
        "ret",
        saved = sym SAVED_RSP,
        inner = sym unwind_from_here,
    )
}

fn read_u64(at: u64) -> u64 {
    let mut bytes = [0; 8];
    assert!(Host::read_into(at as usize, &mut bytes), "reading {at:#x}");
    u64::from_le_bytes(bytes)
}

/// Undoes two frames from `capture`'s caller, and walks the stack from it,
/// recording what it found.
extern "win64" fn unwind_from_here() {
    // SAFETY: written by `capture` before it called us, on this thread.
    let saved = unsafe { (&raw const SAVED_RSP).read() };
    let at_capture = Context {
        rip: read_u64(saved),
        rsp: saved + 8,
        ..Default::default()
    };
    let image = IMAGE.lock().unwrap();
    let (base, pdata) = image.as_ref().expect("the image is recorded");
    let mut ctx = at_capture;
    let first = virtual_unwind(*base, pdata, &mut ctx).expect("unwinding middle");
    FIRST_BEGIN.store(first.map_or(0, |u| u.function.begin), SeqCst);
    AFTER_ONE.store(ctx.rip, SeqCst);
    virtual_unwind(*base, pdata, &mut ctx).expect("unwinding through");
    AFTER_TWO.store(ctx.rip, SeqCst);
    *BACKTRACE.lock().unwrap() = backtrace(at_capture, 8);
}

#[test]
fn title_frames_are_undone_from_their_unwind_info() {
    let image = map_probe();
    #[cfg(not(windows))]
    victory_loader::teb::attach_current_thread();
    let base = image.base() as u64;
    let pdata = image
        .exception_table()
        .expect("the probe has .pdata")
        .to_vec();
    *IMAGE.lock().unwrap() = Some((base, pdata.clone()));
    let rva = |name: &str| (image.export(name).expect(name) as u64 - base) as u32;

    // SAFETY: the fixture's own signature (tests/fixtures/pe-probe).
    let through: extern "win64" fn(extern "win64" fn(u64) -> u64, u64) -> u64 =
        unsafe { export(&image, "probe_call_through") };
    assert_eq!(through(capture, 41), 41 + 41 + 1);

    assert_eq!(FIRST_BEGIN.load(SeqCst), rva("probe_call_middle"));
    let one = AFTER_ONE.load(SeqCst);
    let caller = lookup(&pdata, (one - base) as u32).expect("an entry for the caller");
    assert_eq!(caller.begin, rva("probe_call_through"));
    let two = AFTER_TWO.load(SeqCst);
    assert!(
        victory_loader::images::find(two as usize).is_none(),
        "{two:#x} is the test's own code"
    );

    let frames = BACKTRACE.lock().unwrap().clone();
    assert_eq!(frames.len(), 3, "{frames:x?}");
    let begin = |at: u64| lookup(&pdata, (at - base) as u32).map(|f| f.begin);
    assert_eq!(begin(frames[0]), Some(rva("probe_call_middle")));
    assert_eq!(frames[1], one);
    assert_eq!(begin(frames[1]), Some(rva("probe_call_through")));
    assert_eq!(frames[2], two);
}
