//! Windows.

use std::ffi::c_void;
use std::path::PathBuf;

use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::{Dirs, Host, Memory};

impl Memory for Host {
    unsafe fn read(src: usize, dst: *mut u8, len: usize) -> bool {
        let mut done = 0;
        // SAFETY: the caller's destination of `len` bytes; the kernel checks
        // the source.
        unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                src as *const c_void,
                dst.cast(),
                len,
                &mut done,
            ) != 0
        }
    }
}

impl Dirs for Host {
    fn default_cache_dir() -> Option<PathBuf> {
        crate::local_app_data("cache")
    }

    fn default_data_dir() -> Option<PathBuf> {
        crate::local_app_data("data")
    }
}
