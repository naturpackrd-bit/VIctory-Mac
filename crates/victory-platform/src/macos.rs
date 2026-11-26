//! macOS.

use std::path::PathBuf;

use crate::{Dirs, Host, Memory};

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
}

impl Memory for Host {
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
