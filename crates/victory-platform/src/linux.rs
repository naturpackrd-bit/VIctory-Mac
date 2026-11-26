//! Linux, only so that the portable crates' tests run there.

use std::path::PathBuf;

use crate::{Dirs, Host, Memory, Protection, Thread};

/// `arch_prctl` code: set this thread's `gs` base (`<asm/prctl.h>`).
const ARCH_SET_GS: libc::c_long = 0x1001;

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
        let fixed = if at.is_some() {
            libc::MAP_FIXED_NOREPLACE
        } else {
            0
        };
        // SAFETY: a fresh anonymous mapping; MAP_FIXED_NOREPLACE fails rather
        // than replace what is there.
        let p = unsafe {
            libc::mmap(
                at.unwrap_or(0) as *mut libc::c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | fixed,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return None;
        }
        // Kernels before 4.17 take the flag for a hint.
        if at.is_some_and(|at| at != p as usize) {
            // SAFETY: the mapping just made, used by nobody.
            unsafe { libc::munmap(p, len) };
            return None;
        }
        Some(p as usize)
    }

    unsafe fn protect(at: usize, len: usize, protection: Protection) -> bool {
        // SAFETY: the caller's range of our memory.
        unsafe { libc::mprotect(at as *mut libc::c_void, len, host_protection(protection)) == 0 }
    }

    unsafe fn free(at: usize, len: usize) -> bool {
        // SAFETY: the caller's whole allocation.
        unsafe { libc::munmap(at as *mut libc::c_void, len) == 0 }
    }

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

impl Thread for Host {
    /// The mapping of `/proc/self/maps` that holds a local of this thread.
    fn stack_bounds() -> (usize, usize) {
        let marker = 0u8;
        let here = &raw const marker as usize;
        let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
        maps.lines()
            .filter_map(|line| {
                let (range, _) = line.split_once(' ')?;
                let (start, end) = range.split_once('-')?;
                let (start, end) = (
                    usize::from_str_radix(start, 16).ok()?,
                    usize::from_str_radix(end, 16).ok()?,
                );
                (start..end).contains(&here).then_some((start, end))
            })
            .next()
            .expect("this thread's stack in /proc/self/maps")
    }

    fn thread_id() -> u32 {
        // SAFETY: a plain query.
        unsafe { libc::syscall(libc::SYS_gettid) as u32 }
    }

    fn process_id() -> u32 {
        // SAFETY: a plain query.
        unsafe { libc::getpid() as u32 }
    }

    /// Linux does not use `gs` for its own thread data (that is `fs`), so
    /// the TEB itself is put at the `gs` base, and its `Self` field is read.
    fn teb_pointer_disp() -> u32 {
        0x30
    }

    unsafe fn set_teb_pointer(teb: usize) -> bool {
        // SAFETY: sets this thread's `gs` base, which nothing of the host's
        // uses on x86-64 Linux.
        unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, teb) == 0 }
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
