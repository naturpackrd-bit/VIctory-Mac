//! The process's command line and start-up information, and the thread's
//! stack bounds (`api-ms-win-core-processenvironment-*`,
//! `api-ms-win-core-processthreads-*`): what MSVC's start-up code asks for
//! before `main`.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_char;
use std::mem::size_of;

use crate::modules::ucrt::runtime::{__p__acmdln, __p__wcmdln};
use crate::{HleExport, hle_export};

/// The command line as the C runtime's `_wcmdln`: the program's path in the
/// title's form and its arguments, quoted as `CommandLineToArgvW` reads
/// them.
pub extern "win64" fn GetCommandLineW() -> *mut u16 {
    // SAFETY: `__p__wcmdln` gives the address of the runtime's pointer.
    unsafe { *__p__wcmdln() }
}

/// See [`GetCommandLineW`].
pub extern "win64" fn GetCommandLineA() -> *mut c_char {
    // SAFETY: as above.
    unsafe { *__p__acmdln() }
}

/// `STARTUPINFOW` and `STARTUPINFOA` (the same layout on x64, 104 bytes).
#[repr(C)]
pub struct StartupInfo {
    pub cb: u32,
    pad: u32,
    pub reserved: usize,
    pub desktop: usize,
    pub title: usize,
    pub x: u32,
    pub y: u32,
    pub x_size: u32,
    pub y_size: u32,
    pub x_count_chars: u32,
    pub y_count_chars: u32,
    pub fill_attribute: u32,
    pub flags: u32,
    pub show_window: u16,
    pub reserved2: u16,
    pad2: u32,
    pub reserved2_pointer: usize,
    pub std_input: usize,
    pub std_output: usize,
    pub std_error: usize,
}

const _: () = assert!(size_of::<StartupInfo>() == 104);

/// Everything zero but `cb`: no window position, no `STARTF_USESHOWWINDOW`
/// (so `__scrt_get_show_window_mode` gives `SW_SHOWDEFAULT`), no redirected
/// standard handles (the console streams are the title's own).
fn startup_info(info: *mut StartupInfo) {
    if info.is_null() {
        return;
    }
    // SAFETY: zero is a value of every field.
    let mut value: StartupInfo = unsafe { std::mem::zeroed() };
    value.cb = size_of::<StartupInfo>() as u32;
    // SAFETY: checked non-null; the title's structure.
    unsafe { info.write_unaligned(value) };
}

pub extern "win64" fn GetStartupInfoW(info: *mut StartupInfo) {
    startup_info(info);
}

pub extern "win64" fn GetStartupInfoA(info: *mut StartupInfo) {
    startup_info(info);
}

/// The calling thread's stack, as its TEB has it: `StackLimit` (the lowest
/// address) and `StackBase` (one past the highest).
pub extern "win64" fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize) {
    let teb = victory_loader::teb::current_teb()
        .unwrap_or_else(victory_loader::teb::attach_current_thread);
    // SAFETY: our TEB: `StackBase` at 0x08 and `StackLimit` at 0x10.
    let (base, limit) = unsafe {
        let words = teb as *const u64;
        (*words.add(1) as usize, *words.add(2) as usize)
    };
    if !low.is_null() {
        // SAFETY: checked non-null; the title's output.
        unsafe { low.write_unaligned(limit) };
    }
    if !high.is_null() {
        // SAFETY: as above.
        unsafe { high.write_unaligned(base) };
    }
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(GetCommandLineW),
        hle_export!(GetCommandLineA),
        hle_export!(GetStartupInfoW),
        hle_export!(GetStartupInfoA),
        hle_export!(GetCurrentThreadStackLimits),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::handles::CloseHandle;
    use super::super::sync::{WAIT_OBJECT_0, WaitForSingleObject};
    use super::super::threads::CreateThread;
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn command_line_startup_info_and_stack_limits() {
        victory_loader::teb::attach_current_thread();
        let line = GetCommandLineW();
        assert!(!line.is_null());
        // SAFETY: the runtime's pointer.
        assert_eq!(line, unsafe { *__p__wcmdln() });
        assert!(!GetCommandLineA().is_null());
        // SAFETY: zero is a value of every field.
        let mut info: StartupInfo = unsafe { std::mem::zeroed() };
        info.flags = 0xFFFF;
        GetStartupInfoW(&mut info);
        assert_eq!((info.cb, info.flags, info.show_window), (104, 0, 0));
        let (mut low, mut high) = (0usize, 0usize);
        GetCurrentThreadStackLimits(&mut low, &mut high);
        let here = &raw const low as usize;
        assert!(low < here && here < high, "{low:#x} {here:#x} {high:#x}");
    }

    /// The thread's stack when a commit size is given: a megabyte at least,
    /// as Windows reserves; with the reservation flag, the size itself.
    #[test]
    fn a_commit_size_reserves_a_megabyte() {
        static SIZE: AtomicUsize = AtomicUsize::new(0);
        extern "win64" fn measure(_: *mut c_void) -> u32 {
            let (mut low, mut high) = (0usize, 0usize);
            GetCurrentThreadStackLimits(&mut low, &mut high);
            SIZE.store(high - low, Ordering::Release);
            0
        }
        victory_loader::teb::attach_current_thread();
        let mut id = 0;
        let thread = CreateThread(
            std::ptr::null_mut(),
            64 * 1024,
            measure as *mut c_void,
            std::ptr::null_mut(),
            0,
            &mut id,
        );
        assert!(!thread.is_null());
        assert_eq!(WaitForSingleObject(thread, 5000), WAIT_OBJECT_0);
        assert_eq!(CloseHandle(thread), 1);
        assert!(
            SIZE.load(Ordering::Acquire) >= 1 << 20,
            "{:#x}",
            SIZE.load(Ordering::Acquire)
        );
    }
}
