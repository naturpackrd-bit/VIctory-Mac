//! Linux, only so that the portable crates' tests run there.

use std::path::PathBuf;

use crate::{Dirs, Host, Memory};

impl Memory for Host {
    unsafe fn read(src: usize, dst: *mut u8, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let local = libc::iovec {
            iov_base: dst.cast(),
            iov_len: len,
        };
        let remote = libc::iovec {
            iov_base: src as *mut libc::c_void,
            iov_len: len,
        };
        // SAFETY: the caller's destination of `len` bytes; the kernel checks
        // the source (EFAULT where a page of it is not readable).
        let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
        n == len as isize
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
