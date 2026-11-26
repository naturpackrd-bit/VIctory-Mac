//! Strings, characters and number conversions (`api-ms-win-crt-string`,
//! `api-ms-win-crt-convert`, and `vcruntime140.dll`'s `mem*`).
//!
//! Narrow strings are bytes. The title's ANSI code page is not known here,
//! so case and character classes follow ASCII rules for them (bytes from
//! 0x80 up are of no class and have no case). Wide strings are UTF-16, and
//! their classes and case come from Unicode (Rust's `char`). The multibyte
//! conversions (`mbstowcs` and its family) convert UTF-8 to UTF-16 and back,
//! lossily for the whole-string ones (a bad sequence becomes U+FFFD) and
//! with `EILSEQ` for the one-character ones.
//!
//! Limits: `memcpy` is `memmove` (overlapping copies do what a title that
//! relies on them expects); `mbrtowc` keeps no partial character in its
//! state; `strtof` rounds through a double.
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::{c_char, c_void};

use super::runtime::{_invalid_parameter_noinfo, EILSEQ, EINVAL, ERANGE, STRUNCATE, set_errno};
use crate::{HleExport, hle_export};

/// `_TRUNCATE`: copy what fits.
const TRUNCATE: usize = usize::MAX;

/// A character of a narrow (byte) or wide (UTF-16) string.
trait Unit: Copy + Eq + Default + 'static {
    fn value(self) -> u32;
    fn from_value(value: u32) -> Self;
    fn lower(self) -> Self;
    fn upper(self) -> Self;
    fn is_space(self) -> bool;
}

impl Unit for u8 {
    fn value(self) -> u32 {
        u32::from(self)
    }
    fn from_value(value: u32) -> Self {
        value as u8
    }
    fn lower(self) -> Self {
        self.to_ascii_lowercase()
    }
    fn upper(self) -> Self {
        self.to_ascii_uppercase()
    }
    fn is_space(self) -> bool {
        matches!(self, b' ' | b'\t'..=b'\r')
    }
}

impl Unit for u16 {
    fn value(self) -> u32 {
        u32::from(self)
    }
    fn from_value(value: u32) -> Self {
        value as u16
    }
    fn lower(self) -> Self {
        wide_lower(self)
    }
    fn upper(self) -> Self {
        wide_upper(self)
    }
    fn is_space(self) -> bool {
        wide_class(self) & SPACE != 0
    }
}

/// The one UTF-16 unit `c` maps to under `map`, or `c` itself.
fn wide_case(c: u16, map: impl Fn(char) -> Option<char>) -> u16 {
    char::from_u32(u32::from(c))
        .and_then(map)
        .and_then(|m| u16::try_from(u32::from(m)).ok())
        .unwrap_or(c)
}

fn wide_lower(c: u16) -> u16 {
    wide_case(c, |ch| {
        let mut it = ch.to_lowercase();
        it.next().filter(|_| it.next().is_none())
    })
}

fn wide_upper(c: u16) -> u16 {
    wide_case(c, |ch| {
        let mut it = ch.to_uppercase();
        it.next().filter(|_| it.next().is_none())
    })
}

/// The number of units before the NUL.
///
/// # Safety
/// `s` is a NUL-terminated string.
unsafe fn len<T: Unit>(s: *const T) -> usize {
    let mut n = 0;
    // SAFETY: the caller's string, read up to its NUL.
    while unsafe { *s.add(n) } != T::default() {
        n += 1;
    }
    n
}

/// The number of units before the NUL, at most `max`.
///
/// # Safety
/// `s` has a NUL or `max` readable units.
unsafe fn nlen<T: Unit>(s: *const T, max: usize) -> usize {
    let mut n = 0;
    // SAFETY: as the caller says.
    while n < max && unsafe { *s.add(n) } != T::default() {
        n += 1;
    }
    n
}

/// The length of a wide string, for the other files.
///
/// # Safety
/// `s` is a NUL-terminated wide string.
pub(crate) unsafe fn wide_len(s: *const u16) -> usize {
    // SAFETY: as the caller says.
    unsafe { len(s) }
}

/// A NUL-terminated string's units, without the NUL; empty for null.
///
/// # Safety
/// `s` is null or a NUL-terminated string that outlives the slice.
unsafe fn units<'a, T: Unit>(s: *const T) -> &'a [T] {
    if s.is_null() {
        return &[];
    }
    // SAFETY: as the caller says.
    unsafe { std::slice::from_raw_parts(s, len(s)) }
}

/// The bytes of a narrow string, for the other files; empty for null.
///
/// # Safety
/// As [`units`].
pub(crate) unsafe fn narrow_bytes<'a>(s: *const c_char) -> &'a [u8] {
    // SAFETY: as the caller says.
    unsafe { units(s.cast::<u8>()) }
}

/// The units of a wide string, for the other files; empty for null.
///
/// # Safety
/// As [`units`].
pub(crate) unsafe fn wide_units<'a>(s: *const u16) -> &'a [u16] {
    // SAFETY: as the caller says.
    unsafe { units(s) }
}

// ---------------------------------------------------------------------------
// Memory.

/// Copies `n` bytes (overlapping copies are allowed, as in `memmove`).
pub extern "win64" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    memmove(dst, src, n)
}

/// Copies `n` bytes; the two may overlap.
pub extern "win64" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    if n != 0 && !dst.is_null() && !src.is_null() {
        // SAFETY: the title's buffers of `n` bytes.
        unsafe { std::ptr::copy(src.cast::<u8>(), dst.cast::<u8>(), n) };
    }
    dst
}

/// Fills `n` bytes with `c`.
pub extern "win64" fn memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
    if n != 0 && !dst.is_null() {
        // SAFETY: the title's buffer of `n` bytes.
        unsafe { dst.cast::<u8>().write_bytes(c as u8, n) };
    }
    dst
}

/// The `n` bytes of a buffer; empty for null.
///
/// # Safety
/// `p` is null or has `n` readable bytes.
unsafe fn bytes<'a>(p: *const c_void, n: usize) -> &'a [u8] {
    if p.is_null() || n == 0 {
        return &[];
    }
    // SAFETY: as the caller says.
    unsafe { std::slice::from_raw_parts(p.cast(), n) }
}

/// Compares `n` bytes as unsigned.
pub extern "win64" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    // SAFETY: the title's buffers of `n` bytes.
    let (a, b) = unsafe { (bytes(a, n), bytes(b, n)) };
    a.iter()
        .zip(b)
        .find(|(x, y)| x != y)
        .map_or(0, |(x, y)| i32::from(*x) - i32::from(*y))
}

/// Compares `n` bytes ignoring ASCII case.
pub extern "win64" fn _memicmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    // SAFETY: the title's buffers of `n` bytes.
    let (a, b) = unsafe { (bytes(a, n), bytes(b, n)) };
    a.iter()
        .zip(b)
        .map(|(x, y)| (x.to_ascii_lowercase(), y.to_ascii_lowercase()))
        .find(|(x, y)| x != y)
        .map_or(0, |(x, y)| i32::from(x) - i32::from(y))
}

/// The first byte `c` in `n` bytes; null if there is none.
pub extern "win64" fn memchr(s: *const c_void, c: i32, n: usize) -> *mut c_void {
    // SAFETY: the title's buffer of `n` bytes.
    let found = unsafe { bytes(s, n) }.iter().position(|&b| b == c as u8);
    found.map_or(std::ptr::null_mut(), |i| {
        s.cast::<u8>().wrapping_add(i).cast_mut().cast()
    })
}

/// Copies bytes up to and including the first `c`, at most `n`; the byte
/// after the copied `c` in `dst`, or null if there was none.
pub extern "win64" fn _memccpy(
    dst: *mut c_void,
    src: *const c_void,
    c: i32,
    n: usize,
) -> *mut c_void {
    if dst.is_null() || src.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the title's buffers of `n` bytes (up to the `c`).
    unsafe {
        for i in 0..n {
            let b = src.cast::<u8>().add(i).read();
            dst.cast::<u8>().add(i).write(b);
            if b == c as u8 {
                return dst.cast::<u8>().add(i + 1).cast();
            }
        }
    }
    std::ptr::null_mut()
}

/// See [`_memccpy`].
pub extern "win64" fn memccpy(
    dst: *mut c_void,
    src: *const c_void,
    c: i32,
    n: usize,
) -> *mut c_void {
    _memccpy(dst, src, c, n)
}

// ---------------------------------------------------------------------------
// Generic string operations.

/// Compares at most `n` units, folded to lower case if asked.
///
/// # Safety
/// `a` and `b` are NUL-terminated strings (or have `n` units).
unsafe fn compare<T: Unit>(a: *const T, b: *const T, n: usize, fold: bool) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    for i in 0..n {
        // SAFETY: as the caller says; we stop at the first NUL.
        let (mut x, mut y) = unsafe { (*a.add(i), *b.add(i)) };
        if fold {
            (x, y) = (x.lower(), y.lower());
        }
        if x != y {
            return x.value() as i32 - y.value() as i32;
        }
        if x == T::default() {
            break;
        }
    }
    0
}

/// The first `c` in `s` (its NUL for `c` 0); null if there is none.
///
/// # Safety
/// `s` is a NUL-terminated string.
unsafe fn find<T: Unit>(s: *const T, c: T) -> *mut T {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's string, its NUL included.
    let n = unsafe { len(s) };
    (0..=n)
        // SAFETY: as above.
        .find(|&i| unsafe { *s.add(i) } == c)
        .map_or(std::ptr::null_mut(), |i| s.wrapping_add(i).cast_mut())
}

/// The last `c` in `s` (its NUL for `c` 0); null if there is none.
///
/// # Safety
/// `s` is a NUL-terminated string.
unsafe fn find_last<T: Unit>(s: *const T, c: T) -> *mut T {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's string, its NUL included.
    let n = unsafe { len(s) };
    (0..=n)
        .rev()
        // SAFETY: as above.
        .find(|&i| unsafe { *s.add(i) } == c)
        .map_or(std::ptr::null_mut(), |i| s.wrapping_add(i).cast_mut())
}

/// The first place `needle` appears in `s`; `s` for an empty needle.
///
/// # Safety
/// Both are NUL-terminated strings.
unsafe fn find_string<T: Unit>(s: *const T, needle: *const T) -> *mut T {
    if s.is_null() || needle.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's strings.
    let (hay, needle) = unsafe { (units(s), units(needle)) };
    if needle.is_empty() {
        return s.cast_mut();
    }
    hay.windows(needle.len())
        .position(|w| w == needle)
        .map_or(std::ptr::null_mut(), |i| s.wrapping_add(i).cast_mut())
}

/// The length of the start of `s` made of units in `set` (`accept`) or of
/// units not in it.
///
/// # Safety
/// Both are NUL-terminated strings.
unsafe fn span<T: Unit>(s: *const T, set: *const T, accept: bool) -> usize {
    // SAFETY: the caller's strings.
    let (s, set) = unsafe { (units(s), units(set)) };
    s.iter().take_while(|c| set.contains(c) == accept).count()
}

/// The first unit of `s` in `set`; null if there is none.
///
/// # Safety
/// Both are NUL-terminated strings.
unsafe fn find_any<T: Unit>(s: *const T, set: *const T) -> *mut T {
    // SAFETY: the caller's strings.
    let n = unsafe { span(s, set, false) };
    // SAFETY: `n` is within `s`, at most its NUL.
    if s.is_null() || unsafe { *s.add(n) } == T::default() {
        return std::ptr::null_mut();
    }
    s.wrapping_add(n).cast_mut()
}

/// `strtok_s`: the next token of `s` (or of the string `*context` holds),
/// ended with a NUL; null when there are no more.
///
/// # Safety
/// `s` is null or a writable NUL-terminated string, `delim` a string,
/// `context` a place for the position.
unsafe fn token<T: Unit>(s: *mut T, delim: *const T, context: *mut *mut T) -> *mut T {
    if context.is_null() || delim.is_null() {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: as the caller says.
    unsafe {
        let mut p = if s.is_null() { *context } else { s };
        if p.is_null() {
            return std::ptr::null_mut();
        }
        p = p.add(span(p, delim, true));
        if *p == T::default() {
            *context = p;
            return std::ptr::null_mut();
        }
        let start = p;
        p = p.add(span(p, delim, false));
        if *p != T::default() {
            *p = T::default();
            p = p.add(1);
        }
        *context = p;
        start
    }
}

/// Copies `src` with its NUL to `dst`; `dst`.
///
/// # Safety
/// `src` is a string and `dst` has room for it.
unsafe fn copy<T: Unit>(dst: *mut T, src: *const T) -> *mut T {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    // SAFETY: as the caller says.
    unsafe { std::ptr::copy(src, dst, len(src) + 1) };
    dst
}

/// `strncpy`: at most `n` units of `src`, the rest of the `n` filled with
/// NULs.
///
/// # Safety
/// `src` is a string and `dst` has room for `n` units.
unsafe fn ncopy<T: Unit>(dst: *mut T, src: *const T, n: usize) -> *mut T {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    // SAFETY: as the caller says.
    unsafe {
        let k = nlen(src, n);
        std::ptr::copy(src, dst, k);
        for i in k..n {
            dst.add(i).write(T::default());
        }
    }
    dst
}

/// `strncat`: at most `n` units of `src` after `dst`, then a NUL.
///
/// # Safety
/// Both are strings and `dst` has room.
unsafe fn ncat<T: Unit>(dst: *mut T, src: *const T, n: usize) -> *mut T {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    // SAFETY: as the caller says.
    unsafe {
        let end = dst.add(len(dst));
        let k = nlen(src, n);
        std::ptr::copy(src, end, k);
        end.add(k).write(T::default());
    }
    dst
}

/// A failure of an `_s` function: the destination emptied (if it has room),
/// `errno` set, and the error returned.
///
/// # Safety
/// `dst` is null or has `size` units.
unsafe fn fail<T: Unit>(dst: *mut T, size: usize, error: i32) -> i32 {
    if !dst.is_null() && size != 0 {
        // SAFETY: as the caller says.
        unsafe { dst.write(T::default()) };
    }
    set_errno(error);
    error
}

/// A failure for a null pointer: as [`fail`] with `EINVAL`, after the
/// invalid-parameter handler.
///
/// # Safety
/// As [`fail`].
unsafe fn invalid<T: Unit>(dst: *mut T, size: usize) -> i32 {
    _invalid_parameter_noinfo();
    // SAFETY: as the caller says.
    unsafe { fail(dst, size, EINVAL) }
}

/// `strncpy_s` (`strcpy_s` with `count` `_TRUNCATE` and no truncation
/// allowed: see `allow_truncate`).
///
/// # Safety
/// `dst` has `size` units; `src` is a string or has `count` units.
unsafe fn copy_s<T: Unit>(
    dst: *mut T,
    size: usize,
    src: *const T,
    count: usize,
    allow_truncate: bool,
) -> i32 {
    if count == 0 && dst.is_null() && size == 0 {
        return 0;
    }
    if dst.is_null() || size == 0 {
        // SAFETY: nothing is written.
        return unsafe { invalid(dst, 0) };
    }
    if count == 0 {
        // SAFETY: `dst` has `size` units.
        unsafe { dst.write(T::default()) };
        return 0;
    }
    if src.is_null() {
        // SAFETY: as the caller says.
        return unsafe { invalid(dst, size) };
    }
    // SAFETY: as the caller says.
    let n = unsafe { nlen(src, count) };
    if n < size {
        // SAFETY: `n + 1 <= size` units.
        unsafe {
            std::ptr::copy(src, dst, n);
            dst.add(n).write(T::default());
        }
        return 0;
    }
    if allow_truncate && count == TRUNCATE {
        // SAFETY: `size` units.
        unsafe {
            std::ptr::copy(src, dst, size - 1);
            dst.add(size - 1).write(T::default());
        }
        return STRUNCATE;
    }
    // SAFETY: as the caller says.
    unsafe { fail(dst, size, ERANGE) }
}

/// `strncat_s` (and `strcat_s`, with `count` `_TRUNCATE` and no truncation
/// allowed).
///
/// # Safety
/// `dst` has `size` units; `src` is a string or has `count` units.
unsafe fn cat_s<T: Unit>(
    dst: *mut T,
    size: usize,
    src: *const T,
    count: usize,
    allow_truncate: bool,
) -> i32 {
    if count == 0 && dst.is_null() && size == 0 {
        return 0;
    }
    if dst.is_null() || size == 0 {
        // SAFETY: nothing is written.
        return unsafe { invalid(dst, 0) };
    }
    // SAFETY: `dst` has `size` units.
    let used = unsafe { nlen(dst, size) };
    if used == size {
        // Not terminated within its size.
        // SAFETY: as above.
        return unsafe { invalid(dst, size) };
    }
    if src.is_null() && count != 0 {
        // SAFETY: as above.
        return unsafe { invalid(dst, size) };
    }
    // SAFETY: the rest of `dst` is `size - used` units.
    unsafe { copy_s(dst.add(used), size - used, src, count, allow_truncate) }.pipe_reset(dst, size)
}

/// Resetting the whole destination after a failed append, as the CRT does.
trait PipeReset {
    fn pipe_reset<T: Unit>(self, dst: *mut T, size: usize) -> i32;
}

impl PipeReset for i32 {
    fn pipe_reset<T: Unit>(self, dst: *mut T, size: usize) -> i32 {
        if self == ERANGE || self == EINVAL {
            // SAFETY: `dst` has `size` units (non-zero here).
            unsafe { fail(dst, size, self) }
        } else {
            self
        }
    }
}

/// Maps the units of `s` in place; `s`.
///
/// # Safety
/// `s` is a writable string.
unsafe fn map<T: Unit>(s: *mut T, f: impl Fn(T) -> T) -> *mut T {
    if s.is_null() {
        return s;
    }
    // SAFETY: as the caller says.
    unsafe {
        for i in 0..len(s) {
            *s.add(i) = f(*s.add(i));
        }
    }
    s
}

/// [`map`] of a string checked to end within `size`; `EINVAL` otherwise.
///
/// # Safety
/// `s` is null or has `size` units.
unsafe fn map_s<T: Unit>(s: *mut T, size: usize, f: impl Fn(T) -> T) -> i32 {
    if s.is_null() || size == 0 {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: as the caller says.
    unsafe {
        if nlen(s, size) == size {
            return fail(s, size, EINVAL);
        }
        map(s, f);
    }
    0
}

/// Sets at most `n` units of `s` to `c`; `s`.
///
/// # Safety
/// `s` is a writable string.
unsafe fn set_n<T: Unit>(s: *mut T, c: T, n: usize) -> *mut T {
    if s.is_null() {
        return s;
    }
    // SAFETY: as the caller says.
    unsafe {
        for i in 0..nlen(s, n) {
            *s.add(i) = c;
        }
    }
    s
}

/// Reverses `s` in place; `s`.
///
/// # Safety
/// `s` is a writable string.
unsafe fn reverse<T: Unit>(s: *mut T) -> *mut T {
    if !s.is_null() {
        // SAFETY: as the caller says.
        unsafe { std::slice::from_raw_parts_mut(s, len(s)).reverse() };
    }
    s
}

/// A copy in a block from `malloc`; null for null or no memory.
///
/// # Safety
/// `s` is a string.
unsafe fn duplicate<T: Unit>(s: *const T) -> *mut T {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: as the caller says.
    let n = unsafe { len(s) } + 1;
    let copy = super::heap::malloc(n * std::mem::size_of::<T>()).cast::<T>();
    if !copy.is_null() {
        // SAFETY: `n` units each.
        unsafe { std::ptr::copy_nonoverlapping(s, copy, n) };
    }
    copy
}

/// `strxfrm`: `src` copied if it fits in `n` units with its NUL; its length.
///
/// # Safety
/// `src` is a string and `dst` has `n` units.
unsafe fn transform<T: Unit>(dst: *mut T, src: *const T, n: usize) -> usize {
    if src.is_null() {
        set_errno(EINVAL);
        return usize::MAX;
    }
    // SAFETY: as the caller says.
    let k = unsafe { len(src) };
    if k < n && !dst.is_null() {
        // SAFETY: `k + 1 <= n`.
        unsafe { std::ptr::copy(src, dst, k + 1) };
    }
    k
}

/// Narrow and wide forms of the string functions over [`Unit`].
macro_rules! string_functions {
    ($t:ty, $c:ty, $int:ty,
     len: $len:ident, nlen: $nlen:ident, nlen_s: $nlen_s:ident,
     cpy: $cpy:ident, ncpy: $ncpy:ident, cpy_s: $cpy_s:ident, ncpy_s: $ncpy_s:ident,
     cat: $cat:ident, ncat: $ncat:ident, cat_s: $cat_s:ident, ncat_s: $ncat_s:ident,
     cmp: $cmp:ident, ncmp: $ncmp:ident, icmp: $icmp:ident, nicmp: $nicmp:ident,
     icmp_l: $icmp_l:ident,
     chr: $chr:ident, rchr: $rchr:ident, str: $str:ident,
     tok: $tok:ident, tok_s: $tok_s:ident, tok_context: $tok_context:ident, dup: $dup:ident,
     lwr: $lwr:ident, upr: $upr:ident, lwr_s: $lwr_s:ident, upr_s: $upr_s:ident,
     nset: $nset:ident, rev: $rev:ident, set: $set:ident,
     spn: $spn:ident, cspn: $cspn:ident, pbrk: $pbrk:ident,
     coll: $coll:ident, icoll: $icoll:ident, ncoll: $ncoll:ident, nicoll: $nicoll:ident,
     xfrm: $xfrm:ident) => {
        /// The string's length.
        pub extern "win64" fn $len(s: *const $c) -> usize {
            // SAFETY: the title's string.
            if s.is_null() {
                0
            } else {
                unsafe { len(s.cast::<$t>()) }
            }
        }

        /// The string's length, at most `max`.
        pub extern "win64" fn $nlen(s: *const $c, max: usize) -> usize {
            // SAFETY: the title's string of at most `max` units.
            if s.is_null() {
                0
            } else {
                unsafe { nlen(s.cast::<$t>(), max) }
            }
        }

        /// See the non-`_s` form; 0 for null.
        pub extern "win64" fn $nlen_s(s: *const $c, max: usize) -> usize {
            $nlen(s, max)
        }

        /// Copies a string.
        pub extern "win64" fn $cpy(dst: *mut $c, src: *const $c) -> *mut $c {
            // SAFETY: the title's strings.
            unsafe { copy(dst.cast::<$t>(), src.cast::<$t>()).cast() }
        }

        /// Copies at most `n` units, filling the rest with NULs.
        pub extern "win64" fn $ncpy(dst: *mut $c, src: *const $c, n: usize) -> *mut $c {
            // SAFETY: the title's strings.
            unsafe { ncopy(dst.cast::<$t>(), src.cast::<$t>(), n).cast() }
        }

        /// Copies a string into a buffer of `size` units; `EINVAL` or `ERANGE`
        /// with the destination emptied on failure.
        pub extern "win64" fn $cpy_s(dst: *mut $c, size: usize, src: *const $c) -> i32 {
            // SAFETY: the title's buffer of `size` units and string.
            unsafe { copy_s(dst.cast::<$t>(), size, src.cast::<$t>(), TRUNCATE, false) }
        }

        /// Copies at most `count` units (`_TRUNCATE`: what fits, then
        /// `STRUNCATE`).
        pub extern "win64" fn $ncpy_s(
            dst: *mut $c,
            size: usize,
            src: *const $c,
            count: usize,
        ) -> i32 {
            // SAFETY: the title's buffer of `size` units and string.
            unsafe { copy_s(dst.cast::<$t>(), size, src.cast::<$t>(), count, true) }
        }

        /// Appends a string.
        pub extern "win64" fn $cat(dst: *mut $c, src: *const $c) -> *mut $c {
            // SAFETY: the title's strings.
            unsafe { ncat(dst.cast::<$t>(), src.cast::<$t>(), usize::MAX).cast() }
        }

        /// Appends at most `n` units, then a NUL.
        pub extern "win64" fn $ncat(dst: *mut $c, src: *const $c, n: usize) -> *mut $c {
            // SAFETY: the title's strings.
            unsafe { ncat(dst.cast::<$t>(), src.cast::<$t>(), n).cast() }
        }

        /// Appends a string within a buffer of `size` units.
        pub extern "win64" fn $cat_s(dst: *mut $c, size: usize, src: *const $c) -> i32 {
            // SAFETY: the title's buffer of `size` units and string.
            unsafe { cat_s(dst.cast::<$t>(), size, src.cast::<$t>(), TRUNCATE, false) }
        }

        /// Appends at most `count` units within a buffer of `size` units.
        pub extern "win64" fn $ncat_s(
            dst: *mut $c,
            size: usize,
            src: *const $c,
            count: usize,
        ) -> i32 {
            // SAFETY: the title's buffer of `size` units and string.
            unsafe { cat_s(dst.cast::<$t>(), size, src.cast::<$t>(), count, true) }
        }

        /// Compares two strings.
        pub extern "win64" fn $cmp(a: *const $c, b: *const $c) -> i32 {
            // SAFETY: the title's strings.
            unsafe { compare(a.cast::<$t>(), b.cast::<$t>(), usize::MAX, false) }
        }

        /// Compares at most `n` units.
        pub extern "win64" fn $ncmp(a: *const $c, b: *const $c, n: usize) -> i32 {
            // SAFETY: the title's strings.
            unsafe { compare(a.cast::<$t>(), b.cast::<$t>(), n, false) }
        }

        /// Compares two strings ignoring case.
        pub extern "win64" fn $icmp(a: *const $c, b: *const $c) -> i32 {
            // SAFETY: the title's strings.
            unsafe { compare(a.cast::<$t>(), b.cast::<$t>(), usize::MAX, true) }
        }

        /// Compares at most `n` units ignoring case.
        pub extern "win64" fn $nicmp(a: *const $c, b: *const $c, n: usize) -> i32 {
            // SAFETY: the title's strings.
            unsafe { compare(a.cast::<$t>(), b.cast::<$t>(), n, true) }
        }

        /// See the form without a locale.
        pub extern "win64" fn $icmp_l(a: *const $c, b: *const $c, _locale: *mut c_void) -> i32 {
            $icmp(a, b)
        }

        /// The first `c`; null if there is none.
        pub extern "win64" fn $chr(s: *const $c, c: $int) -> *mut $c {
            // SAFETY: the title's string.
            unsafe { find(s.cast::<$t>(), <$t>::from_value(c as u32)).cast() }
        }

        /// The last `c`; null if there is none.
        pub extern "win64" fn $rchr(s: *const $c, c: $int) -> *mut $c {
            // SAFETY: the title's string.
            unsafe { find_last(s.cast::<$t>(), <$t>::from_value(c as u32)).cast() }
        }

        /// The first place `needle` appears.
        pub extern "win64" fn $str(s: *const $c, needle: *const $c) -> *mut $c {
            // SAFETY: the title's strings.
            unsafe { find_string(s.cast::<$t>(), needle.cast::<$t>()).cast() }
        }

        thread_local! {
            static $tok_context: Cell<usize> = const { Cell::new(0) };
        }

        /// The next token, the position kept per thread.
        pub extern "win64" fn $tok(s: *mut $c, delim: *const $c) -> *mut $c {
            $tok_context.with(|context| {
                let mut position = context.get() as *mut $t;
                // SAFETY: the title's strings; the position is in the string
                // the title gave last.
                let token = unsafe { token(s.cast::<$t>(), delim.cast::<$t>(), &mut position) };
                context.set(position as usize);
                token.cast()
            })
        }

        /// The next token, the position in `*context`.
        pub extern "win64" fn $tok_s(
            s: *mut $c,
            delim: *const $c,
            context: *mut *mut $c,
        ) -> *mut $c {
            // SAFETY: the title's strings and place for the position.
            unsafe { token(s.cast::<$t>(), delim.cast::<$t>(), context.cast()).cast() }
        }

        /// A copy in a block from `malloc`.
        pub extern "win64" fn $dup(s: *const $c) -> *mut $c {
            // SAFETY: the title's string.
            unsafe { duplicate(s.cast::<$t>()).cast() }
        }

        /// Lower-cases the string in place.
        pub extern "win64" fn $lwr(s: *mut $c) -> *mut $c {
            // SAFETY: the title's string.
            unsafe { map(s.cast::<$t>(), <$t>::lower).cast() }
        }

        /// Upper-cases the string in place.
        pub extern "win64" fn $upr(s: *mut $c) -> *mut $c {
            // SAFETY: the title's string.
            unsafe { map(s.cast::<$t>(), <$t>::upper).cast() }
        }

        /// Lower-cases a string within `size` units.
        pub extern "win64" fn $lwr_s(s: *mut $c, size: usize) -> i32 {
            // SAFETY: the title's buffer of `size` units.
            unsafe { map_s(s.cast::<$t>(), size, <$t>::lower) }
        }

        /// Upper-cases a string within `size` units.
        pub extern "win64" fn $upr_s(s: *mut $c, size: usize) -> i32 {
            // SAFETY: the title's buffer of `size` units.
            unsafe { map_s(s.cast::<$t>(), size, <$t>::upper) }
        }

        /// Sets at most `n` units to `c`.
        pub extern "win64" fn $nset(s: *mut $c, c: $int, n: usize) -> *mut $c {
            // SAFETY: the title's string.
            unsafe { set_n(s.cast::<$t>(), <$t>::from_value(c as u32), n).cast() }
        }

        /// Reverses the string in place.
        pub extern "win64" fn $rev(s: *mut $c) -> *mut $c {
            // SAFETY: the title's string.
            unsafe { reverse(s.cast::<$t>()).cast() }
        }

        /// Sets every unit to `c`.
        pub extern "win64" fn $set(s: *mut $c, c: $int) -> *mut $c {
            $nset(s, c, usize::MAX)
        }

        /// The length of the start made of units in `set`.
        pub extern "win64" fn $spn(s: *const $c, set: *const $c) -> usize {
            // SAFETY: the title's strings.
            unsafe { span(s.cast::<$t>(), set.cast::<$t>(), true) }
        }

        /// The length of the start made of units not in `set`.
        pub extern "win64" fn $cspn(s: *const $c, set: *const $c) -> usize {
            // SAFETY: the title's strings.
            unsafe { span(s.cast::<$t>(), set.cast::<$t>(), false) }
        }

        /// The first unit in `set`.
        pub extern "win64" fn $pbrk(s: *const $c, set: *const $c) -> *mut $c {
            // SAFETY: the title's strings.
            unsafe { find_any(s.cast::<$t>(), set.cast::<$t>()).cast() }
        }

        /// Collation is by units (the "C" locale).
        pub extern "win64" fn $coll(a: *const $c, b: *const $c) -> i32 {
            $cmp(a, b)
        }

        /// See the case-insensitive comparison.
        pub extern "win64" fn $icoll(a: *const $c, b: *const $c) -> i32 {
            $icmp(a, b)
        }

        /// See the bounded comparison.
        pub extern "win64" fn $ncoll(a: *const $c, b: *const $c, n: usize) -> i32 {
            $ncmp(a, b, n)
        }

        /// See the bounded case-insensitive comparison.
        pub extern "win64" fn $nicoll(a: *const $c, b: *const $c, n: usize) -> i32 {
            $nicmp(a, b, n)
        }

        /// The "C" locale's transformation: a copy if it fits; the length.
        pub extern "win64" fn $xfrm(dst: *mut $c, src: *const $c, n: usize) -> usize {
            // SAFETY: the title's buffer of `n` units and string.
            unsafe { transform(dst.cast::<$t>(), src.cast::<$t>(), n) }
        }
    };
}

string_functions!(u8, c_char, i32,
    len: strlen, nlen: strnlen, nlen_s: strnlen_s,
    cpy: strcpy, ncpy: strncpy, cpy_s: strcpy_s, ncpy_s: strncpy_s,
    cat: strcat, ncat: strncat, cat_s: strcat_s, ncat_s: strncat_s,
    cmp: strcmp, ncmp: strncmp, icmp: _stricmp, nicmp: _strnicmp, icmp_l: _stricmp_l,
    chr: strchr, rchr: strrchr, str: strstr,
    tok: strtok, tok_s: strtok_s, tok_context: NARROW_TOKEN, dup: _strdup,
    lwr: _strlwr, upr: _strupr, lwr_s: _strlwr_s, upr_s: _strupr_s,
    nset: _strnset, rev: _strrev, set: _strset,
    spn: strspn, cspn: strcspn, pbrk: strpbrk,
    coll: strcoll, icoll: _stricoll, ncoll: _strncoll, nicoll: _strnicoll,
    xfrm: strxfrm);

string_functions!(u16, u16, u16,
    len: wcslen, nlen: wcsnlen, nlen_s: wcsnlen_s,
    cpy: wcscpy, ncpy: wcsncpy, cpy_s: wcscpy_s, ncpy_s: wcsncpy_s,
    cat: wcscat, ncat: wcsncat, cat_s: wcscat_s, ncat_s: wcsncat_s,
    cmp: wcscmp, ncmp: wcsncmp, icmp: _wcsicmp, nicmp: _wcsnicmp, icmp_l: _wcsicmp_l,
    chr: wcschr, rchr: wcsrchr, str: wcsstr,
    tok: wcstok, tok_s: wcstok_s, tok_context: WIDE_TOKEN, dup: _wcsdup,
    lwr: _wcslwr, upr: _wcsupr, lwr_s: _wcslwr_s, upr_s: _wcsupr_s,
    nset: _wcsnset, rev: _wcsrev, set: _wcsset,
    spn: wcsspn, cspn: wcscspn, pbrk: wcspbrk,
    coll: wcscoll, icoll: _wcsicoll, ncoll: _wcsncoll, nicoll: _wcsnicoll,
    xfrm: wcsxfrm);

/// `_strcmpi`: see [`_stricmp`].
pub extern "win64" fn _strcmpi(a: *const c_char, b: *const c_char) -> i32 {
    _stricmp(a, b)
}

/// Sets at most `n` units of a string within `size` to `c`.
///
/// # Safety
/// `s` is null or has `size` units.
unsafe fn set_n_s<T: Unit>(s: *mut T, size: usize, c: T, n: usize) -> i32 {
    if s.is_null() || size == 0 {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: as the caller says.
    unsafe {
        if nlen(s, size) == size {
            return fail(s, size, EINVAL);
        }
        set_n(s, c, n);
    }
    0
}

/// `_strnset` within `size` bytes.
pub extern "win64" fn _strnset_s(s: *mut c_char, size: usize, c: i32, n: usize) -> i32 {
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { set_n_s(s.cast::<u8>(), size, c as u8, n) }
}

/// `_strset` within `size` bytes.
pub extern "win64" fn _strset_s(s: *mut c_char, size: usize, c: i32) -> i32 {
    _strnset_s(s, size, c, usize::MAX)
}

// ---------------------------------------------------------------------------
// Character classes.

const UPPER: u16 = 0x1;
const LOWER: u16 = 0x2;
const DIGIT: u16 = 0x4;
const SPACE: u16 = 0x8;
const PUNCT: u16 = 0x10;
const CONTROL: u16 = 0x20;
const BLANK: u16 = 0x40;
const HEX: u16 = 0x80;
const ALPHA: u16 = 0x103;
/// An alphabetic character that is neither upper nor lower case.
const LETTER: u16 = 0x100;

/// The "C" locale's class of an ASCII character (0 for the others).
const fn ascii_class(c: u8) -> u16 {
    match c {
        b'\t' => CONTROL | SPACE | BLANK,
        0x0A..=0x0D => CONTROL | SPACE,
        0x00..=0x1F | 0x7F => CONTROL,
        b' ' => SPACE | BLANK,
        b'0'..=b'9' => DIGIT | HEX,
        b'A'..=b'F' => UPPER | HEX | LETTER,
        b'G'..=b'Z' => UPPER | LETTER,
        b'a'..=b'f' => LOWER | HEX | LETTER,
        b'g'..=b'z' => LOWER | LETTER,
        0x21..=0x7E => PUNCT,
        _ => 0,
    }
}

/// The class table, entry 0 for `EOF` (-1), then 0 to 255.
static CLASSES: [u16; 257] = {
    let mut table = [0u16; 257];
    let mut c = 0;
    while c < 256 {
        table[c + 1] = ascii_class(c as u8);
        c += 1;
    }
    table
};

/// The class of a narrow character (`EOF` and out of range: none).
fn class(c: i32) -> u16 {
    usize::try_from(c + 1)
        .ok()
        .and_then(|i| CLASSES.get(i))
        .copied()
        .unwrap_or(0)
}

/// The class of a wide character: the table's for ASCII, else from
/// Unicode's properties.
fn wide_class(c: u16) -> u16 {
    if c < 0x80 {
        return ascii_class(c as u8);
    }
    let Some(ch) = char::from_u32(u32::from(c)) else {
        return 0;
    };
    let mut class = 0;
    if ch.is_control() {
        class |= CONTROL;
    }
    if ch.is_whitespace() {
        class |= SPACE;
        if c == 0xA0 {
            class |= BLANK;
        }
    }
    if ch.is_alphabetic() {
        class |= LETTER;
        if ch.is_uppercase() {
            class |= UPPER;
        }
        if ch.is_lowercase() {
            class |= LOWER;
        }
    } else if ch.is_numeric() {
        class |= DIGIT;
    } else if class == 0 {
        class = PUNCT;
    }
    class
}

/// The class table (`_pctype`): may be indexed with -1.
pub extern "win64" fn __pctype_func() -> *const u16 {
    CLASSES[1..].as_ptr()
}

/// The wide class table (`_pwctype`) for the first 256 characters.
pub extern "win64" fn __pwctype_func() -> *const u16 {
    static WIDE: std::sync::OnceLock<[u16; 256]> = std::sync::OnceLock::new();
    WIDE.get_or_init(|| std::array::from_fn(|c| wide_class(c as u16)))
        .as_ptr()
}

/// The class bits of `c` in `mask`.
pub extern "win64" fn _isctype(c: i32, mask: i32) -> i32 {
    i32::from(class(c) & mask as u16)
}

/// The class bits of `c` in `mask`.
pub extern "win64" fn iswctype(c: u16, mask: u16) -> i32 {
    i32::from(wide_class(c) & mask)
}

/// See [`iswctype`].
pub extern "win64" fn _iswctype_l(c: u16, mask: u16, _locale: *mut c_void) -> i32 {
    iswctype(c, mask)
}

/// Narrow and wide class predicates.
macro_rules! classes {
    ($($narrow:ident, $wide:ident, $mask:expr;)*) => {
        $(
            /// Whether the character is of the class (its class bits).
            pub extern "win64" fn $narrow(c: i32) -> i32 {
                i32::from(class(c) & ($mask))
            }

            /// Whether the wide character is of the class (its class bits).
            pub extern "win64" fn $wide(c: u16) -> i32 {
                i32::from(wide_class(c) & ($mask))
            }
        )*
    };
}

classes! {
    isalpha, iswalpha, ALPHA;
    isdigit, iswdigit, DIGIT;
    isalnum, iswalnum, ALPHA | DIGIT;
    isspace, iswspace, SPACE;
    isupper, iswupper, UPPER;
    islower, iswlower, LOWER;
    isxdigit, iswxdigit, HEX;
    ispunct, iswpunct, PUNCT;
    isprint, iswprint, BLANK | PUNCT | ALPHA | DIGIT;
    iscntrl, iswcntrl, CONTROL;
    isgraph, iswgraph, PUNCT | ALPHA | DIGIT;
    isblank, iswblank, BLANK;
}

/// Whether `c` is ASCII.
pub extern "win64" fn __isascii(c: i32) -> i32 {
    i32::from((c as u32) < 0x80)
}

/// Whether `c` is ASCII.
pub extern "win64" fn iswascii(c: u16) -> i32 {
    i32::from(c < 0x80)
}

/// Whether `c` may start an identifier (a letter or `_`).
pub extern "win64" fn __iswcsymf(c: u16) -> i32 {
    i32::from(c == u16::from(b'_') || wide_class(c) & ALPHA != 0)
}

/// Whether `c` may be in an identifier (a letter, digit or `_`).
pub extern "win64" fn __iswcsym(c: u16) -> i32 {
    i32::from(c == u16::from(b'_') || wide_class(c) & (ALPHA | DIGIT) != 0)
}

/// No byte leads a multibyte character here.
pub extern "win64" fn isleadbyte(_c: i32) -> i32 {
    0
}

/// See [`isleadbyte`].
pub extern "win64" fn _ismbblead(_c: u32) -> i32 {
    0
}

/// The lower case of an ASCII letter; others as they are.
pub extern "win64" fn tolower(c: i32) -> i32 {
    if (i32::from(b'A')..=i32::from(b'Z')).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// The upper case of an ASCII letter; others as they are.
pub extern "win64" fn toupper(c: i32) -> i32 {
    if (i32::from(b'a')..=i32::from(b'z')).contains(&c) {
        c - 32
    } else {
        c
    }
}

/// `c - 'A' + 'a'`, whatever `c` is.
pub extern "win64" fn _tolower(c: i32) -> i32 {
    c + 32
}

/// `c - 'a' + 'A'`, whatever `c` is.
pub extern "win64" fn _toupper(c: i32) -> i32 {
    c - 32
}

/// The lower case of a wide character (a one-unit mapping), else itself.
pub extern "win64" fn towlower(c: u16) -> u16 {
    wide_lower(c)
}

/// The upper case of a wide character (a one-unit mapping), else itself.
pub extern "win64" fn towupper(c: u16) -> u16 {
    wide_upper(c)
}

// ---------------------------------------------------------------------------
// Numbers from strings.

/// The digit value of a unit in any base up to 36.
fn digit(c: u32) -> Option<u32> {
    char::from_u32(c).and_then(|c| c.to_digit(36))
}

/// An integer read from a string: its sign and magnitude (saturated, with
/// `overflow` set), and the units read (0 when there was no number).
struct Integer {
    negative: bool,
    magnitude: u64,
    overflow: bool,
    read: usize,
}

/// Reads an integer as `strtol` does: spaces, a sign, a `0x` prefix for base
/// 16 (and base 0's detection), then digits; `None` for a bad base.
///
/// # Safety
/// `s` is a NUL-terminated string.
unsafe fn read_integer<T: Unit>(s: *const T, base: i32) -> Option<Integer> {
    if base < 0 || base == 1 || base > 36 {
        return None;
    }
    // SAFETY: the caller's string, read up to its NUL.
    let at = |i: usize| unsafe { (*s.add(i)).value() };
    let mut i = 0;
    while at(i) != 0 && T::from_value(at(i)).is_space() {
        i += 1;
    }
    let negative = at(i) == u32::from(b'-');
    if negative || at(i) == u32::from(b'+') {
        i += 1;
    }
    let mut base = base as u32;
    let hex_prefix = at(i) == u32::from(b'0')
        && (at(i + 1) | 0x20) == u32::from(b'x')
        && digit(at(i + 2)).is_some_and(|d| d < 16);
    if (base == 0 || base == 16) && hex_prefix {
        base = 16;
        i += 2;
    } else if base == 0 {
        base = if at(i) == u32::from(b'0') { 8 } else { 10 };
    }
    let start = i;
    let mut magnitude: u64 = 0;
    let mut overflow = false;
    while let Some(d) = digit(at(i)).filter(|&d| d < base) {
        match magnitude
            .checked_mul(u64::from(base))
            .and_then(|m| m.checked_add(u64::from(d)))
        {
            Some(m) => magnitude = m,
            None => overflow = true,
        }
        i += 1;
    }
    let read = if i == start { 0 } else { i };
    Some(Integer {
        negative,
        magnitude: if overflow { u64::MAX } else { magnitude },
        overflow,
        read,
    })
}

/// The integer `s` starts with, in a signed range `min..=max`, clamped with
/// `ERANGE`; `*end` past it.
///
/// # Safety
/// `s` is a NUL-terminated string; `end` is null or writable.
unsafe fn signed<T: Unit>(s: *const T, end: *mut *mut T, base: i32, max: u64) -> i64 {
    // SAFETY: as the caller says.
    let parsed = unsafe { parse(s, end, base) };
    let Some(n) = parsed else { return 0 };
    let limit = if n.negative { max + 1 } else { max };
    if n.overflow || n.magnitude > limit {
        set_errno(ERANGE);
        return if n.negative {
            -(max as i64) - 1
        } else {
            max as i64
        };
    }
    if n.negative {
        (n.magnitude as i64).wrapping_neg()
    } else {
        n.magnitude as i64
    }
}

/// The integer `s` starts with, unsigned up to `max` (a negative one
/// negated in that range, as C has it), clamped with `ERANGE`.
///
/// # Safety
/// As [`signed`].
unsafe fn unsigned<T: Unit>(s: *const T, end: *mut *mut T, base: i32, max: u64) -> u64 {
    // SAFETY: as the caller says.
    let parsed = unsafe { parse(s, end, base) };
    let Some(n) = parsed else { return 0 };
    if n.overflow || n.magnitude > max {
        set_errno(ERANGE);
        return max;
    }
    if n.negative {
        n.magnitude.wrapping_neg() & max
    } else {
        n.magnitude
    }
}

/// [`read_integer`], with `*end` set (to `s` when there was no number) and
/// `EINVAL` for a bad base or null string.
///
/// # Safety
/// As [`signed`].
unsafe fn parse<T: Unit>(s: *const T, end: *mut *mut T, base: i32) -> Option<Integer> {
    if s.is_null() {
        set_errno(EINVAL);
        return None;
    }
    // SAFETY: as the caller says.
    let n = unsafe { read_integer(s, base) };
    if !end.is_null() {
        let read = n.as_ref().map_or(0, |n| n.read);
        // SAFETY: as the caller says.
        unsafe { end.write(s.wrapping_add(read).cast_mut()) };
    }
    if n.is_none() {
        set_errno(EINVAL);
    }
    n.filter(|n| n.read != 0)
}

/// `strtol` and its family, narrow and wide.
macro_rules! integer_parsers {
    ($t:ty, $c:ty; $($name:ident: $out:ty, $kind:ident, $max:expr;)*) => {
        $(
            /// The integer the string starts with (see [`read_integer`]),
            /// clamped to the type's range with `ERANGE`.
            pub extern "win64" fn $name(s: *const $c, end: *mut *mut $c, base: i32) -> $out {
                // SAFETY: the title's string and place for the end.
                unsafe { $kind(s.cast::<$t>(), end.cast(), base, $max as u64) as $out }
            }
        )*
    };
}

integer_parsers! { u8, c_char;
    strtol: i32, signed, i32::MAX;
    strtoul: u32, unsigned, u32::MAX;
    strtoll: i64, signed, i64::MAX;
    strtoull: u64, unsigned, u64::MAX;
    _strtoi64: i64, signed, i64::MAX;
    _strtoui64: u64, unsigned, u64::MAX;
}

integer_parsers! { u16, u16;
    wcstol: i32, signed, i32::MAX;
    wcstoul: u32, unsigned, u32::MAX;
    wcstoll: i64, signed, i64::MAX;
    wcstoull: u64, unsigned, u64::MAX;
    _wcstoi64: i64, signed, i64::MAX;
    _wcstoui64: u64, unsigned, u64::MAX;
}

/// `strtol(s, NULL, 10)`.
pub extern "win64" fn atoi(s: *const c_char) -> i32 {
    strtol(s, std::ptr::null_mut(), 10)
}

/// See [`atoi`].
pub extern "win64" fn atol(s: *const c_char) -> i32 {
    strtol(s, std::ptr::null_mut(), 10)
}

/// `strtoll(s, NULL, 10)`.
pub extern "win64" fn atoll(s: *const c_char) -> i64 {
    strtoll(s, std::ptr::null_mut(), 10)
}

/// See [`atoll`].
pub extern "win64" fn _atoi64(s: *const c_char) -> i64 {
    strtoll(s, std::ptr::null_mut(), 10)
}

/// `wcstol(s, NULL, 10)`.
pub extern "win64" fn _wtoi(s: *const u16) -> i32 {
    wcstol(s, std::ptr::null_mut(), 10)
}

/// See [`_wtoi`].
pub extern "win64" fn _wtol(s: *const u16) -> i32 {
    wcstol(s, std::ptr::null_mut(), 10)
}

/// `wcstoll(s, NULL, 10)`.
pub extern "win64" fn _wtoi64(s: *const u16) -> i64 {
    wcstoll(s, std::ptr::null_mut(), 10)
}

/// See [`_wtoi64`].
pub extern "win64" fn _wtoll(s: *const u16) -> i64 {
    wcstoll(s, std::ptr::null_mut(), 10)
}

/// A double built from `mantissa * 2^exponent`, rounded to nearest even
/// (`sticky`: non-zero bits below the mantissa); whether it overflowed or
/// underflowed to zero.
fn compose(mantissa: u64, sticky: bool, exponent: i64) -> (f64, bool) {
    if mantissa == 0 {
        return (0.0, false);
    }
    let shift = mantissa.leading_zeros();
    let m = mantissa << shift;
    let exponent = exponent - i64::from(shift);
    // The value's binary exponent, with `m` in [2^63, 2^64).
    let e = exponent + 63;
    if e > 1023 {
        return (f64::INFINITY, true);
    }
    // Bits dropped: 11 for a normal result, more for a subnormal one.
    let drop = if e >= -1022 { 11 } else { 11 + (-1022 - e) };
    if drop > 64 {
        return (0.0, true);
    }
    let (q, rem, half) = if drop == 64 {
        (0, m, 1u64 << 63)
    } else {
        (m >> drop, m & ((1u64 << drop) - 1), 1u64 << (drop - 1))
    };
    let up = rem > half || (rem == half && (sticky || q & 1 == 1));
    let q = q + u64::from(up);
    if e >= -1022 {
        // `q` in [2^52, 2^53]: rounding up may carry into the exponent.
        let (q, e) = if q == 1 << 53 {
            (q >> 1, e + 1)
        } else {
            (q, e)
        };
        if e > 1023 {
            return (f64::INFINITY, true);
        }
        let bits = (((e + 1023) as u64) << 52) | (q & ((1 << 52) - 1));
        (f64::from_bits(bits), false)
    } else {
        // Subnormal (or, carried, the smallest normal): the bits are `q`.
        (f64::from_bits(q), q == 0)
    }
}

/// Reads a floating-point number as `strtod` does: spaces, a sign, then a
/// decimal number (parsed by Rust's `f64::from_str` on exactly the span
/// the C syntax takes), a hexadecimal one (`0x`, parsed by hand and
/// rounded to nearest even), `inf`/`infinity` or `nan`/`nan(...)`. The
/// value, the units read (0 for none), and whether it was out of range.
///
/// # Safety
/// `s` is a NUL-terminated string.
unsafe fn read_float<T: Unit>(s: *const T) -> (f64, usize, bool) {
    // SAFETY: the caller's string, read up to its NUL.
    let at = |i: usize| unsafe { (*s.add(i)).value() };
    let is = |i: usize, c: u8| at(i) == u32::from(c) || at(i) == u32::from(c.to_ascii_uppercase());
    let word = |i: usize, w: &[u8]| w.iter().enumerate().all(|(k, &c)| is(i + k, c));
    let mut i = 0;
    while at(i) != 0 && T::from_value(at(i)).is_space() {
        i += 1;
    }
    let negative = at(i) == u32::from(b'-');
    if negative || at(i) == u32::from(b'+') {
        i += 1;
    }
    let sign = if negative { -1.0 } else { 1.0 };
    if word(i, b"infinity") {
        return (sign * f64::INFINITY, i + 8, false);
    }
    if word(i, b"inf") {
        return (sign * f64::INFINITY, i + 3, false);
    }
    if word(i, b"nan") {
        let mut j = i + 3;
        if at(j) == u32::from(b'(') {
            let mut k = j + 1;
            while digit(at(k)).is_some() || at(k) == u32::from(b'_') {
                k += 1;
            }
            if at(k) == u32::from(b')') {
                j = k + 1;
            }
        }
        return (sign * f64::NAN, j, false);
    }
    let is_digit = |c: u32, base: u32| digit(c).is_some_and(|d| d < base);
    let hex = at(i) == u32::from(b'0') && is(i + 1, b'x');
    if hex && (is_digit(at(i + 2), 16) || (at(i + 2) == u32::from(b'.') && is_digit(at(i + 3), 16)))
    {
        let mut j = i + 2;
        let mut mantissa: u64 = 0;
        let mut sticky = false;
        let mut exponent: i64 = 0;
        let mut seen_point = false;
        loop {
            let c = at(j);
            if c == u32::from(b'.') && !seen_point {
                seen_point = true;
            } else if let Some(d) = digit(c).filter(|&d| d < 16) {
                if mantissa >> 60 == 0 {
                    mantissa = (mantissa << 4) | u64::from(d);
                    if seen_point {
                        exponent -= 4;
                    }
                } else {
                    sticky |= d != 0;
                    if !seen_point {
                        exponent += 4;
                    }
                }
            } else {
                break;
            }
            j += 1;
        }
        if is(j, b'p') {
            let mut k = j + 1;
            let minus = at(k) == u32::from(b'-');
            if minus || at(k) == u32::from(b'+') {
                k += 1;
            }
            if is_digit(at(k), 10) {
                let mut e: i64 = 0;
                while let Some(d) = digit(at(k)).filter(|&d| d < 10) {
                    e = (e * 10 + i64::from(d)).min(1 << 20);
                    k += 1;
                }
                exponent += if minus { -e } else { e };
                j = k;
            }
        }
        let (value, range) = compose(mantissa, sticky, exponent);
        return (sign * value, j, range);
    }
    // A decimal number: digits, a point, digits (one at least), an exponent.
    let mut text = String::new();
    let mut j = i;
    let mut digits = 0;
    let mut nonzero = false;
    while is_digit(at(j), 10) {
        nonzero |= at(j) != u32::from(b'0');
        text.push(char::from_u32(at(j)).unwrap_or('0'));
        digits += 1;
        j += 1;
    }
    if at(j) == u32::from(b'.') {
        text.push('.');
        j += 1;
        while is_digit(at(j), 10) {
            nonzero |= at(j) != u32::from(b'0');
            text.push(char::from_u32(at(j)).unwrap_or('0'));
            digits += 1;
            j += 1;
        }
    }
    if digits == 0 {
        return (0.0, 0, false);
    }
    if is(j, b'e') {
        let mut k = j + 1;
        let mut exponent = String::from("e");
        if at(k) == u32::from(b'-') || at(k) == u32::from(b'+') {
            exponent.push(char::from_u32(at(k)).unwrap_or('+'));
            k += 1;
        }
        if is_digit(at(k), 10) {
            while is_digit(at(k), 10) {
                exponent.push(char::from_u32(at(k)).unwrap_or('0'));
                k += 1;
            }
            text.push_str(&exponent);
            j = k;
        }
    }
    let value: f64 = text.parse().unwrap_or(0.0);
    let range = value.is_infinite() || (value == 0.0 && nonzero);
    (sign * value, j, range)
}

/// [`read_float`] with `*end` set and `ERANGE` on overflow or underflow to
/// zero.
///
/// # Safety
/// `s` is a NUL-terminated string; `end` null or writable.
unsafe fn float<T: Unit>(s: *const T, end: *mut *mut T) -> f64 {
    if s.is_null() {
        set_errno(EINVAL);
        return 0.0;
    }
    // SAFETY: as the caller says.
    let (value, read, range) = unsafe { read_float(s) };
    if range {
        set_errno(ERANGE);
    }
    if !end.is_null() {
        // SAFETY: as the caller says.
        unsafe { end.write(s.wrapping_add(read).cast_mut()) };
    }
    value
}

/// A double cast to float, `ERANGE` if that overflows or underflows to
/// zero.
fn narrow_float(value: f64) -> f32 {
    let narrow = value as f32;
    if (narrow.is_infinite() && value.is_finite()) || (narrow == 0.0 && value != 0.0) {
        set_errno(ERANGE);
    }
    narrow
}

/// The floating-point number the string starts with (see [`read_float`]).
pub extern "win64" fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64 {
    // SAFETY: the title's string and place for the end.
    unsafe { float(s.cast::<u8>(), end.cast()) }
}

/// See [`strtod`]; the locale is not used.
pub extern "win64" fn _strtod_l(
    s: *const c_char,
    end: *mut *mut c_char,
    _locale: *mut c_void,
) -> f64 {
    strtod(s, end)
}

/// [`strtod`] rounded to a float.
pub extern "win64" fn strtof(s: *const c_char, end: *mut *mut c_char) -> f32 {
    narrow_float(strtod(s, end))
}

/// `strtod(s, NULL)`.
pub extern "win64" fn atof(s: *const c_char) -> f64 {
    strtod(s, std::ptr::null_mut())
}

/// The wide form of [`strtod`].
pub extern "win64" fn wcstod(s: *const u16, end: *mut *mut u16) -> f64 {
    // SAFETY: the title's string and place for the end.
    unsafe { float(s, end) }
}

/// The wide form of [`strtof`].
pub extern "win64" fn wcstof(s: *const u16, end: *mut *mut u16) -> f32 {
    narrow_float(wcstod(s, end))
}

/// `wcstod(s, NULL)`.
pub extern "win64" fn _wtof(s: *const u16) -> f64 {
    wcstod(s, std::ptr::null_mut())
}

// ---------------------------------------------------------------------------
// Strings from numbers.

/// The digits of a number in `radix` (2 to 36), with a `-` if asked.
fn to_radix(mut value: u64, negative: bool, radix: u32) -> Vec<u8> {
    let mut digits = Vec::new();
    loop {
        let d = (value % u64::from(radix)) as u32;
        digits.push(char::from_digit(d, radix).map_or(b'0', |c| c as u8));
        value /= u64::from(radix);
        if value == 0 {
            break;
        }
    }
    if negative {
        digits.push(b'-');
    }
    digits.reverse();
    digits
}

/// Writes `text` and a NUL to a buffer without a size; the buffer.
///
/// # Safety
/// `buffer` has room for `text` and its NUL.
unsafe fn write_unbounded<T: Unit>(buffer: *mut T, text: &[u8]) -> *mut T {
    if buffer.is_null() {
        return buffer;
    }
    // SAFETY: as the caller says.
    unsafe {
        for (i, &b) in text.iter().enumerate() {
            buffer.add(i).write(T::from_value(u32::from(b)));
        }
        buffer.add(text.len()).write(T::default());
    }
    buffer
}

/// Writes `text` and a NUL to a buffer of `size` units: `EINVAL` for a null
/// buffer or bad radix, `ERANGE` if it does not fit (the buffer emptied).
///
/// # Safety
/// `buffer` is null or has `size` units.
unsafe fn write_bounded<T: Unit>(buffer: *mut T, size: usize, text: &[u8], radix: u32) -> i32 {
    if buffer.is_null() || size == 0 {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: as the caller says.
    unsafe {
        if !(2..=36).contains(&radix) {
            return fail(buffer, size, EINVAL);
        }
        if text.len() >= size {
            return fail(buffer, size, ERANGE);
        }
        write_unbounded(buffer, text);
    }
    0
}

/// The text of a number: a `-` only for a negative one in radix 10 (as the
/// CRT has it: other radices show the two's complement of the type).
fn number_text(value: i64, bits: u32, signed: bool, radix: u32) -> Vec<u8> {
    if !(2..=36).contains(&radix) {
        return Vec::new();
    }
    let negative = signed && radix == 10 && value < 0;
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let magnitude = if negative {
        value.unsigned_abs()
    } else {
        value as u64 & mask
    };
    to_radix(magnitude, negative, radix)
}

/// `_itoa` and its family, narrow and wide.
macro_rules! number_writers {
    ($($name:ident, $name_s:ident: $t:ty, $in:ty, $bits:expr, $signed:expr;)*) => {
        $(
            /// The number's text in `radix`, written to `buffer`.
            pub extern "win64" fn $name(value: $in, buffer: *mut $t, radix: i32) -> *mut $t {
                let text = number_text(value as i64, $bits, $signed, radix as u32);
                // SAFETY: the title's buffer, which has room for the number.
                unsafe { write_unbounded(buffer, &text) }
            }

            /// The number's text in `radix`, in a buffer of `size` units.
            pub extern "win64" fn $name_s(
                value: $in,
                buffer: *mut $t,
                size: usize,
                radix: i32,
            ) -> i32 {
                let text = number_text(value as i64, $bits, $signed, radix as u32);
                // SAFETY: the title's buffer of `size` units.
                unsafe { write_bounded(buffer, size, &text, radix as u32) }
            }
        )*
    };
}

number_writers! {
    _itoa, _itoa_s: c_char, i32, 32, true;
    _ltoa, _ltoa_s: c_char, i32, 32, true;
    _ultoa, _ultoa_s: c_char, u32, 32, false;
    _i64toa, _i64toa_s: c_char, i64, 64, true;
    _ui64toa, _ui64toa_s: c_char, u64, 64, false;
    _itow, _itow_s: u16, i32, 32, true;
    _ltow, _ltow_s: u16, i32, 32, true;
    _ultow, _ultow_s: u16, u32, 32, false;
    _i64tow, _i64tow_s: u16, i64, 64, true;
    _ui64tow, _ui64tow_s: u16, u64, 64, false;
}

// The `c_char` buffers are written as bytes: `c_char` is `i8`, which
// `Unit` does not cover, so the macro's `$t` is cast.
impl Unit for i8 {
    fn value(self) -> u32 {
        u32::from(self as u8)
    }
    fn from_value(value: u32) -> Self {
        value as u8 as i8
    }
    fn lower(self) -> Self {
        (self as u8).to_ascii_lowercase() as i8
    }
    fn upper(self) -> Self {
        (self as u8).to_ascii_uppercase() as i8
    }
    fn is_space(self) -> bool {
        (self as u8).is_space()
    }
}

// ---------------------------------------------------------------------------
// Multibyte (UTF-8) and wide (UTF-16) conversions.

/// Why one character could not be decoded.
enum Decode {
    /// The bytes end within a character.
    Incomplete,
    /// Not UTF-8.
    Invalid,
}

/// The character at the start of `bytes` and its length.
fn decode(bytes: &[u8]) -> Result<(char, usize), Decode> {
    let Some(&first) = bytes.first() else {
        return Err(Decode::Incomplete);
    };
    let n = match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return Err(Decode::Invalid),
    };
    let available = bytes.len().min(n);
    if bytes[1..available].iter().any(|&b| b & 0xC0 != 0x80) {
        return Err(Decode::Invalid);
    }
    if available < n {
        return Err(Decode::Incomplete);
    }
    match std::str::from_utf8(&bytes[..n]) {
        Ok(s) => s.chars().next().map(|c| (c, n)).ok_or(Decode::Invalid),
        Err(_) => Err(Decode::Invalid),
    }
}

/// The UTF-16 of a UTF-8 string, lossily, and for each unit the number of
/// bytes read once it is written (to place the end of a partial
/// conversion).
fn to_wide(bytes: &[u8]) -> Vec<(u16, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (c, n) = decode(&bytes[i..]).unwrap_or((char::REPLACEMENT_CHARACTER, 1));
        i += n;
        let mut buffer = [0u16; 2];
        for &unit in c.encode_utf16(&mut buffer).iter() {
            out.push((unit, i));
        }
    }
    out
}

/// The UTF-8 of a UTF-16 string, lossily, one character at a time with the
/// units it took.
fn to_narrow(units: &[u16]) -> Vec<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    for c in char::decode_utf16(units.iter().copied()) {
        let (c, n) = match c {
            Ok(c) => (c, c.len_utf16()),
            Err(_) => (char::REPLACEMENT_CHARACTER, 1),
        };
        i += n;
        let mut buffer = [0u8; 4];
        out.push((c.encode_utf8(&mut buffer).as_bytes().to_vec(), i));
    }
    out
}

/// Converts a UTF-8 string to UTF-16: the units needed (without the NUL)
/// for a null `dst`, else at most `count` written (and a NUL if it fits)
/// and their number.
pub extern "win64" fn mbstowcs(dst: *mut u16, src: *const c_char, count: usize) -> usize {
    if src.is_null() {
        set_errno(EINVAL);
        return usize::MAX;
    }
    // SAFETY: the title's string.
    let wide = to_wide(unsafe { narrow_bytes(src) });
    if dst.is_null() {
        return wide.len();
    }
    let n = wide.len().min(count);
    // SAFETY: the title's buffer of `count` units.
    unsafe {
        for (i, &(unit, _)) in wide[..n].iter().enumerate() {
            dst.add(i).write(unit);
        }
        if n < count {
            dst.add(n).write(0);
        }
    }
    n
}

/// Converts a UTF-16 string to UTF-8: the bytes needed (without the NUL)
/// for a null `dst`, else at most `count` written, whole characters only
/// (and a NUL if it fits), and their number.
pub extern "win64" fn wcstombs(dst: *mut c_char, src: *const u16, count: usize) -> usize {
    if src.is_null() {
        set_errno(EINVAL);
        return usize::MAX;
    }
    // SAFETY: the title's string.
    let narrow = to_narrow(unsafe { wide_units(src) });
    if dst.is_null() {
        return narrow.iter().map(|(b, _)| b.len()).sum();
    }
    let mut n = 0;
    for (bytes, _) in &narrow {
        if n + bytes.len() > count {
            break;
        }
        // SAFETY: the title's buffer of `count` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(n).cast(), bytes.len()) };
        n += bytes.len();
    }
    if n < count {
        // SAFETY: as above.
        unsafe { dst.add(n).write(0) };
    }
    n
}

/// The `_s` conversions: `units` (whole characters, each a group) into a
/// buffer of `size`, at most `count` (`_TRUNCATE`: what fits); the units
/// written with the NUL in `*converted`.
///
/// # Safety
/// `dst` is null or has `size` units; `converted` null or writable.
unsafe fn convert_s<T: Unit>(
    converted: *mut usize,
    dst: *mut T,
    size: usize,
    groups: &[Vec<T>],
    count: usize,
) -> i32 {
    let total: usize = groups.iter().map(Vec::len).sum();
    let set = |n: usize| {
        if !converted.is_null() {
            // SAFETY: as the caller says.
            unsafe { converted.write(n) };
        }
    };
    if dst.is_null() {
        set(total + 1);
        return 0;
    }
    let limit = if count == TRUNCATE {
        size.saturating_sub(1)
    } else {
        count.min(total)
    };
    let mut n = 0;
    for group in groups {
        if n + group.len() > limit {
            break;
        }
        // SAFETY: within the title's buffer (`limit < size` checked below).
        unsafe {
            std::ptr::copy_nonoverlapping(
                group.as_ptr(),
                dst.add(n),
                group.len().min(size.saturating_sub(n)),
            )
        };
        n += group.len();
    }
    if n >= size {
        set(0);
        // SAFETY: as the caller says.
        return unsafe { fail(dst, size, ERANGE) };
    }
    // SAFETY: `n < size`.
    unsafe { dst.add(n).write(T::default()) };
    set(n + 1);
    if count == TRUNCATE && n < total {
        STRUNCATE
    } else {
        0
    }
}

/// [`mbstowcs`] into a buffer of `dst_size` units; `EINVAL`, `ERANGE` or
/// `STRUNCATE` as the CRT returns them.
pub extern "win64" fn mbstowcs_s(
    converted: *mut usize,
    dst: *mut u16,
    dst_size: usize,
    src: *const c_char,
    count: usize,
) -> i32 {
    if dst.is_null() != (dst_size == 0) || src.is_null() {
        // SAFETY: the title's buffer of `dst_size` units.
        return unsafe { fail(dst, dst_size, EINVAL) };
    }
    // SAFETY: the title's string.
    let wide = to_wide(unsafe { narrow_bytes(src) });
    let groups: Vec<Vec<u16>> = wide.iter().map(|&(u, _)| vec![u]).collect();
    // SAFETY: the title's buffer and place for the count.
    unsafe { convert_s(converted, dst, dst_size, &groups, count) }
}

/// [`wcstombs`] into a buffer of `dst_size` bytes.
pub extern "win64" fn wcstombs_s(
    converted: *mut usize,
    dst: *mut c_char,
    dst_size: usize,
    src: *const u16,
    count: usize,
) -> i32 {
    let dst = dst.cast::<u8>();
    if dst.is_null() != (dst_size == 0) || src.is_null() {
        // SAFETY: the title's buffer of `dst_size` bytes.
        return unsafe { fail(dst, dst_size, EINVAL) };
    }
    // SAFETY: the title's string.
    let narrow = to_narrow(unsafe { wide_units(src) });
    let groups: Vec<Vec<u8>> = narrow.into_iter().map(|(b, _)| b).collect();
    // SAFETY: the title's buffer and place for the count.
    unsafe { convert_s(converted, dst, dst_size, &groups, count) }
}

/// The bytes at `s`, at most `n` and not past a NUL.
///
/// # Safety
/// `s` has a NUL or `n` readable bytes.
unsafe fn prefix<'a>(s: *const c_char, n: usize) -> &'a [u8] {
    // SAFETY: as the caller says.
    unsafe { std::slice::from_raw_parts(s.cast::<u8>(), nlen(s.cast::<u8>(), n)) }
}

/// The length of the character at `s` (0 for the NUL), with the unit it
/// decodes to; -1 and `EILSEQ` for one that is not UTF-8 or not in one
/// unit, or that does not end within `n` bytes.
///
/// # Safety
/// As [`prefix`].
unsafe fn one_wide(s: *const c_char, n: usize) -> Result<(u16, usize), ()> {
    // SAFETY: as the caller says.
    let bytes = unsafe { prefix(s, n) };
    if bytes.is_empty() {
        return if n == 0 { Err(()) } else { Ok((0, 0)) };
    }
    match decode(bytes) {
        Ok((c, len)) => u16::try_from(u32::from(c))
            .map(|u| (u, len))
            .map_err(|_| ()),
        Err(_) => Err(()),
    }
}

/// The wide character of the multibyte one at `s`; its length.
pub extern "win64" fn mbtowc(dst: *mut u16, s: *const c_char, n: usize) -> i32 {
    if s.is_null() {
        return 0;
    }
    // SAFETY: the title's bytes.
    match unsafe { one_wide(s, n) } {
        Ok((unit, len)) => {
            if !dst.is_null() {
                // SAFETY: the title's place for the character.
                unsafe { dst.write(unit) };
            }
            len as i32
        }
        Err(()) => {
            set_errno(EILSEQ);
            -1
        }
    }
}

/// The length of the multibyte character at `s`.
pub extern "win64" fn mblen(s: *const c_char, n: usize) -> i32 {
    mbtowc(std::ptr::null_mut(), s, n)
}

/// The number of characters of a UTF-8 string (a bad byte counts as one).
pub extern "win64" fn _mbstrlen(s: *const c_char) -> usize {
    // SAFETY: the title's string.
    String::from_utf8_lossy(unsafe { narrow_bytes(s) })
        .chars()
        .count()
}

/// The UTF-8 of one wide character; `EILSEQ` for a surrogate.
fn one_narrow(wc: u16) -> Option<Vec<u8>> {
    char::from_u32(u32::from(wc)).map(|c| c.to_string().into_bytes())
}

/// Writes the multibyte form of `wc` at `s`; its length, -1 and `EILSEQ` for
/// a surrogate, 0 for null `s` (no shift state).
pub extern "win64" fn wctomb(s: *mut c_char, wc: u16) -> i32 {
    if s.is_null() {
        return 0;
    }
    let Some(bytes) = one_narrow(wc) else {
        set_errno(EILSEQ);
        return -1;
    };
    // SAFETY: the title's buffer, which has room for a character.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), s.cast(), bytes.len()) };
    bytes.len() as i32
}

/// [`wctomb`] into a buffer of `size` bytes, the length in `*written`.
pub extern "win64" fn wctomb_s(written: *mut i32, s: *mut c_char, size: usize, wc: u16) -> i32 {
    let set = |n: i32| {
        if !written.is_null() {
            // SAFETY: the title's place for the length.
            unsafe { written.write(n) };
        }
    };
    if s.is_null() {
        if size != 0 {
            set_errno(EINVAL);
            return EINVAL;
        }
        set(0);
        return 0;
    }
    let Some(bytes) = one_narrow(wc) else {
        set(-1);
        set_errno(EILSEQ);
        return EILSEQ;
    };
    if bytes.len() > size {
        set(-1);
        set_errno(ERANGE);
        return ERANGE;
    }
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), s.cast(), bytes.len()) };
    set(bytes.len() as i32);
    0
}

/// [`mbtowc`] with a state: -2 when the character does not end within `n`
/// bytes (the partial bytes are not kept in the state).
pub extern "win64" fn mbrtowc(
    dst: *mut u16,
    s: *const c_char,
    n: usize,
    _state: *mut c_void,
) -> usize {
    if s.is_null() {
        return 0;
    }
    // SAFETY: the title's bytes.
    let bytes = unsafe { prefix(s, n) };
    if bytes.is_empty() && n != 0 {
        if !dst.is_null() {
            // SAFETY: the title's place for the character.
            unsafe { dst.write(0) };
        }
        return 0;
    }
    match decode(bytes) {
        Ok((c, len)) => match u16::try_from(u32::from(c)) {
            Ok(unit) => {
                if !dst.is_null() {
                    // SAFETY: as above.
                    unsafe { dst.write(unit) };
                }
                len
            }
            Err(_) => {
                set_errno(EILSEQ);
                usize::MAX
            }
        },
        Err(Decode::Incomplete) => usize::MAX - 1,
        Err(Decode::Invalid) => {
            set_errno(EILSEQ);
            usize::MAX
        }
    }
}

/// [`wctomb`] with a state; 1 for null `s`.
pub extern "win64" fn wcrtomb(s: *mut c_char, wc: u16, _state: *mut c_void) -> usize {
    if s.is_null() {
        return 1;
    }
    match wctomb(s, wc) {
        -1 => usize::MAX,
        n => n as usize,
    }
}

/// Converts the UTF-8 string `*src` to at most `len` UTF-16 units; `*src`
/// becomes null if the NUL was reached, else the first byte not converted.
/// The units (without the NUL); all of them for a null `dst`.
pub extern "win64" fn mbsrtowcs(
    dst: *mut u16,
    src: *mut *const c_char,
    len: usize,
    _state: *mut c_void,
) -> usize {
    // SAFETY: the title's place for the string.
    if src.is_null() || unsafe { (*src).is_null() } {
        set_errno(EINVAL);
        return usize::MAX;
    }
    // SAFETY: as above, and the string it holds.
    let start = unsafe { *src };
    // SAFETY: the title's string.
    let wide = to_wide(unsafe { narrow_bytes(start) });
    if dst.is_null() {
        return wide.len();
    }
    let n = wide.len().min(len);
    // SAFETY: the title's buffer of `len` units and place for the string.
    unsafe {
        for (i, &(unit, _)) in wide[..n].iter().enumerate() {
            dst.add(i).write(unit);
        }
        if n < len {
            dst.add(n).write(0);
            *src = std::ptr::null();
        } else {
            let read = if n == 0 { 0 } else { wide[n - 1].1 };
            *src = start.add(read);
        }
    }
    n
}

/// Converts the UTF-16 string `*src` to at most `len` bytes of UTF-8 (whole
/// characters); `*src` as [`mbsrtowcs`] sets it.
pub extern "win64" fn wcsrtombs(
    dst: *mut c_char,
    src: *mut *const u16,
    len: usize,
    _state: *mut c_void,
) -> usize {
    // SAFETY: the title's place for the string.
    if src.is_null() || unsafe { (*src).is_null() } {
        set_errno(EINVAL);
        return usize::MAX;
    }
    // SAFETY: as above, and the string it holds.
    let start = unsafe { *src };
    // SAFETY: the title's string.
    let narrow = to_narrow(unsafe { wide_units(start) });
    if dst.is_null() {
        return narrow.iter().map(|(b, _)| b.len()).sum();
    }
    let mut n = 0;
    let mut read = 0;
    let mut all = true;
    for (bytes, after) in &narrow {
        if n + bytes.len() > len {
            all = false;
            break;
        }
        // SAFETY: the title's buffer of `len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(n).cast(), bytes.len()) };
        n += bytes.len();
        read = *after;
    }
    // SAFETY: as above, and the title's place for the string.
    unsafe {
        if all && n < len {
            dst.add(n).write(0);
            *src = std::ptr::null();
        } else {
            *src = start.add(read);
        }
    }
    n
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(memcpy),
        hle_export!(memmove),
        hle_export!(memset),
        hle_export!(memcmp),
        hle_export!(memchr),
        hle_export!(_memccpy),
        hle_export!(memccpy),
        hle_export!(_memicmp),
        hle_export!(strlen),
        hle_export!(strnlen),
        hle_export!(strnlen_s),
        hle_export!(strcpy),
        hle_export!(strncpy),
        hle_export!(strcpy_s),
        hle_export!(strncpy_s),
        hle_export!(strcat),
        hle_export!(strncat),
        hle_export!(strcat_s),
        hle_export!(strncat_s),
        hle_export!(strcmp),
        hle_export!(strncmp),
        hle_export!(_stricmp),
        hle_export!(_strnicmp),
        hle_export!(_strcmpi),
        hle_export!(_stricmp_l),
        hle_export!(strchr),
        hle_export!(strrchr),
        hle_export!(strstr),
        hle_export!(strtok),
        hle_export!(strtok_s),
        hle_export!(_strdup),
        hle_export!(_strlwr),
        hle_export!(_strupr),
        hle_export!(_strlwr_s),
        hle_export!(_strupr_s),
        hle_export!(_strnset),
        hle_export!(_strnset_s),
        hle_export!(_strrev),
        hle_export!(_strset),
        hle_export!(_strset_s),
        hle_export!(strspn),
        hle_export!(strcspn),
        hle_export!(strpbrk),
        hle_export!(strcoll),
        hle_export!(_stricoll),
        hle_export!(_strncoll),
        hle_export!(_strnicoll),
        hle_export!(strxfrm),
        hle_export!(wcslen),
        hle_export!(wcsnlen),
        hle_export!(wcsnlen_s),
        hle_export!(wcscpy),
        hle_export!(wcsncpy),
        hle_export!(wcscpy_s),
        hle_export!(wcsncpy_s),
        hle_export!(wcscat),
        hle_export!(wcsncat),
        hle_export!(wcscat_s),
        hle_export!(wcsncat_s),
        hle_export!(wcscmp),
        hle_export!(wcsncmp),
        hle_export!(_wcsicmp),
        hle_export!(_wcsnicmp),
        hle_export!(_wcsicmp_l),
        hle_export!(wcschr),
        hle_export!(wcsrchr),
        hle_export!(wcsstr),
        hle_export!(wcstok),
        hle_export!(wcstok_s),
        hle_export!(_wcsdup),
        hle_export!(_wcslwr),
        hle_export!(_wcsupr),
        hle_export!(_wcslwr_s),
        hle_export!(_wcsupr_s),
        hle_export!(_wcsnset),
        hle_export!(_wcsrev),
        hle_export!(_wcsset),
        hle_export!(wcsspn),
        hle_export!(wcscspn),
        hle_export!(wcspbrk),
        hle_export!(wcscoll),
        hle_export!(_wcsicoll),
        hle_export!(_wcsncoll),
        hle_export!(_wcsnicoll),
        hle_export!(wcsxfrm),
        hle_export!(tolower),
        hle_export!(toupper),
        hle_export!(_tolower),
        hle_export!(_toupper),
        hle_export!(towlower),
        hle_export!(towupper),
        hle_export!(isalpha),
        hle_export!(isdigit),
        hle_export!(isalnum),
        hle_export!(isspace),
        hle_export!(isupper),
        hle_export!(islower),
        hle_export!(isxdigit),
        hle_export!(ispunct),
        hle_export!(isprint),
        hle_export!(iscntrl),
        hle_export!(isgraph),
        hle_export!(isblank),
        hle_export!(__isascii),
        hle_export!(_isctype),
        hle_export!(__pctype_func),
        hle_export!(__pwctype_func),
        hle_export!(iswalpha),
        hle_export!(iswdigit),
        hle_export!(iswspace),
        hle_export!(iswalnum),
        hle_export!(iswupper),
        hle_export!(iswlower),
        hle_export!(iswxdigit),
        hle_export!(iswprint),
        hle_export!(iswpunct),
        hle_export!(iswcntrl),
        hle_export!(iswgraph),
        hle_export!(iswblank),
        hle_export!(iswascii),
        hle_export!(iswctype),
        hle_export!(_iswctype_l),
        hle_export!(__iswcsymf),
        hle_export!(__iswcsym),
        hle_export!(isleadbyte),
        hle_export!(_ismbblead),
        hle_export!(atoi),
        hle_export!(atol),
        hle_export!(atoll),
        hle_export!(atof),
        hle_export!(_atoi64),
        hle_export!(strtol),
        hle_export!(strtoul),
        hle_export!(strtoll),
        hle_export!(strtoull),
        hle_export!(_strtoi64),
        hle_export!(_strtoui64),
        hle_export!(strtod),
        hle_export!(strtof),
        hle_export!(_strtod_l),
        hle_export!(wcstol),
        hle_export!(wcstoul),
        hle_export!(wcstoll),
        hle_export!(wcstoull),
        hle_export!(_wcstoi64),
        hle_export!(_wcstoui64),
        hle_export!(wcstod),
        hle_export!(wcstof),
        hle_export!(_wtoi),
        hle_export!(_wtol),
        hle_export!(_wtoi64),
        hle_export!(_wtof),
        hle_export!(_wtoll),
        hle_export!(_itoa),
        hle_export!(_itoa_s),
        hle_export!(_ltoa),
        hle_export!(_ltoa_s),
        hle_export!(_ultoa),
        hle_export!(_ultoa_s),
        hle_export!(_i64toa),
        hle_export!(_i64toa_s),
        hle_export!(_ui64toa),
        hle_export!(_ui64toa_s),
        hle_export!(_itow),
        hle_export!(_itow_s),
        hle_export!(_ltow),
        hle_export!(_ltow_s),
        hle_export!(_ultow),
        hle_export!(_ultow_s),
        hle_export!(_i64tow),
        hle_export!(_i64tow_s),
        hle_export!(_ui64tow),
        hle_export!(_ui64tow_s),
        hle_export!(mbstowcs),
        hle_export!(mbstowcs_s),
        hle_export!(wcstombs),
        hle_export!(wcstombs_s),
        hle_export!(mbtowc),
        hle_export!(wctomb),
        hle_export!(wctomb_s),
        hle_export!(mblen),
        hle_export!(_mbstrlen),
        hle_export!(mbrtowc),
        hle_export!(wcrtomb),
        hle_export!(mbsrtowcs),
        hle_export!(wcsrtombs),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn integers_parse_as_strtol_does() {
        let cases: &[(&CStr, i32, i32, usize)] = &[
            (c"  42xyz", 10, 42, 4),
            (c"-17", 10, -17, 3),
            (c"0x1F", 16, 31, 4),
            (c"0x1F", 0, 31, 4),
            (c"017", 0, 15, 3),
            (c"0xg", 16, 0, 1),
            (c"zz", 36, 1295, 2),
            (c"abc", 10, 0, 0),
            (c"2147483648", 10, i32::MAX, 10),
            (c"-2147483648", 10, i32::MIN, 11),
        ];
        for &(text, base, value, read) in cases {
            let mut end = std::ptr::null_mut();
            assert_eq!(strtol(text.as_ptr(), &mut end, base), value, "{text:?}");
            assert_eq!(end as usize - text.as_ptr() as usize, read, "{text:?}");
        }
        set_errno(0);
        strtol(c"99999999999".as_ptr(), std::ptr::null_mut(), 10);
        assert_eq!(super::super::runtime::errno(), ERANGE);
        assert_eq!(strtoul(c"-1".as_ptr(), std::ptr::null_mut(), 10), u32::MAX);
        assert_eq!(
            strtoul(c"4294967295".as_ptr(), std::ptr::null_mut(), 10),
            u32::MAX
        );
        assert_eq!(
            strtoull(c"18446744073709551615".as_ptr(), std::ptr::null_mut(), 0),
            u64::MAX
        );
        assert_eq!(
            strtoll(c"-9223372036854775808".as_ptr(), std::ptr::null_mut(), 10),
            i64::MIN
        );
        assert_eq!(atoi(c"  -123abc".as_ptr()), -123);
        assert_eq!(atoi(c"+7".as_ptr()), 7);
        assert_eq!(_wtoi(wide(" 314").as_ptr()), 314);
        assert_eq!(strtol(c"1".as_ptr(), std::ptr::null_mut(), 1), 0);
    }

    #[test]
    fn floats_parse_as_strtod_does() {
        let cases: &[(&CStr, f64, usize)] = &[
            (c"3.25", 3.25, 4),
            (c"  -1e3x", -1000.0, 6),
            (c".5", 0.5, 2),
            (c"1.", 1.0, 2),
            (c"1e", 1.0, 1),
            (c"0x1p4", 16.0, 5),
            (c"0x1.8p1", 3.0, 7),
            (c"0xAp-1", 5.0, 6),
            (c"INFINITY", f64::INFINITY, 8),
            (c"-inf", f64::NEG_INFINITY, 4),
            (c"abc", 0.0, 0),
            (c"1e400", f64::INFINITY, 5),
            (c"2.2250738585072014e-308", f64::MIN_POSITIVE, 23),
        ];
        for &(text, value, read) in cases {
            let mut end = std::ptr::null_mut();
            assert_eq!(strtod(text.as_ptr(), &mut end), value, "{text:?}");
            assert_eq!(end as usize - text.as_ptr() as usize, read, "{text:?}");
        }
        let mut end = std::ptr::null_mut();
        assert!(strtod(c"nan(ind)z".as_ptr(), &mut end).is_nan());
        assert_eq!(end as usize - c"nan(ind)z".as_ptr() as usize, 8);
        // The smallest subnormal and the largest double, in hex.
        assert_eq!(
            strtod(c"0x1p-1074".as_ptr(), std::ptr::null_mut()),
            f64::from_bits(1)
        );
        let max = strtod(c"0x1.fffffffffffffp1023".as_ptr(), std::ptr::null_mut());
        assert_eq!(max, f64::MAX);
        set_errno(0);
        assert_eq!(strtod(c"1e-400".as_ptr(), std::ptr::null_mut()), 0.0);
        assert_eq!(super::super::runtime::errno(), ERANGE);
        assert_eq!(atof(c"2.5".as_ptr()), 2.5);
        assert_eq!(
            wcstod(wide("-0.125").as_ptr(), std::ptr::null_mut()),
            -0.125
        );
        assert_eq!(strtof(c"0.1".as_ptr(), std::ptr::null_mut()), 0.1f32);
    }

    #[test]
    fn case_and_classes_follow_ascii() {
        assert_eq!(tolower(i32::from(b'A')), i32::from(b'a'));
        assert_eq!(tolower(i32::from(b'1')), i32::from(b'1'));
        assert_eq!(toupper(i32::from(b'z')), i32::from(b'Z'));
        assert_eq!(towupper(0xE9), 0xC9);
        assert_eq!(towlower(u16::from(b'Q')), u16::from(b'q'));
        assert_ne!(isalpha(i32::from(b'x')), 0);
        assert_eq!(isalpha(0xE9), 0);
        assert_eq!(isdigit(-1), 0);
        assert_ne!(isspace(i32::from(b'\n')), 0);
        assert_ne!(isprint(i32::from(b'\t')), 0);
        assert_ne!(isxdigit(i32::from(b'f')), 0);
        assert_ne!(ispunct(i32::from(b'!')), 0);
        assert_ne!(iswalpha(0x3B1), 0);
        // SAFETY: the table is valid from -1 to 255.
        assert_eq!(unsafe { *__pctype_func().offset(-1) }, 0);
        // SAFETY: as above.
        assert_eq!(
            unsafe { *__pctype_func().add(usize::from(b'A')) },
            UPPER | HEX | LETTER
        );
        let mut text = *b"MiXeD\0";
        _strlwr(text.as_mut_ptr().cast());
        assert_eq!(&text, b"mixed\0");
        _strupr(text.as_mut_ptr().cast());
        assert_eq!(&text, b"MIXED\0");
        assert_eq!(_stricmp(c"Hello".as_ptr(), c"hELLO".as_ptr()), 0);
        assert!(_stricmp(c"a".as_ptr(), c"B".as_ptr()) < 0);
        assert!(strcmp(c"\x80".as_ptr(), c"a".as_ptr()) > 0);
        assert_eq!(_wcsicmp(wide("ÉTÉ").as_ptr(), wide("été").as_ptr()), 0);
    }

    #[test]
    fn strtok_s_splits_and_searches_work() {
        let mut text = *b",,a,bb;;c,\0";
        let mut context = std::ptr::null_mut();
        let delim = c",;".as_ptr();
        let mut tokens = Vec::new();
        let mut s = text.as_mut_ptr().cast::<c_char>();
        loop {
            let token = strtok_s(s, delim, &mut context);
            if token.is_null() {
                break;
            }
            // SAFETY: a NUL-terminated token in `text`.
            tokens.push(
                unsafe { CStr::from_ptr(token) }
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
            s = std::ptr::null_mut();
        }
        assert_eq!(tokens, ["a", "bb", "c"]);
        let hay = c"needle in a haystack";
        assert_eq!(
            strstr(hay.as_ptr(), c"hay".as_ptr()) as usize - hay.as_ptr() as usize,
            12
        );
        assert!(strstr(hay.as_ptr(), c"straw".as_ptr()).is_null());
        assert_eq!(
            strchr(hay.as_ptr(), i32::from(b'e')) as usize - hay.as_ptr() as usize,
            1
        );
        assert_eq!(
            strrchr(hay.as_ptr(), i32::from(b'a')) as usize - hay.as_ptr() as usize,
            17
        );
        assert_eq!(strspn(c"aabbc".as_ptr(), c"ab".as_ptr()), 4);
        assert_eq!(strcspn(c"xyz,".as_ptr(), c",".as_ptr()), 3);
        let copy = _strdup(hay.as_ptr());
        assert_eq!(strcmp(copy, hay.as_ptr()), 0);
        super::super::heap::free(copy.cast());
        let mut rev = *b"abc\0";
        _strrev(rev.as_mut_ptr().cast());
        assert_eq!(&rev, b"cba\0");
    }

    #[test]
    fn numbers_are_written_in_any_radix() {
        let mut buffer = [0 as c_char; 40];
        let text = |b: &[c_char]| {
            // SAFETY: NUL-terminated by the writer.
            unsafe { CStr::from_ptr(b.as_ptr()) }
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(_itoa_s(255, buffer.as_mut_ptr(), 40, 16), 0);
        assert_eq!(text(&buffer), "ff");
        assert_eq!(_itoa_s(-5, buffer.as_mut_ptr(), 40, 10), 0);
        assert_eq!(text(&buffer), "-5");
        assert_eq!(_itoa_s(-1, buffer.as_mut_ptr(), 40, 16), 0);
        assert_eq!(text(&buffer), "ffffffff");
        assert_eq!(_itoa_s(5, buffer.as_mut_ptr(), 40, 2), 0);
        assert_eq!(text(&buffer), "101");
        assert_eq!(_i64toa_s(i64::MIN, buffer.as_mut_ptr(), 40, 10), 0);
        assert_eq!(text(&buffer), "-9223372036854775808");
        assert_eq!(_ui64toa_s(35, buffer.as_mut_ptr(), 40, 36), 0);
        assert_eq!(text(&buffer), "z");
        assert_eq!(_itoa_s(5, buffer.as_mut_ptr(), 40, 37), EINVAL);
        assert_eq!(buffer[0], 0);
        assert_eq!(_itoa_s(12345, buffer.as_mut_ptr(), 5, 10), ERANGE);
        assert_eq!(buffer[0], 0);
        let mut wbuffer = [0u16; 8];
        _itow(-42, wbuffer.as_mut_ptr(), 10);
        assert_eq!(&wbuffer[..4], &wide("-42")[..]);
    }

    #[test]
    fn secure_copies_fail_by_emptying_the_destination() {
        let mut small = [b'x' as c_char; 4];
        assert_eq!(strcpy_s(small.as_mut_ptr(), 4, c"abc".as_ptr()), 0);
        assert_eq!(strcmp(small.as_ptr(), c"abc".as_ptr()), 0);
        assert_eq!(strcpy_s(small.as_mut_ptr(), 4, c"abcd".as_ptr()), ERANGE);
        assert_eq!(small[0], 0);
        assert_eq!(strcpy_s(small.as_mut_ptr(), 4, c"ab".as_ptr()), 0);
        assert_eq!(strcat_s(small.as_mut_ptr(), 4, c"c".as_ptr()), 0);
        assert_eq!(strcat_s(small.as_mut_ptr(), 4, c"d".as_ptr()), ERANGE);
        assert_eq!(small[0], 0);
        assert_eq!(strncpy_s(small.as_mut_ptr(), 4, c"abcdef".as_ptr(), 2), 0);
        assert_eq!(strcmp(small.as_ptr(), c"ab".as_ptr()), 0);
        // A null source goes to the invalid-parameter handler first.
        extern "win64" fn ignore(_: *const u16, _: *const u16, _: *const u16, _: u32, _: usize) {}
        let old = super::super::runtime::_set_thread_local_invalid_parameter_handler(Some(ignore));
        assert_eq!(strcpy_s(small.as_mut_ptr(), 4, std::ptr::null()), EINVAL);
        assert_eq!(small[0], 0);
        super::super::runtime::_set_thread_local_invalid_parameter_handler(old);

        let mut wsmall = [1u16; 4];
        let long = wide("truncated");
        assert_eq!(
            wcsncpy_s(wsmall.as_mut_ptr(), 4, long.as_ptr(), TRUNCATE),
            STRUNCATE
        );
        assert_eq!(
            wsmall,
            [u16::from(b't'), u16::from(b'r'), u16::from(b'u'), 0]
        );
        assert_eq!(wcsncpy_s(wsmall.as_mut_ptr(), 4, long.as_ptr(), 9), ERANGE);
        assert_eq!(wsmall[0], 0);
    }

    #[test]
    fn multibyte_conversions_round_trip() {
        let text = c"héllo";
        let needed = mbstowcs(std::ptr::null_mut(), text.as_ptr(), 0);
        assert_eq!(needed, 5);
        let mut buffer = [0u16; 8];
        assert_eq!(mbstowcs(buffer.as_mut_ptr(), text.as_ptr(), 8), 5);
        assert_eq!(&buffer[..6], &wide("héllo")[..]);
        let mut back = [0 as c_char; 8];
        assert_eq!(wcstombs(back.as_mut_ptr(), buffer.as_ptr(), 8), 6);
        assert_eq!(strcmp(back.as_ptr(), text.as_ptr()), 0);
        // Whole characters only: "h" and not half of the "é".
        assert_eq!(wcstombs(back.as_mut_ptr(), buffer.as_ptr(), 2), 1);
        let mut converted = 0;
        assert_eq!(
            mbstowcs_s(
                &mut converted,
                buffer.as_mut_ptr(),
                8,
                text.as_ptr(),
                TRUNCATE
            ),
            0
        );
        assert_eq!(converted, 6);
        assert_eq!(
            mbstowcs_s(
                &mut converted,
                buffer.as_mut_ptr(),
                3,
                text.as_ptr(),
                TRUNCATE
            ),
            STRUNCATE
        );
        assert_eq!(converted, 3);
        assert_eq!(
            wcstombs_s(
                &mut converted,
                back.as_mut_ptr(),
                8,
                buffer.as_ptr(),
                TRUNCATE
            ),
            0
        );
        let mut unit = 0;
        assert_eq!(mbtowc(&mut unit, c"é".as_ptr(), 4), 2);
        assert_eq!(unit, 0xE9);
        assert_eq!(mbtowc(&mut unit, c"\xFF".as_ptr(), 4), -1);
        let mut out = [0 as c_char; 4];
        assert_eq!(wctomb(out.as_mut_ptr(), 0x20AC), 3);
        assert_eq!(wctomb(out.as_mut_ptr(), 0xD800), -1);
        assert_eq!(
            mbrtowc(&mut unit, c"\xE2\x82".as_ptr(), 2, std::ptr::null_mut()),
            usize::MAX - 1
        );
        assert_eq!(_mbstrlen(text.as_ptr()), 5);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 186);
    }
}
