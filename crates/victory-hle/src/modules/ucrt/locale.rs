//! Locales, the environment and the utilities (`api-ms-win-crt-locale`,
//! `api-ms-win-crt-environment`, `api-ms-win-crt-utility`).
//!
//! There is one locale, "C": `setlocale` accepts any name and answers "C",
//! and a `_locale_t` is a 16-byte marker of zeros (title code that looks
//! inside one finds null pointers). The environment is `runtime.rs`'s list,
//! which `_putenv` changes; nothing reads the host's. `system` runs nothing.
//! `rand_s` is not cryptographic: it hashes a counter with Rust's
//! `RandomState`. `qsort` is a heap sort of our own, which cannot read out
//! of bounds whatever the title's comparison answers (`slice::sort_by` may
//! panic on an inconsistent one).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::ffi::{c_char, c_void};
use std::hash::BuildHasher;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use super::runtime::{
    EINVAL, ENOENT, ENOMEM, ERANGE, environment, environment_entries, leak_list, leak_narrow,
    leak_wide, lock, set_errno,
};
use super::string::{narrow_bytes, wide_units};
use crate::{HleExport, hle_export};

/// The locale's name.
static C_NAME: [u8; 2] = *b"C\0";
static C_WIDE_NAME: [u16; 2] = [b'C' as u16, 0];

/// The current locale's name, whatever was asked: "C".
pub extern "win64" fn setlocale(_category: i32, _locale: *const c_char) -> *mut c_char {
    C_NAME.as_ptr().cast_mut().cast()
}

/// See [`setlocale`].
pub extern "win64" fn _wsetlocale(_category: i32, _locale: *const u16) -> *mut u16 {
    C_WIDE_NAME.as_ptr().cast_mut()
}

thread_local! {
    /// `_DISABLE_PER_THREAD_LOCALE` until the thread asks otherwise.
    static THREAD_LOCALE: Cell<i32> = const { Cell::new(2) };
}

/// Whether the thread has its own locale (kept only): 1 enables, 2
/// disables, 0 asks, -1 resets to the global setting; the previous value,
/// or -1 and `EINVAL` for another flag.
pub extern "win64" fn _configthreadlocale(flag: i32) -> i32 {
    THREAD_LOCALE
        .try_with(|state| {
            let previous = state.get();
            match flag {
                0 => {}
                1 | 2 => state.set(flag),
                -1 => state.set(2),
                _ => {
                    set_errno(EINVAL);
                    return -1;
                }
            }
            previous
        })
        .unwrap_or(2)
}

/// The CRT's `struct lconv`.
#[repr(C)]
pub struct Lconv {
    decimal_point: *mut c_char,
    thousands_sep: *mut c_char,
    grouping: *mut c_char,
    int_curr_symbol: *mut c_char,
    currency_symbol: *mut c_char,
    mon_decimal_point: *mut c_char,
    mon_thousands_sep: *mut c_char,
    mon_grouping: *mut c_char,
    positive_sign: *mut c_char,
    negative_sign: *mut c_char,
    int_frac_digits: i8,
    frac_digits: i8,
    p_cs_precedes: i8,
    p_sep_by_space: i8,
    n_cs_precedes: i8,
    n_sep_by_space: i8,
    p_sign_posn: i8,
    n_sign_posn: i8,
    w_decimal_point: *mut u16,
    w_thousands_sep: *mut u16,
    w_int_curr_symbol: *mut u16,
    w_currency_symbol: *mut u16,
    w_mon_decimal_point: *mut u16,
    w_mon_thousands_sep: *mut u16,
    w_positive_sign: *mut u16,
    w_negative_sign: *mut u16,
}

const _: () = assert!(std::mem::size_of::<Lconv>() == 152);

/// The "C" locale's numeric and monetary conventions: `.` as the decimal
/// point, everything else empty or `CHAR_MAX`.
pub extern "win64" fn localeconv() -> *mut Lconv {
    static LCONV: OnceLock<usize> = OnceLock::new();
    *LCONV.get_or_init(|| {
        let point = leak_narrow(".");
        let empty = leak_narrow("");
        let wide_point = leak_wide(".");
        let wide_empty = leak_wide("");
        let max = i8::MAX;
        Box::into_raw(Box::new(Lconv {
            decimal_point: point,
            thousands_sep: empty,
            grouping: empty,
            int_curr_symbol: empty,
            currency_symbol: empty,
            mon_decimal_point: empty,
            mon_thousands_sep: empty,
            mon_grouping: empty,
            positive_sign: empty,
            negative_sign: empty,
            int_frac_digits: max,
            frac_digits: max,
            p_cs_precedes: max,
            p_sep_by_space: max,
            n_cs_precedes: max,
            n_sep_by_space: max,
            p_sign_posn: max,
            n_sign_posn: max,
            w_decimal_point: wide_point,
            w_thousands_sep: wide_empty,
            w_int_curr_symbol: wide_empty,
            w_currency_symbol: wide_empty,
            w_mon_decimal_point: wide_empty,
            w_mon_thousands_sep: wide_empty,
            w_positive_sign: wide_empty,
            w_negative_sign: wide_empty,
        })) as usize
    }) as *mut Lconv
}

/// A new `_locale_t`: a marker (two null pointers), kept for the process's
/// life.
fn locale_marker() -> *mut c_void {
    Box::into_raw(Box::new([0usize; 2])).cast()
}

/// See [`locale_marker`]; any name gives the "C" locale.
pub extern "win64" fn _create_locale(_category: i32, _name: *const c_char) -> *mut c_void {
    locale_marker()
}

/// See [`_create_locale`].
pub extern "win64" fn _wcreate_locale(_category: i32, _name: *const u16) -> *mut c_void {
    locale_marker()
}

/// Frees a `_locale_t`: markers are kept, so nothing.
pub extern "win64" fn _free_locale(_locale: *mut c_void) {}

/// The current locale as a `_locale_t` (a marker).
pub extern "win64" fn _get_current_locale() -> *mut c_void {
    locale_marker()
}

/// The locale's code page: 1252.
pub extern "win64" fn ___lc_codepage_func() -> u32 {
    1252
}

/// The locale's collation code page: 1252.
pub extern "win64" fn ___lc_collate_cp_func() -> u32 {
    1252
}

static LOCALE_NAMES: [AtomicPtr<u16>; 6] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 6];

/// The names of the locale's categories: six null pointers (the "C"
/// locale).
pub extern "win64" fn ___lc_locale_name_func() -> *mut *mut u16 {
    LOCALE_NAMES.as_ptr().cast_mut().cast()
}

/// `MB_CUR_MAX`: 1.
pub extern "win64" fn ___mb_cur_max_func() -> i32 {
    1
}

/// See [`___mb_cur_max_func`].
pub extern "win64" fn ___mb_cur_max_l_func(_locale: *mut c_void) -> i32 {
    1
}

/// Nothing to change in the "C" locale's conventions.
pub extern "win64" fn __initialize_lconv_for_unsigned_char() {}

/// The multibyte code page: 0 (none).
pub extern "win64" fn _getmbcp() -> i32 {
    0
}

/// Sets the multibyte code page: kept as none; 0.
pub extern "win64" fn _setmbcp(_code_page: i32) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// The environment.

/// Changes to the environment, so that `__p__environ` remakes its arrays.
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// The value of a variable (its name in any case).
fn lookup(name: &str) -> Option<String> {
    lock(environment())
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// Sets a variable, or removes it for an empty value.
fn put(name: &str, value: &str) -> i32 {
    if name.is_empty() || name.contains('=') {
        set_errno(EINVAL);
        return -1;
    }
    {
        let mut list = lock(environment());
        list.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        if !value.is_empty() {
            list.push((name.to_owned(), value.to_owned()));
        }
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
    0
}

/// Sets from a `NAME=value` entry.
fn put_entry(entry: &str) -> i32 {
    match entry.split_once('=') {
        Some((name, value)) => put(name, value),
        None => {
            set_errno(EINVAL);
            -1
        }
    }
}

fn narrow_text(s: *const c_char) -> Option<String> {
    // SAFETY: the title's string, if non-null.
    (!s.is_null()).then(|| String::from_utf8_lossy(unsafe { narrow_bytes(s) }).into_owned())
}

fn wide_text(s: *const u16) -> Option<String> {
    // SAFETY: the title's string, if non-null.
    (!s.is_null()).then(|| String::from_utf16_lossy(unsafe { wide_units(s) }))
}

/// The value of `name` in a `NAME=value` array (`_environ` or
/// `_wenviron`): the address just after the `=` of the first entry whose
/// name matches ignoring ASCII case, inside the array's own string; null
/// when there is none.
///
/// # Safety
/// `array` is a null-terminated array of NUL-terminated entries; `name`
/// has no NUL.
unsafe fn value_in<T: Copy + Into<u32>>(array: *mut *mut T, name: &[T]) -> *mut T {
    let same = |a: T, b: T| {
        let (a, b): (u32, u32) = (a.into(), b.into());
        a == b || (a < 128 && b < 128 && (a as u8).eq_ignore_ascii_case(&(b as u8)))
    };
    // SAFETY: as the caller says; each entry is read up to its NUL (a
    // mismatch or the `=` comes first).
    unsafe {
        let mut entry = array;
        while !entry.is_null() && !(*entry).is_null() {
            let text = *entry;
            let mut i = 0;
            while i < name.len() && same(*text.add(i), name[i]) {
                i += 1;
            }
            if i == name.len() && (*text.add(i)).into() == u32::from(b'=') {
                return text.add(i + 1);
            }
            entry = entry.add(1);
        }
    }
    std::ptr::null_mut()
}

/// A variable's value (its name in any ASCII case), inside `_environ`'s
/// array: valid until the environment next changes (`_putenv` makes a new
/// array and keeps the old one), as on Windows; null when it is not set.
pub extern "win64" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the title's string; the array `__p__environ` keeps.
    unsafe { value_in((*__p__environ()).cast::<*mut u8>(), narrow_bytes(name)).cast() }
}

/// See [`getenv`]; inside `_wenviron`'s array.
pub extern "win64" fn _wgetenv(name: *const u16) -> *mut u16 {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the title's string; the array `__p__wenviron` keeps.
    unsafe { value_in(*__p__wenviron(), wide_units(name)) }
}

/// A value into a buffer of `size` units, its length with the NUL in
/// `*len` (0 when unset); `ERANGE` if it does not fit.
///
/// # Safety
/// `buffer` is null or has `size` units.
unsafe fn value_into<T: Copy + Default>(
    len: *mut usize,
    buffer: *mut T,
    size: usize,
    value: Option<Vec<T>>,
) -> i32 {
    if len.is_null() || (buffer.is_null() && size != 0) {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: the title's place for the length, and its buffer.
    unsafe {
        if !buffer.is_null() {
            buffer.write(T::default());
        }
        let Some(value) = value else {
            len.write_unaligned(0);
            return 0;
        };
        len.write_unaligned(value.len() + 1);
        if buffer.is_null() {
            return 0;
        }
        if value.len() >= size {
            set_errno(ERANGE);
            return ERANGE;
        }
        std::ptr::copy_nonoverlapping(value.as_ptr(), buffer, value.len());
        buffer.add(value.len()).write(T::default());
    }
    0
}

/// A variable's value into the title's buffer.
pub extern "win64" fn getenv_s(
    len: *mut usize,
    buffer: *mut c_char,
    size: usize,
    name: *const c_char,
) -> i32 {
    let Some(name) = narrow_text(name) else {
        set_errno(EINVAL);
        return EINVAL;
    };
    let value = lookup(&name).map(String::into_bytes);
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { value_into(len, buffer.cast::<u8>(), size, value) }
}

/// See [`getenv_s`].
pub extern "win64" fn _wgetenv_s(
    len: *mut usize,
    buffer: *mut u16,
    size: usize,
    name: *const u16,
) -> i32 {
    let Some(name) = wide_text(name) else {
        set_errno(EINVAL);
        return EINVAL;
    };
    let value = lookup(&name).map(|v| v.encode_utf16().collect());
    // SAFETY: the title's buffer of `size` units.
    unsafe { value_into(len, buffer, size, value) }
}

/// A copy of `value` in a block from `malloc`, its length with the NUL in
/// `*len`; null and 0 when unset.
///
/// # Safety
/// `buffer` is writable; `len` null or writable.
unsafe fn duplicate_into<T: Copy + Default>(
    buffer: *mut *mut T,
    len: *mut usize,
    value: Option<Vec<T>>,
) -> i32 {
    // SAFETY: the title's places for the copy and its length.
    unsafe {
        buffer.write_unaligned(std::ptr::null_mut());
        if !len.is_null() {
            len.write_unaligned(0);
        }
        let Some(mut value) = value else { return 0 };
        value.push(T::default());
        let copy = super::heap::malloc(value.len() * std::mem::size_of::<T>()).cast::<T>();
        if copy.is_null() {
            set_errno(ENOMEM);
            return ENOMEM;
        }
        std::ptr::copy_nonoverlapping(value.as_ptr(), copy, value.len());
        buffer.write_unaligned(copy);
        if !len.is_null() {
            len.write_unaligned(value.len());
        }
    }
    0
}

/// A variable's value copied to a block from `malloc`.
pub extern "win64" fn _dupenv_s(
    buffer: *mut *mut c_char,
    len: *mut usize,
    name: *const c_char,
) -> i32 {
    if buffer.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    let value = narrow_text(name).map(|n| lookup(&n).map(String::into_bytes));
    let Some(value) = value else {
        set_errno(EINVAL);
        return EINVAL;
    };
    // SAFETY: the title's places for the copy and its length.
    unsafe { duplicate_into(buffer.cast::<*mut u8>(), len, value) }
}

/// See [`_dupenv_s`].
pub extern "win64" fn _wdupenv_s(buffer: *mut *mut u16, len: *mut usize, name: *const u16) -> i32 {
    if buffer.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    let value = wide_text(name).map(|n| lookup(&n).map(|v| v.encode_utf16().collect()));
    let Some(value) = value else {
        set_errno(EINVAL);
        return EINVAL;
    };
    // SAFETY: the title's places for the copy and its length.
    unsafe { duplicate_into(buffer, len, value) }
}

/// Sets a variable from `NAME=value` (`NAME=` removes it); 0, or -1 and
/// `EINVAL`.
pub extern "win64" fn _putenv(entry: *const c_char) -> i32 {
    narrow_text(entry).map_or_else(
        || {
            set_errno(EINVAL);
            -1
        },
        |e| put_entry(&e),
    )
}

/// See [`_putenv`].
pub extern "win64" fn _wputenv(entry: *const u16) -> i32 {
    wide_text(entry).map_or_else(
        || {
            set_errno(EINVAL);
            -1
        },
        |e| put_entry(&e),
    )
}

/// Sets a variable (an empty value removes it); 0 or `EINVAL`.
pub extern "win64" fn _putenv_s(name: *const c_char, value: *const c_char) -> i32 {
    match (narrow_text(name), narrow_text(value)) {
        (Some(name), Some(value)) if put(&name, &value) == 0 => 0,
        _ => {
            set_errno(EINVAL);
            EINVAL
        }
    }
}

/// See [`_putenv_s`].
pub extern "win64" fn _wputenv_s(name: *const u16, value: *const u16) -> i32 {
    match (wide_text(name), wide_text(value)) {
        (Some(name), Some(value)) if put(&name, &value) == 0 => 0,
        _ => {
            set_errno(EINVAL);
            EINVAL
        }
    }
}

static ENVIRON: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());
static WENVIRON: AtomicPtr<*mut u16> = AtomicPtr::new(std::ptr::null_mut());
static ENVIRON_MADE: AtomicU64 = AtomicU64::new(0);
static WENVIRON_MADE: AtomicU64 = AtomicU64::new(0);

/// `_environ`: remade (and the old array kept) after the environment
/// changed.
pub extern "win64" fn __p__environ() -> *mut *mut *mut c_char {
    let generation = GENERATION.load(Ordering::Acquire);
    if ENVIRON_MADE.swap(generation, Ordering::AcqRel) != generation {
        let list = environment_entries()
            .iter()
            .map(|e| leak_narrow(e))
            .collect();
        ENVIRON.store(leak_list(list), Ordering::Release);
    }
    ENVIRON.as_ptr()
}

/// `_wenviron`: see [`__p__environ`].
pub extern "win64" fn __p__wenviron() -> *mut *mut *mut u16 {
    let generation = GENERATION.load(Ordering::Acquire);
    if WENVIRON_MADE.swap(generation, Ordering::AcqRel) != generation {
        let list = environment_entries().iter().map(|e| leak_wide(e)).collect();
        WENVIRON.store(leak_list(list), Ordering::Release);
    }
    WENVIRON.as_ptr()
}

/// Searches the paths of a variable for a file: none is found (the empty
/// string, `ENOENT`).
pub extern "win64" fn _searchenv(_file: *const c_char, _var: *const c_char, out: *mut c_char) {
    if !out.is_null() {
        // SAFETY: the title's buffer for a path.
        unsafe { out.write(0) };
    }
    set_errno(ENOENT);
}

/// See [`_searchenv`].
pub extern "win64" fn _wsearchenv(_file: *const u16, _var: *const u16, out: *mut u16) {
    if !out.is_null() {
        // SAFETY: the title's buffer for a path.
        unsafe { out.write(0) };
    }
    set_errno(ENOENT);
}

/// Runs a command: nothing runs (-1, `ENOENT`); for null, 0: there is no
/// command processor.
pub extern "win64" fn system(command: *const c_char) -> i32 {
    if command.is_null() {
        return 0;
    }
    tracing::warn!(command = ?narrow_text(command), "the title runs a command: refused");
    set_errno(ENOENT);
    -1
}

/// See [`system`].
pub extern "win64" fn _wsystem(command: *const u16) -> i32 {
    if command.is_null() {
        return 0;
    }
    tracing::warn!(command = ?wide_text(command), "the title runs a command: refused");
    set_errno(ENOENT);
    -1
}

// ---------------------------------------------------------------------------
// Random numbers.

thread_local! {
    static SEED: Cell<u32> = const { Cell::new(1) };
}

/// The CRT's generator: `seed = seed * 214013 + 2531011`, bits 16..31 of
/// it; per thread, seeded with 1.
pub extern "win64" fn rand() -> i32 {
    SEED.try_with(|seed| {
        let next = seed.get().wrapping_mul(214_013).wrapping_add(2_531_011);
        seed.set(next);
        ((next >> 16) & 0x7FFF) as i32
    })
    .unwrap_or(0)
}

/// Seeds the calling thread's [`rand`].
pub extern "win64" fn srand(seed: u32) {
    let _ = SEED.try_with(|s| s.set(seed));
}

/// A random number (not cryptographic: see the module's notes); `EINVAL`
/// for null.
pub extern "win64" fn rand_s(out: *mut u32) -> i32 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    let hash = RandomState::new().hash_one(COUNTER.fetch_add(1, Ordering::Relaxed));
    // SAFETY: the title's place for the number.
    unsafe { out.write_unaligned(hash as u32) };
    0
}

// ---------------------------------------------------------------------------
// Sorting and searching.

/// The title's comparison of two elements.
pub type Compare = Option<extern "win64" fn(*const c_void, *const c_void) -> i32>;
/// The title's comparison with a context.
pub type CompareContext =
    Option<extern "win64" fn(*mut c_void, *const c_void, *const c_void) -> i32>;

/// Swaps two elements of `size` bytes.
///
/// # Safety
/// Both are within the array and distinct.
unsafe fn swap(a: *mut u8, b: *mut u8, size: usize) {
    // SAFETY: as the caller says.
    unsafe { std::ptr::swap_nonoverlapping(a, b, size) };
}

/// Heap-sorts `count` elements of `size` bytes at `base` by `compare`;
/// every index it touches is below `count`, whatever `compare` answers.
///
/// # Safety
/// `base` holds `count` elements of `size` bytes.
unsafe fn heap_sort(
    base: *mut u8,
    count: usize,
    size: usize,
    compare: &mut dyn FnMut(*const u8, *const u8) -> i32,
) {
    let at = |i: usize| base.wrapping_add(i * size);
    let sift =
        |mut root: usize, end: usize, compare: &mut dyn FnMut(*const u8, *const u8) -> i32| {
            loop {
                let mut child = 2 * root + 1;
                if child >= end {
                    break;
                }
                if child + 1 < end && compare(at(child), at(child + 1)) < 0 {
                    child += 1;
                }
                if compare(at(root), at(child)) >= 0 {
                    break;
                }
                // SAFETY: `root < child < end <= count`.
                unsafe { swap(at(root), at(child), size) };
                root = child;
            }
        };
    for start in (0..count / 2).rev() {
        sift(start, count, compare);
    }
    for end in (1..count).rev() {
        // SAFETY: `0 < end < count`.
        unsafe { swap(at(0), at(end), size) };
        sift(0, end, compare);
    }
}

/// Whether an array of `count` elements of `size` bytes can be sorted.
fn sortable(base: *mut c_void, count: usize, size: usize) -> bool {
    if count > 1 && (base.is_null() || size == 0 || count.checked_mul(size).is_none()) {
        set_errno(EINVAL);
        return false;
    }
    count > 1
}

/// Sorts an array by the title's comparison (heap sort: not stable).
pub extern "win64" fn qsort(base: *mut c_void, count: usize, size: usize, compare: Compare) {
    let Some(compare) = compare else {
        set_errno(EINVAL);
        return;
    };
    if sortable(base, count, size) {
        let mut by = |a: *const u8, b: *const u8| compare(a.cast(), b.cast());
        // SAFETY: the title's array.
        unsafe { heap_sort(base.cast(), count, size, &mut by) };
    }
}

/// [`qsort`] with a context for the comparison.
pub extern "win64" fn qsort_s(
    base: *mut c_void,
    count: usize,
    size: usize,
    compare: CompareContext,
    context: *mut c_void,
) {
    let Some(compare) = compare else {
        set_errno(EINVAL);
        return;
    };
    if sortable(base, count, size) {
        let mut by = |a: *const u8, b: *const u8| compare(context, a.cast(), b.cast());
        // SAFETY: the title's array.
        unsafe { heap_sort(base.cast(), count, size, &mut by) };
    }
}

/// A binary search of a sorted array; the element found or null.
fn binary_search(
    key: *const c_void,
    base: *const c_void,
    count: usize,
    size: usize,
    compare: &mut dyn FnMut(*const c_void, *const c_void) -> i32,
) -> *mut c_void {
    if base.is_null() || size == 0 {
        return std::ptr::null_mut();
    }
    let (mut low, mut high) = (0, count);
    while low < high {
        let mid = low + (high - low) / 2;
        let element = base.cast::<u8>().wrapping_add(mid * size).cast::<c_void>();
        match compare(key, element) {
            0 => return element.cast_mut(),
            c if c < 0 => high = mid,
            _ => low = mid + 1,
        }
    }
    std::ptr::null_mut()
}

/// Searches a sorted array for `key`.
pub extern "win64" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    count: usize,
    size: usize,
    compare: Compare,
) -> *mut c_void {
    let Some(compare) = compare else {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    };
    binary_search(key, base, count, size, &mut |a, b| compare(a, b))
}

/// [`bsearch`] with a context for the comparison.
pub extern "win64" fn bsearch_s(
    key: *const c_void,
    base: *const c_void,
    count: usize,
    size: usize,
    compare: CompareContext,
    context: *mut c_void,
) -> *mut c_void {
    let Some(compare) = compare else {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    };
    binary_search(key, base, count, size, &mut |a, b| compare(context, a, b))
}

/// Searches an array of `*count` elements one by one for one the
/// comparison says is equal (0).
pub extern "win64" fn _lfind(
    key: *const c_void,
    base: *const c_void,
    count: *mut u32,
    size: u32,
    compare: Compare,
) -> *mut c_void {
    let (Some(compare), false, false) = (compare, count.is_null(), base.is_null()) else {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    };
    // SAFETY: the title's element count.
    let n = unsafe { count.read_unaligned() } as usize;
    (0..n)
        .map(|i| {
            base.cast::<u8>()
                .wrapping_add(i * size as usize)
                .cast::<c_void>()
        })
        .find(|&element| compare(key, element) == 0)
        .map_or(std::ptr::null_mut(), <*const c_void>::cast_mut)
}

// ---------------------------------------------------------------------------
// Bits.

/// Rotates left (`unsigned long` is 32 bits).
pub extern "win64" fn _lrotl(value: u32, shift: i32) -> u32 {
    value.rotate_left(shift as u32 & 31)
}

/// Rotates right.
pub extern "win64" fn _lrotr(value: u32, shift: i32) -> u32 {
    value.rotate_right(shift as u32 & 31)
}

/// Rotates left.
pub extern "win64" fn _rotl(value: u32, shift: i32) -> u32 {
    value.rotate_left(shift as u32 & 31)
}

/// Rotates right.
pub extern "win64" fn _rotr(value: u32, shift: i32) -> u32 {
    value.rotate_right(shift as u32 & 31)
}

/// Rotates left.
pub extern "win64" fn _rotl64(value: u64, shift: i32) -> u64 {
    value.rotate_left(shift as u32 & 63)
}

/// Rotates right.
pub extern "win64" fn _rotr64(value: u64, shift: i32) -> u64 {
    value.rotate_right(shift as u32 & 63)
}

/// Swaps the bytes.
pub extern "win64" fn _byteswap_ushort(value: u16) -> u16 {
    value.swap_bytes()
}

/// Swaps the bytes.
pub extern "win64" fn _byteswap_ulong(value: u32) -> u32 {
    value.swap_bytes()
}

/// Swaps the bytes.
pub extern "win64" fn _byteswap_uint64(value: u64) -> u64 {
    value.swap_bytes()
}

/// Copies `n` bytes swapping each pair (an odd last byte is not copied).
pub extern "win64" fn _swab(src: *mut c_char, dst: *mut c_char, n: i32) {
    if src.is_null() || dst.is_null() || n < 0 {
        set_errno(EINVAL);
        return;
    }
    for i in (0..n as usize / 2).map(|i| 2 * i) {
        // SAFETY: the title's buffers of `n` bytes.
        unsafe {
            let (a, b) = (src.add(i).read(), src.add(i + 1).read());
            dst.add(i).write(b);
            dst.add(i + 1).write(a);
        }
    }
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(setlocale),
        hle_export!(_wsetlocale),
        hle_export!(_configthreadlocale),
        hle_export!(localeconv),
        hle_export!(_create_locale),
        hle_export!(_wcreate_locale),
        hle_export!(_free_locale),
        hle_export!(_get_current_locale),
        hle_export!(___lc_codepage_func),
        hle_export!(___lc_collate_cp_func),
        hle_export!(___lc_locale_name_func),
        hle_export!(___mb_cur_max_func),
        hle_export!(___mb_cur_max_l_func),
        hle_export!(__initialize_lconv_for_unsigned_char),
        hle_export!(_getmbcp),
        hle_export!(_setmbcp),
        hle_export!(getenv),
        hle_export!(_wgetenv),
        hle_export!(getenv_s),
        hle_export!(_wgetenv_s),
        hle_export!(_dupenv_s),
        hle_export!(_wdupenv_s),
        hle_export!(_putenv),
        hle_export!(_wputenv),
        hle_export!(_putenv_s),
        hle_export!(_wputenv_s),
        hle_export!(__p__environ),
        hle_export!(__p__wenviron),
        hle_export!(_searchenv),
        hle_export!(_wsearchenv),
        hle_export!(system),
        hle_export!(_wsystem),
        hle_export!(rand),
        hle_export!(srand),
        hle_export!(rand_s),
        hle_export!(qsort),
        hle_export!(qsort_s),
        hle_export!(bsearch),
        hle_export!(bsearch_s),
        hle_export!(_lfind),
        hle_export!(_lrotl),
        hle_export!(_lrotr),
        hle_export!(_rotl),
        hle_export!(_rotr),
        hle_export!(_rotl64),
        hle_export!(_rotr64),
        hle_export!(_byteswap_ushort),
        hle_export!(_byteswap_ulong),
        hle_export!(_byteswap_uint64),
        hle_export!(_swab),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    extern "win64" fn reversed(a: *const c_void, b: *const c_void) -> i32 {
        // SAFETY: two `u32`s of the array being sorted.
        let (a, b) = unsafe { (*a.cast::<u32>(), *b.cast::<u32>()) };
        b.cmp(&a) as i32
    }

    extern "win64" fn ascending(a: *const c_void, b: *const c_void) -> i32 {
        // SAFETY: two `u32`s.
        let (a, b) = unsafe { (*a.cast::<u32>(), *b.cast::<u32>()) };
        a.cmp(&b) as i32
    }

    extern "win64" fn liar(_: *const c_void, _: *const c_void) -> i32 {
        static FLIP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);
        FLIP.fetch_xor(-2, Ordering::Relaxed)
    }

    #[test]
    fn qsort_sorts_and_bsearch_finds() {
        let mut seed = 12_345u32;
        let mut values: Vec<u32> = (0..1000)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                seed
            })
            .collect();
        let mut expected = values.clone();
        expected.sort_unstable();
        expected.reverse();
        qsort(values.as_mut_ptr().cast(), values.len(), 4, Some(reversed));
        assert_eq!(values, expected);
        // An inconsistent comparison stays in bounds.
        qsort(values.as_mut_ptr().cast(), values.len(), 4, Some(liar));
        values.sort_unstable();
        let key = values[500];
        let found = bsearch(
            (&raw const key).cast(),
            values.as_ptr().cast(),
            1000,
            4,
            Some(ascending),
        );
        // SAFETY: an element of `values`.
        assert_eq!(unsafe { *found.cast::<u32>() }, key);
        let missing = values.iter().max().unwrap().wrapping_add(1);
        let found = bsearch(
            (&raw const missing).cast(),
            values.as_ptr().cast(),
            1000,
            4,
            Some(ascending),
        );
        assert!(found.is_null() || missing == 0);
        let mut n = 1000u32;
        let found = _lfind(
            (&raw const key).cast(),
            values.as_ptr().cast(),
            &mut n,
            4,
            Some(ascending),
        );
        assert!(!found.is_null());
    }

    #[test]
    fn the_environment_round_trips() {
        assert_eq!(_putenv(c"VICTORY_TEST=one".as_ptr()), 0);
        // SAFETY: a per-thread NUL-terminated value.
        let value = unsafe { CStr::from_ptr(getenv(c"victory_test".as_ptr())) };
        assert_eq!(value.to_str(), Ok("one"));
        // SAFETY: the null-terminated array `__p__environ` made.
        let seen = unsafe {
            let mut entry = *__p__environ();
            let mut seen = false;
            while !(*entry).is_null() {
                seen |= CStr::from_ptr(*entry).to_bytes() == b"VICTORY_TEST=one";
                entry = entry.add(1);
            }
            seen
        };
        assert!(seen);
        let mut len = 0;
        let mut buffer = [0 as c_char; 8];
        assert_eq!(
            getenv_s(&mut len, buffer.as_mut_ptr(), 8, c"VICTORY_TEST".as_ptr()),
            0
        );
        assert_eq!(len, 4);
        assert_eq!(
            getenv_s(&mut len, buffer.as_mut_ptr(), 2, c"VICTORY_TEST".as_ptr()),
            ERANGE
        );
        let mut copy = std::ptr::null_mut();
        assert_eq!(_dupenv_s(&mut copy, &mut len, c"VICTORY_TEST".as_ptr()), 0);
        // SAFETY: a NUL-terminated copy.
        assert_eq!(unsafe { CStr::from_ptr(copy) }.to_str(), Ok("one"));
        super::super::heap::free(copy.cast());
        assert_eq!(_putenv(c"VICTORY_TEST=".as_ptr()), 0);
        assert!(getenv(c"VICTORY_TEST".as_ptr()).is_null());
        assert_eq!(_dupenv_s(&mut copy, &mut len, c"VICTORY_TEST".as_ptr()), 0);
        assert!(copy.is_null() && len == 0);
        assert_eq!(_putenv(c"no equals".as_ptr()), -1);
        assert!(!getenv(c"NUMBER_OF_PROCESSORS".as_ptr()).is_null());
    }

    #[test]
    fn getenv_pointers_stay_valid() {
        let first = getenv(c"computername".as_ptr());
        let second = getenv(c"PROCESSOR_ARCHITECTURE".as_ptr());
        assert!(!first.is_null() && !second.is_null());
        // SAFETY: values inside the leaked environment arrays.
        unsafe {
            assert_eq!(CStr::from_ptr(first).to_bytes(), b"VICTORY");
            assert_eq!(CStr::from_ptr(second).to_bytes(), b"AMD64");
        }
        let wide: Vec<u16> = "ComputerName\0".encode_utf16().collect();
        let value = _wgetenv(wide.as_ptr());
        assert!(!value.is_null());
        // SAFETY: as above, NUL-terminated.
        let units = unsafe { super::super::string::wide_units(value) };
        assert_eq!(String::from_utf16_lossy(units), "VICTORY");
        assert!(getenv(c"COMPUTERNAM".as_ptr()).is_null());
        assert!(getenv(c"COMPUTERNAME=".as_ptr()).is_null());
    }

    #[test]
    fn rand_follows_the_msvc_sequence() {
        std::thread::spawn(|| {
            let first: Vec<i32> = (0..5).map(|_| rand()).collect();
            assert_eq!(first, [41, 18467, 6334, 26500, 19169]);
            srand(1);
            assert_eq!(rand(), 41);
        })
        .join()
        .unwrap();
        let mut a = 0;
        let mut b = 0;
        assert_eq!(rand_s(&mut a), 0);
        assert_eq!(rand_s(&mut b), 0);
        assert_ne!(a, b);
    }

    #[test]
    fn locale_and_bits() {
        // SAFETY: the static name.
        assert_eq!(
            unsafe { CStr::from_ptr(setlocale(0, c"".as_ptr())) }.to_str(),
            Ok("C")
        );
        // SAFETY: the static conventions.
        let point = unsafe { CStr::from_ptr((*localeconv()).decimal_point) };
        assert_eq!(point.to_str(), Ok("."));
        assert_eq!(_configthreadlocale(0), 2);
        assert_eq!(_rotl(0x8000_0001, 1), 3);
        assert_eq!(_rotr64(1, 1), 1 << 63);
        assert_eq!(_byteswap_ulong(0x1122_3344), 0x4433_2211);
        let mut src = *b"abcde";
        let mut dst = [0u8; 5];
        _swab(src.as_mut_ptr().cast(), dst.as_mut_ptr().cast(), 5);
        assert_eq!(&dst[..4], b"badc");
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 50);
    }
}
