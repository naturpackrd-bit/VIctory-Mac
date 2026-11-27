//! The C heap (`api-ms-win-crt-heap`): `malloc` and its family, and the
//! aligned blocks of `_aligned_malloc`, over Rust's allocator.
//!
//! A block from `malloc` has a 16-byte header before it (its size, and 16,
//! its alignment), as the process heaps of `win32/memory.rs` have. An
//! aligned block has its own 16-byte header just before it, which holds the
//! address and size of the allocation it was placed in; the two kinds must
//! not be mixed (as on Windows).
//!
//! Limits: `_expand` never grows a block in place (it fails), and the heap
//! inspection functions report an empty, healthy heap.
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::Layout;
use std::ffi::c_void;

use super::runtime::{EINVAL, ENOMEM, set_errno};
use crate::{HleExport, hle_export};

/// The size of the header before every block.
const HEADER: usize = 16;
/// The alignment of every allocation we make.
const ALIGN: usize = 16;
/// `_heapchk`: the heap is consistent.
const HEAPOK: i32 = -2;
/// `_heapwalk`: no more entries.
const HEAPEND: i32 = -5;

/// What comes before a block from [`malloc`].
#[repr(C)]
struct Header {
    size: usize,
    align: usize,
}

/// What comes before a block from [`_aligned_malloc`]: the allocation it
/// lives in.
#[repr(C)]
struct AlignedHeader {
    original: usize,
    total: usize,
}

const _: () = assert!(std::mem::size_of::<Header>() == HEADER);
const _: () = assert!(std::mem::size_of::<AlignedHeader>() == HEADER);

/// The allocation behind a block of `size` bytes: header and block. A block
/// of 0 bytes gets 16 (Windows returns a usable pointer).
fn block_layout(size: usize) -> Option<Layout> {
    let payload = if size == 0 { 16 } else { size };
    Layout::from_size_align(payload.checked_add(HEADER)?, ALIGN).ok()
}

/// A block of `size` bytes, zeroed if asked; null and `ENOMEM` if there is
/// no room.
fn alloc(size: usize, zero: bool) -> *mut c_void {
    let Some(layout) = block_layout(size) else {
        set_errno(ENOMEM);
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
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    }
    // SAFETY: the start of the fresh allocation, 16-byte aligned and at least
    // 32 bytes long.
    unsafe {
        raw.cast::<Header>().write(Header { size, align: ALIGN });
        raw.add(HEADER).cast()
    }
}

/// The header of a block from [`malloc`].
///
/// # Safety
/// `block` is a live block from [`malloc`].
unsafe fn header(block: *const c_void) -> *mut Header {
    // SAFETY: the caller's block has its header just before it.
    unsafe { block.cast::<u8>().sub(HEADER).cast::<Header>().cast_mut() }
}

/// The size of a live block from [`malloc`]; 0 for null.
fn block_size(block: *const c_void) -> usize {
    if block.is_null() {
        return 0;
    }
    // SAFETY: a live block the title got from us.
    unsafe { (*header(block)).size }
}

/// A block of `size` bytes, 16-byte aligned; null and `ENOMEM` if there is
/// no room.
pub extern "win64" fn malloc(size: usize) -> *mut c_void {
    alloc(size, false)
}

/// A zeroed block of `count * size` bytes; null and `ENOMEM` if the product
/// overflows or there is no room.
pub extern "win64" fn calloc(count: usize, size: usize) -> *mut c_void {
    match count.checked_mul(size) {
        Some(total) => alloc(total, true),
        None => {
            set_errno(ENOMEM);
            std::ptr::null_mut()
        }
    }
}

/// Gives back a block from [`malloc`]; nothing for null.
pub extern "win64" fn free(block: *mut c_void) {
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

/// Resizes a block (possibly moving it), zeroing the growth if asked; null
/// (the old block kept) and `ENOMEM` if there is no room.
fn resize(block: *mut c_void, size: usize, zero: bool) -> *mut c_void {
    if block.is_null() {
        return alloc(size, zero);
    }
    let old = block_size(block);
    let (Some(old_layout), Some(new_layout)) = (block_layout(old), block_layout(size)) else {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    };
    // SAFETY: a live block the title got from us, reallocated with the
    // layout it was allocated with; on success its header and the growth are
    // written within the new allocation.
    unsafe {
        let raw = std::alloc::realloc(header(block).cast(), old_layout, new_layout.size());
        if raw.is_null() {
            set_errno(ENOMEM);
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

/// Resizes a block, keeping its bytes; null `block` is [`malloc`], size 0
/// frees it and returns null (as Windows' `realloc` does).
pub extern "win64" fn realloc(block: *mut c_void, size: usize) -> *mut c_void {
    if !block.is_null() && size == 0 {
        free(block);
        return std::ptr::null_mut();
    }
    resize(block, size, false)
}

/// [`realloc`] to `count * size` bytes, zeroing the growth.
pub extern "win64" fn _recalloc(block: *mut c_void, count: usize, size: usize) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    };
    if !block.is_null() && total == 0 {
        free(block);
        return std::ptr::null_mut();
    }
    resize(block, total, true)
}

/// The size the block was asked with; -1 and `EINVAL` for null.
pub extern "win64" fn _msize(block: *mut c_void) -> usize {
    if block.is_null() {
        set_errno(EINVAL);
        return usize::MAX;
    }
    block_size(block)
}

/// Would resize the block in place; ours never can, so it fails (null and
/// `ENOMEM`), and the title keeps its block as it was.
pub extern "win64" fn _expand(_block: *mut c_void, _size: usize) -> *mut c_void {
    set_errno(ENOMEM);
    std::ptr::null_mut()
}

/// A block of `size` bytes whose byte at `offset` is aligned to `alignment`
/// (a power of two): placed in an allocation of `size + alignment + 16`
/// bytes, with the header just before it.
pub extern "win64" fn _aligned_offset_malloc(
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    if !alignment.is_power_of_two() || (offset != 0 && offset >= size) {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    }
    let layout = size
        .checked_add(alignment)
        .and_then(|n| n.checked_add(HEADER))
        .and_then(|total| Layout::from_size_align(total, ALIGN).ok());
    let Some(layout) = layout else {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    };
    // SAFETY: a layout of non-zero size.
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    }
    let original = raw as usize;
    // The first place past the header whose byte at `offset` is aligned:
    // at most `alignment - 1` bytes further, so the block ends within the
    // allocation.
    let first = original + HEADER;
    let place = (first + offset).next_multiple_of(alignment) - offset;
    // SAFETY: `place - 16 .. place + size` lies within the allocation; the
    // header may be unaligned when `offset` is.
    unsafe {
        let block = raw.add(place - original);
        block
            .sub(HEADER)
            .cast::<AlignedHeader>()
            .write_unaligned(AlignedHeader {
                original,
                total: layout.size(),
            });
        block.cast()
    }
}

/// A block of `size` bytes aligned to `alignment` (a power of two; 0 is
/// the default, 16).
pub extern "win64" fn _aligned_malloc(size: usize, alignment: usize) -> *mut c_void {
    // An alignment of 0 is the default one.
    let alignment = if alignment == 0 { ALIGN } else { alignment };
    _aligned_offset_malloc(size, alignment, 0)
}

/// The header of a block from [`_aligned_offset_malloc`].
///
/// # Safety
/// `block` is a live aligned block.
unsafe fn aligned_header(block: *const c_void) -> AlignedHeader {
    // SAFETY: the caller's block has its header just before it.
    unsafe {
        block
            .cast::<u8>()
            .sub(HEADER)
            .cast::<AlignedHeader>()
            .read_unaligned()
    }
}

/// Gives back a block from [`_aligned_malloc`]; nothing for null.
pub extern "win64" fn _aligned_free(block: *mut c_void) {
    if block.is_null() {
        return;
    }
    // SAFETY: a live aligned block the title gives back; its header names
    // the allocation it was placed in.
    unsafe {
        let header = aligned_header(block);
        if let Ok(layout) = Layout::from_size_align(header.total, ALIGN) {
            std::alloc::dealloc(header.original as *mut u8, layout);
        }
    }
}

/// The size of an aligned block, given the alignment and offset it was
/// asked with (as the CRT computes it: the allocation less what the
/// placement took); -1 and `EINVAL` for null.
pub extern "win64" fn _aligned_msize(
    block: *mut c_void,
    alignment: usize,
    _offset: usize,
) -> usize {
    if block.is_null() || !alignment.is_power_of_two() {
        set_errno(EINVAL);
        return usize::MAX;
    }
    // SAFETY: a live aligned block the title got from us.
    let header = unsafe { aligned_header(block) };
    header.total.saturating_sub(alignment + HEADER)
}

/// The bytes of an aligned block from its start to the end of its
/// allocation: all that may be copied from it.
fn aligned_room(block: *const c_void) -> usize {
    // SAFETY: a live aligned block the title got from us.
    let header = unsafe { aligned_header(block) };
    (header.original + header.total).saturating_sub(block as usize)
}

/// Moves an aligned block to a new one of `size` bytes, keeping its bytes;
/// null `block` is a new block, size 0 frees it and returns null. On
/// failure the old block is kept.
pub extern "win64" fn _aligned_offset_realloc(
    block: *mut c_void,
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    if block.is_null() {
        return _aligned_offset_malloc(size, alignment, offset);
    }
    if size == 0 {
        _aligned_free(block);
        return std::ptr::null_mut();
    }
    let new = _aligned_offset_malloc(size, alignment, offset);
    if new.is_null() {
        return new;
    }
    let keep = aligned_room(block).min(size);
    // SAFETY: `keep` bytes lie within both blocks, which are distinct.
    unsafe { std::ptr::copy_nonoverlapping(block.cast::<u8>(), new.cast::<u8>(), keep) };
    _aligned_free(block);
    new
}

/// [`_aligned_offset_realloc`] with offset 0.
pub extern "win64" fn _aligned_realloc(
    block: *mut c_void,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    _aligned_offset_realloc(block, size, alignment, 0)
}

/// [`_aligned_realloc`] to `count * size` bytes, zeroing the growth.
pub extern "win64" fn _aligned_recalloc(
    block: *mut c_void,
    count: usize,
    size: usize,
    alignment: usize,
) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    };
    let old = if block.is_null() {
        0
    } else {
        _aligned_msize(block, alignment, 0)
    };
    let new = _aligned_realloc(block, total, alignment);
    if !new.is_null() && total > old {
        // SAFETY: the new block is `total` bytes long.
        unsafe { new.cast::<u8>().add(old).write_bytes(0, total - old) };
    }
    new
}

/// Gives unused memory back to the system: nothing to do here.
pub extern "win64" fn _heapmin() -> i32 {
    0
}

/// The heap is consistent (`_HEAPOK`).
pub extern "win64" fn _heapchk() -> i32 {
    HEAPOK
}

/// Walks the heap's blocks: ours lists none (`_HEAPEND`).
pub extern "win64" fn _heapwalk(_entry: *mut c_void) -> i32 {
    HEAPEND
}

/// The process heap's handle (`GetProcessHeap`).
pub extern "win64" fn _get_heap_handle() -> isize {
    super::super::win32::memory::GetProcessHeap() as isize
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(malloc),
        hle_export!(calloc),
        hle_export!(realloc),
        hle_export!(free),
        hle_export!(_msize),
        hle_export!(_expand),
        hle_export!(_recalloc),
        hle_export!(_aligned_malloc),
        hle_export!(_aligned_offset_malloc),
        hle_export!(_aligned_realloc),
        hle_export!(_aligned_recalloc),
        hle_export!(_aligned_offset_realloc),
        hle_export!(_aligned_free),
        hle_export!(_aligned_msize),
        hle_export!(_heapmin),
        hle_export!(_heapchk),
        hle_export!(_heapwalk),
        hle_export!(_get_heap_handle),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_aligned_sized_and_resized() {
        let p = malloc(40);
        assert!(!p.is_null() && (p as usize).is_multiple_of(16));
        assert_eq!(_msize(p), 40);
        // SAFETY: 40 bytes of ours.
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0xAB, 40) };
        let q = realloc(p, 4000);
        assert_eq!(_msize(q), 4000);
        // SAFETY: the first 40 bytes were kept.
        assert!(
            unsafe { std::slice::from_raw_parts(q.cast::<u8>(), 40) }
                .iter()
                .all(|&b| b == 0xAB)
        );
        assert!(realloc(q, 0).is_null());

        let zero = malloc(0);
        assert!(!zero.is_null());
        assert_eq!(_msize(zero), 0);
        free(zero);
        free(std::ptr::null_mut());

        let c = calloc(10, 30);
        // SAFETY: 300 bytes of ours.
        assert!(
            unsafe { std::slice::from_raw_parts(c.cast::<u8>(), 300) }
                .iter()
                .all(|&b| b == 0)
        );
        let c = _recalloc(c, 20, 30);
        // SAFETY: 600 bytes of ours.
        assert!(
            unsafe { std::slice::from_raw_parts(c.cast::<u8>(), 600) }
                .iter()
                .all(|&b| b == 0)
        );
        free(c);
        assert!(calloc(usize::MAX, 2).is_null());
        assert!(_expand(std::ptr::null_mut(), 8).is_null());
    }

    #[test]
    fn aligned_blocks_meet_their_alignment_and_offset() {
        for alignment in [16, 64, 4096] {
            for offset in [0, 8, 24] {
                let p = _aligned_offset_malloc(100, alignment, offset);
                assert!(!p.is_null());
                assert!((p as usize + offset).is_multiple_of(alignment));
                assert_eq!(_aligned_msize(p, alignment, offset), 100);
                // SAFETY: 100 bytes of ours.
                unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0x5A, 100) };
                let q = _aligned_offset_realloc(p, 300, alignment, offset);
                assert!((q as usize + offset).is_multiple_of(alignment));
                // SAFETY: the first 100 bytes were kept.
                let kept = unsafe { std::slice::from_raw_parts(q.cast::<u8>(), 100) };
                assert!(kept.iter().all(|&b| b == 0x5A));
                _aligned_free(q);
            }
        }
        assert!(_aligned_malloc(8, 3).is_null());
        let default = _aligned_malloc(24, 0);
        assert!(!default.is_null() && (default as usize).is_multiple_of(16));
        _aligned_free(default);
        let r = _aligned_recalloc(std::ptr::null_mut(), 4, 8, 64);
        // SAFETY: 32 bytes of ours.
        assert!(
            unsafe { std::slice::from_raw_parts(r.cast::<u8>(), 32) }
                .iter()
                .all(|&b| b == 0)
        );
        _aligned_free(r);
        _aligned_free(std::ptr::null_mut());
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 18);
    }
}
