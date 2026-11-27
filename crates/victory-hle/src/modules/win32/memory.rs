//! `VirtualAlloc` and its family over the host's memory
//! (`victory_platform::Memory`), with reservations and their pages' states
//! kept here, since the host has no reserve/commit distinction; the process
//! heaps over Rust's allocator.
//!
//! Only memory reserved through these functions is known here: what the
//! host, Rust or the loader own looks free to `VirtualQuery`.
#![allow(non_snake_case, non_camel_case_types)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]

use std::alloc::Layout;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard, Once};

use victory_platform::system::System;
use victory_platform::{Host, Memory, Protection};

use crate::{HleExport, hle_export};

type HANDLE = *mut c_void;
type LPVOID = *mut c_void;
type BOOL = i32;
type DWORD = u32;
type SIZE_T = usize;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

pub(crate) const MEM_COMMIT: u32 = 0x1000;
pub(crate) const MEM_RESERVE: u32 = 0x2000;
pub(crate) const MEM_DECOMMIT: u32 = 0x4000;
pub(crate) const MEM_RELEASE: u32 = 0x8000;
pub(crate) const MEM_FREE: u32 = 0x10000;
pub(crate) const MEM_PRIVATE: u32 = 0x20000;
/// The pages' contents may be dropped (advice: nothing is done here).
const MEM_RESET: u32 = 0x8_0000;
/// The pages' contents are wanted again after a `MEM_RESET` (they were kept).
const MEM_RESET_UNDO: u32 = 0x100_0000;
pub(crate) const MEM_TOP_DOWN: u32 = 0x100000;
pub(crate) const MEM_LARGE_PAGES: u32 = 0x20000000;

pub(crate) const PAGE_NOACCESS: u32 = 1;
pub(crate) const PAGE_READONLY: u32 = 2;
pub(crate) const PAGE_READWRITE: u32 = 4;
pub(crate) const PAGE_WRITECOPY: u32 = 8;
pub(crate) const PAGE_EXECUTE: u32 = 0x10;
pub(crate) const PAGE_EXECUTE_READ: u32 = 0x20;
pub(crate) const PAGE_EXECUTE_READWRITE: u32 = 0x40;
pub(crate) const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
pub(crate) const PAGE_GUARD: u32 = 0x100;
pub(crate) const PAGE_NOCACHE: u32 = 0x200;
pub(crate) const PAGE_WRITECOMBINE: u32 = 0x400;

/// Windows' allocation granularity: reservations start at multiples of it.
pub(crate) const GRANULARITY: usize = 0x10000;

/// One past the highest address `VirtualQuery` reports on.
const ADDRESS_LIMIT: usize = 0x7FFF_FFFF_0000;
/// The highest address a title may use, as `GetSystemInfo` reports it.
const MAX_APPLICATION_ADDRESS: u64 = 0x7FFF_FFFE_FFFF;

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_NO_MORE_ITEMS: u32 = 259;
const ERROR_INVALID_ADDRESS: u32 = 487;
const ERROR_NOACCESS: u32 = 998;

/// `GetCurrentProcess()`'s pseudo handle: the only process we serve.
const CURRENT_PROCESS: HANDLE = usize::MAX as HANDLE;

/// Sets this thread's last error in its TEB; nothing on a thread without one.
fn set_last_error(code: u32) {
    if let Some(teb) = victory_loader::teb::current_teb() {
        // SAFETY: this thread's own TEB, which only this thread writes.
        unsafe { (*(teb as *mut victory_loader::teb::Teb)).last_error = code };
    }
}

/// The host protection a Windows page protection asks for; `None` for one
/// Windows refuses. Caching attributes have no meaning here and are
/// dropped. A guard page becomes a page with no access: the guard itself
/// (one fault, then the page's own protection) is not kept.
fn protection(page_protect: u32) -> Option<Protection> {
    let page_protect = page_protect & !(PAGE_NOCACHE | PAGE_WRITECOMBINE);
    if page_protect & PAGE_GUARD != 0 {
        protection(page_protect & !PAGE_GUARD)?;
        return Some(Protection::NoAccess);
    }
    Some(match page_protect {
        PAGE_NOACCESS => Protection::NoAccess,
        PAGE_READONLY => Protection::Read,
        PAGE_READWRITE | PAGE_WRITECOPY => Protection::ReadWrite,
        PAGE_EXECUTE | PAGE_EXECUTE_READ => Protection::ReadExecute,
        PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY => Protection::ReadWriteExecute,
        _ => return None,
    })
}

/// One page of a reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Page {
    committed: bool,
    /// The Windows protection it was committed with (0 while reserved).
    protect: u32,
}

/// One reservation, keyed in [`REGIONS`] by its base.
#[derive(Debug)]
struct Region {
    /// Its length in bytes, a whole number of pages.
    size: usize,
    /// The protection `VirtualAlloc` was given when it was reserved.
    allocation_protect: u32,
    pages: Vec<Page>,
    /// The host allocation it lies in, given back whole on release: with
    /// the reservation's address chosen by the host, it has slack before
    /// and after the reservation that is not part of the region.
    host_start: usize,
    host_len: usize,
    /// Carved out of the low 4 GiB the runner reserved
    /// (`victory_platform::low`), and given back there on release.
    low: bool,
}

static REGIONS: Mutex<BTreeMap<usize, Region>> = Mutex::new(BTreeMap::new());

/// The reservations. Nothing here panics while holding the lock, so a
/// poisoned lock still holds a whole map and is used as it is.
fn regions() -> MutexGuard<'static, BTreeMap<usize, Region>> {
    REGIONS.lock().unwrap_or_else(|e| e.into_inner())
}

fn page() -> usize {
    Host::page_size()
}

fn round_up(value: usize, to: usize) -> Option<usize> {
    Some(value.checked_add(to - 1)? & !(to - 1))
}

/// The region `address` lies in: its base and itself.
fn region_at(map: &mut BTreeMap<usize, Region>, address: usize) -> Option<(usize, &mut Region)> {
    let (&base, region) = map.range_mut(..=address).next_back()?;
    (address < base + region.size).then_some((base, region))
}

/// The pages `[address, address + size)` covers in the region at `base` of
/// `pages` pages: the first page's index and one past the last's; `None`
/// if the range leaves the region.
fn page_range(base: usize, pages: usize, address: usize, size: usize) -> Option<(usize, usize)> {
    let page = page();
    let first = (address - base) / page;
    let end = round_up(address.checked_add(size)?, page)?;
    let last = (end - base).div_ceil(page);
    (last <= pages).then_some((first, last))
}

/// Applies `f` to each run of consecutive pages in `[first, last)` for
/// which `select` holds: the run's first index and length.
fn runs(
    pages: &[Page],
    first: usize,
    last: usize,
    select: impl Fn(&Page) -> bool,
    mut f: impl FnMut(usize, usize),
) {
    let mut i = first;
    while i < last {
        if !select(&pages[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < last && select(&pages[i]) {
            i += 1;
        }
        f(start, i - start);
    }
}

/// Reserves `size` bytes, at `address` (rounded down to the granularity)
/// or wherever the host has room: the region's base.
fn reserve(
    map: &mut BTreeMap<usize, Region>,
    address: usize,
    size: usize,
    protect: u32,
) -> Result<usize, u32> {
    let page = page();
    let (base, len, host_start, host_len, low) = if address == 0 {
        let len = round_up(size, page).ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
        let host_len = len
            .checked_add(GRANULARITY)
            .ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
        let host_start = Host::allocate(host_len, None).ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
        let base = round_up(host_start, GRANULARITY).ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
        (base, len, host_start, host_len, false)
    } else {
        let base = address & !(GRANULARITY - 1);
        let end = address
            .checked_add(size)
            .and_then(|end| round_up(end, page))
            .ok_or(ERROR_INVALID_ADDRESS)?;
        let len = end - base;
        // Below 4 GiB the runner reserved what was free at start: the range
        // is taken out of that reservation (and given back to it on
        // release). Where nothing was reserved (no runner: tests), the host
        // is asked for it as above 4 GiB.
        let low =
            base < victory_platform::low::LOW_LIMIT && victory_platform::low::carve_at(base, len);
        if !low && Host::allocate(len, Some(base)) != Some(base) {
            return Err(ERROR_INVALID_ADDRESS);
        }
        (base, len, base, len, low)
    };
    // Every page starts reserved: no access until committed. A refusal
    // leaves them read-write, which no title relies on; the region stays.
    // SAFETY: part of the allocation just made, which nothing refers into.
    unsafe { Host::protect(base, len, Protection::NoAccess) };
    let pages = vec![
        Page {
            committed: false,
            protect: 0,
        };
        len / page
    ];
    map.insert(
        base,
        Region {
            size: len,
            allocation_protect: protect,
            pages,
            host_start,
            host_len,
            low,
        },
    );
    Ok(base)
}

/// Reserves `round_up(size, page)` bytes at an `align`-aligned base with
/// the whole reservation within `[low, high)`: below 4 GiB from what the
/// runner reserved there at start (`victory_platform::low::carve`), else
/// in the first free range of the host's map that fits. The region's base.
// `xmem` reserves through `virtual_alloc_within`; this is for callers that
// reserve without committing, none yet.
#[allow(dead_code)]
pub(crate) fn reserve_within(
    low: usize,
    high: usize,
    size: usize,
    align: usize,
    protect: u32,
) -> Result<usize, u32> {
    reserve_within_in(&mut regions(), low, high, size, align, protect)
}

/// [`reserve_within`] with the reservations held.
fn reserve_within_in(
    map: &mut BTreeMap<usize, Region>,
    low: usize,
    high: usize,
    size: usize,
    align: usize,
    protect: u32,
) -> Result<usize, u32> {
    let page = page();
    if size == 0 || !align.is_power_of_two() {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let align = align.max(page);
    let len = round_up(size, page).ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
    let fits = |base: usize| base >= low && base.checked_add(len).is_some_and(|end| end <= high);
    let (base, from_low) = if high <= 1 << 32 {
        let base = victory_platform::low::carve(len, align).ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
        if !fits(base) {
            victory_platform::low::give_back(base, len);
            return Err(ERROR_NOT_ENOUGH_MEMORY);
        }
        (base, true)
    } else {
        // Another allocation may take a range between the look and the
        // allocation: the next range is tried then.
        let base = victory_platform::low::free_ranges_between(low, high)
            .into_iter()
            .find_map(|range| {
                let base = round_up(range.start, align)?;
                let end = base.checked_add(len)?;
                if end > range.start + range.len || !fits(base) {
                    return None;
                }
                (Host::allocate(len, Some(base)) == Some(base)).then_some(base)
            })
            .ok_or(ERROR_NOT_ENOUGH_MEMORY)?;
        (base, false)
    };
    // Every page starts reserved, as in `reserve`. A refusal leaves them
    // read-write, which no title relies on; the region stays.
    // SAFETY: the memory just allocated or carved, which nothing refers
    // into.
    unsafe { Host::protect(base, len, Protection::NoAccess) };
    let pages = vec![
        Page {
            committed: false,
            protect: 0,
        };
        len / page
    ];
    map.insert(
        base,
        Region {
            size: len,
            allocation_protect: protect,
            pages,
            host_start: base,
            host_len: len,
            low: from_low,
        },
    );
    Ok(base)
}

/// Commits the pages of `[address, address + size)`, which must lie in one
/// region, with `protect`: fresh pages are zeroed, committed ones keep
/// their contents, and the whole range gets `protect` (as on Windows). The
/// first page's address.
fn commit(
    map: &mut BTreeMap<usize, Region>,
    address: usize,
    size: usize,
    protect: u32,
    host: Protection,
) -> Result<usize, u32> {
    let page = page();
    let (base, region) = region_at(map, address).ok_or(ERROR_INVALID_ADDRESS)?;
    let (first, last) =
        page_range(base, region.pages.len(), address, size).ok_or(ERROR_INVALID_ADDRESS)?;
    let mut refused = false;
    runs(
        &region.pages,
        first,
        last,
        |p| !p.committed,
        |start, count| {
            let at = base + start * page;
            let len = count * page;
            // SAFETY: pages of our reservation that nothing refers into
            // (uncommitted): made writable and zeroed (a decommitted page
            // still holds what it had).
            unsafe {
                refused |= !Host::protect(at, len, Protection::ReadWrite);
                if !refused {
                    (at as *mut u8).write_bytes(0, len);
                }
            }
        },
    );
    // The whole range, fresh pages and committed ones, gets what was asked.
    // SAFETY: pages of our reservation, which the title commits with this
    // protection.
    if refused || !unsafe { Host::protect(base + first * page, (last - first) * page, host) } {
        return Err(ERROR_NOT_ENOUGH_MEMORY);
    }
    region.pages[first..last].fill(Page {
        committed: true,
        protect,
    });
    Ok(base + first * page)
}

/// `MEM_RESET` and `MEM_RESET_UNDO`, which Windows takes with no other
/// flag: none when `kind` asks for neither. The range must be committed
/// pages of one region; its page-aligned start is returned and nothing is
/// done to the pages (the reset is advice: their contents are kept).
fn reset(
    map: &mut BTreeMap<usize, Region>,
    address: usize,
    size: usize,
    kind: u32,
) -> Option<Result<usize, u32>> {
    if kind & (MEM_RESET | MEM_RESET_UNDO) == 0 {
        return None;
    }
    if kind & (MEM_RESERVE | MEM_COMMIT) != 0 {
        return Some(Err(ERROR_INVALID_PARAMETER));
    }
    let committed = region_at(map, address).and_then(|(base, region)| {
        let (first, last) = page_range(base, region.pages.len(), address, size)?;
        let pages = &region.pages[first..last];
        pages
            .iter()
            .all(|p| p.committed)
            .then_some(base + first * page())
    });
    Some(committed.ok_or(ERROR_INVALID_ADDRESS))
}

fn virtual_alloc(address: usize, size: usize, kind: u32, protect: u32) -> Result<usize, u32> {
    // Where the host puts memory and with which pages is its own choice.
    let kind = kind & !(MEM_TOP_DOWN | MEM_LARGE_PAGES);
    if size == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let host = protection(protect).ok_or(ERROR_INVALID_PARAMETER)?;
    let mut map = regions();
    if let Some(reset) = reset(&mut map, address, size, kind) {
        return reset;
    }
    match (kind & MEM_RESERVE != 0, kind & MEM_COMMIT != 0) {
        // A null address with `MEM_COMMIT` alone reserves too, as on Windows.
        (reserve, commit_too) if reserve || address == 0 => {
            let base = self::reserve(&mut map, address, size, protect)?;
            if commit_too {
                let len = map.get(&base).map_or(0, |r| r.size);
                if let Err(code) = commit(&mut map, base, len, protect, host) {
                    release(&mut map, base);
                    return Err(code);
                }
            }
            Ok(base)
        }
        (false, true) if address != 0 => commit(&mut map, address, size, protect, host),
        _ => Err(ERROR_INVALID_PARAMETER),
    }
}

/// As [`virtual_alloc`], but a reservation at no address of the title's
/// lies within `[low, high)` at an `align`-aligned base
/// ([`reserve_within`]).
pub(crate) fn virtual_alloc_within(
    address: usize,
    size: usize,
    kind: u32,
    protect: u32,
    low: usize,
    high: usize,
    align: usize,
) -> Result<usize, u32> {
    let kind = kind & !(MEM_TOP_DOWN | MEM_LARGE_PAGES);
    if size == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let host = protection(protect).ok_or(ERROR_INVALID_PARAMETER)?;
    let mut map = regions();
    if let Some(reset) = reset(&mut map, address, size, kind) {
        return reset;
    }
    match (kind & MEM_RESERVE != 0, kind & MEM_COMMIT != 0) {
        // A null address with `MEM_COMMIT` alone reserves too, as on Windows.
        (reserve, commit_too) if reserve || address == 0 => {
            let base = if address == 0 {
                reserve_within_in(&mut map, low, high, size, align, protect)?
            } else {
                self::reserve(&mut map, address, size, protect)?
            };
            if commit_too {
                let len = map.get(&base).map_or(0, |r| r.size);
                if let Err(code) = commit(&mut map, base, len, protect, host) {
                    release(&mut map, base);
                    return Err(code);
                }
            }
            Ok(base)
        }
        (false, true) if address != 0 => commit(&mut map, address, size, protect, host),
        _ => Err(ERROR_INVALID_PARAMETER),
    }
}

/// Records the pages of `[address, address + len)`, which must lie in one
/// region, as committed with `protect`: they show other memory now (a view
/// of physical pages), which the caller maps there. Nothing is done to the
/// host. False if the range is not in one region.
pub(crate) fn mark_mapped(address: usize, len: usize, protect: u32) -> bool {
    let to = Page {
        committed: true,
        protect,
    };
    mark(address, len, to)
}

/// Records the pages of `[address, address + len)`, which must lie in one
/// region, as reserved again (the caller took what they showed away).
/// Nothing is done to the host. False if the range is not in one region.
pub(crate) fn mark_reserved(address: usize, len: usize) -> bool {
    let to = Page {
        committed: false,
        protect: 0,
    };
    mark(address, len, to)
}

/// Gives the pages of `[address, address + len)` the state `to`, if they
/// lie in one region.
fn mark(address: usize, len: usize, to: Page) -> bool {
    let mut map = regions();
    let Some((base, region)) = region_at(&mut map, address) else {
        return false;
    };
    let Some((first, last)) = page_range(base, region.pages.len(), address, len.max(1)) else {
        return false;
    };
    region.pages[first..last].fill(to);
    true
}

/// What `VirtualQuery` says about the page `address` is in.
pub(crate) fn query(address: usize) -> MEMORY_BASIC_INFORMATION {
    virtual_query(address)
}

/// Gives back the region at `base` and its whole host allocation.
fn release(map: &mut BTreeMap<usize, Region>, base: usize) -> bool {
    let Some(region) = map.remove(&base) else {
        return false;
    };
    if region.low {
        return victory_platform::low::give_back(region.host_start, region.host_len);
    }
    // SAFETY: the whole host allocation of a region the title released, so
    // nothing uses it any more.
    unsafe { Host::free(region.host_start, region.host_len) }
}

/// Reserves or commits pages (see the module doc).
pub extern "win64" fn VirtualAlloc(
    address: LPVOID,
    size: SIZE_T,
    kind: DWORD,
    protect: DWORD,
) -> LPVOID {
    match virtual_alloc(address as usize, size, kind, protect) {
        Ok(at) => at as LPVOID,
        Err(code) => {
            set_last_error(code);
            std::ptr::null_mut()
        }
    }
}

/// [`VirtualAlloc`] in this process (the only one served).
pub extern "win64" fn VirtualAllocEx(
    process: HANDLE,
    address: LPVOID,
    size: SIZE_T,
    kind: DWORD,
    protect: DWORD,
) -> LPVOID {
    if process != CURRENT_PROCESS {
        set_last_error(ERROR_INVALID_HANDLE);
        return std::ptr::null_mut();
    }
    VirtualAlloc(address, size, kind, protect)
}

fn virtual_free(address: usize, size: usize, kind: u32) -> Result<(), u32> {
    let mut map = regions();
    match kind {
        MEM_RELEASE => {
            if size != 0 {
                return Err(ERROR_INVALID_PARAMETER);
            }
            let Some(size) = map.get(&address).map(|r| r.size) else {
                return Err(ERROR_INVALID_ADDRESS);
            };
            release(&mut map, address);
            // After the regions lock: `xmem` takes its own lock first, then
            // ours, never the other way round.
            drop(map);
            crate::modules::xmem::forget_views(address, size);
            Ok(())
        }
        MEM_DECOMMIT => {
            let page = page();
            let (base, region) = region_at(&mut map, address).ok_or(ERROR_INVALID_ADDRESS)?;
            let size = if size == 0 {
                base + region.size - address
            } else {
                size
            };
            let (first, last) =
                page_range(base, region.pages.len(), address, size).ok_or(ERROR_INVALID_ADDRESS)?;
            // The pages lose access but keep what they held until committed
            // again, when they are zeroed.
            // SAFETY: pages of our reservation the title gives up.
            unsafe {
                Host::protect(
                    base + first * page,
                    (last - first) * page,
                    Protection::NoAccess,
                )
            };
            for p in &mut region.pages[first..last] {
                *p = Page {
                    committed: false,
                    protect: 0,
                };
            }
            Ok(())
        }
        _ => Err(ERROR_INVALID_PARAMETER),
    }
}

/// Releases a reservation or decommits pages of one.
pub extern "win64" fn VirtualFree(address: LPVOID, size: SIZE_T, kind: DWORD) -> BOOL {
    match virtual_free(address as usize, size, kind) {
        Ok(()) => TRUE,
        Err(code) => {
            set_last_error(code);
            FALSE
        }
    }
}

/// [`VirtualFree`] in this process (the only one served).
pub extern "win64" fn VirtualFreeEx(
    process: HANDLE,
    address: LPVOID,
    size: SIZE_T,
    kind: DWORD,
) -> BOOL {
    if process != CURRENT_PROCESS {
        set_last_error(ERROR_INVALID_HANDLE);
        return FALSE;
    }
    VirtualFree(address, size, kind)
}

fn virtual_protect(address: usize, size: usize, new_protect: u32) -> Result<u32, u32> {
    let host = protection(new_protect).ok_or(ERROR_INVALID_PARAMETER)?;
    let page = page();
    let mut map = regions();
    let Some((base, region)) = region_at(&mut map, address) else {
        drop(map);
        return protect_image(address, size, host);
    };
    // A size of 0 still names the page `address` is in.
    let (first, last) =
        page_range(base, region.pages.len(), address, size.max(1)).ok_or(ERROR_INVALID_ADDRESS)?;
    let range = &mut region.pages[first..last];
    if !range.iter().all(|p| p.committed) {
        return Err(ERROR_INVALID_ADDRESS);
    }
    let old = range[0].protect;
    // Every page of the range gets the same protection: one run.
    // SAFETY: committed pages of our reservation, whose protection the title
    // asks to change.
    if !unsafe { Host::protect(base + first * page, (last - first) * page, host) } {
        return Err(ERROR_INVALID_ADDRESS);
    }
    for p in range {
        p.protect = new_protect;
    }
    Ok(old)
}

/// `VirtualProtect` of pages outside our regions: within a mapped image of
/// the title's (its sections, which the loader mapped), the host's
/// protection is changed; anywhere else is `ERROR_INVALID_ADDRESS`. The old
/// protection is not known (the loader keeps no record of its sections'),
/// so `PAGE_EXECUTE_READWRITE` stands for it.
fn protect_image(address: usize, size: usize, host: Protection) -> Result<u32, u32> {
    let page = page();
    let start = address & !(page - 1);
    let end = address
        .checked_add(size.max(1))
        .and_then(|end| round_up(end, page))
        .ok_or(ERROR_INVALID_ADDRESS)?;
    let image = victory_loader::images::find(address).ok_or(ERROR_INVALID_ADDRESS)?;
    if start < image.base || end > image.base + image.size {
        return Err(ERROR_INVALID_ADDRESS);
    }
    // SAFETY: pages of a mapped image whose protection the title asks to
    // change, as Windows lets it.
    if !unsafe { Host::protect(start, end - start, host) } {
        return Err(ERROR_INVALID_ADDRESS);
    }
    Ok(PAGE_EXECUTE_READWRITE)
}

/// Changes the protection of committed pages; `*old` gets the first's.
pub extern "win64" fn VirtualProtect(
    address: LPVOID,
    size: SIZE_T,
    new_protect: DWORD,
    old: *mut DWORD,
) -> BOOL {
    if old.is_null() {
        set_last_error(ERROR_NOACCESS);
        return FALSE;
    }
    match virtual_protect(address as usize, size, new_protect) {
        Ok(previous) => {
            // SAFETY: the title's out parameter, checked non-null above.
            unsafe { old.write(previous) };
            TRUE
        }
        Err(code) => {
            set_last_error(code);
            FALSE
        }
    }
}

/// [`VirtualProtect`] in this process (the only one served).
pub extern "win64" fn VirtualProtectEx(
    process: HANDLE,
    address: LPVOID,
    size: SIZE_T,
    new_protect: DWORD,
    old: *mut DWORD,
) -> BOOL {
    if process != CURRENT_PROCESS {
        set_last_error(ERROR_INVALID_HANDLE);
        return FALSE;
    }
    VirtualProtect(address, size, new_protect, old)
}

/// What `VirtualQuery` reports about a run of pages alike.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MEMORY_BASIC_INFORMATION {
    pub base_address: *mut c_void,
    pub allocation_base: *mut c_void,
    pub allocation_protect: u32,
    pub partition_id: u16,
    pub region_size: usize,
    pub state: u32,
    pub protect: u32,
    pub type_: u32,
}

const _: () = assert!(std::mem::size_of::<MEMORY_BASIC_INFORMATION>() == 48);

fn virtual_query(address: usize) -> MEMORY_BASIC_INFORMATION {
    let page = page();
    let at = address & !(page - 1);
    let mut map = regions();
    if let Some((base, region)) = region_at(&mut map, at) {
        let first = (at - base) / page;
        let like = region.pages[first];
        let run = region.pages[first..]
            .iter()
            .take_while(|p| **p == like)
            .count();
        return MEMORY_BASIC_INFORMATION {
            base_address: at as *mut c_void,
            allocation_base: base as *mut c_void,
            allocation_protect: region.allocation_protect,
            partition_id: 0,
            region_size: run * page,
            state: if like.committed {
                MEM_COMMIT
            } else {
                MEM_RESERVE
            },
            protect: like.protect,
            type_: MEM_PRIVATE,
        };
    }
    let next = map
        .range(at..)
        .next()
        .map_or(ADDRESS_LIMIT, |(&base, _)| base.min(ADDRESS_LIMIT));
    MEMORY_BASIC_INFORMATION {
        base_address: at as *mut c_void,
        allocation_base: std::ptr::null_mut(),
        allocation_protect: 0,
        partition_id: 0,
        region_size: next - at,
        state: MEM_FREE,
        protect: PAGE_NOACCESS,
        type_: 0,
    }
}

/// Describes the run of pages alike from the page `address` is in. Only
/// our reservations are known: memory the host or Rust own looks free.
pub extern "win64" fn VirtualQuery(
    address: *const c_void,
    info: *mut MEMORY_BASIC_INFORMATION,
    len: SIZE_T,
) -> SIZE_T {
    let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    if len < size || info.is_null() || address as usize >= ADDRESS_LIMIT {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let result = virtual_query(address as usize);
    // SAFETY: the title's buffer, at least as large as the struct; it need
    // not be aligned.
    unsafe { info.write_unaligned(result) };
    size
}

/// [`VirtualQuery`] in this process (the only one served).
pub extern "win64" fn VirtualQueryEx(
    process: HANDLE,
    address: *const c_void,
    info: *mut MEMORY_BASIC_INFORMATION,
    len: SIZE_T,
) -> SIZE_T {
    if process != CURRENT_PROCESS {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    VirtualQuery(address, info, len)
}

/// Nothing is paged out that a title would notice: always succeeds.
pub extern "win64" fn VirtualLock(_address: LPVOID, _size: SIZE_T) -> BOOL {
    TRUE
}

/// See [`VirtualLock`].
pub extern "win64" fn VirtualUnlock(_address: LPVOID, _size: SIZE_T) -> BOOL {
    TRUE
}

/// x86-64 keeps instruction caches coherent itself (and Rosetta 2 notices
/// written code): nothing to do.
pub extern "win64" fn FlushInstructionCache(
    _process: HANDLE,
    _address: *const c_void,
    _size: SIZE_T,
) -> BOOL {
    TRUE
}

/// Warns once that write watching was asked for.
fn warn_write_watch() {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| tracing::warn!("win32: write watching is not supported"));
}

/// Write watching is the Windows GPU path's, not a title's: always fails.
pub extern "win64" fn GetWriteWatch(
    _flags: DWORD,
    _base: LPVOID,
    _size: SIZE_T,
    _addresses: *mut LPVOID,
    _count: *mut usize,
    _granularity: *mut DWORD,
) -> u32 {
    warn_write_watch();
    set_last_error(ERROR_INVALID_PARAMETER);
    u32::MAX
}

/// See [`GetWriteWatch`].
pub extern "win64" fn ResetWriteWatch(_base: LPVOID, _size: SIZE_T) -> u32 {
    warn_write_watch();
    set_last_error(ERROR_INVALID_PARAMETER);
    1
}

// The heaps.

const HEAP_ZERO_MEMORY: u32 = 8;
const LMEM_ZEROINIT: u32 = 0x40;
const LMEM_MODIFY: u32 = 0x80;
const GMEM_ZEROINIT: u32 = 0x40;

/// A heap a title made with `HeapCreate`, or the process heap. Blocks are
/// Rust allocations that carry their size and heap in a header, so a heap
/// itself tracks nothing.
#[derive(Debug)]
struct Heap {
    /// `HeapCreate`'s options, kept for diagnostics (and so that every heap
    /// has its own address).
    _options: u32,
}

static DEFAULT_HEAP: Heap = Heap { _options: 0 };

/// The heaps `HeapCreate` made and `HeapDestroy` has not destroyed, so
/// that a handle that is none of them is refused.
static HEAPS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// What comes before every block.
#[repr(C)]
struct Header {
    size: usize,
    heap: *const Heap,
}

const HEADER: usize = 16;
const _: () = assert!(std::mem::size_of::<Header>() == HEADER);

/// The allocation behind a block of `size` bytes: header and block, 16-byte
/// aligned. A block of 0 bytes gets 16 (Windows returns a usable pointer).
fn block_layout(size: usize) -> Option<Layout> {
    let payload = if size == 0 { 16 } else { size };
    Layout::from_size_align(payload.checked_add(HEADER)?, 16).ok()
}

/// A block of `size` bytes, zeroed if asked; null if Rust's allocator has
/// no room.
fn heap_alloc(heap: HANDLE, size: usize, zero: bool) -> LPVOID {
    let Some(layout) = block_layout(size) else {
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    };
    // SAFETY: a layout of non-zero size.
    let raw = unsafe {
        if zero {
            std::alloc::alloc_zeroed(layout)
        } else {
            std::alloc::alloc(layout)
        }
    };
    if raw.is_null() {
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    // SAFETY: the start of the fresh allocation, 16-byte aligned, at least
    // 32 bytes long.
    unsafe {
        raw.cast::<Header>().write(Header {
            size,
            heap: heap as *const Heap,
        });
        raw.add(HEADER).cast()
    }
}

/// The header of a block from [`heap_alloc`].
///
/// # Safety
/// `block` is a live block from [`heap_alloc`].
unsafe fn header(block: *const c_void) -> *mut Header {
    // SAFETY: the caller's block, which has its header just before it.
    unsafe { block.cast::<u8>().sub(HEADER).cast::<Header>().cast_mut() }
}

/// Gives back a block from [`heap_alloc`]; nothing for null.
fn heap_free(block: LPVOID) {
    if block.is_null() {
        return;
    }
    // SAFETY: a block the title got from us and gives back; its header
    // names the layout it was allocated with.
    unsafe {
        let header = header(block);
        if let Some(layout) = block_layout((*header).size) {
            std::alloc::dealloc(header.cast(), layout);
        }
    }
}

/// The block's size; 0 for null.
fn heap_size(block: *const c_void) -> usize {
    if block.is_null() {
        return 0;
    }
    // SAFETY: a live block the title got from us.
    unsafe { (*header(block)).size }
}

/// Resizes a block (possibly moving it), zeroing the growth if asked; null
/// (the old block kept) if Rust's allocator has no room.
fn heap_realloc(heap: HANDLE, block: LPVOID, size: usize, zero: bool) -> LPVOID {
    if block.is_null() {
        return heap_alloc(heap, size, zero);
    }
    let Some(new_layout) = block_layout(size) else {
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    };
    let old = heap_size(block);
    let Some(old_layout) = block_layout(old) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    // SAFETY: a live block the title got from us, reallocated with the
    // layout it was allocated with; on success its header and the growth
    // are written within the new allocation.
    unsafe {
        let raw = std::alloc::realloc(header(block).cast(), old_layout, new_layout.size());
        if raw.is_null() {
            set_last_error(ERROR_NOT_ENOUGH_MEMORY);
            return std::ptr::null_mut();
        }
        (*raw.cast::<Header>()).size = size;
        let data = raw.add(HEADER);
        if zero && size > old {
            data.add(old).write_bytes(0, size - old);
        }
        data.cast()
    }
}

/// A new heap: the handle is the address of its record.
pub extern "win64" fn HeapCreate(options: DWORD, _initial: SIZE_T, _maximum: SIZE_T) -> HANDLE {
    let heap = Box::into_raw(Box::new(Heap { _options: options })) as usize;
    HEAPS.lock().unwrap_or_else(|e| e.into_inner()).push(heap);
    heap as HANDLE
}

/// Destroys a heap made by [`HeapCreate`]. The blocks allocated from it are
/// not given back (they are Rust allocations of their own, which the heap
/// does not track).
pub extern "win64" fn HeapDestroy(heap: HANDLE) -> BOOL {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(i) = heaps.iter().position(|&h| h == heap as usize) else {
        set_last_error(ERROR_INVALID_HANDLE);
        return FALSE;
    };
    heaps.swap_remove(i);
    // SAFETY: a heap record `HeapCreate` leaked and nobody destroyed yet.
    drop(unsafe { Box::from_raw(heap.cast::<Heap>()) });
    TRUE
}

/// The process heap, one for the process's life.
pub extern "win64" fn GetProcessHeap() -> HANDLE {
    (&raw const DEFAULT_HEAP).cast_mut().cast()
}

/// The process heap alone (the ones titles make are not listed).
pub extern "win64" fn GetProcessHeaps(count: DWORD, heaps: *mut HANDLE) -> DWORD {
    if count >= 1 && !heaps.is_null() {
        // SAFETY: the title's array of `count` handles.
        unsafe { heaps.write(GetProcessHeap()) };
    }
    1
}

/// A block of `size` bytes (`HEAP_ZERO_MEMORY` zeroes it;
/// `HEAP_GENERATE_EXCEPTIONS` is ignored: a failure returns null).
pub extern "win64" fn HeapAlloc(heap: HANDLE, flags: DWORD, size: SIZE_T) -> LPVOID {
    heap_alloc(heap, size, flags & HEAP_ZERO_MEMORY != 0)
}

/// Gives a block back; null is nothing to free.
pub extern "win64" fn HeapFree(_heap: HANDLE, _flags: DWORD, block: LPVOID) -> BOOL {
    heap_free(block);
    TRUE
}

/// Resizes a block, which may move (`HEAP_REALLOC_IN_PLACE_ONLY`, 0x10, is
/// not honoured); `HEAP_ZERO_MEMORY` zeroes the growth.
pub extern "win64" fn HeapReAlloc(
    heap: HANDLE,
    flags: DWORD,
    block: LPVOID,
    size: SIZE_T,
) -> LPVOID {
    if block.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    heap_realloc(heap, block, size, flags & HEAP_ZERO_MEMORY != 0)
}

/// The size the block was asked with; -1 for null.
pub extern "win64" fn HeapSize(_heap: HANDLE, _flags: DWORD, block: *const c_void) -> SIZE_T {
    if block.is_null() {
        return usize::MAX;
    }
    heap_size(block)
}

/// Blocks are not checked: every heap is valid.
pub extern "win64" fn HeapValidate(_heap: HANDLE, _flags: DWORD, _block: *const c_void) -> BOOL {
    TRUE
}

/// Nothing to compact: reports no free block.
pub extern "win64" fn HeapCompact(_heap: HANDLE, _flags: DWORD) -> SIZE_T {
    0
}

/// Heap options (the low-fragmentation heap and the like) change nothing.
pub extern "win64" fn HeapSetInformation(
    _heap: HANDLE,
    _class: i32,
    _info: *mut c_void,
    _len: SIZE_T,
) -> BOOL {
    TRUE
}

/// No heap information is kept.
pub extern "win64" fn HeapQueryInformation(
    _heap: HANDLE,
    _class: i32,
    _info: *mut c_void,
    _len: SIZE_T,
    _returned: *mut SIZE_T,
) -> BOOL {
    set_last_error(ERROR_INVALID_PARAMETER);
    FALSE
}

/// Rust's allocator locks itself: nothing to hold.
pub extern "win64" fn HeapLock(_heap: HANDLE) -> BOOL {
    TRUE
}

/// See [`HeapLock`].
pub extern "win64" fn HeapUnlock(_heap: HANDLE) -> BOOL {
    TRUE
}

/// Heaps' blocks are not tracked: the walk ends at once.
pub extern "win64" fn HeapWalk(_heap: HANDLE, _entry: *mut c_void) -> BOOL {
    set_last_error(ERROR_NO_MORE_ITEMS);
    FALSE
}

/// A block of the process heap; `LMEM_ZEROINIT` zeroes it. Moveable memory
/// is a plain pointer too (`LMEM_MOVEABLE` is ignored).
pub extern "win64" fn LocalAlloc(flags: u32, size: SIZE_T) -> HANDLE {
    heap_alloc(GetProcessHeap(), size, flags & LMEM_ZEROINIT != 0)
}

/// Gives a block back: null on success.
pub extern "win64" fn LocalFree(block: HANDLE) -> HANDLE {
    heap_free(block);
    std::ptr::null_mut()
}

/// Resizes a block; `LMEM_ZEROINIT` zeroes the growth, `LMEM_MODIFY`
/// changes nothing.
pub extern "win64" fn LocalReAlloc(block: HANDLE, size: SIZE_T, flags: u32) -> HANDLE {
    if flags & LMEM_MODIFY != 0 {
        return block;
    }
    heap_realloc(GetProcessHeap(), block, size, flags & LMEM_ZEROINIT != 0)
}

/// The block's size; 0 for null.
pub extern "win64" fn LocalSize(block: HANDLE) -> SIZE_T {
    heap_size(block)
}

/// See [`LocalAlloc`] (`GMEM_ZEROINIT`).
pub extern "win64" fn GlobalAlloc(flags: u32, size: SIZE_T) -> HANDLE {
    heap_alloc(GetProcessHeap(), size, flags & GMEM_ZEROINIT != 0)
}

/// See [`LocalFree`].
pub extern "win64" fn GlobalFree(block: HANDLE) -> HANDLE {
    heap_free(block);
    std::ptr::null_mut()
}

/// See [`LocalReAlloc`] (`GMEM_ZEROINIT`, `GMEM_MODIFY`).
pub extern "win64" fn GlobalReAlloc(block: HANDLE, size: SIZE_T, flags: u32) -> HANDLE {
    if flags & LMEM_MODIFY != 0 {
        return block;
    }
    heap_realloc(GetProcessHeap(), block, size, flags & GMEM_ZEROINIT != 0)
}

/// See [`LocalSize`].
pub extern "win64" fn GlobalSize(block: HANDLE) -> SIZE_T {
    heap_size(block)
}

/// The handle is the pointer itself.
pub extern "win64" fn GlobalLock(block: HANDLE) -> LPVOID {
    block
}

/// Nothing was locked.
pub extern "win64" fn GlobalUnlock(_block: HANDLE) -> BOOL {
    TRUE
}

/// The machine's memory, as `GlobalMemoryStatusEx` reports it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MEMORYSTATUSEX {
    pub length: u32,
    pub memory_load: u32,
    pub total_phys: u64,
    pub avail_phys: u64,
    pub total_page_file: u64,
    pub avail_page_file: u64,
    pub total_virtual: u64,
    pub avail_virtual: u64,
    pub avail_extended_virtual: u64,
}

const _: () = assert!(std::mem::size_of::<MEMORYSTATUSEX>() == 64);

/// The machine's memory, as `GlobalMemoryStatus` reports it (`SIZE_T`
/// fields: 64-bit in an x64 process).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MEMORYSTATUS {
    pub length: u32,
    pub memory_load: u32,
    pub total_phys: usize,
    pub avail_phys: usize,
    pub total_page_file: usize,
    pub avail_page_file: usize,
    pub total_virtual: usize,
    pub avail_virtual: usize,
}

const _: () = assert!(std::mem::size_of::<MEMORYSTATUS>() == 56);

/// Bytes committed in our reservations.
pub(crate) fn committed_bytes() -> u64 {
    let page = page() as u64;
    regions()
        .values()
        .map(|r| r.pages.iter().filter(|p| p.committed).count() as u64 * page)
        .sum()
}

/// The physical memory the host reports, the page file taken to be as
/// large, and the title's address space less what it committed here.
fn memory_status() -> MEMORYSTATUSEX {
    let (total, avail) = Host::physical_memory();
    let avail = avail.min(total);
    let memory_load = if total == 0 {
        0
    } else {
        ((total - avail) as u128 * 100 / total as u128) as u32
    };
    MEMORYSTATUSEX {
        length: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        memory_load,
        total_phys: total,
        avail_phys: avail,
        total_page_file: total,
        avail_page_file: avail,
        total_virtual: MAX_APPLICATION_ADDRESS,
        avail_virtual: MAX_APPLICATION_ADDRESS.saturating_sub(committed_bytes()),
        avail_extended_virtual: 0,
    }
}

/// Fills in the machine's memory; `length` must be the struct's size.
pub extern "win64" fn GlobalMemoryStatusEx(buffer: *mut MEMORYSTATUSEX) -> BOOL {
    // SAFETY: the title's struct, whose `length` it set.
    if buffer.is_null() || unsafe { (*buffer).length } != 64 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    // SAFETY: as above.
    unsafe { buffer.write(memory_status()) };
    TRUE
}

/// [`GlobalMemoryStatusEx`] in the older struct.
pub extern "win64" fn GlobalMemoryStatus(buffer: *mut MEMORYSTATUS) {
    if buffer.is_null() {
        return;
    }
    let s = memory_status();
    let size = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
    // SAFETY: the title's struct.
    unsafe {
        buffer.write(MEMORYSTATUS {
            length: std::mem::size_of::<MEMORYSTATUS>() as u32,
            memory_load: s.memory_load,
            total_phys: size(s.total_phys),
            avail_phys: size(s.avail_phys),
            total_page_file: size(s.total_page_file),
            avail_page_file: size(s.avail_page_file),
            total_virtual: size(s.total_virtual),
            avail_virtual: size(s.avail_virtual),
        })
    };
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(VirtualAlloc),
        hle_export!(VirtualAllocEx),
        hle_export!(VirtualFree),
        hle_export!(VirtualFreeEx),
        hle_export!(VirtualProtect),
        hle_export!(VirtualProtectEx),
        hle_export!(VirtualQuery),
        hle_export!(VirtualQueryEx),
        hle_export!(VirtualLock),
        hle_export!(VirtualUnlock),
        hle_export!(FlushInstructionCache),
        hle_export!(GetWriteWatch),
        hle_export!(ResetWriteWatch),
        hle_export!(HeapCreate),
        hle_export!(HeapDestroy),
        hle_export!(GetProcessHeap),
        hle_export!(GetProcessHeaps),
        hle_export!(HeapAlloc),
        hle_export!(HeapFree),
        hle_export!(HeapReAlloc),
        hle_export!(HeapSize),
        hle_export!(HeapValidate),
        hle_export!(HeapCompact),
        hle_export!(HeapSetInformation),
        hle_export!(HeapQueryInformation),
        hle_export!(HeapLock),
        hle_export!(HeapUnlock),
        hle_export!(HeapWalk),
        hle_export!(LocalAlloc),
        hle_export!(LocalFree),
        hle_export!(LocalReAlloc),
        hle_export!(LocalSize),
        hle_export!(GlobalAlloc),
        hle_export!(GlobalFree),
        hle_export!(GlobalReAlloc),
        hle_export!(GlobalSize),
        hle_export!(GlobalLock),
        hle_export!(GlobalUnlock),
        hle_export!(GlobalMemoryStatusEx),
        hle_export!(GlobalMemoryStatus),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_error() -> u32 {
        let teb = victory_loader::teb::attach_current_thread();
        // SAFETY: this thread's own TEB.
        unsafe { (*(teb as *const victory_loader::teb::Teb)).last_error }
    }

    fn query(at: usize) -> MEMORY_BASIC_INFORMATION {
        // SAFETY: zero is a value of every field (null pointers, integers).
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = VirtualQuery(at as *const c_void, &mut info, size_of_val(&info));
        assert_eq!(n, 48);
        info
    }

    #[test]
    fn reserve_commit_protect_query_free() {
        victory_loader::teb::attach_current_thread();
        let page = page();
        let mib = 1 << 20;
        let base = VirtualAlloc(std::ptr::null_mut(), mib, MEM_RESERVE, PAGE_READWRITE) as usize;
        assert_ne!(base, 0);
        assert_eq!(base % GRANULARITY, 0);
        let info = query(base);
        assert_eq!(info.state, MEM_RESERVE);
        assert_eq!(info.region_size, mib);
        assert_eq!(info.allocation_base as usize, base);
        assert_eq!(info.allocation_protect, PAGE_READWRITE);
        assert_eq!(info.type_, MEM_PRIVATE);

        let middle = base + 16 * page;
        let committed = VirtualAlloc(
            (middle + 10) as LPVOID,
            3 * page - 10,
            MEM_COMMIT,
            PAGE_READWRITE,
        ) as usize;
        assert_eq!(committed, middle);
        // SAFETY: three pages just committed read-write.
        let bytes = unsafe { std::slice::from_raw_parts_mut(middle as *mut u8, 3 * page) };
        assert!(bytes.iter().all(|&b| b == 0));
        bytes.fill(0xab);

        let info = query(middle + 5);
        assert_eq!(info.base_address as usize, middle);
        assert_eq!(info.state, MEM_COMMIT);
        assert_eq!(info.protect, PAGE_READWRITE);
        assert_eq!(info.region_size, 3 * page);
        let before = query(middle - page);
        assert_eq!(before.state, MEM_RESERVE);
        assert_eq!(before.base_address as usize, middle - page);
        assert_eq!(before.region_size, page);
        assert_eq!(query(base).region_size, 16 * page);
        let after = query(middle + 3 * page);
        assert_eq!(after.state, MEM_RESERVE);
        assert_eq!(after.region_size, mib - 19 * page);

        let mut old = 0;
        assert_eq!(
            VirtualProtect(middle as LPVOID, 3 * page, PAGE_READONLY, &mut old),
            TRUE
        );
        assert_eq!(old, PAGE_READWRITE);
        assert_eq!(query(middle).protect, PAGE_READONLY);
        assert_eq!(
            VirtualProtect(base as LPVOID, page, PAGE_READONLY, &mut old),
            FALSE,
            "reserved pages have no protection to change"
        );
        assert_eq!(last_error(), ERROR_INVALID_ADDRESS);
        assert_eq!(
            VirtualProtect(middle as LPVOID, 3 * page, PAGE_READWRITE, &mut old),
            TRUE
        );
        assert_eq!(old, PAGE_READONLY);

        let second = middle + page;
        assert_eq!(VirtualFree(second as LPVOID, page, MEM_DECOMMIT), TRUE);
        assert_eq!(query(second).state, MEM_RESERVE);
        assert_eq!(query(middle).region_size, page);
        assert_eq!(
            VirtualAlloc(second as LPVOID, page, MEM_COMMIT, PAGE_READWRITE) as usize,
            second
        );
        // SAFETY: committed read-write again.
        let again = unsafe { std::slice::from_raw_parts(second as *const u8, page) };
        assert!(
            again.iter().all(|&b| b == 0),
            "recommitted pages are zeroed"
        );
        // SAFETY: the first page, committed all along.
        assert_eq!(unsafe { *(middle as *const u8) }, 0xab, "kept");

        assert_eq!(VirtualFree(middle as LPVOID, 0, MEM_RELEASE), FALSE);
        assert_eq!(last_error(), ERROR_INVALID_ADDRESS);
        assert_eq!(VirtualFree(base as LPVOID, 0, MEM_RELEASE), TRUE);
        let gone = query(base);
        assert_eq!(gone.state, MEM_FREE);
        assert!(gone.allocation_base.is_null());

        assert!(VirtualAlloc(std::ptr::null_mut(), 0, MEM_RESERVE, PAGE_READWRITE).is_null());
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        // `MEM_COMMIT` alone at no address reserves and commits.
        let p = VirtualAlloc(std::ptr::null_mut(), page, MEM_COMMIT, PAGE_READWRITE);
        assert!(!p.is_null());
        let info = query(p as usize);
        assert_eq!((info.state, info.protect), (MEM_COMMIT, PAGE_READWRITE));
        assert_eq!(VirtualFree(p, 0, MEM_RELEASE), TRUE);
        assert!(VirtualAlloc(std::ptr::null_mut(), page, MEM_RESERVE, 0x3).is_null());
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn commit_at_a_chosen_address() {
        let size = 4 * GRANULARITY;
        // Another test's reservation may take the address meanwhile: a few
        // tries.
        let placed = (0..8).find_map(|_| {
            let first = VirtualAlloc(std::ptr::null_mut(), size, MEM_RESERVE, PAGE_READWRITE);
            assert!(!first.is_null());
            assert_eq!(VirtualFree(first, 0, MEM_RELEASE), TRUE);
            let again = VirtualAlloc(first, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
            (!again.is_null()).then_some((first, again))
        });
        let (first, again) = placed.expect("a reservation at a free address");
        assert_eq!(again, first);
        let info = query(again as usize);
        assert_eq!(info.state, MEM_COMMIT);
        assert_eq!(info.region_size, size);
        // SAFETY: committed read-write.
        unsafe { *(again as *mut u8).add(size - 1) = 1 };
        assert_eq!(VirtualFree(again, 0, MEM_RELEASE), TRUE);
    }

    #[test]
    fn reset_keeps_committed_pages() {
        victory_loader::teb::attach_current_thread();
        let page = page();
        let base =
            VirtualAlloc(std::ptr::null_mut(), 4 * page, MEM_RESERVE, PAGE_READWRITE) as usize;
        assert_ne!(base, 0);
        let committed = VirtualAlloc(base as LPVOID, 2 * page, MEM_COMMIT, PAGE_READWRITE);
        assert_eq!(committed as usize, base);
        // SAFETY: committed read-write.
        unsafe { *(base as *mut u8).add(page + 3) = 9 };
        let reset = VirtualAlloc((base + 5) as LPVOID, 2 * page - 5, MEM_RESET, PAGE_NOACCESS);
        assert_eq!(reset as usize, base);
        // SAFETY: still committed read-write: a reset is advice.
        assert_eq!(unsafe { *(base as *const u8).add(page + 3) }, 9);
        let undo = VirtualAlloc(base as LPVOID, page, MEM_RESET_UNDO, PAGE_READWRITE);
        assert_eq!(undo as usize, base);
        assert_eq!(query(base).state, MEM_COMMIT);
        // Reserved pages in the range: refused.
        let reset = VirtualAlloc(base as LPVOID, 3 * page, MEM_RESET, PAGE_READWRITE);
        assert!(reset.is_null());
        assert_eq!(last_error(), ERROR_INVALID_ADDRESS);
        let mixed = VirtualAlloc(base as LPVOID, page, MEM_RESET | MEM_COMMIT, PAGE_READWRITE);
        assert!(mixed.is_null());
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(VirtualFree(base as LPVOID, 0, MEM_RELEASE), TRUE);
    }

    #[test]
    fn committing_again_sets_the_protection() {
        victory_loader::teb::attach_current_thread();
        let page = page();
        let p = VirtualAlloc(
            std::ptr::null_mut(),
            2 * page,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        );
        assert!(!p.is_null());
        // SAFETY: committed read-write.
        unsafe { *(p as *mut u8) = 5 };
        let mut old = 0;
        assert_eq!(VirtualProtect(p, 2 * page, PAGE_READONLY, &mut old), TRUE);
        assert_eq!(query(p as usize).protect, PAGE_READONLY);
        assert_eq!(VirtualAlloc(p, 2 * page, MEM_COMMIT, PAGE_READWRITE), p);
        assert_eq!(query(p as usize).protect, PAGE_READWRITE);
        // SAFETY: read-write again; the contents were kept.
        unsafe {
            assert_eq!(*(p as *const u8), 5);
            *(p as *mut u8).add(page) = 6;
        }
        assert_eq!(VirtualFree(p, 0, MEM_RELEASE), TRUE);
    }

    #[test]
    fn protect_outside_regions() {
        victory_loader::teb::attach_current_thread();
        let page = page();
        let p = VirtualAlloc(std::ptr::null_mut(), page, MEM_COMMIT, PAGE_READWRITE);
        assert!(!p.is_null());
        let mut old = 0;
        assert_eq!(VirtualProtect(p, page, PAGE_READONLY, &mut old), TRUE);
        assert_eq!(old, PAGE_READWRITE);
        assert_eq!(VirtualFree(p, 0, MEM_RELEASE), TRUE);
        // No region and no image there (below every host's lowest mapping).
        let wild = 0x2000 as LPVOID;
        assert_eq!(VirtualProtect(wild, page, PAGE_READONLY, &mut old), FALSE);
        assert_eq!(last_error(), ERROR_INVALID_ADDRESS);
    }

    #[test]
    fn heaps_track_sizes() {
        let heap = GetProcessHeap();
        assert_eq!(heap, GetProcessHeap(), "stable");
        let p = HeapAlloc(heap, 0, 100);
        assert!(!p.is_null());
        assert_eq!(p as usize % 16, 0);
        assert_eq!(HeapSize(heap, 0, p), 100);
        // SAFETY: our 100-byte block.
        unsafe { std::slice::from_raw_parts_mut(p as *mut u8, 100).fill(7) };
        let q = HeapReAlloc(heap, HEAP_ZERO_MEMORY, p, 300);
        assert_eq!(HeapSize(heap, 0, q), 300);
        // SAFETY: our 300-byte block.
        let bytes = unsafe { std::slice::from_raw_parts(q as *const u8, 300) };
        assert!(bytes[..100].iter().all(|&b| b == 7), "kept");
        assert!(bytes[100..].iter().all(|&b| b == 0), "growth zeroed");
        assert_eq!(HeapFree(heap, 0, q), TRUE);
        assert_eq!(HeapFree(heap, 0, std::ptr::null_mut()), TRUE);
        assert_eq!(HeapSize(heap, 0, std::ptr::null()), usize::MAX);

        let z = HeapAlloc(heap, HEAP_ZERO_MEMORY, 4096);
        // SAFETY: our zeroed block.
        assert!(
            unsafe { std::slice::from_raw_parts(z as *const u8, 4096) }
                .iter()
                .all(|&b| b == 0)
        );
        HeapFree(heap, 0, z);
        let empty = HeapAlloc(heap, 0, 0);
        assert!(!empty.is_null());
        assert_eq!(HeapSize(heap, 0, empty), 0);
        HeapFree(heap, 0, empty);

        let mine = HeapCreate(0, 0, 0);
        assert!(!mine.is_null());
        assert_ne!(mine, heap);
        let b = HeapAlloc(mine, 0, 10);
        HeapFree(mine, 0, b);
        assert_eq!(HeapDestroy(mine), TRUE);
        assert_eq!(HeapDestroy(mine), FALSE);
        assert_eq!(HeapDestroy(heap), FALSE, "the process heap stays");

        let local = LocalAlloc(LMEM_ZEROINIT, 64);
        // SAFETY: our zeroed block.
        assert!(
            unsafe { std::slice::from_raw_parts(local as *const u8, 64) }
                .iter()
                .all(|&b| b == 0)
        );
        assert_eq!(LocalSize(local), 64);
        assert!(LocalFree(local).is_null());
        let global = GlobalAlloc(GMEM_ZEROINIT, 64);
        assert_eq!(GlobalLock(global), global);
        // SAFETY: our zeroed block.
        assert!(
            unsafe { std::slice::from_raw_parts(global as *const u8, 64) }
                .iter()
                .all(|&b| b == 0)
        );
        assert_eq!(GlobalSize(global), 64);
        assert_eq!(GlobalUnlock(global), TRUE);
        assert!(GlobalFree(global).is_null());

        let mut heaps = [std::ptr::null_mut(); 2];
        assert_eq!(GetProcessHeaps(2, heaps.as_mut_ptr()), 1);
        assert_eq!(heaps[0], heap);
    }

    #[test]
    fn memory_status_is_sane() {
        victory_loader::teb::attach_current_thread();
        let mut status = MEMORYSTATUSEX {
            length: 64,
            ..Default::default()
        };
        assert_eq!(GlobalMemoryStatusEx(&mut status), TRUE);
        assert!(status.total_phys >= status.avail_phys && status.avail_phys > 0);
        assert!(status.memory_load <= 100);
        assert_eq!(status.total_virtual, MAX_APPLICATION_ADDRESS);
        assert!(status.avail_virtual <= status.total_virtual);
        let mut bad = MEMORYSTATUSEX::default();
        assert_eq!(GlobalMemoryStatusEx(&mut bad), FALSE);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        let mut old = MEMORYSTATUS::default();
        GlobalMemoryStatus(&mut old);
        assert_eq!(old.length, 56);
        assert_eq!(old.total_phys as u64, status.total_phys);
    }
}
