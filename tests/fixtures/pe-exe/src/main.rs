//! A stand-in for a title's executable, written by us, that any host can
//! build: what `victory run` must get right to start a title and see it
//! end. It writes one line per step to standard output, all through the
//! Windows API our runtime implements, and ends with `ExitProcess(42)`.
#![no_std]
#![no_main]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "win64" {
    fn GetStdHandle(id: u32) -> *mut c_void;
    fn WriteFile(
        file: *mut c_void,
        buffer: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn ReadFile(
        file: *mut c_void,
        buffer: *mut u8,
        len: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn CreateThread(
        attributes: *mut c_void,
        stack_size: usize,
        start: extern "win64" fn(*mut c_void) -> u32,
        parameter: *mut c_void,
        flags: u32,
        thread_id: *mut u32,
    ) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn GetExitCodeThread(thread: *mut c_void, code: *mut u32) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn ExitProcess(code: u32) -> !;
}

unsafe extern "C" {
    /// The image's own base: the linker's symbol for it.
    static __ImageBase: u8;
}

const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
const GENERIC_READ: u32 = 0x8000_0000;
const FILE_SHARE_READ: u32 = 1;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const INFINITE: u32 = 0xFFFF_FFFF;

/// A NUL-terminated UTF-16 copy of the ASCII `text`; `N` is its length
/// plus one.
const fn wide<const N: usize>(text: &[u8]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut i = 0;
    while i < text.len() {
        out[i] = text[i] as u16;
        i += 1;
    }
    out
}

static PROBE_FILE: [u16; 13] = wide(b"G:\\probe.txt");
static KERNEL32: [u16; 13] = wide(b"kernel32.dll");

/// What the worker thread leaves.
static VALUE: AtomicU32 = AtomicU32::new(0);

/// Writes `text` to standard output.
fn say(text: &[u8]) {
    let mut written = 0u32;
    // SAFETY: the imports' own signatures; `text` is valid for its length.
    unsafe {
        WriteFile(
            GetStdHandle(STD_OUTPUT_HANDLE),
            text.as_ptr(),
            text.len() as u32,
            &mut written,
            core::ptr::null_mut(),
        );
    }
}

/// `n` in decimal, at the end of `buf`.
fn number(mut n: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[at..]
}

/// `n` in hexadecimal with `0x`, at the end of `buf`.
fn hex(mut n: usize, buf: &mut [u8; 18]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b"0123456789abcdef"[n & 0xf];
        n >>= 4;
        if n == 0 {
            break;
        }
    }
    at -= 2;
    buf[at] = b'0';
    buf[at + 1] = b'x';
    &buf[at..]
}

extern "win64" fn worker(p: *mut c_void) -> u32 {
    VALUE.store(p as u32 * 2 + 1, Ordering::Relaxed);
    7
}

/// The entry point (`/ENTRY:probe_main`), in place of `mainCRTStartup`.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_main() -> i32 {
    say(b"pe-exe: start\n");

    // SAFETY (every call below): the imports' own signatures, with
    // arguments valid for them.
    let thread = unsafe {
        CreateThread(
            core::ptr::null_mut(),
            0,
            worker,
            42 as *mut c_void,
            0,
            core::ptr::null_mut(),
        )
    };
    let mut code = 0u32;
    // SAFETY: as above.
    unsafe {
        WaitForSingleObject(thread, INFINITE);
        GetExitCodeThread(thread, &mut code);
        CloseHandle(thread);
    }
    let mut buf = [0u8; 10];
    say(b"pe-exe: thread ");
    say(number(code, &mut buf));
    say(b" value ");
    say(number(VALUE.load(Ordering::Relaxed), &mut buf));
    say(b"\n");

    // SAFETY: as above.
    let file = unsafe {
        CreateFileW(
            PROBE_FILE.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            core::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if file == -1 as isize as *mut c_void {
        say(b"pe-exe: file missing\n");
    } else {
        let mut bytes = [0u8; 64];
        let mut read = 0u32;
        // SAFETY: as above; `bytes` holds 64.
        unsafe {
            ReadFile(
                file,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut read,
                core::ptr::null_mut(),
            );
            CloseHandle(file);
        }
        say(b"pe-exe: file ");
        say(&bytes[..(read as usize).min(bytes.len())]);
        say(b"\n");
    }

    // SAFETY: as above.
    let module = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
    if module == &raw const __ImageBase as usize {
        say(b"pe-exe: module ok\n");
    } else {
        let mut buf = [0u8; 18];
        say(b"pe-exe: module ");
        say(hex(module, &mut buf));
        say(b"\n");
    }

    // SAFETY: as above.
    let exit = unsafe {
        GetProcAddress(
            GetModuleHandleW(KERNEL32.as_ptr()),
            c"ExitProcess".as_ptr().cast(),
        )
    };
    if exit.is_null() {
        say(b"pe-exe: kernel32 ExitProcess missing\n");
    } else {
        say(b"pe-exe: kernel32 ExitProcess found\n");
    }

    // SAFETY: as above.
    unsafe { ExitProcess(42) }
}

/// Never reached: nothing here panics.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
