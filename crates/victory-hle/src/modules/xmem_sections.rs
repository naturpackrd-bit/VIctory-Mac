//! `xmem.dll` on hosts with no placeholders or `MapViewOfFile3`:
//! reservations through our own `VirtualAlloc` (`win32::memory`), physical
//! pages as sections shown by views (`victory_platform::sections`), and no
//! GPU side yet (the Windows module's `victory_gpu::title_memory_*` calls
//! are left out; M4's Metal back end reads title memory where it is).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use victory_platform::Host;
use victory_platform::sections::{Section, Sections};

use super::win32::handles::set_last_error;
use super::win32::memory::{
    self, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS,
    PAGE_READWRITE,
};
use crate::{HleModule, hle_export};

type HANDLE = *mut c_void;
type BOOL = i32;

const MIB: usize = 1 << 20;

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000), and write-combining and uncached
/// are dropped too.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// Windows' placeholder flags, which titles pass as on Windows: a
/// reservation here is what a placeholder is there.
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_RESERVE_PLACEHOLDER: u32 = 0x4_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

/// Reserves or commits memory where the console's flags allow it.
pub extern "win64" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let align = if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
        2 * MIB
    } else {
        64 << 10
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`): a plain reservation here.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder && kind & MEM_REPLACE_PLACEHOLDER != 0 {
        // The title split a reservation: nothing to do.
        return base;
    }
    let mut kind =
        kind & !(PAGES_2M | PAGES_64K_BIT | MEM_RESERVE_PLACEHOLDER | MEM_REPLACE_PLACEHOLDER);
    if placeholder {
        kind |= MEM_RESERVE;
        protect = PAGE_NOACCESS;
    }
    if base.is_null() {
        kind |= MEM_RESERVE;
    }
    let result = memory::virtual_alloc_within(
        base as usize,
        size,
        kind,
        protect,
        lowest,
        highest + 1,
        align,
    );
    static CALLS: AtomicU64 = AtomicU64::new(0);
    if CALLS.fetch_add(1, Ordering::Relaxed) < 16 {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{result:x?}"),
            "XMemVirtualAlloc"
        );
    }
    match result {
        Ok(at) => at as *mut c_void,
        Err(code) => {
            tracing::warn!(
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                error = code,
                "XMemVirtualAlloc refused"
            );
            set_last_error(code);
            std::ptr::null_mut()
        }
    }
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, as the
// Windows module does.
/// Describes the run of pages alike from the page `address` is in, with
/// the XMem flags.
pub extern "win64" fn XMemVirtualQuery(
    _process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let host = memory::query(address as usize);
    let info = BasicInformation {
        flags: if host.state == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        },
        host,
        tag_code: 0,
        tag: [0; 4],
    };
    // SAFETY: the title passes room for the whole structure; it need not be
    // aligned.
    unsafe { out.write_unaligned(info) };
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "win64" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "win64" fn(*mut c_void, u64);

/// Where the title's exe exports `__XMemAllocationHooks__`; 0 if it does
/// not.
static HOOKS: AtomicUsize = AtomicUsize::new(0);

/// The address of the title's `__XMemAllocationHooks__` export, which the
/// runner looks up once the exe is mapped: two routine pointers, allocate
/// and free, which [`XMemAlloc`] and [`XMemFree`] call.
pub fn set_allocation_hooks(address: usize) {
    HOOKS.store(address, Ordering::Release);
}

/// The title's allocation and free routines; zeros where it has none.
fn hooks() -> [usize; 2] {
    let at = HOOKS.load(Ordering::Acquire);
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers, in the mapped exe.
    unsafe { (at as *const [usize; 2]).read_unaligned() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "win64" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc = unsafe { std::mem::transmute::<usize, AllocRoutine>(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "win64" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free = unsafe { std::mem::transmute::<usize, FreeRoutine>(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "win64" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's, as in the Windows module.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        tracing::warn!(size, "XMemAllocDefault: the host heap is out of memory");
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "win64" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    if attributes & ALLOC_32BIT != 0 {
        memory::VirtualFree(address, 0, MEM_RELEASE);
        return;
    }
    // SAFETY: the address came from `XMemAllocDefault` with these
    // attributes: a header-prefixed block.
    unsafe {
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "win64" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a section, its pages are 64 KiB slices of it, and a mapping is a view
// of consecutive slices in place of what the reservation had there.

const PAGE: usize = 64 << 10;

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds them with
/// `<< 4`.
const FRAMES: usize = PAGE >> 12;

/// Bytes of the sections alive now.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// One `XMemAllocatePhysicalPages` call's memory.
struct PhysicalSection {
    /// Always there until dropped (taken out to be destroyed).
    section: Option<Section>,
    pages: usize,
}

impl PhysicalSection {
    /// Where our own code reads and writes the section; 0 once gone.
    fn window(&self) -> usize {
        self.section.as_ref().map_or(0, |s| s.window)
    }
}

impl Drop for PhysicalSection {
    fn drop(&mut self) {
        if let Some(section) = self.section.take() {
            // Views still mapped stay valid until unmapped.
            Host::section_destroy(section);
            LIVE.fetch_sub(self.pages * PAGE, Ordering::Relaxed);
        }
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<PhysicalSection>,
    index: usize,
}

/// The title's pages by number.
struct Pages {
    next_page: usize,
    pages: HashMap<usize, Slice>,
}

/// Taken after [`PHYSICAL`] where both are.
static PAGES: LazyLock<RwLock<Pages>> = LazyLock::new(|| {
    RwLock::new(Pages {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        pages: HashMap::new(),
    })
});

fn page_table() -> RwLockReadGuard<'static, Pages> {
    PAGES.read().unwrap_or_else(|e| e.into_inner())
}

fn page_table_mut() -> RwLockWriteGuard<'static, Pages> {
    PAGES.write().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct Physical {
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
}

/// The views, changed by the title's mapping calls.
static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(Default::default);

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "win64" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    let Some(section) = Host::section_create(size) else {
        tracing::warn!(n, "XMemAllocatePhysicalPages: the host refused a section");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    };
    LIVE.fetch_add(size, Ordering::Relaxed);
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    let section = Arc::new(PhysicalSection {
        section: Some(section),
        pages: n,
    });
    let mut table = page_table_mut();
    let first = table.next_page;
    // `size` did not overflow, so neither does `n * FRAMES`.
    let Some(next) = first.checked_add(n * FRAMES) else {
        drop(table);
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    };
    table.next_page = next;
    for index in 0..n {
        let slice = Slice {
            section: section.clone(),
            index,
        };
        table.pages.insert(first + index * FRAMES, slice);
    }
    // Title memory is written without our locks.
    drop(table);
    for index in 0..n {
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(first + index * FRAMES) };
    }
    1
}

/// Pages still mapped stay readable through their views until unmapped.
pub extern "win64" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    // SAFETY: the title passes `n` page numbers.
    let numbers = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
    let mut table = page_table_mut();
    // Dropped without the lock: the last page of a section gives its window
    // back.
    let gone: Vec<Slice> = numbers
        .iter()
        .filter_map(|number| table.pages.remove(number))
        .collect();
    drop(table);
    drop(gone);
    1
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "win64" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    let Some(len) = n
        .checked_mul(PAGE)
        .filter(|&len| base.is_multiple_of(PAGE) && base.checked_add(len).is_some())
    else {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            n,
            "XMemMapPhysicalPages: not a range of 64 KiB pages"
        );
        return std::ptr::null_mut();
    };
    // Read before any lock is taken.
    // SAFETY: the title passes `n` page numbers, or null.
    let pages =
        (!pages.is_null()).then(|| unsafe { std::slice::from_raw_parts(pages, n) }.to_vec());
    let mut physical = physical();
    unmap(&mut physical, base, len);
    let Some(pages) = pages else {
        return address;
    };
    let slices: Vec<Option<Slice>> = {
        let table = page_table();
        pages
            .iter()
            .map(|page| table.pages.get(page).cloned())
            .collect()
    };
    let mut i = 0;
    while i < n {
        let Some(first) = slices[i].clone() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + slices[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(slice, k)| {
                slice.as_ref().is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "win64" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Shows `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one reservation of ours, and records the view.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    // Recorded first: a range that is not in one reservation is refused
    // before the host replaces anything.
    if !memory::mark_mapped(base, len, PAGE_READWRITE) {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            "mapping physical pages failed (not in one reservation)"
        );
        return false;
    }
    let shown = slice
        .section
        .section
        .as_ref()
        .is_some_and(|section| Host::section_map(section, slice.index * PAGE, len, base));
    if !shown {
        memory::mark_reserved(base, len);
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            "mapping physical pages failed (the host refused)"
        );
        return false;
    }
    physical.views.insert(base, (len, slice));
    true
}

/// Returns `[base, base + len)` to reserved memory; the parts of views
/// reaching past it are shown again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return (and the range of views starting inside
    // would run backwards, which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base.saturating_add(len);
    // Views do not overlap: the one holding `base`, then those starting
    // inside.
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        // A view whose reservation the title released (and perhaps
        // reserved again) is no longer there to unmap: only forgotten.
        if memory::query(at).state != MEM_COMMIT {
            continue;
        }
        let (lo, hi) = (at.max(base), (at + size).min(end));
        if Host::unmap_in_place(lo, hi - lo) {
            memory::mark_reserved(lo, hi - lo);
        } else {
            tracing::warn!(
                address = format_args!("{lo:#x}"),
                len = hi - lo,
                "unmapping physical pages failed"
            );
        }
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. `None` if a page is unknown or `pages`
/// ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let table = page_table();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let Some(slice) = table.pages.get(page) else {
            tracing::warn!(
                page,
                offset,
                len,
                "a physical page the title does not own (any more)"
            );
            return None;
        };
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether the title owns every page under bytes `[offset, offset + len)`.
// For the DirectStorage port, which does not exist off Windows yet.
#[allow(dead_code)]
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    let Some(end) = offset.checked_add(len) else {
        return false;
    };
    let table = page_table();
    pages
        .get(offset / PAGE..end.div_ceil(PAGE))
        .is_some_and(|pages| pages.iter().all(|page| table.pages.contains_key(page)))
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations),
/// through the sections' windows. False if a page is unknown.
// For the DirectStorage port, which does not exist off Windows yet.
#[allow(dead_code)]
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let window = slice.section.window();
        if window == 0 {
            return false;
        }
        let k = (n * PAGE - within).min(data.len() - done);
        // SAFETY: bytes of the section's read-write window, inside the run
        // (`runs` holds the section); the title's other uses of them race
        // as on the console.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data[done..].as_ptr(),
                (window + slice.index * PAGE + within) as *mut u8,
                k,
            )
        };
        (within, done) = (0, done + k);
    }
    true
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget, and what it committed through us and allocated as
/// physical pages as what it uses: the pages a view shows are committed in
/// `memory`'s books and counted with their section, so the views' bytes
/// are taken off once. No tools memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0), as in the
// Windows module.
pub extern "win64" fn XMemGetWorkingSetStatistics(which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let (gpu_optimal, standard) = crate::console::current().title_memory();
    let shown: usize = physical().views.values().map(|(len, _)| *len).sum();
    let used = title_used(
        memory::committed_bytes() as usize,
        LIVE.load(Ordering::Relaxed),
        shown,
    );
    let stats = WorkingSet {
        title_limit: (gpu_optimal + standard).saturating_sub(TRANSFERRED.load(Ordering::Relaxed)),
        title_used: used,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th.
    static ASKED: AtomicU64 = AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// What the title uses: the bytes committed in `memory`'s books, which
/// count the pages its views show, plus its sections' bytes, which count
/// those pages too, so the views' bytes come off once.
fn title_used(committed: usize, live: usize, shown: usize) -> usize {
    (committed + live).saturating_sub(shown)
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "win64" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = crate::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use victory_platform::Memory;

    use super::*;

    /// Whether memory below 4 GiB can be had: the runner's reservation of
    /// it is made once here. Not on a host whose test binaries have nothing
    /// free there (macOS's 4 GiB `__PAGEZERO`).
    fn low_memory() -> bool {
        static LOW: OnceLock<bool> = OnceLock::new();
        let low = *LOW.get_or_init(|| {
            let before = victory_platform::low::free_ranges();
            let taken = victory_platform::low::reserve();
            !(before.is_empty() && taken.is_empty())
        });
        if !low {
            println!("nothing free below 4 GiB: the 32-bit part is skipped");
        }
        low
    }

    fn query(address: usize) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        assert_eq!(
            XMemVirtualQuery(usize::MAX as HANDLE, address as *const c_void, &mut info),
            1
        );
        info
    }

    fn release(at: *mut c_void) {
        assert_eq!(memory::VirtualFree(at, 0, MEM_RELEASE), 1);
    }

    fn allocate(n: usize) -> Vec<usize> {
        let mut pages = vec![0usize; n];
        let mut count = n;
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()),
            1
        );
        assert_eq!(count, n);
        pages
    }

    /// A reservation of 64 KiB pages, as titles make them for mapping.
    fn placeholder(pages: usize) -> usize {
        let at = XMemVirtualAlloc(
            std::ptr::null_mut(),
            pages * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!at.is_null());
        at as usize
    }

    fn byte(at: usize) -> Option<u8> {
        let mut b = [0u8];
        Host::read_into(at, &mut b).then_some(b[0])
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        if low_memory() {
            let p = XMemAlloc(4096, ALLOC_32BIT);
            assert!(!p.is_null() && (p as usize) < 1 << 32, "{p:p}");
            // SAFETY: committed read-write.
            unsafe { p.cast::<u8>().write(1) };
            XMemFree(p, ALLOC_32BIT);
            assert_eq!(query(p as usize).host.state, memory::MEM_FREE);
        }
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let committed = |p: *mut c_void| {
            let info = query(p as usize);
            assert_eq!(info.host.state, MEM_COMMIT, "{p:p}");
            assert_eq!(info.host.protect, PAGE_READWRITE, "{p:p}");
            assert_eq!(info.flags, GRAPHICS, "{p:p}");
        };
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0, "{p:p}");
        assert!((p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + 4 * MIB <= 1 << 40, "{p:p}");
        committed(p);
        release(p);
        assert_eq!(query(p as usize).flags, 0);

        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            MIB,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 5 << 12, // 42-bit addresses
            PAGE_READWRITE,
        );
        assert!(!p.is_null());
        assert!((p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + MIB <= 1 << 42, "{p:p}");
        committed(p);
        release(p);

        if low_memory() {
            let p = XMemVirtualAlloc(
                std::ptr::null_mut(),
                MIB,
                MEM_RESERVE | MEM_COMMIT,
                GRAPHICS | ADDRESS_32BIT,
                PAGE_READWRITE,
            );
            assert!(!p.is_null());
            assert!((p as usize) + MIB <= 1 << 32, "{p:p}");
            committed(p);
            release(p);
        }
    }

    #[test]
    fn physical_pages_are_shown_where_the_title_asks() {
        let pages = allocate(8);
        for p in &pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, *p, "packs into 21 bits");
        }
        let r = placeholder(16);
        let at = |page: usize| r + page * PAGE;
        assert_eq!(query(r).host.state, MEM_RESERVE);
        assert_eq!(
            XMemMapPhysicalPages(at(4) as *mut c_void, 3, pages[2..5].as_ptr()) as usize,
            at(4)
        );
        // SAFETY: three pages mapped read-write.
        unsafe {
            (at(4) as *mut u8).write(0x22);
            (at(5) as *mut u8).write(0x33);
            (at(6) as *mut u8).write(0x44);
        }
        assert_eq!(
            XMemMapPhysicalPages(at(12) as *mut c_void, 1, pages[3..].as_ptr()) as usize,
            at(12)
        );
        assert_eq!(byte(at(12)), Some(0x33), "page 3 at a second address");

        let shown = query(at(4));
        assert_eq!(shown.host.state, MEM_COMMIT);
        assert_eq!(shown.host.protect, PAGE_READWRITE);
        assert_eq!(shown.host.region_size, 3 * PAGE);
        assert_eq!(shown.flags, GRAPHICS);
        assert_eq!(query(at(3)).host.state, MEM_RESERVE);
        assert_eq!(query(at(7)).host.state, MEM_RESERVE);

        // Page 3 alone at +4: the old view's other parts stay.
        assert_eq!(
            XMemMapPhysicalPages(at(4) as *mut c_void, 1, pages[3..].as_ptr()) as usize,
            at(4)
        );
        assert_eq!(byte(at(4)), Some(0x33));
        assert_eq!(byte(at(5)), Some(0x33));
        assert_eq!(byte(at(6)), Some(0x44));

        assert_eq!(
            XMemMapPhysicalPages(at(4) as *mut c_void, 3, std::ptr::null()) as usize,
            at(4)
        );
        let gone = query(at(4));
        assert_eq!(gone.host.state, MEM_RESERVE);
        assert!(gone.host.region_size >= 3 * PAGE);
        assert_eq!(byte(at(4)), None, "no access");
        assert_eq!(byte(at(6)), None, "no access");

        assert_eq!(XMemFreePhysicalPages(8, pages.as_ptr()), 1);
        assert_eq!(byte(at(12)), Some(0x33), "the view outlives the pages");
        assert!(
            XMemMapPhysicalPages(at(0) as *mut c_void, 1, pages.as_ptr()).is_null(),
            "freed pages are unknown"
        );
        release(r as *mut c_void);
    }

    #[test]
    fn mapping_outside_a_reservation_is_refused() {
        let pages = allocate(2);
        let r = placeholder(2);
        assert!(
            XMemMapPhysicalPages((r + PAGE) as *mut c_void, 2, pages.as_ptr()).is_null(),
            "past the reservation's end"
        );
        assert_eq!(query(r + PAGE).host.state, MEM_RESERVE, "left as it was");
        assert!(XMemMapPhysicalPages((r + 1) as *mut c_void, 1, pages.as_ptr()).is_null());
        release(r as *mut c_void);
        XMemFreePhysicalPages(2, pages.as_ptr());
    }

    #[test]
    fn scatter_maps_and_unmaps_page_by_page() {
        let pages = allocate(2);
        let r = placeholder(4);
        let addresses = [(r + 3 * PAGE) as *mut c_void, r as *mut c_void];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, pages.as_ptr()),
            1
        );
        // SAFETY: both pages mapped read-write.
        unsafe {
            (r as *mut u8).write(0x11);
            ((r + 3 * PAGE) as *mut u8).write(0x22);
        }
        assert_eq!(query(r).host.state, MEM_COMMIT);
        assert_eq!(query(r).host.region_size, PAGE);
        assert_eq!(query(r + PAGE).host.state, MEM_RESERVE);
        assert_eq!(query(r + 3 * PAGE).host.state, MEM_COMMIT);

        // Swapped: each address shows the other page.
        let swapped = [pages[1], pages[0]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, swapped.as_ptr()),
            1
        );
        assert_eq!(byte(r), Some(0x22));
        assert_eq!(byte(r + 3 * PAGE), Some(0x11));

        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 2, [0usize, 0].as_ptr()),
            1
        );
        assert_eq!(query(r).host.state, MEM_RESERVE);
        assert_eq!(query(r + 3 * PAGE).host.state, MEM_RESERVE);
        assert_eq!(byte(r), None);
        assert_eq!(
            XMemMapPhysicalPagesScatter(std::ptr::null(), 1, pages.as_ptr()),
            0
        );
        release(r as *mut c_void);
        XMemFreePhysicalPages(2, pages.as_ptr());
    }

    #[test]
    fn write_physical_reaches_pages_through_their_windows() {
        let pages = allocate(3);
        let data: Vec<u8> = (1..=8).collect();
        // Across the first two pages' boundary, before any mapping.
        assert!(write_physical(&pages, PAGE - 4, &data));
        assert!(pages_known(&pages, 0, 3 * PAGE));
        assert!(!pages_known(&pages, 0, 4 * PAGE));
        let r = placeholder(3);
        assert_eq!(
            XMemMapPhysicalPages(r as *mut c_void, 3, pages.as_ptr()) as usize,
            r
        );
        let mut read = [0u8; 8];
        assert!(Host::read_into(r + PAGE - 4, &mut read));
        assert_eq!(read[..], data[..]);
        // And while mapped: the view shows it at once.
        assert!(write_physical(&pages, 2 * PAGE + 1, &[0x5a]));
        assert_eq!(byte(r + 2 * PAGE + 1), Some(0x5a));
        assert!(!write_physical(&[0x7777_0000], 0, &[1]), "an unknown page");
        release(r as *mut c_void);
        XMemFreePhysicalPages(3, pages.as_ptr());
        assert!(!pages_known(&pages, 0, PAGE));
    }

    /// Two pages allocated and one of them shown twice (other tests' memory
    /// moves the live figures, so the arithmetic is checked on its own):
    /// the pages count once, the views not at all.
    #[test]
    fn shown_pages_count_once() {
        let committed = 3 * MIB + 2 * PAGE;
        assert_eq!(
            title_used(committed, 2 * PAGE, 2 * PAGE),
            3 * MIB + 2 * PAGE
        );
        assert_eq!(title_used(3 * MIB, 2 * PAGE, 0), 3 * MIB + 2 * PAGE);
        assert_eq!(title_used(0, 0, PAGE), 0, "never below zero");
    }

    #[test]
    fn working_set_and_transfer() {
        let mine = XMemVirtualAlloc(
            std::ptr::null_mut(),
            MIB,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS,
            PAGE_READWRITE,
        );
        assert!(!mine.is_null());
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_limit > 0);
        assert!(before.title_used >= MIB, "{}", before.title_used);
        assert_eq!(XMemGetWorkingSetStatistics(0, std::ptr::null_mut()), 0);
        release(mine);

        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = crate::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }
}
