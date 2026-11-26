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

#[cfg(unix)]
pub mod faults;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(unix)]
pub mod low;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
pub mod system;
#[cfg(windows)]
mod windows;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
compile_error!("victory-platform supports Windows and macOS (and Linux, for tests)");

/// This process's host: every subsystem trait is implemented for it.
pub struct Host;

/// What code may do with a range of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    NoAccess,
    Read,
    ReadWrite,
    ReadExecute,
    ReadWriteExecute,
}

/// This process's memory, as the kernel sees it.
pub trait Memory {
    /// The page size: the unit of [`Memory::allocate`] and
    /// [`Memory::protect`] (4 KiB on Windows and for x86-64 processes under
    /// Rosetta 2).
    fn page_size() -> usize;

    /// `len` bytes of fresh, zeroed, read-write memory starting at a page:
    /// at `at` if the whole range is free there (what is already mapped is
    /// never replaced), else `None`; with `at` `None`, wherever the host puts
    /// it.
    fn allocate(len: usize, at: Option<usize>) -> Option<usize>;

    /// Gives the pages of `[at, at + len)` `protection`; false if the host
    /// refused.
    ///
    /// # Safety
    /// The range lies in memory from [`Memory::allocate`], and nothing that
    /// holds a reference into it is left with access it no longer has.
    unsafe fn protect(at: usize, len: usize, protection: Protection) -> bool;

    /// Gives back one whole allocation of [`Memory::allocate`].
    ///
    /// # Safety
    /// `at` and `len` are those of the allocation, and nothing uses it any
    /// more.
    unsafe fn free(at: usize, len: usize) -> bool;

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

/// This thread, for the TEB we keep for it where Windows has the real one
/// (macOS; Linux for tests).
#[cfg(unix)]
pub trait Thread {
    /// This thread's stack: its lowest address and one past its highest.
    fn stack_bounds() -> (usize, usize);
    /// This thread's id, as the host numbers threads (the low 32 bits of a
    /// wider one).
    fn thread_id() -> u32;
    fn process_id() -> u32;
    /// Where x86-64 code finds this thread's TEB once
    /// [`Thread::set_teb_pointer`] has run: `gs:[disp]`, with the `disp` this
    /// returns, the same on every thread. The TEB's own `Self` field (offset
    /// 0x30) must hold its address: on Linux `gs` is made to point at the
    /// TEB itself, and `disp` is 0x30.
    fn teb_pointer_disp() -> u32;
    /// Makes `gs:[teb_pointer_disp()]` read `teb` on this thread.
    ///
    /// # Safety
    /// `teb` is a TEB whose `Self` field holds `teb`, and it stays valid for
    /// as long as this thread may read it.
    unsafe fn set_teb_pointer(teb: usize) -> bool;
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
    fn readable_then_no_access() -> (usize, usize) {
        let size = Host::page_size();
        let p = Host::allocate(2 * size, None).expect("two pages");
        // SAFETY: the first of our two fresh pages; then the second's
        // protection, which nothing refers into.
        unsafe {
            (p as *mut u8).write_bytes(7, size);
            assert!(Host::protect(p + size, size, Protection::NoAccess));
        }
        (p, size)
    }

    #[test]
    fn allocations_are_zeroed_writable_and_given_back() {
        let size = Host::page_size();
        assert!(size >= 4096 && size.is_power_of_two(), "{size}");
        let p = Host::allocate(3 * size, None).expect("three pages");
        assert_eq!(p % size, 0, "starts at a page");
        // SAFETY: our fresh, read-write allocation.
        let bytes = unsafe { std::slice::from_raw_parts_mut(p as *mut u8, 3 * size) };
        assert!(bytes.iter().all(|&b| b == 0));
        bytes[3 * size - 1] = 0x5a;
        assert_eq!(bytes[3 * size - 1], 0x5a);
        // SAFETY: the whole allocation, no longer used.
        assert!(unsafe { Host::free(p, 3 * size) });
    }

    /// An allocation at an address fails where something is mapped, and
    /// what is there is left as it was; where nothing is, it is given there.
    #[test]
    fn fixed_allocations_never_replace_a_mapping() {
        let size = Host::page_size();
        let taken = Host::allocate(2 * size, None).expect("two pages");
        // SAFETY: our fresh allocation.
        unsafe { (taken as *mut u8).write(0x77) };
        assert_eq!(Host::allocate(size, Some(taken)), None);
        // SAFETY: as above, still mapped and unchanged.
        assert_eq!(unsafe { (taken as *const u8).read() }, 0x77);
        // SAFETY: the whole allocation, no longer used.
        assert!(unsafe { Host::free(taken, 2 * size) });
        // Somewhere free: just given back, unless another test's allocation
        // took it meanwhile (then a fresh one is found and given back).
        let placed = (0..8).find_map(|_| {
            let free = Host::allocate(size, None)?;
            // SAFETY: as above.
            assert!(unsafe { Host::free(free, size) });
            Host::allocate(size, Some(free)).map(|p| (p, free))
        });
        let (p, free) = placed.expect("an allocation at a free address");
        assert_eq!(p, free);
        // SAFETY: as above.
        assert!(unsafe { Host::free(p, size) });
    }

    /// Code written into an allocation runs once the allocation is made
    /// executable: the loader maps title code so (under Rosetta 2 on
    /// Apple silicon too).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn written_code_runs_once_executable() {
        // mov eax, 42; ret
        const CODE: [u8; 6] = [0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3];
        let size = Host::page_size();
        let p = Host::allocate(size, None).expect("a page");
        // SAFETY: our fresh page; made read-execute once written, then
        // called as the function the bytes are; freed when done.
        unsafe {
            std::ptr::copy_nonoverlapping(CODE.as_ptr(), p as *mut u8, CODE.len());
            assert!(Host::protect(p, size, Protection::ReadExecute));
            let f: extern "C" fn() -> u32 = std::mem::transmute(p);
            assert_eq!(f(), 42);
            assert!(Host::free(p, size));
        }
    }

    /// A TEB-shaped block whose `Self` field (0x30) holds its own address.
    #[cfg(all(unix, target_arch = "x86_64"))]
    #[repr(C, align(16))]
    struct FakeTeb([u64; 16]);

    #[cfg(all(unix, target_arch = "x86_64"))]
    fn fake_teb() -> Box<FakeTeb> {
        let mut teb = Box::new(FakeTeb([0; 16]));
        teb.0[6] = &raw const *teb as usize as u64;
        teb
    }

    /// What `gs:[disp]` holds on this thread.
    #[cfg(all(unix, target_arch = "x86_64"))]
    fn read_gs(disp: u32) -> usize {
        let value: usize;
        // SAFETY: a load through `gs` at a displacement the host gave us.
        unsafe {
            std::arch::asm!("mov {0}, gs:[{1}]", out(reg) value, in(reg) disp as usize,
                options(nostack, readonly, preserves_flags))
        };
        value
    }

    /// Title code finds its TEB where the host says: here and on another
    /// thread, each its own.
    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn the_teb_pointer_is_read_through_gs() {
        let disp = Host::teb_pointer_disp();
        assert_eq!(disp % 8, 0, "{disp:#x}");
        let mine = fake_teb();
        let at = &raw const *mine as usize;
        // SAFETY: a TEB-shaped block with its address at 0x30, kept alive
        // below.
        assert!(unsafe { Host::set_teb_pointer(at) });
        assert_eq!(read_gs(disp), at);
        let other = std::thread::spawn(move || {
            let theirs = fake_teb();
            let at = &raw const *theirs as usize;
            // SAFETY: as above.
            assert!(unsafe { Host::set_teb_pointer(at) });
            (read_gs(disp) == at, at)
        })
        .join()
        .unwrap();
        assert!(other.0, "the other thread reads its own");
        assert_ne!(other.1, at);
        assert_eq!(read_gs(disp), at, "ours is untouched by the other thread's");
    }

    #[cfg(unix)]
    #[test]
    fn this_thread_is_known() {
        let (low, high) = Host::stack_bounds();
        let here = &raw const low as usize;
        assert!(
            low < here && here < high,
            "{low:#x} < {here:#x} < {high:#x}"
        );
        assert!(high - low >= 64 * 1024, "{} bytes of stack", high - low);
        let mine = Host::thread_id();
        let theirs = std::thread::spawn(Host::thread_id).join().unwrap();
        assert_ne!(mine, theirs);
        assert_eq!(Host::thread_id(), mine, "stable");
        assert_eq!(Host::process_id(), std::process::id());
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
