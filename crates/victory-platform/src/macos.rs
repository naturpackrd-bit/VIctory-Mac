//! macOS.

use std::path::PathBuf;

use crate::{Dirs, Host, Memory, Protection};

/// `<mach/vm_statistics.h>` (libc's copies are deprecated).
const VM_FLAGS_FIXED: libc::c_int = 0;
const VM_FLAGS_ANYWHERE: libc::c_int = 1;

unsafe extern "C" {
    /// `<mach/mach_init.h>`: this task's port, which libSystem sets before
    /// `main` and never changes (C's `mach_task_self()` reads it; libc's
    /// wrapper of it is deprecated).
    static mach_task_self_: libc::mach_port_t;

    /// `<mach/mach_vm.h>`: the kernel copies `size` bytes at `address` of
    /// `target_task` to `data` in ours. Fails (`KERN_INVALID_ADDRESS`,
    /// `KERN_PROTECTION_FAILURE`) when a page of the source is not there or
    /// not readable, instead of faulting.
    fn mach_vm_read_overwrite(
        target_task: libc::vm_map_t,
        address: libc::mach_vm_address_t,
        size: libc::mach_vm_size_t,
        data: libc::mach_vm_address_t,
        outsize: *mut libc::mach_vm_size_t,
    ) -> libc::kern_return_t;

    /// `<mach/mach_vm.h>`: zeroed read-write memory; with `VM_FLAGS_FIXED`
    /// at `*address` or not at all (`KERN_NO_SPACE` where something is
    /// mapped), never replacing a mapping.
    fn mach_vm_allocate(
        target: libc::vm_map_t,
        address: *mut libc::mach_vm_address_t,
        size: libc::mach_vm_size_t,
        flags: libc::c_int,
    ) -> libc::kern_return_t;

    /// `<mach/mach_vm.h>`.
    fn mach_vm_deallocate(
        target: libc::vm_map_t,
        address: libc::mach_vm_address_t,
        size: libc::mach_vm_size_t,
    ) -> libc::kern_return_t;
}

fn host_protection(protection: Protection) -> libc::c_int {
    match protection {
        Protection::NoAccess => libc::PROT_NONE,
        Protection::Read => libc::PROT_READ,
        Protection::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
        Protection::ReadExecute => libc::PROT_READ | libc::PROT_EXEC,
        Protection::ReadWriteExecute => libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
    }
}

impl Memory for Host {
    fn page_size() -> usize {
        // SAFETY: a plain query.
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    fn allocate(len: usize, at: Option<usize>) -> Option<usize> {
        let mut address = at.unwrap_or(0) as libc::mach_vm_address_t;
        let flags = if at.is_some() {
            VM_FLAGS_FIXED
        } else {
            VM_FLAGS_ANYWHERE
        };
        // SAFETY: a fresh allocation in our own task; a fixed one fails
        // rather than replace what is there. The task port is set before
        // `main`.
        let result = unsafe {
            mach_vm_allocate(
                mach_task_self_,
                &mut address,
                len as libc::mach_vm_size_t,
                flags,
            )
        };
        (result == libc::KERN_SUCCESS).then_some(address as usize)
    }

    unsafe fn protect(at: usize, len: usize, protection: Protection) -> bool {
        // SAFETY: the caller's range of our memory.
        unsafe { libc::mprotect(at as *mut libc::c_void, len, host_protection(protection)) == 0 }
    }

    unsafe fn free(at: usize, len: usize) -> bool {
        // SAFETY: the caller's whole allocation, in our own task.
        unsafe {
            mach_vm_deallocate(
                mach_task_self_,
                at as libc::mach_vm_address_t,
                len as libc::mach_vm_size_t,
            ) == libc::KERN_SUCCESS
        }
    }

    unsafe fn read(src: usize, dst: *mut u8, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let mut copied: libc::mach_vm_size_t = 0;
        // SAFETY: the caller's destination of `len` bytes; the kernel checks
        // the source. The task port is set before `main`.
        let result = unsafe {
            mach_vm_read_overwrite(
                mach_task_self_,
                src as libc::mach_vm_address_t,
                len as libc::mach_vm_size_t,
                dst as libc::mach_vm_address_t,
                &mut copied,
            )
        };
        result == libc::KERN_SUCCESS && copied == len as libc::mach_vm_size_t
    }
}

/// The user's home folder, if `HOME` names one.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

impl Dirs for Host {
    /// `~/Library/Caches/victory`: where macOS expects what can be made
    /// again.
    fn default_cache_dir() -> Option<PathBuf> {
        Some(home()?.join("Library").join("Caches").join("victory"))
    }

    /// `~/Library/Application Support/victory`: kept, and backed up.
    fn default_data_dir() -> Option<PathBuf> {
        Some(
            home()?
                .join("Library")
                .join("Application Support")
                .join("victory"),
        )
    }
}
