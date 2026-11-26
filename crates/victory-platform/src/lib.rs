//! The host operating system behind one set of traits.
//!
//! Each subsystem is a trait, and [`Host`] implements every one of them on
//! each host: Windows and macOS, the two places titles run, and Linux only
//! so that the portable crates' tests run there too (titles do not). A host
//! that lacks a function is then a compile error on that host, not a
//! surprise at run time.
//!
//! Subsystems come here as the macOS port first needs them, and the Windows
//! code they stand for moves with them (`docs/macos.md`).

use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
compile_error!("victory-platform supports Windows and macOS (and Linux, for tests)");

/// This process's host: every subsystem trait is implemented for it.
pub struct Host;

/// This process's memory, as the kernel sees it.
pub trait Memory {
    /// Copies `len` bytes of this process's memory at `src` to `dst`, the
    /// kernel checking the source: false if a page of it is not readable
    /// (`dst` may then hold some of it). Nothing faults, whatever `src` is.
    ///
    /// # Safety
    /// `dst` is writable memory of `len` bytes, which nothing else reads or
    /// writes until the call returns.
    unsafe fn read(src: usize, dst: *mut u8, len: usize) -> bool;

    /// [`Memory::read`] into a buffer.
    fn read_into(src: usize, dst: &mut [u8]) -> bool {
        // SAFETY: a buffer we hold the only reference to.
        unsafe { Self::read(src, dst.as_mut_ptr(), dst.len()) }
    }
}

/// Where VIctory keeps its own files when the user names no place.
pub trait Dirs {
    /// The default `cache_dir` (derived data, which may be deleted at any
    /// time); `None` if the user's directories cannot be found.
    fn default_cache_dir() -> Option<PathBuf>;
    /// The default `data_dir` (what titles save), never inside the cache.
    fn default_data_dir() -> Option<PathBuf>;
}

/// `<base>/victory/<name>`, the base being `%LOCALAPPDATA%`, else the XDG
/// or home cache: where Windows (and Linux) have always kept both.
#[cfg(any(windows, target_os = "linux"))]
fn local_app_data(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("victory").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_is_mapped() {
        let src: Vec<u8> = (0..=255).collect();
        let mut dst = [0u8; 256];
        assert!(Host::read_into(src.as_ptr() as usize, &mut dst));
        assert_eq!(dst[..], src[..]);
        assert!(
            Host::read_into(src.as_ptr() as usize, &mut []),
            "nothing asked"
        );
    }

    #[test]
    fn refuses_the_null_page_without_faulting() {
        let mut dst = [0u8; 8];
        assert!(!Host::read_into(16, &mut dst));
    }

    /// A page that is mapped but not readable, as guarded title memory is,
    /// fails the read; so does a read that runs into one from a readable
    /// page, while the readable part alone reads.
    #[test]
    fn refuses_a_page_with_no_access() {
        let (readable, size) = readable_then_no_access();
        let guarded = readable + size;
        let mut dst = [0u8; 16];
        assert!(!Host::read_into(guarded, &mut dst[..8]));
        assert!(Host::read_into(guarded - 8, &mut dst[..8]));
        assert_eq!(dst[..8], [7; 8]);
        assert!(!Host::read_into(guarded - 8, &mut dst));
    }

    /// A readable page of 7s and a page with no access after it: (the
    /// readable page, the page size). Never freed (two pages, for the
    /// test's life).
    #[cfg(unix)]
    fn readable_then_no_access() -> (usize, usize) {
        // SAFETY: a page-size query, a fresh anonymous mapping of two pages
        // and changes to that mapping only.
        unsafe {
            let size = libc::sysconf(libc::_SC_PAGESIZE) as usize;
            let p = libc::mmap(
                std::ptr::null_mut(),
                2 * size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            assert_ne!(p, libc::MAP_FAILED);
            p.cast::<u8>().write_bytes(7, size);
            assert_eq!(
                libc::mprotect(p.cast::<u8>().add(size).cast(), size, libc::PROT_NONE),
                0
            );
            (p as usize, size)
        }
    }

    #[cfg(windows)]
    fn readable_then_no_access() -> (usize, usize) {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
        };
        // Windows on x64 has 4 KiB pages.
        const SIZE: usize = 4096;
        // SAFETY: a fresh allocation of two pages and changes to it only.
        unsafe {
            let p = VirtualAlloc(
                std::ptr::null(),
                2 * SIZE,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            assert!(!p.is_null());
            p.cast::<u8>().write_bytes(7, SIZE);
            let mut old = 0;
            assert_ne!(
                VirtualProtect(
                    p.cast::<u8>().add(SIZE).cast(),
                    SIZE,
                    PAGE_NOACCESS,
                    &mut old
                ),
                0
            );
            (p as usize, SIZE)
        }
    }

    #[test]
    fn default_dirs_are_apart() {
        let (Some(cache), Some(data)) = (Host::default_cache_dir(), Host::default_data_dir())
        else {
            return; // no user directory in this environment
        };
        assert_ne!(cache, data);
        assert!(
            !data.starts_with(&cache),
            "what titles save is never in the cache"
        );
        #[cfg(target_os = "macos")]
        {
            assert!(
                cache.ends_with("Library/Caches/victory"),
                "{}",
                cache.display()
            );
            assert!(
                data.ends_with("Library/Application Support/victory"),
                "{}",
                data.display()
            );
        }
        #[cfg(any(windows, target_os = "linux"))]
        {
            assert!(cache.ends_with("victory/cache"), "{}", cache.display());
            assert!(data.ends_with("victory/data"), "{}", data.display());
        }
    }
}
