//! A stand-in for a title, written by us, that any host can build: what
//! the loader must get right to run title code, one export each. Imports
//! come from a DLL that does not exist (`victory-probe.dll`); the tests'
//! resolver answers them.
#![no_std]

mod cxx;
mod entry;
mod faults;
mod seh;
mod unwind;

use core::arch::asm;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[link(name = "victory-probe", kind = "raw-dylib")]
unsafe extern "win64" {
    fn ProbeValue() -> u32;
    fn ProbeSum6(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64;
    fn ProbeMissing() -> u32;
}

// Delay-loaded (`/DELAYLOAD`): the loader binds it ahead, with the others.
#[link(name = "victory-probe-delayed", kind = "raw-dylib")]
unsafe extern "win64" {
    fn ProbeDelayed() -> u32;
}

// Structured exception handling's: the handler `seh`'s functions name in
// their unwind info, and the raise they call, both from assembly.
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "win64" {
    fn __C_specific_handler(record: *mut c_void, frame: u64, ctx: *mut c_void, dc: *mut c_void)
    -> u32;
}

// C++ exception handling's: the language handler `cxx`'s functions (and
// core's unwind tables) name, and the throw they call, both from assembly.
#[link(name = "vcruntime140", kind = "raw-dylib")]
unsafe extern "win64" {
    fn __CxxFrameHandler3(record: *mut c_void, frame: u64, ctx: *mut c_void, dc: *mut c_void)
    -> u32;
    #[allow(dead_code)]
    fn _CxxThrowException(object: *mut c_void, throw_info: *const c_void);
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "win64" {
    #[allow(dead_code)]
    fn RaiseException(code: u32, flags: u32, count: u32, args: *const u64);
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

/// An import the tests' resolver leaves missing: bound to a stub, or the
/// mapping is refused.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_missing() -> u32 {
    // SAFETY: as above.
    unsafe { ProbeMissing() }
}

/// A delay import, bound by the loader ahead.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_delayed() -> u32 {
    // SAFETY: as above.
    unsafe { ProbeDelayed() }
}

/// What lld-link requires of an image with delay imports: the helper the
/// delay IAT slots point at until bound. It never runs, because our loader
/// binds delay imports ahead, so it faults if it ever does.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub extern "win64" fn __delayLoadHelper2() {
    core::arch::naked_asm!("ud2")
}

/// The title calls a function of ours that it is given.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_callback(f: extern "win64" fn(u64) -> u64, x: u64) -> u64 {
    f(x).wrapping_mul(2)
}

// Static TLS, as MSVC's CRT lays it out: `_tls_used` is the directory the
// linker files in the image (lld-link, like link.exe, puts the symbol of
// that name in the TLS data directory), between `.tls` and `.tls$ZZZ` lie
// the variables, and `_tls_index` is what the loader writes.

#[repr(C)]
pub struct TlsDirectory {
    start: *const u8,
    end: *const u8,
    index: *const u32,
    callbacks: *const usize,
    zero_fill: u32,
    characteristics: u32,
}

// SAFETY: data the linker and the loader read; nothing here follows the
// pointers.
unsafe impl Sync for TlsDirectory {}

#[unsafe(link_section = ".tls")]
static TLS_START: AtomicU64 = AtomicU64::new(0);
#[unsafe(link_section = ".tls$ZZZ")]
static TLS_END: AtomicU64 = AtomicU64::new(0);
/// A thread-local counter, 5 to begin with on every thread.
#[unsafe(link_section = ".tls$")]
static TLS_COUNTER: AtomicU32 = AtomicU32::new(5);
#[unsafe(no_mangle)]
pub static _tls_index: AtomicU32 = AtomicU32::new(0);
#[unsafe(no_mangle)]
pub static _tls_used: TlsDirectory = TlsDirectory {
    start: (&raw const TLS_START).cast(),
    end: (&raw const TLS_END).cast(),
    index: (&raw const _tls_index).cast(),
    callbacks: (&raw const entry::TLS_CALLBACKS).cast(),
    zero_fill: 0,
    characteristics: 0,
};

/// Adds `n` to this thread's counter and returns it: the thread's TLS array
/// from the TEB (`mov rax, qword ptr gs:[0x58]`, as MSVC writes it, the
/// bytes the loader patches), the image's block at `_tls_index`, the
/// variable at its offset in the block.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_tls_add(n: u32) -> u32 {
    let array: *const *mut u8;
    // SAFETY: a load from the TEB.
    unsafe {
        asm!(".byte 0x65, 0x48, 0x8b, 0x04, 0x25, 0x58, 0x00, 0x00, 0x00",
             out("rax") array, options(preserves_flags))
    };
    let offset = (&raw const TLS_COUNTER as usize) - (&raw const TLS_START as usize);
    // SAFETY: the loader's array has this image's block at its index; the
    // block is a copy of the image's TLS data.
    unsafe {
        let block = *array.add(_tls_index.load(Ordering::Relaxed) as usize);
        let counter = &*block.add(offset).cast::<AtomicU32>();
        counter.fetch_add(n, Ordering::Relaxed) + n
    }
}

/// `NtCurrentTeb()`: `mov rax, qword ptr gs:[0x30]`.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_teb() -> u64 {
    let teb: u64;
    // SAFETY: a load from the TEB.
    unsafe {
        asm!(".byte 0x65, 0x48, 0x8b, 0x04, 0x25, 0x30, 0x00, 0x00, 0x00",
             out("rax") teb, options(preserves_flags))
    };
    teb
}

/// `GetCurrentThreadId()` as the compiler inlines it: `mov eax, dword ptr
/// gs:[0x48]` (no REX: an 8-byte instruction).
#[unsafe(no_mangle)]
pub extern "win64" fn probe_thread_id() -> u32 {
    let id: u64;
    // SAFETY: a load from the TEB.
    unsafe {
        asm!(".byte 0x65, 0x8b, 0x04, 0x25, 0x48, 0x00, 0x00, 0x00",
             out("rax") id, options(preserves_flags))
    };
    id as u32
}

/// The stack limit as `__chkstk` reads it: `mov r11, qword ptr gs:[0x10]`.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_stack_limit() -> u64 {
    let limit: u64;
    // SAFETY: a load from the TEB.
    unsafe {
        asm!(".byte 0x65, 0x4c, 0x8b, 0x1c, 0x25, 0x10, 0x00, 0x00, 0x00",
             out("r11") limit, options(preserves_flags))
    };
    limit
}

/// `SetLastError` inlined: `mov dword ptr gs:[0x68], ecx`.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_set_last_error(error: u32) {
    // SAFETY: a store to the TEB.
    unsafe {
        asm!(".byte 0x65, 0x89, 0x0c, 0x25, 0x68, 0x00, 0x00, 0x00",
             in("rcx") u64::from(error), options(preserves_flags))
    };
}

/// `GetLastError` inlined: `mov eax, dword ptr gs:[0x68]`.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_last_error() -> u32 {
    let error: u64;
    // SAFETY: a load from the TEB.
    unsafe {
        asm!(".byte 0x65, 0x8b, 0x04, 0x25, 0x68, 0x00, 0x00, 0x00",
             out("rax") error, options(preserves_flags))
    };
    error as u32
}

/// `mov qword ptr gs:[0x28], rax` (`ArbitraryUserPointer`): a 64-bit store
/// whose source is rax, the thunk's usual scratch register.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_set_user_pointer(value: u64) {
    // SAFETY: a store to the TEB.
    unsafe {
        asm!(".byte 0x65, 0x48, 0x89, 0x04, 0x25, 0x28, 0x00, 0x00, 0x00",
             in("rax") value, options(preserves_flags))
    };
}

/// `mov r9, qword ptr gs:[0x28]`: a load into a high register.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_user_pointer() -> u64 {
    let value: u64;
    // SAFETY: a load from the TEB.
    unsafe {
        asm!(".byte 0x65, 0x4c, 0x8b, 0x0c, 0x25, 0x28, 0x00, 0x00, 0x00",
             out("r9") value, options(preserves_flags))
    };
    value
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
