//! A stand-in for a title, written by us, that any host can build: what
//! the loader must get right to run title code, one export each. Imports
//! come from a DLL that does not exist (`victory-probe.dll`); the tests'
//! resolver answers them.
#![no_std]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

#[link(name = "victory-probe", kind = "raw-dylib")]
unsafe extern "win64" {
    fn ProbeValue() -> u32;
    fn ProbeSum6(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64;
}

/// The Win64 convention, arguments in registers.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_add(a: u64, b: u64) -> u64 {
    a.wrapping_add(b)
}

/// An import bound by the loader, called from title code.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_import() -> u32 {
    // SAFETY: bound by the loader to a function of this signature.
    unsafe { ProbeValue() }
}

/// Six arguments both ways: the fifth and sixth go on the stack, above the
/// 32 bytes the caller leaves for the first four.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_sum6(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64 {
    // SAFETY: as above.
    unsafe { ProbeSum6(f, e, d, c, b, a) }
}

static VALUE: AtomicU32 = AtomicU32::new(0x0D15_EA5E);
/// An absolute address in the image's data: the base relocation the loader
/// applies when the image is not at its preferred base.
static POINTER: &AtomicU32 = &VALUE;

/// Reads `VALUE` through `POINTER`, then adds one to it. The pointer is
/// read as stored (volatile), not worked out by the compiler, so it is the
/// relocated address that is used.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_reloc() -> u32 {
    // SAFETY: a static, read where it lies.
    let value = unsafe { core::ptr::read_volatile(&raw const POINTER) };
    value.fetch_add(1, Ordering::Relaxed)
}

/// The title calls a function of ours that it is given.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_callback(f: extern "win64" fn(u64) -> u64, x: u64) -> u64 {
    f(x).wrapping_mul(2)
}

/// The entry point of the DLL; the tests' loader does not call it.
#[unsafe(no_mangle)]
pub extern "win64" fn _DllMainCRTStartup(_: *mut u8, _: u32, _: *mut u8) -> i32 {
    1
}

/// A fault the tests see, never a hang.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: raises an invalid-instruction fault.
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}

// What the C runtime would supply. Byte loops through volatile accesses, so
// the compiler cannot turn them back into calls of themselves.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(to: *mut c_void, from: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (to.cast::<u8>(), from.cast::<u8>());
    for i in 0..n {
        // SAFETY: the caller's `n` bytes at each.
        unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) }
    }
    to
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(to: *mut c_void, from: *const c_void, n: usize) -> *mut c_void {
    if (to as usize) <= (from as usize) {
        // SAFETY: the caller's `n` bytes at each; forwards when `to` is lower.
        return unsafe { memcpy(to, from, n) };
    }
    let (d, s) = (to.cast::<u8>(), from.cast::<u8>());
    for i in (0..n).rev() {
        // SAFETY: as above, backwards.
        unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) }
    }
    to
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(to: *mut c_void, byte: i32, n: usize) -> *mut c_void {
    let d = to.cast::<u8>();
    for i in 0..n {
        // SAFETY: the caller's `n` bytes.
        unsafe { d.add(i).write_volatile(byte as u8) }
    }
    to
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
    for i in 0..n {
        // SAFETY: the caller's `n` bytes at each.
        let (x, y) = unsafe { (a.add(i).read_volatile(), b.add(i).read_volatile()) };
        if x != y {
            return i32::from(x) - i32::from(y);
        }
    }
    0
}

/// What MSVC objects expect when they use floating point.
#[unsafe(no_mangle)]
pub static _fltused: i32 = 0;

/// Named by core's unwind tables; never called, panics abort.
#[unsafe(no_mangle)]
pub extern "C" fn __CxxFrameHandler3() -> ! {
    // SAFETY: as in `panic`.
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}
