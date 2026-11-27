//! The low 4 GiB of the address space, which titles ask for memory in
//! (`ADDRESS_32BIT`). The host's own things are kept out of it by reserving
//! what is free there at start ([`reserve`]); titles' requests are then
//! carved out of what was reserved ([`carve`]).

use std::sync::{Mutex, MutexGuard};

use crate::{Host, Memory, Protection};

/// One past the highest address of the low range.
pub const LOW_LIMIT: usize = 1 << 32;

/// Below this nothing is handed out: the null page and Linux's `vm.mmap_min_addr`.
pub const LOW_FLOOR: usize = 0x1_0000;

/// A range of addresses: `[start, start + len)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// The first address.
    pub start: usize,
    /// Its length in bytes.
    pub len: usize,
}

/// The pieces of the reserved ranges still available to [`carve`], lowest
/// first. Each is mapped with no access until carved.
static RESERVED: Mutex<Vec<Range>> = Mutex::new(Vec::new());

/// What [`carve`] handed out and [`give_back`] has not taken back, each as
/// carved (its length in whole pages).
static CARVED: Mutex<Vec<Range>> = Mutex::new(Vec::new());

/// The reserved pieces. Nothing in this module panics while holding the
/// lock, so a poisoned lock still holds a whole list and is used as it is.
fn reserved() -> MutexGuard<'static, Vec<Range>> {
    RESERVED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The ranges between LOW_FLOOR and LOW_LIMIT with nothing mapped, lowest
/// first, page-granular, from the host's map of this process.
pub fn free_ranges() -> Vec<Range> {
    free_ranges_between(LOW_FLOOR, LOW_LIMIT)
}

/// The ranges within `[low, high)` with nothing mapped, lowest first,
/// page-granular, from the host's map of this process.
pub fn free_ranges_between(low: usize, high: usize) -> Vec<Range> {
    if high <= low {
        return Vec::new();
    }
    gaps(mapped_ranges(low, high), Host::page_size(), low, high)
}

/// Reserves every free range below 4 GiB for titles (read-write memory from
/// `Memory::allocate` at its address, then no access until `carve` hands a
/// piece out), so nothing of the host's lands there later; returns what was
/// taken now. A second call takes only what became free since.
pub fn reserve() -> Vec<Range> {
    let mut pieces = reserved();
    let mut taken = Vec::new();
    for range in free_ranges() {
        // Something else may have mapped part of it since the map was read:
        // the allocation then fails and the range is left to its owner.
        if Host::allocate(range.len, Some(range.start)) != Some(range.start) {
            continue;
        }
        // A refusal leaves the range read-write, but still ours: it is kept
        // all the same, and made read-write again when carved.
        // SAFETY: the whole of the allocation just made, which nothing
        // refers into yet.
        unsafe { Host::protect(range.start, range.len, Protection::NoAccess) };
        pieces.push(range);
        taken.push(range);
    }
    pieces.sort_by_key(|r| r.start);
    taken
}

/// `len` bytes (rounded up to pages) at an address that is a multiple of
/// `align` (a power of two, at least a page), from the reserved ranges,
/// lowest fit first, made read-write; `None` when nothing fits.
pub fn carve(len: usize, align: usize) -> Option<usize> {
    let page = Host::page_size();
    if len == 0 || !align.is_power_of_two() || align < page {
        return None;
    }
    let len = len.checked_add(page - 1)? & !(page - 1);
    let mut pieces = reserved();
    let (index, at) = pieces.iter().enumerate().find_map(|(i, piece)| {
        let at = piece.start.checked_add(align - 1)? & !(align - 1);
        let end = at.checked_add(len)?;
        (end <= piece.start + piece.len).then_some((i, at))
    })?;
    // SAFETY: a range inside a reserved piece: memory from `allocate`, with
    // no access until now, which nothing refers into.
    if !unsafe { Host::protect(at, len, Protection::ReadWrite) } {
        return None;
    }
    let piece = pieces[index];
    let before = Range {
        start: piece.start,
        len: at - piece.start,
    };
    let after = Range {
        start: at + len,
        len: piece.start + piece.len - (at + len),
    };
    let rest: Vec<Range> = [before, after].into_iter().filter(|r| r.len > 0).collect();
    pieces.splice(index..=index, rest);
    // Taken after RESERVED where both are.
    CARVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Range { start: at, len });
    Some(at)
}

/// Takes `[base, base + len)` (`base` a multiple of the page size, `len`
/// rounded up to pages) out of the reserved piece it lies in, splitting the
/// piece, and makes it read-write, as [`carve`] does: for a title that asks
/// for memory at an address of its own. False, taking nothing, when the
/// range does not lie wholly within one piece still held (not reserved, or
/// carved already). What it takes goes back with [`give_back`].
pub fn carve_at(base: usize, len: usize) -> bool {
    let page = Host::page_size();
    if len == 0 || !base.is_multiple_of(page) {
        return false;
    }
    let Some(len) = len.checked_next_multiple_of(page) else {
        return false;
    };
    let Some(end) = base.checked_add(len) else {
        return false;
    };
    let mut pieces = reserved();
    let Some(index) = pieces
        .iter()
        .position(|piece| piece.start <= base && end <= piece.start + piece.len)
    else {
        return false;
    };
    // SAFETY: a range inside a reserved piece: memory from `allocate`, with
    // no access until now, which nothing refers into.
    if !unsafe { Host::protect(base, len, Protection::ReadWrite) } {
        return false;
    }
    let piece = pieces[index];
    let before = Range {
        start: piece.start,
        len: base - piece.start,
    };
    let after = Range {
        start: end,
        len: piece.start + piece.len - end,
    };
    let rest: Vec<Range> = [before, after].into_iter().filter(|r| r.len > 0).collect();
    pieces.splice(index..=index, rest);
    // Taken after RESERVED where both are.
    CARVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Range { start: base, len });
    true
}

/// Returns a range [`carve`] handed out (`start` as carved, `len` as asked
/// or as rounded) to the reserved pieces, with no access again and nothing
/// backing it; false if it is not one `carve` handed out (or was given back
/// already).
pub fn give_back(start: usize, len: usize) -> bool {
    use crate::sections::Sections;
    let page = Host::page_size();
    let Some(len) = len.checked_next_multiple_of(page) else {
        return false;
    };
    let mut pieces = reserved();
    {
        let mut carved = CARVED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(i) = carved.iter().position(|r| *r == Range { start, len }) else {
            return false;
        };
        carved.swap_remove(i);
    }
    // Fresh memory in place, so that what the title left there is dropped
    // and nothing of the host's can land in the range meanwhile; if the
    // host refuses, the pages only lose access and keep what they held.
    if !Host::unmap_in_place(start, len) {
        // SAFETY: a range `carve` handed out and its user gave back, which
        // nothing refers into any more.
        unsafe { Host::protect(start, len, Protection::NoAccess) };
    }
    // Back among the pieces, joined to its neighbours.
    let at = pieces.partition_point(|r| r.start < start);
    pieces.insert(at, Range { start, len });
    if at + 1 < pieces.len() && start + len == pieces[at + 1].start {
        pieces[at].len += pieces[at + 1].len;
        pieces.remove(at + 1);
    }
    if at > 0 && pieces[at - 1].start + pieces[at - 1].len == start {
        pieces[at - 1].len += pieces[at].len;
        pieces.remove(at);
    }
    true
}

/// The unmapped, page-granular ranges within `[low, high)`, given the
/// mapped ranges as `(start, end)` in any order.
fn gaps(mut mapped: Vec<(usize, usize)>, page: usize, low: usize, high: usize) -> Vec<Range> {
    mapped.sort_unstable();
    let mut free = Vec::new();
    let mut push = |start: usize, end: usize| {
        // Rounded inwards to pages; what is left of less than a page is not
        // a range.
        let start = start.next_multiple_of(page);
        let end = end - end % page;
        if end > start {
            free.push(Range {
                start,
                len: end - start,
            });
        }
    };
    let mut cursor = low;
    for (start, end) in mapped {
        if start >= high {
            break;
        }
        if start > cursor {
            push(cursor, start);
        }
        cursor = cursor.max(end);
    }
    if cursor < high {
        push(cursor, high);
    }
    free
}

/// The mapped ranges, `(start, end)`, of `/proc/self/maps` (`start-end` in
/// hex at the start of each line): all of them, whatever the bounds, which
/// [`gaps`] applies; none if it cannot be read.
#[cfg(target_os = "linux")]
fn mapped_ranges(_low: usize, _high: usize) -> Vec<(usize, usize)> {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return Vec::new();
    };
    maps.lines()
        .filter_map(|line| {
            let (range, _) = line.split_once(' ')?;
            let (start, end) = range.split_once('-')?;
            Some((
                usize::from_str_radix(start, 16).ok()?,
                usize::from_str_radix(end, 16).ok()?,
            ))
        })
        .collect()
}

#[cfg(target_os = "macos")]
mod mach {
    /// `<mach/vm_region.h>`: the flavor of `vm_region_basic_info_64`.
    pub const VM_REGION_BASIC_INFO_64: libc::c_int = 9;

    /// `<mach/vm_region.h>`, inside its `#pragma pack(push, 4)`:
    ///
    /// ```c
    /// struct vm_region_basic_info_64 {
    ///     vm_prot_t               protection;
    ///     vm_prot_t               max_protection;
    ///     vm_inherit_t            inheritance;
    ///     boolean_t               shared;
    ///     boolean_t               reserved;
    ///     memory_object_offset_t  offset;
    ///     vm_behavior_t           behavior;
    ///     unsigned short          user_wired_count;
    /// };
    /// #define VM_REGION_BASIC_INFO_COUNT_64 ((mach_msg_type_number_t) \
    ///     (sizeof(vm_region_basic_info_data_64_t)/sizeof(int)))
    /// ```
    ///
    /// `vm_prot_t` and `vm_behavior_t` are `int`, `vm_inherit_t` is
    /// `unsigned int`, `boolean_t` a 32-bit integer and
    /// `memory_object_offset_t` a `unsigned long long`: 36 bytes, a count
    /// of 9.
    #[repr(C, packed(4))]
    #[derive(Default)]
    pub struct BasicInfo64 {
        pub protection: i32,
        pub max_protection: i32,
        pub inheritance: u32,
        pub shared: u32,
        pub reserved: u32,
        pub offset: u64,
        pub behavior: i32,
        pub user_wired_count: u16,
    }

    const _: () = assert!(size_of::<BasicInfo64>() == 36);

    /// `VM_REGION_BASIC_INFO_COUNT_64`.
    pub const BASIC_INFO_64_COUNT: u32 = (size_of::<BasicInfo64>() / 4) as u32;

    unsafe extern "C" {
        /// `<mach/mach_init.h>`: this task's port, which libSystem sets
        /// before `main` and never changes.
        pub static mach_task_self_: libc::mach_port_t;

        /// `<mach-o/getsect.h>` (libmacho, in libSystem): the main
        /// executable's segment of that name from its own Mach-O header, or
        /// null when it has none.
        pub fn getsegbyname(segname: *const libc::c_char) -> *const libc::segment_command_64;

        /// `<mach/mach_vm.h>`: the first region of `target` at or after
        /// `*address`, its start and size written to `*address` and
        /// `*size` and its `flavor` of information to `info` (`*count`
        /// ints long); `KERN_INVALID_ADDRESS` when there is none.
        pub fn mach_vm_region(
            target: libc::vm_map_t,
            address: *mut libc::mach_vm_address_t,
            size: *mut libc::mach_vm_size_t,
            flavor: libc::c_int,    // vm_region_flavor_t
            info: *mut libc::c_int, // vm_region_info_t
            count: *mut u32,        // mach_msg_type_number_t
            object_name: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
}

/// The task's regions from `low` (or the end of the executable's
/// `__PAGEZERO`, if higher) to `high`, `(start, end)`: `__PAGEZERO` first,
/// then what `mach_vm_region` reports, which returns each region at or
/// after the address it is given.
#[cfg(target_os = "macos")]
fn mapped_ranges(low: usize, high: usize) -> Vec<(usize, usize)> {
    let mut mapped = Vec::new();
    let page_zero = page_zero();
    if page_zero > 0 {
        mapped.push((0, page_zero));
    }
    let mut address = low.max(page_zero) as libc::mach_vm_address_t;
    while address < high as libc::mach_vm_address_t {
        let mut size: libc::mach_vm_size_t = 0;
        let mut info = mach::BasicInfo64::default();
        let mut count = mach::BASIC_INFO_64_COUNT;
        let mut object_name: libc::mach_port_t = 0;
        // SAFETY: a query about our own task into our own variables, `info`
        // being `count` ints long. The task port is set before `main`.
        let result = unsafe {
            mach::mach_vm_region(
                mach::mach_task_self_,
                &mut address,
                &mut size,
                mach::VM_REGION_BASIC_INFO_64,
                (&raw mut info).cast(),
                &mut count,
                &mut object_name,
            )
        };
        // KERN_INVALID_ADDRESS: no region at or after `address`. Any other
        // failure ends the walk the same way.
        if result != libc::KERN_SUCCESS || address >= high as libc::mach_vm_address_t {
            break;
        }
        let Some(end) = address.checked_add(size) else {
            break;
        };
        mapped.push((address as usize, end as usize));
        address = end;
    }
    mapped
}

/// The size of the executable's `__PAGEZERO` segment, which the kernel maps
/// nothing for: it raises the floor of the task's map instead
/// (`vm_map_raise_min_offset` in xnu's `mach_loader.c`), so `mach_vm_region`
/// never reports it, yet an allocation inside it fails. It is 4 GiB unless
/// the link said otherwise, as apps/victory's build script does.
#[cfg(target_os = "macos")]
fn page_zero() -> usize {
    // SAFETY: a NUL-terminated name; what is returned points into the
    // executable's own header, mapped for the life of the process.
    unsafe {
        let segment = mach::getsegbyname(c"__PAGEZERO".as_ptr());
        if segment.is_null() {
            0
        } else {
            (*segment).vmsize as usize
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn gaps_lie_between_mappings_and_within_the_range() {
        let page = 0x1000;
        let mapped = vec![
            (0x40_0000, 0x50_0000),
            (0x0, 0x2_0000),
            (0x4_0000, 0x4_1000),
        ];
        let free = gaps(mapped, page, LOW_FLOOR, LOW_LIMIT);
        assert_eq!(
            free,
            [
                Range {
                    start: 0x2_0000,
                    len: 0x2_0000
                },
                Range {
                    start: 0x4_1000,
                    len: 0x40_0000 - 0x4_1000
                },
                Range {
                    start: 0x50_0000,
                    len: LOW_LIMIT - 0x50_0000
                },
            ]
        );
        assert_eq!(gaps(vec![(0, usize::MAX)], page, LOW_FLOOR, LOW_LIMIT), []);
        // Other bounds: only what lies within them.
        assert_eq!(
            gaps(vec![(0x4_0000, 0x5_0000)], page, 0x3_0000, 0x6_0000),
            [
                Range {
                    start: 0x3_0000,
                    len: 0x1_0000
                },
                Range {
                    start: 0x5_0000,
                    len: 0x1_0000
                },
            ]
        );
    }

    #[test]
    fn carve_at_takes_only_what_is_reserved() {
        let page = Host::page_size();
        // Below the floor nothing is ever reserved; nor above the low range.
        assert!(!carve_at(0, page));
        assert!(!carve_at(LOW_FLOOR - page, page));
        assert!(!carve_at(LOW_LIMIT, page));
        // Not a whole page, or nothing at all.
        assert!(!carve_at(LOW_FLOOR + 1, page));
        assert!(!carve_at(LOW_FLOOR, 0));
        assert!(!carve_at(usize::MAX - page + 1, page));
    }
}
