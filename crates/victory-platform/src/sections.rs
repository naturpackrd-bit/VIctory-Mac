//! Memory a title's physical pages live in. A section is a block of the
//! host's anonymous memory mapped once at an address of its own (its
//! window, where our own code reads and writes it) and shown again at title
//! addresses (views), several at a time, each view replacing what was at
//! its address with no gap in which another allocation could land.
//!
//! Linux: a memfd and `mmap(MAP_SHARED | MAP_FIXED)` views. macOS:
//! `mach_vm_allocate` for the window and
//! `mach_vm_remap(VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE)` for views, which
//! share the memory object, so the window may go while views stay.

use crate::Host;

/// A section: its host identity, its window and its length.
#[derive(Debug)]
pub struct Section {
    /// Linux: the memfd; macOS: unused (0).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    id: u64,
    /// Where the whole section is mapped read-write for our own use.
    pub window: usize,
    /// Its length in bytes, a multiple of the page size.
    pub len: usize,
}

/// Sections of the host's memory and views of them.
pub trait Sections {
    /// A section of `len` bytes (a multiple of the page size, > 0),
    /// zero-filled lazily; `None` if the host refused.
    fn section_create(len: usize) -> Option<Section>;

    /// Shows bytes `[offset, offset + len)` of `section` (page-aligned) at
    /// `at` (page-aligned), read-write, in place of whatever is mapped
    /// there; false if the host refused (what was at `at` is then as it
    /// was).
    fn section_map(section: &Section, offset: usize, len: usize, at: usize) -> bool;

    /// Replaces `[at, at + len)` with fresh reserved memory (no access,
    /// nothing backing it) in place; false if refused.
    fn unmap_in_place(at: usize, len: usize) -> bool;

    /// Gives the window and the host identity back; views stay valid until
    /// unmapped.
    fn section_destroy(section: Section);
}

impl Sections for Host {
    fn section_create(len: usize) -> Option<Section> {
        host::section_create(len)
    }

    fn section_map(section: &Section, offset: usize, len: usize, at: usize) -> bool {
        // A view never reaches past the section's end.
        if offset.checked_add(len).is_none_or(|end| end > section.len) {
            return false;
        }
        host::section_map(section, offset, len, at)
    }

    fn unmap_in_place(at: usize, len: usize) -> bool {
        host::unmap_in_place(at, len)
    }

    fn section_destroy(section: Section) {
        host::section_destroy(section)
    }
}

#[cfg(target_os = "linux")]
mod host {
    use super::Section;

    pub(super) fn section_create(len: usize) -> Option<Section> {
        if len == 0 {
            return None;
        }
        let size = libc::off_t::try_from(len).ok()?;
        // SAFETY: a NUL-terminated name; the descriptor is ours.
        let fd = unsafe { libc::memfd_create(c"victory-section".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: our own descriptor, sized and then mapped shared whole;
        // closed again if either fails.
        unsafe {
            if libc::ftruncate(fd, size) != 0 {
                libc::close(fd);
                return None;
            }
            let window = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if window == libc::MAP_FAILED {
                libc::close(fd);
                return None;
            }
            Some(Section {
                id: fd as u64,
                window: window as usize,
                len,
            })
        }
    }

    pub(super) fn section_map(section: &Section, offset: usize, len: usize, at: usize) -> bool {
        let Ok(offset) = libc::off_t::try_from(offset) else {
            return false;
        };
        // SAFETY: maps our memfd over the caller's range, which it says is
        // its own to replace; MAP_FIXED replaces what is there in one step.
        let p = unsafe {
            libc::mmap(
                at as *mut libc::c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                section.id as libc::c_int,
                offset,
            )
        };
        p as usize == at
    }

    pub(super) fn unmap_in_place(at: usize, len: usize) -> bool {
        // SAFETY: a fresh anonymous mapping with no access over the caller's
        // range, which it says is its own to replace.
        let p = unsafe {
            libc::mmap(
                at as *mut libc::c_void,
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        p as usize == at
    }

    pub(super) fn section_destroy(section: Section) {
        // SAFETY: the window and descriptor of a section given up; views
        // hold the memory on their own.
        unsafe {
            libc::munmap(section.window as *mut libc::c_void, section.len);
            libc::close(section.id as libc::c_int);
        }
    }
}

#[cfg(target_os = "macos")]
mod host {
    use super::Section;

    /// `<mach/vm_statistics.h>` (libc's copies are deprecated).
    const VM_FLAGS_FIXED: libc::c_int = 0;
    const VM_FLAGS_ANYWHERE: libc::c_int = 1;
    const VM_FLAGS_OVERWRITE: libc::c_int = 0x4000;
    /// `<mach/vm_prot.h>`.
    const VM_PROT_NONE: libc::vm_prot_t = 0;
    const VM_PROT_READ: libc::vm_prot_t = 1;
    const VM_PROT_WRITE: libc::vm_prot_t = 2;
    const VM_PROT_ALL: libc::vm_prot_t = 7;
    /// `<mach/vm_inherit.h>`: the child of a fork gets nothing here.
    const VM_INHERIT_NONE: libc::vm_inherit_t = 2;
    /// `<mach/memory_object_types.h>`: `MEMORY_OBJECT_NULL`, anonymous
    /// memory.
    const MEMORY_OBJECT_NULL: libc::mem_entry_name_port_t = 0;

    unsafe extern "C" {
        /// `<mach/mach_init.h>`: this task's port, which libSystem sets
        /// before `main` and never changes.
        static mach_task_self_: libc::mach_port_t;

        /// `<mach/mach_vm.h>`: zeroed read-write memory, anywhere with
        /// `VM_FLAGS_ANYWHERE`.
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

        /// `<mach/mach_vm.h>`: maps `size` bytes of `src_task` at
        /// `src_address` into `target_task` at `*target_address`; with
        /// `copy` false the two share the memory object, and with
        /// `VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE` what was at the target is
        /// replaced in the same step. The protections are written back.
        fn mach_vm_remap(
            target_task: libc::vm_map_t,
            target_address: *mut libc::mach_vm_address_t,
            size: libc::mach_vm_size_t,
            mask: libc::mach_vm_offset_t,
            flags: libc::c_int,
            src_task: libc::vm_map_t,
            src_address: libc::mach_vm_address_t,
            copy: libc::boolean_t,
            cur_protection: *mut libc::vm_prot_t,
            max_protection: *mut libc::vm_prot_t,
            inheritance: libc::vm_inherit_t,
        ) -> libc::kern_return_t;

        /// `<mach/mach_vm.h>`: sets the current (or, with `set_maximum`,
        /// the maximum) protection of a range.
        fn mach_vm_protect(
            target_task: libc::vm_map_t,
            address: libc::mach_vm_address_t,
            size: libc::mach_vm_size_t,
            set_maximum: libc::boolean_t,
            new_protection: libc::vm_prot_t,
        ) -> libc::kern_return_t;

        /// `<mach/mach_vm.h>`: maps `object` (with `MEMORY_OBJECT_NULL`,
        /// fresh anonymous memory) at `*address`, replacing what is there
        /// with `VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE`. (libc's copy is
        /// deprecated.)
        fn mach_vm_map(
            target_task: libc::vm_map_t,
            address: *mut libc::mach_vm_address_t,
            size: libc::mach_vm_size_t,
            mask: libc::mach_vm_offset_t,
            flags: libc::c_int,
            object: libc::mem_entry_name_port_t,
            offset: libc::memory_object_offset_t,
            copy: libc::boolean_t,
            cur_protection: libc::vm_prot_t,
            max_protection: libc::vm_prot_t,
            inheritance: libc::vm_inherit_t,
        ) -> libc::kern_return_t;
    }

    pub(super) fn section_create(len: usize) -> Option<Section> {
        if len == 0 {
            return None;
        }
        let mut address: libc::mach_vm_address_t = 0;
        // SAFETY: a fresh allocation in our own task, wherever the kernel
        // has room. The task port is set before `main`.
        let result = unsafe {
            mach_vm_allocate(
                mach_task_self_,
                &mut address,
                len as libc::mach_vm_size_t,
                VM_FLAGS_ANYWHERE,
            )
        };
        (result == libc::KERN_SUCCESS).then_some(Section {
            id: 0,
            window: address as usize,
            len,
        })
    }

    pub(super) fn section_map(section: &Section, offset: usize, len: usize, at: usize) -> bool {
        let mut target = at as libc::mach_vm_address_t;
        let (mut cur, mut max): (libc::vm_prot_t, libc::vm_prot_t) = (0, 0);
        // SAFETY: shares part of our own window over the caller's range,
        // which it says is its own to replace; the protections are written
        // into our own variables.
        let result = unsafe {
            mach_vm_remap(
                mach_task_self_,
                &mut target,
                len as libc::mach_vm_size_t,
                0,
                VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
                mach_task_self_,
                (section.window + offset) as libc::mach_vm_address_t,
                0,
                &mut cur,
                &mut max,
                VM_INHERIT_NONE,
            )
        };
        if result != libc::KERN_SUCCESS || target != at as libc::mach_vm_address_t {
            return false;
        }
        // The view keeps the window's protection, which is read-write: this
        // only makes sure.
        // SAFETY: the view just made.
        unsafe {
            mach_vm_protect(
                mach_task_self_,
                at as libc::mach_vm_address_t,
                len as libc::mach_vm_size_t,
                0,
                VM_PROT_READ | VM_PROT_WRITE,
            ) == libc::KERN_SUCCESS
        }
    }

    pub(super) fn unmap_in_place(at: usize, len: usize) -> bool {
        let mut address = at as libc::mach_vm_address_t;
        // SAFETY: fresh anonymous memory with no access over the caller's
        // range, which it says is its own to replace.
        let result = unsafe {
            mach_vm_map(
                mach_task_self_,
                &mut address,
                len as libc::mach_vm_size_t,
                0,
                VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
                MEMORY_OBJECT_NULL,
                0,
                0,
                VM_PROT_NONE,
                VM_PROT_ALL,
                VM_INHERIT_NONE,
            )
        };
        result == libc::KERN_SUCCESS && address == at as libc::mach_vm_address_t
    }

    pub(super) fn section_destroy(section: Section) {
        // SAFETY: the window of a section given up; views share the memory
        // object and keep it on their own.
        unsafe {
            mach_vm_deallocate(
                mach_task_self_,
                section.window as libc::mach_vm_address_t,
                section.len as libc::mach_vm_size_t,
            )
        };
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::{Memory, Protection};

    fn byte(at: usize) -> u8 {
        let mut b = [0u8];
        assert!(Host::read_into(at, &mut b), "{at:#x} readable");
        b[0]
    }

    #[test]
    fn a_section_is_shown_where_asked_and_outlives_its_window() {
        let page = Host::page_size();
        let section = Host::section_create(4 * page).expect("a section");
        assert_eq!(section.len, 4 * page);
        // SAFETY: the section's own window, read-write, 4 pages long.
        unsafe {
            for i in 0..4 {
                ((section.window + i * page) as *mut u8).write_bytes(i as u8 + 1, page);
            }
        }
        // The title's reservation.
        let reservation = Host::allocate(8 * page, None).expect("8 pages");
        // SAFETY: our fresh allocation, which nothing refers into.
        assert!(unsafe { Host::protect(reservation, 8 * page, Protection::NoAccess) });

        let view = reservation + 2 * page;
        assert!(Host::section_map(&section, page, 2 * page, view));
        assert_eq!(byte(view), 2);
        assert_eq!(byte(view + 2 * page - 1), 3);
        // SAFETY: the view, read-write.
        unsafe { (view as *mut u8).write(0x77) };
        assert_eq!(byte(section.window + page), 0x77, "the window shows it");

        let second = reservation + 6 * page;
        assert!(Host::section_map(&section, page, page, second));
        assert_eq!(byte(second), 0x77);
        assert_eq!(byte(view), 0x77);
        assert!(
            !Host::section_map(&section, 3 * page, 2 * page, second),
            "past the end"
        );

        assert!(Host::unmap_in_place(view, 2 * page));
        let mut b = [0u8];
        assert!(!Host::read_into(view, &mut b), "no access");
        assert_eq!(
            Host::allocate(page, Some(view)),
            None,
            "still ours, no hole"
        );

        Host::section_destroy(section);
        assert_eq!(byte(second), 0x77, "the view outlives the window");
        // SAFETY: the whole reservation, no longer used.
        assert!(unsafe { Host::free(reservation, 8 * page) });
    }
}
