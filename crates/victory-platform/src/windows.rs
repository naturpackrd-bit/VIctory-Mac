//! Windows.

use std::ffi::c_void;
use std::path::PathBuf;

use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_NOACCESS,
    PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    VirtualProtect,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::{Dirs, Host, Memory, Protection};

fn host_protection(protection: Protection) -> PAGE_PROTECTION_FLAGS {
    match protection {
        Protection::NoAccess => PAGE_NOACCESS,
        Protection::Read => PAGE_READONLY,
        Protection::ReadWrite => PAGE_READWRITE,
        Protection::ReadExecute => PAGE_EXECUTE_READ,
        Protection::ReadWriteExecute => PAGE_EXECUTE_READWRITE,
    }
}

impl Memory for Host {
    fn page_size() -> usize {
        // SAFETY: plain data, filled in by the call.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer.
        unsafe { GetSystemInfo(&mut info) };
        info.dwPageSize as usize
    }

    fn allocate(len: usize, at: Option<usize>) -> Option<usize> {
        let asked = at.unwrap_or(0);
        // SAFETY: a fresh reservation; Windows refuses a range in use.
        let got = unsafe {
            VirtualAlloc(
                asked as *const c_void,
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        if got == 0 {
            return None;
        }
        // A reservation's address is rounded down to the allocation
        // granularity (64 KiB): another address is not the one asked for.
        if at.is_some() && got != asked {
            // SAFETY: the allocation just made, used by nobody.
            unsafe { VirtualFree(got as *mut c_void, 0, MEM_RELEASE) };
            return None;
        }
        Some(got)
    }

    unsafe fn protect(at: usize, len: usize, protection: Protection) -> bool {
        let mut old = 0;
        // SAFETY: the caller's range of our memory.
        unsafe {
            VirtualProtect(
                at as *const c_void,
                len,
                host_protection(protection),
                &mut old,
            ) != 0
        }
    }

    unsafe fn free(at: usize, _len: usize) -> bool {
        // SAFETY: the caller's whole allocation (Windows frees it by its base).
        unsafe { VirtualFree(at as *mut c_void, 0, MEM_RELEASE) != 0 }
    }

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
