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

/// The soft `RLIMIT_STACK`; `None` when unlimited or unknown.
fn stack_rlimit() -> Option<usize> {
    // SAFETY: plain data, for which zero is a value; a query into it.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::getrlimit(libc::RLIMIT_STACK, &mut limit) } != 0
        || limit.rlim_cur == libc::RLIM_INFINITY
    {
        return None;
    }
    usize::try_from(limit.rlim_cur).ok()
}

/// The stack in `maps` (`/proc/self/maps`) that holds `here`. The main
/// thread's (`[stack]`) is listed as far as it has grown: its lowest
/// address is then where it may grow to, its end less `rlimit` (the soft
/// `RLIMIT_STACK`), but not below the end of the next lower mapping.
fn stack_extent(maps: &str, here: usize, rlimit: Option<usize>) -> Option<(usize, usize)> {
    let mut below = 0; // the end of the mapping before
    for line in maps.lines() {
        let Some((range, rest)) = line.split_once(' ') else {
            continue;
        };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(end, 16),
        ) else {
            continue;
        };
        if !(start..end).contains(&here) {
            below = end;
            continue;
        }
        if !rest.trim_end().ends_with("[stack]") {
            return Some((start, end));
        }
        let low = rlimit.map_or(below, |limit| end.saturating_sub(limit).max(below));
        return Some((low.min(start), end));
    }
    None
}

impl Thread for Host {
    /// The mapping of `/proc/self/maps` that holds a local of this thread;
    /// for the main thread, as far as it may grow ([`stack_extent`]).
    fn stack_bounds() -> (usize, usize) {
        let marker = 0u8;
        let here = &raw const marker as usize;
        let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
        stack_extent(&maps, here, stack_rlimit()).expect("this thread's stack in /proc/self/maps")
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

    // `arg` is not read here, only handed to `start` on the new thread.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn spawn_thread(
        stack: usize,
        start: extern "C" fn(*mut libc::c_void) -> *mut libc::c_void,
        arg: *mut libc::c_void,
    ) -> bool {
        let Some(stack) = stack
            .max(libc::PTHREAD_STACK_MIN)
            .checked_next_multiple_of(Host::page_size())
        else {
            return false;
        };
        // SAFETY: an attribute object of our own, initialized before use and
        // destroyed after; the new thread gets `start` and `arg` as the
        // caller gave them, and is detached, so nothing joins it.
        unsafe {
            let mut attr: libc::pthread_attr_t = std::mem::zeroed();
            if libc::pthread_attr_init(&mut attr) != 0 {
                return false;
            }
            let mut thread: libc::pthread_t = std::mem::zeroed();
            let made = libc::pthread_attr_setstacksize(&mut attr, stack) == 0
                && libc::pthread_attr_setdetachstate(&mut attr, libc::PTHREAD_CREATE_DETACHED) == 0
                && libc::pthread_create(&mut thread, &attr, start, arg) == 0;
            libc::pthread_attr_destroy(&mut attr);
            made
        }
    }

    fn exit_thread() -> ! {
        // SAFETY: ends this thread; its thread-local destructors run.
        unsafe { libc::pthread_exit(std::ptr::null_mut()) }
    }

    fn name_current_thread(name: &str) {
        // The name stops at a NUL, and is cut at a character within the
        // kernel's 16 bytes (15 and the NUL).
        let name = name.split('\0').next().unwrap_or_default();
        let mut end = name.len().min(15);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = name.as_bytes()[..end].to_vec();
        bytes.push(0);
        // SAFETY: this thread, and a NUL-terminated name we hold.
        unsafe { libc::pthread_setname_np(libc::pthread_self(), bytes.as_ptr().cast()) };
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

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
55d0c0000000-55d0c0021000 r-xp 00000000 08:01 1234 /usr/bin/victory
7f0000000000-7f0000100000 rw-p 00000000 00:00 0
7ffc00000000-7ffc00021000 rw-p 00000000 00:00 0                          [stack]
";

    #[test]
    fn the_main_threads_stack_reaches_its_limit() {
        let here = 0x7ffc_0002_0000;
        // 132 KiB grown so far; 8 MiB allowed.
        assert_eq!(
            stack_extent(MAPS, here, Some(8 << 20)),
            Some((0x7ffc_0002_1000 - (8 << 20), 0x7ffc_0002_1000))
        );
        // Not below the mapping under it, nor above what is mapped.
        assert_eq!(
            stack_extent(MAPS, here, Some(1 << 40)),
            Some((0x7f00_0010_0000, 0x7ffc_0002_1000))
        );
        assert_eq!(
            stack_extent(MAPS, here, Some(0x1000)),
            Some((0x7ffc_0000_0000, 0x7ffc_0002_1000))
        );
        // Another thread's stack is the mapping itself.
        assert_eq!(
            stack_extent(MAPS, 0x7f00_0000_0040, Some(8 << 20)),
            Some((0x7f00_0000_0000, 0x7f00_0010_0000))
        );
        assert_eq!(stack_extent(MAPS, 0x10, None), None);
    }

    #[test]
    fn this_processs_main_thread_stack_spans_at_least_a_mebibyte() {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let line = maps.lines().find(|l| l.ends_with("[stack]")).unwrap();
        let start = usize::from_str_radix(line.split('-').next().unwrap(), 16).unwrap();
        let (low, high) = stack_extent(&maps, start, stack_rlimit()).unwrap();
        assert!(high - low >= 1 << 20, "{low:#x}..{high:#x}");
        if Host::thread_id() == Host::process_id() {
            let (low, high) = Host::stack_bounds();
            assert!(high - low >= 1 << 20, "{low:#x}..{high:#x}");
        }
    }
}
