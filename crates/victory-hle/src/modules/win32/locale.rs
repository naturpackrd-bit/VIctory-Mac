//! Code pages, the locale (one: `en-US`), strings as the locale compares,
//! maps and classifies them, messages (`FormatMessage`), dates and times in
//! the locale's pictures, time zones (UTC) and the processors' topology
//! that `time.rs` does not report.
//!
//! Limits:
//! - The ANSI and OEM code pages (1252, 437) are Latin-1: each byte is the
//!   code unit of the same value (Windows maps 0x80-0x9F of 1252 and the
//!   upper half of 437 to other characters).
//! - Comparisons are by code unit (with simple case folding for the
//!   ignore-case forms), not by the locale's linguistic order; sort keys
//!   follow the same order.
//! - `FormatMessage` knows the texts of the common Win32 errors only (no
//!   message tables of modules); its printf-style inserts take no floating
//!   point (neither does Windows').
//! - Every locale name is answered with `en-US`'s data.
#![allow(non_snake_case, non_camel_case_types)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]
// Windows' signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;

use victory_platform::Host;
use victory_platform::system::System;

use super::handles::{ERROR_INVALID_PARAMETER, set_last_error};
use super::memory::LocalAlloc;
use super::time::{
    FILETIME, FileTimeToSystemTime, GROUP_AFFINITY, GetSystemTime, PROCESSOR_NUMBER, SYSTEMTIME,
    SystemTimeToFileTime, TIME_ZONE_INFORMATION,
};
use crate::{HleExport, hle_export};

type BOOL = i32;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_MR_MID_NOT_FOUND: u32 = 317;
const ERROR_INVALID_FLAGS: u32 = 1004;
const ERROR_NO_UNICODE_TRANSLATION: u32 = 1113;

/// Sets the last error to `code` and gives back `value`.
fn fail<T>(code: u32, value: T) -> T {
    set_last_error(code);
    value
}

// The title's strings.

/// A code unit of a narrow or a wide string.
trait Unit: Copy + Default + PartialEq {}
impl Unit for u8 {}
impl Unit for u16 {}

/// The units of a NUL-terminated string at `text`, without the NUL.
///
/// # Safety
/// `text` is a NUL-terminated string.
unsafe fn terminated<'a, T: Unit>(text: *const T) -> &'a [T] {
    // SAFETY: the caller's string.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != T::default()).count();
        std::slice::from_raw_parts(text, len)
    }
}

/// The title's `len` units at `text`; `-1` is up to the NUL, which
/// `with_nul` counts. None for a null pointer or a length below -1.
fn units<'a, T: Unit>(text: *const T, len: i32, with_nul: bool) -> Option<&'a [T]> {
    if text.is_null() || len < -1 {
        return None;
    }
    if len == -1 {
        // SAFETY: the title passes a NUL-terminated string.
        let found = unsafe { terminated(text) };
        let len = found.len() + usize::from(with_nul);
        // SAFETY: the string and its NUL.
        return Some(unsafe { std::slice::from_raw_parts(text, len) });
    }
    // SAFETY: the title's `len` units.
    Some(unsafe { std::slice::from_raw_parts(text, len as usize) })
}

/// A NUL-terminated wide string as text; `None` for null.
fn wide(text: *const u16) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string.
    (!text.is_null()).then(|| String::from_utf16_lossy(unsafe { terminated(text) }))
}

/// A NUL-terminated narrow string (the ANSI code page: Latin-1) as text;
/// `None` for null.
fn narrow(text: *const u8) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string.
    (!text.is_null()).then(|| latin1(unsafe { terminated(text) }))
}

/// Latin-1 bytes as text.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Text as Latin-1 bytes, `?` for what has no byte.
fn to_latin1(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

/// Text and a NUL, as UTF-16.
fn wide_z(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Text and a NUL, as Latin-1.
fn narrow_z(text: &str) -> Vec<u8> {
    let mut bytes = to_latin1(text);
    bytes.push(0);
    bytes
}

/// Writes `text` (with its NUL, if it has one) to the title's buffer of
/// `cch` units: the units written; the units needed when `cch` is 0; 0 and
/// `ERROR_INSUFFICIENT_BUFFER` when it does not fit.
fn put<T: Unit>(text: &[T], out: *mut T, cch: i32) -> i32 {
    if cch < 0 || (cch > 0 && out.is_null()) {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    if cch == 0 {
        return text.len() as i32;
    }
    if (cch as usize) < text.len() {
        return fail(ERROR_INSUFFICIENT_BUFFER, 0);
    }
    // SAFETY: the title's buffer holds `cch` units, at least `text.len()`.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    text.len() as i32
}

// Code pages.

const CP_ACP: u32 = 0;
const CP_OEMCP: u32 = 1;
const CP_THREAD_ACP: u32 = 3;
const CP_UTF8: u32 = 65001;
const ANSI_PAGE: u32 = 1252;
const OEM_PAGE: u32 = 437;

const MB_PRECOMPOSED: u32 = 1;
const MB_COMPOSITE: u32 = 2;
const MB_USEGLYPHCHARS: u32 = 4;
const MB_ERR_INVALID_CHARS: u32 = 8;
const WC_COMPOSITECHECK: u32 = 0x200;
const WC_DISCARDNS: u32 = 0x10;
const WC_SEPCHARS: u32 = 0x20;
const WC_DEFAULTCHAR: u32 = 0x40;
const WC_ERR_INVALID_CHARS: u32 = 0x80;
const WC_NO_BEST_FIT_CHARS: u32 = 0x400;

/// The code pages converted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Latin1,
    Utf8,
}

/// The page `page` names: the ANSI and OEM pages are Latin-1 here; UTF-16
/// and the rest are not converted (`ERROR_INVALID_PARAMETER`).
fn page(page: u32) -> Option<Page> {
    match page {
        CP_ACP | CP_OEMCP | CP_THREAD_ACP | ANSI_PAGE | OEM_PAGE => Some(Page::Latin1),
        CP_UTF8 => Some(Page::Utf8),
        _ => None,
    }
}

/// Narrow text to UTF-16: `MultiByteToWideChar`'s body.
fn decode(page: Page, bytes: &[u8], strict: bool) -> Result<Vec<u16>, u32> {
    match page {
        Page::Latin1 => Ok(bytes.iter().map(|&b| u16::from(b)).collect()),
        Page::Utf8 => {
            let mut out = Vec::with_capacity(bytes.len());
            for chunk in bytes.utf8_chunks() {
                out.extend(chunk.valid().encode_utf16());
                if !chunk.invalid().is_empty() {
                    if strict {
                        return Err(ERROR_NO_UNICODE_TRANSLATION);
                    }
                    out.push(0xFFFD);
                }
            }
            Ok(out)
        }
    }
}

/// UTF-16 to narrow text: `WideCharToMultiByte`'s body; whether a
/// character became the default one.
fn encode(page: Page, units: &[u16], strict: bool, default: u8) -> Result<(Vec<u8>, bool), u32> {
    let mut out = Vec::with_capacity(units.len());
    let mut defaulted = false;
    for c in char::decode_utf16(units.iter().copied()) {
        match (page, c) {
            (Page::Utf8, Ok(c)) => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            }
            (Page::Utf8, Err(_)) if strict => return Err(ERROR_NO_UNICODE_TRANSLATION),
            (Page::Utf8, Err(_)) => out.extend_from_slice("\u{FFFD}".as_bytes()),
            (Page::Latin1, Ok(c)) if u32::from(c) < 256 => out.push(u32::from(c) as u8),
            (Page::Latin1, _) => {
                out.push(default);
                defaulted = true;
            }
        }
    }
    Ok((out, defaulted))
}

/// Narrow text to UTF-16 (`len` -1: NUL-terminated, the NUL converted
/// too); the units written, or needed when `cch` is 0.
pub extern "win64" fn MultiByteToWideChar(
    page_id: u32,
    flags: u32,
    text: *const u8,
    len: i32,
    out: *mut u16,
    cch: i32,
) -> i32 {
    let Some(page) = page(page_id) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    let allowed = match page {
        Page::Utf8 => MB_ERR_INVALID_CHARS,
        Page::Latin1 => MB_PRECOMPOSED | MB_COMPOSITE | MB_USEGLYPHCHARS | MB_ERR_INVALID_CHARS,
    };
    if flags & !allowed != 0 || flags & MB_PRECOMPOSED != 0 && flags & MB_COMPOSITE != 0 {
        return fail(ERROR_INVALID_FLAGS, 0);
    }
    let Some(bytes) = units(text, len, true).filter(|b| !b.is_empty()) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    match decode(page, bytes, flags & MB_ERR_INVALID_CHARS != 0) {
        Ok(wide) => put(&wide, out, cch),
        Err(code) => fail(code, 0),
    }
}

/// UTF-16 to narrow text (`len` -1: NUL-terminated, the NUL converted
/// too); the bytes written, or needed when `cb` is 0. A character the page
/// lacks becomes `default_char` (`?` when null), noted in `used_default`.
pub extern "win64" fn WideCharToMultiByte(
    page_id: u32,
    flags: u32,
    text: *const u16,
    len: i32,
    out: *mut u8,
    cb: i32,
    default_char: *const u8,
    used_default: *mut i32,
) -> i32 {
    let Some(page) = page(page_id) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    let allowed = match page {
        Page::Utf8 => WC_ERR_INVALID_CHARS,
        Page::Latin1 => {
            WC_COMPOSITECHECK
                | WC_DISCARDNS
                | WC_SEPCHARS
                | WC_DEFAULTCHAR
                | WC_ERR_INVALID_CHARS
                | WC_NO_BEST_FIT_CHARS
        }
    };
    if flags & !allowed != 0 {
        return fail(ERROR_INVALID_FLAGS, 0);
    }
    // UTF-8 has every character: no default to give, as on Windows.
    if page == Page::Utf8 && (!default_char.is_null() || !used_default.is_null()) {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    let Some(wide) = units(text, len, true).filter(|w| !w.is_empty()) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    let default = if default_char.is_null() {
        b'?'
    } else {
        // SAFETY: checked non-null; the title's default character.
        unsafe { *default_char }
    };
    match encode(page, wide, flags & WC_ERR_INVALID_CHARS != 0, default) {
        Ok((bytes, defaulted)) => {
            let written = put(&bytes, out, cb);
            if !used_default.is_null() {
                // SAFETY: checked non-null; the title's flag.
                unsafe { used_default.write(i32::from(defaulted)) };
            }
            written
        }
        Err(code) => fail(code, 0),
    }
}

/// The ANSI code page.
pub extern "win64" fn GetACP() -> u32 {
    ANSI_PAGE
}

/// The OEM code page.
pub extern "win64" fn GetOEMCP() -> u32 {
    OEM_PAGE
}

/// A code page's character sizes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CPINFO {
    pub max_char_size: u32,
    pub default_char: [u8; 2],
    pub lead_byte: [u8; 12],
}

const _: () = assert!(std::mem::size_of::<CPINFO>() == 20);

/// [`CPINFO`], with the page's number and name.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CPINFOEXW {
    pub max_char_size: u32,
    pub default_char: [u8; 2],
    pub lead_byte: [u8; 12],
    pub unicode_default_char: u16,
    pub code_page: u32,
    pub code_page_name: [u16; 260],
}

const _: () = assert!(std::mem::size_of::<CPINFOEXW>() == 544);

/// The page a code page number stands for, and its name.
fn page_info(page_id: u32) -> Option<(u32, &'static str)> {
    Some(match page_id {
        CP_ACP | CP_THREAD_ACP | ANSI_PAGE => (ANSI_PAGE, "1252  (ANSI - Latin I)"),
        CP_OEMCP | OEM_PAGE => (OEM_PAGE, "437   (OEM - United States)"),
        CP_UTF8 => (CP_UTF8, "65001 (UTF-8)"),
        _ => return None,
    })
}

/// No lead bytes; one byte a character but in UTF-8 (four at most).
fn cp_info(page: u32) -> CPINFO {
    CPINFO {
        max_char_size: if page == CP_UTF8 { 4 } else { 1 },
        default_char: [b'?', 0],
        lead_byte: [0; 12],
    }
}

/// See [`cp_info`].
pub extern "win64" fn GetCPInfo(page_id: u32, out: *mut CPINFO) -> BOOL {
    let Some((page, _)) = page_info(page_id).filter(|_| !out.is_null()) else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    // SAFETY: checked non-null; the title's output.
    unsafe { out.write_unaligned(cp_info(page)) };
    TRUE
}

/// See [`cp_info`]; the page's number and name too.
pub extern "win64" fn GetCPInfoExW(page_id: u32, _flags: u32, out: *mut CPINFOEXW) -> BOOL {
    let Some((page, name)) = page_info(page_id).filter(|_| !out.is_null()) else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    let base = cp_info(page);
    let mut info = CPINFOEXW {
        max_char_size: base.max_char_size,
        default_char: base.default_char,
        lead_byte: base.lead_byte,
        unicode_default_char: if page == CP_UTF8 {
            0xFFFD
        } else {
            u16::from(b'?')
        },
        code_page: page,
        code_page_name: [0; 260],
    };
    for (slot, unit) in info.code_page_name.iter_mut().zip(name.encode_utf16()) {
        *slot = unit;
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { out.write_unaligned(info) };
    TRUE
}

/// The pages the converters here take (no UTF-7, no UTF-16 pages).
pub extern "win64" fn IsValidCodePage(page: u32) -> BOOL {
    BOOL::from(matches!(page, 1252 | 437 | 65001))
}

/// No page here has lead bytes.
pub extern "win64" fn IsDBCSLeadByte(_byte: u8) -> BOOL {
    FALSE
}

/// See [`IsDBCSLeadByte`].
pub extern "win64" fn IsDBCSLeadByteEx(_page: u32, _byte: u8) -> BOOL {
    FALSE
}

// The locale.

const LOCALE_NAME: &str = "en-US";
const LCID_EN_US: u32 = 0x0409;
const LOCALE_NAME_SYSTEM_DEFAULT: &str = "!x-sys-default-locale";
const MUI_LANGUAGE_ID: u32 = 4;
const LOCALE_SPECIFICDATA: u32 = 2;

/// `en-US` and its NUL into the title's buffer: 6, or 0 when it is shorter.
fn locale_name(out: *mut u16, cch: i32) -> i32 {
    if cch < 6 || out.is_null() {
        return fail(ERROR_INSUFFICIENT_BUFFER, 0);
    }
    put(&wide_z(LOCALE_NAME), out, cch)
}

/// See [`locale_name`].
pub extern "win64" fn GetUserDefaultLocaleName(out: *mut u16, cch: i32) -> i32 {
    locale_name(out, cch)
}

/// See [`locale_name`].
pub extern "win64" fn GetSystemDefaultLocaleName(out: *mut u16, cch: i32) -> i32 {
    locale_name(out, cch)
}

/// English (United States).
pub extern "win64" fn GetUserDefaultLCID() -> u32 {
    LCID_EN_US
}

/// See [`GetUserDefaultLCID`].
pub extern "win64" fn GetSystemDefaultLCID() -> u32 {
    LCID_EN_US
}

/// See [`GetUserDefaultLCID`].
pub extern "win64" fn GetUserDefaultLangID() -> u16 {
    LCID_EN_US as u16
}

/// See [`GetUserDefaultLCID`].
pub extern "win64" fn GetSystemDefaultLangID() -> u16 {
    LCID_EN_US as u16
}

/// See [`GetUserDefaultLCID`].
pub extern "win64" fn GetUserDefaultUILanguage() -> u16 {
    LCID_EN_US as u16
}

/// See [`GetUserDefaultLCID`].
pub extern "win64" fn GetSystemDefaultUILanguage() -> u16 {
    LCID_EN_US as u16
}

/// See [`GetUserDefaultLCID`].
pub extern "win64" fn GetThreadUILanguage() -> u16 {
    LCID_EN_US as u16
}

/// See [`GetUserDefaultLCID`].
pub extern "win64" fn GetThreadLocale() -> u32 {
    LCID_EN_US
}

/// Accepted; the thread's locale stays English (United States).
pub extern "win64" fn SetThreadLocale(_lcid: u32) -> BOOL {
    TRUE
}

/// The language the thread keeps: English (United States).
pub extern "win64" fn SetThreadUILanguage(_language: u16) -> u16 {
    LCID_EN_US as u16
}

/// The one preferred language as a double-NUL list: its name, or with
/// `MUI_LANGUAGE_ID` its number in hex.
fn preferred_languages(flags: u32, count: *mut u32, buffer: *mut u16, size: *mut u32) -> BOOL {
    if count.is_null() || size.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let name = if flags & MUI_LANGUAGE_ID != 0 {
        "0409"
    } else {
        LOCALE_NAME
    };
    let mut list = wide_z(name);
    list.push(0);
    // SAFETY: checked non-null; the title's in-out size and its count.
    unsafe {
        let room = size.read() as usize;
        size.write(list.len() as u32);
        count.write(1);
        if buffer.is_null() || room == 0 {
            return TRUE;
        }
        if room < list.len() {
            return fail(ERROR_INSUFFICIENT_BUFFER, FALSE);
        }
        std::ptr::copy_nonoverlapping(list.as_ptr(), buffer, list.len());
    }
    TRUE
}

/// See [`preferred_languages`].
pub extern "win64" fn GetThreadPreferredUILanguages(
    flags: u32,
    count: *mut u32,
    buffer: *mut u16,
    size: *mut u32,
) -> BOOL {
    preferred_languages(flags, count, buffer, size)
}

/// See [`preferred_languages`].
pub extern "win64" fn GetUserPreferredUILanguages(
    flags: u32,
    count: *mut u32,
    buffer: *mut u16,
    size: *mut u32,
) -> BOOL {
    preferred_languages(flags, count, buffer, size)
}

/// A language of 2 or 3 ASCII letters, a region of 2 after a `-`, or none
/// (the invariant locale).
fn valid_locale_name(name: &str) -> bool {
    let letters = |s: &str, n: std::ops::RangeInclusive<usize>| {
        n.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphabetic())
    };
    match name.split_once('-') {
        _ if name.is_empty() => true,
        Some((language, region)) => letters(language, 2..=3) && letters(region, 2..=2),
        None => letters(name, 2..=3),
    }
}

/// See [`valid_locale_name`].
pub extern "win64" fn IsValidLocaleName(name: *const u16) -> BOOL {
    BOOL::from(wide(name).is_some_and(|n| valid_locale_name(&n)))
}

/// The name as given; `en-US` for none.
pub extern "win64" fn ResolveLocaleName(name: *const u16, out: *mut u16, cch: i32) -> i32 {
    let name = wide(name).filter(|n| !n.is_empty());
    let resolved = name.unwrap_or_else(|| LOCALE_NAME.to_owned());
    if out.is_null() {
        return wide_z(&resolved).len() as i32;
    }
    put(&wide_z(&resolved), out, cch)
}

/// English's names; 0 and `ERROR_INVALID_PARAMETER` for others.
pub extern "win64" fn LocaleNameToLCID(name: *const u16, _flags: u32) -> u32 {
    match wide(name).as_deref() {
        None | Some("en-US" | "en" | "" | LOCALE_NAME_SYSTEM_DEFAULT) => LCID_EN_US,
        Some(_) => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

/// English's numbers and the default ones (user, system, neutral);
/// 0 and `ERROR_INVALID_PARAMETER` for others.
pub extern "win64" fn LCIDToLocaleName(lcid: u32, out: *mut u16, cch: i32, _flags: u32) -> i32 {
    match lcid {
        0 | 0x0009 | 0x0400 | 0x0409 | 0x0800 => put(&wide_z(LOCALE_NAME), out, cch),
        _ => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

const LOCALE_NOUSEROVERRIDE: u32 = 0x8000_0000;
const LOCALE_USE_CP_ACP: u32 = 0x4000_0000;
const LOCALE_RETURN_NUMBER: u32 = 0x2000_0000;
/// English month names have no genitive form: the same names.
const LOCALE_RETURN_GENITIVE_NAMES: u32 = 0x1000_0000;
const LOCALE_ALLOW_NEUTRAL_NAMES: u32 = 0x0800_0000;

const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// What `en-US` answers for an `LCTYPE`, and whether it is a number
/// (`LOCALE_RETURN_NUMBER` takes those only; `LOCALE_ILANGUAGE`'s is hex).
fn locale_info(lctype: u32) -> Option<(String, bool)> {
    let number = |s: &str| Some((s.to_owned(), true));
    let text = |s: &str| Some((s.to_owned(), false));
    match lctype {
        0x2A..=0x30 => text(DAYS[(lctype - 0x2A) as usize]),
        0x31..=0x37 => text(&DAYS[(lctype - 0x31) as usize][..3]),
        0x38..=0x43 => text(MONTHS[(lctype - 0x38) as usize]),
        0x44..=0x4F => text(&MONTHS[(lctype - 0x44) as usize][..3]),
        0x60..=0x66 => text(&DAYS[(lctype - 0x60) as usize][..2]),
        0x1 | 0x9 => number("0409"),
        0x2 | 0x72 | 0x73 => text("English (United States)"),
        0x3 => text("ENU"),
        0x4 | 0x6F | 0x1001 => text("English"),
        0x5 | 0xA => number("1"),
        0x6 | 0x8 | 0x1002 => text("United States"),
        0x7 | 0x68 => text("USA"),
        0xB => number("437"),
        0xC => text(","),
        0xD => number("1"),
        0xE => text("."),
        0xF => text(","),
        0x10 | 0x18 => text("3;0"),
        0x11 => number("2"),
        0x12 => number("1"),
        0x13 => text("0123456789"),
        0x14 => text("$"),
        0x15 => text("USD"),
        0x16 => text("."),
        0x17 => text(","),
        0x19 | 0x1A => number("2"),
        0x1B | 0x74 | 0x75 => number("0"),
        0x1C => number("0"),
        0x1D => text("/"),
        0x1E => text(":"),
        0x1F => text("M/d/yyyy"),
        0x20 => text("dddd, MMMM d, yyyy"),
        0x21..=0x23 => number("0"),
        0x24 => number("1"),
        0x25..=0x27 => number("0"),
        0x28 => text("AM"),
        0x29 => text("PM"),
        0x50 => text(""),
        0x51 => text("-"),
        0x52 => number("3"),
        0x53 => number("0"),
        0x54 => number("1"),
        0x55 => number("0"),
        0x56 => number("1"),
        0x57 => number("0"),
        0x59 => text("en"),
        0x5A => text("US"),
        0x5B => number("244"),
        0x5C | 0x6E => text(LOCALE_NAME),
        0x5D => text("h:mm:ss"),
        0x67 => text("eng"),
        0x69 => text("NaN"),
        0x6A => text("Infinity"),
        0x6B => text("-Infinity"),
        0x6C => text("Latn;"),
        0x6D => text("en"),
        0x70 | 0x71 => number("0"),
        0x76 => text("%"),
        0x77 => text("\u{2030}"),
        0x78 => text("MMMM d"),
        0x79 => text("h:mm tt"),
        0x1003 => text("h:mm:ss tt"),
        0x1004 => number("1252"),
        0x1005 => number("0"),
        0x1006 => text("MMMM yyyy"),
        0x1007 | 0x1008 => text("US Dollar"),
        0x1009 => number("1"),
        0x100A => number("1"),
        0x100C => number("6"),
        0x100D => number("0"),
        0x100E | 0x100F => text(""),
        0x1010 => number("1"),
        0x1011 => number("10000"),
        0x1012 => number("037"),
        0x1014 => number("1"),
        _ => None,
    }
}

/// `GetLocaleInfo`'s body: the value as text and a NUL, or with
/// `LOCALE_RETURN_NUMBER` as a number (`units` code units of it).
fn locale_value(lctype: u32) -> Result<Result<String, u32>, u32> {
    let flags = LOCALE_NOUSEROVERRIDE
        | LOCALE_USE_CP_ACP
        | LOCALE_RETURN_NUMBER
        | LOCALE_ALLOW_NEUTRAL_NAMES
        | LOCALE_RETURN_GENITIVE_NAMES;
    let (value, numeric) = locale_info(lctype & !flags).ok_or(ERROR_INVALID_FLAGS)?;
    if lctype & LOCALE_RETURN_NUMBER == 0 {
        return Ok(Ok(value));
    }
    if !numeric {
        return Err(ERROR_INVALID_FLAGS);
    }
    let radix = if lctype & !flags == 1 || lctype & !flags == 9 {
        16
    } else {
        10
    };
    Ok(Err(u32::from_str_radix(&value, radix).unwrap_or(0)))
}

/// [`locale_value`] into a wide buffer.
fn locale_info_w(lctype: u32, out: *mut u16, cch: i32) -> i32 {
    match locale_value(lctype) {
        Ok(Ok(text)) => put(&wide_z(&text), out, cch),
        Ok(Err(number)) => put(&[number as u16, (number >> 16) as u16], out, cch),
        Err(code) => fail(code, 0),
    }
}

/// Whether a locale name is one this answers for (every valid one, with
/// `en-US`'s data; null is the user's).
fn known_locale(name: *const u16) -> bool {
    match wide(name) {
        None => true,
        Some(name) => name == LOCALE_NAME_SYSTEM_DEFAULT || valid_locale_name(&name),
    }
}

/// See [`locale_info`].
pub extern "win64" fn GetLocaleInfoEx(
    name: *const u16,
    lctype: u32,
    out: *mut u16,
    cch: i32,
) -> i32 {
    if !known_locale(name) {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    locale_info_w(lctype, out, cch)
}

/// See [`locale_info`]; every LCID answers as English (United States).
pub extern "win64" fn GetLocaleInfoW(_lcid: u32, lctype: u32, out: *mut u16, cch: i32) -> i32 {
    locale_info_w(lctype, out, cch)
}

/// See [`GetLocaleInfoW`]; a number takes 4 bytes.
pub extern "win64" fn GetLocaleInfoA(_lcid: u32, lctype: u32, out: *mut u8, cch: i32) -> i32 {
    match locale_value(lctype) {
        Ok(Ok(text)) => put(&narrow_z(&text), out, cch),
        Ok(Err(number)) => put(&number.to_le_bytes(), out, cch),
        Err(code) => fail(code, 0),
    }
}

/// A locale enumeration's callback.
type LocaleCallback = Option<extern "win64" fn(*const u16, u32, isize) -> BOOL>;

/// The one locale, `en-US`, to the callback.
pub extern "win64" fn EnumSystemLocalesEx(
    callback: LocaleCallback,
    _flags: u32,
    param: isize,
    _reserved: *mut c_void,
) -> BOOL {
    let Some(callback) = callback else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    let name = wide_z(LOCALE_NAME);
    callback(name.as_ptr(), LOCALE_SPECIFICDATA, param);
    TRUE
}

// Mapping, comparing and classifying strings.

const LCMAP_LOWERCASE: u32 = 0x100;
const LCMAP_UPPERCASE: u32 = 0x200;
const LCMAP_TITLECASE: u32 = LCMAP_LOWERCASE | LCMAP_UPPERCASE;
const LCMAP_SORTKEY: u32 = 0x400;
const LCMAP_BYTEREV: u32 = 0x800;
const LCMAP_HIRAGANA: u32 = 0x10_0000;
const LCMAP_KATAKANA: u32 = 0x20_0000;
const LCMAP_HALFWIDTH: u32 = 0x40_0000;
const LCMAP_FULLWIDTH: u32 = 0x80_0000;
const LCMAP_SIMPLIFIED_CHINESE: u32 = 0x200_0000;
const LCMAP_TRADITIONAL_CHINESE: u32 = 0x400_0000;
const NORM_IGNORECASE: u32 = 1;
const LINGUISTIC_IGNORECASE: u32 = 0x10;

const CSTR_LESS_THAN: i32 = 1;
const CSTR_EQUAL: i32 = 2;
const CSTR_GREATER_THAN: i32 = 3;

/// `c`'s simple upper-case mapping (one character for one), else `c`.
fn simple_upper(c: char) -> char {
    let mut mapped = c.to_uppercase();
    match (mapped.next(), mapped.next()) {
        (Some(u), None) if u.len_utf16() == c.len_utf16() => u,
        _ => c,
    }
}

/// `c`'s simple lower-case mapping, else `c`.
fn simple_lower(c: char) -> char {
    let mut mapped = c.to_lowercase();
    match (mapped.next(), mapped.next()) {
        (Some(l), None) if l.len_utf16() == c.len_utf16() => l,
        _ => c,
    }
}

/// Maps each character of `text` (a lone surrogate stays as it is).
/// Each word's first letter upper-cased and the rest lower-cased (a word
/// is a run of letters and digits).
fn title_case(text: &[u16]) -> Vec<u16> {
    let mut start = true;
    let mut out = Vec::with_capacity(text.len());
    for c in char::decode_utf16(text.iter().copied()) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        let mapped = if !c.is_alphanumeric() {
            start = true;
            c
        } else if start {
            start = false;
            simple_upper(c)
        } else {
            simple_lower(c)
        };
        out.extend(mapped.encode_utf16(&mut [0; 2]).iter().copied());
    }
    out
}

fn map_units(text: &[u16], map: impl Fn(char) -> char) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.len());
    for c in char::decode_utf16(text.iter().copied()) {
        match c {
            Ok(c) => {
                let mut buffer = [0u16; 2];
                out.extend_from_slice(map(c).encode_utf16(&mut buffer));
            }
            Err(e) => out.push(e.unpaired_surrogate()),
        }
    }
    out
}

/// Case-folded for comparison: simple upper case, as Windows' ordinal
/// ignore-case comparisons do.
fn fold(text: &[u16]) -> Vec<u16> {
    map_units(text, simple_upper)
}

/// The sort key of `text`: each code unit (folded with `NORM_IGNORECASE`)
/// as three bytes, `unit + 1` in base 255 with each digit plus 1 (so no
/// byte is 0 and keys order as their units), then a 0.
fn sort_key(text: &[u16], ignore_case: bool) -> Vec<u8> {
    let text = if ignore_case {
        fold(text)
    } else {
        text.to_vec()
    };
    let mut key = Vec::with_capacity(text.len() * 3 + 1);
    for unit in text {
        let value = u32::from(unit) + 1;
        key.extend([value / 255 / 255, value / 255 % 255, value % 255].map(|d| d as u8 + 1));
    }
    key.push(0);
    key
}

/// `LCMapString`'s body.
fn lc_map(flags: u32, src: *const u16, len: i32, out: *mut u16, cch: i32) -> i32 {
    let Some(text) = units(src, len, true).filter(|t| !t.is_empty()) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    let case = flags & (LCMAP_LOWERCASE | LCMAP_UPPERCASE);
    let others = LCMAP_BYTEREV
        | LCMAP_HIRAGANA
        | LCMAP_KATAKANA
        | LCMAP_HALFWIDTH
        | LCMAP_FULLWIDTH
        | LCMAP_SIMPLIFIED_CHINESE
        | LCMAP_TRADITIONAL_CHINESE;
    if flags & LCMAP_SORTKEY != 0 {
        if case != 0 || flags & others != 0 {
            return fail(ERROR_INVALID_FLAGS, 0);
        }
        // The key leaves the NUL out; `out` is bytes.
        let text = text
            .strip_suffix(&[0])
            .filter(|_| len == -1)
            .unwrap_or(text);
        let ignore = flags & (NORM_IGNORECASE | LINGUISTIC_IGNORECASE) != 0;
        return put(&sort_key(text, ignore), out.cast::<u8>(), cch);
    }
    if flags & (case | others) == 0 {
        return fail(ERROR_INVALID_FLAGS, 0);
    }
    let mut mapped = match case {
        LCMAP_UPPERCASE => map_units(text, simple_upper),
        LCMAP_LOWERCASE => map_units(text, simple_lower),
        // `LCMAP_TITLECASE` (both bits): each word's first letter up, the
        // rest down.
        LCMAP_TITLECASE => title_case(text),
        // Kana, widths and Chinese scripts: no tables, kept as they are.
        _ => text.to_vec(),
    };
    if flags & LCMAP_BYTEREV != 0 {
        mapped.iter_mut().for_each(|u| *u = u.swap_bytes());
    }
    put(&mapped, out, cch)
}

/// See [`lc_map`]: case (simple mappings; `LCMAP_LINGUISTIC_CASING` is
/// ignored), sort keys and byte order.
pub extern "win64" fn LCMapStringEx(
    _name: *const u16,
    flags: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
    cch: i32,
    _version: *mut c_void,
    _reserved: *mut c_void,
    _sort_handle: isize,
) -> i32 {
    lc_map(flags, src, len, out, cch)
}

/// See [`lc_map`].
pub extern "win64" fn LCMapStringW(
    _lcid: u32,
    flags: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
    cch: i32,
) -> i32 {
    lc_map(flags, src, len, out, cch)
}

/// Compares by code unit, folded when `ignore_case`.
fn compare(a: &[u16], b: &[u16], ignore_case: bool) -> i32 {
    let order = if ignore_case {
        fold(a).cmp(&fold(b))
    } else {
        a.cmp(b)
    };
    match order {
        std::cmp::Ordering::Less => CSTR_LESS_THAN,
        std::cmp::Ordering::Equal => CSTR_EQUAL,
        std::cmp::Ordering::Greater => CSTR_GREATER_THAN,
    }
}

/// `CompareString`'s body.
fn compare_strings(flags: u32, a: *const u16, la: i32, b: *const u16, lb: i32) -> i32 {
    let (Some(a), Some(b)) = (units(a, la, false), units(b, lb, false)) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    compare(a, b, flags & (NORM_IGNORECASE | LINGUISTIC_IGNORECASE) != 0)
}

/// See [`compare`]: `CSTR_LESS_THAN`, `CSTR_EQUAL` or `CSTR_GREATER_THAN`.
pub extern "win64" fn CompareStringEx(
    _name: *const u16,
    flags: u32,
    a: *const u16,
    la: i32,
    b: *const u16,
    lb: i32,
    _version: *mut c_void,
    _reserved: *mut c_void,
    _param: isize,
) -> i32 {
    compare_strings(flags, a, la, b, lb)
}

/// See [`compare`].
pub extern "win64" fn CompareStringW(
    _lcid: u32,
    flags: u32,
    a: *const u16,
    la: i32,
    b: *const u16,
    lb: i32,
) -> i32 {
    compare_strings(flags, a, la, b, lb)
}

/// See [`compare`].
pub extern "win64" fn CompareStringOrdinal(
    a: *const u16,
    la: i32,
    b: *const u16,
    lb: i32,
    ignore_case: BOOL,
) -> i32 {
    let flags = if ignore_case != 0 { NORM_IGNORECASE } else { 0 };
    compare_strings(flags, a, la, b, lb)
}

const CT_CTYPE1: u32 = 1;
const CT_CTYPE2: u32 = 2;
const CT_CTYPE3: u32 = 4;

const C1_UPPER: u16 = 1;
const C1_LOWER: u16 = 2;
const C1_DIGIT: u16 = 4;
const C1_SPACE: u16 = 8;
const C1_PUNCT: u16 = 0x10;
const C1_CNTRL: u16 = 0x20;
const C1_BLANK: u16 = 0x40;
const C1_XDIGIT: u16 = 0x80;
const C1_ALPHA: u16 = 0x100;
const C1_DEFINED: u16 = 0x200;

const C2_LEFTTORIGHT: u16 = 1;
const C2_EUROPENUMBER: u16 = 3;
const C2_WHITESPACE: u16 = 8;
const C2_OTHERNEUTRAL: u16 = 11;

const C3_NONSPACING: u16 = 1;
const C3_ALPHA: u16 = 0x8000;

/// A combining mark of the common blocks.
fn combining(c: char) -> bool {
    matches!(
        c,
        '\u{300}'..='\u{36F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// A character's `CT_CTYPE1` class.
fn ctype1(c: char) -> u16 {
    let mut class = C1_DEFINED;
    if c.is_uppercase() {
        class |= C1_UPPER;
    }
    if c.is_lowercase() {
        class |= C1_LOWER;
    }
    if c.is_ascii_digit() {
        class |= C1_DIGIT;
    }
    if c.is_whitespace() {
        class |= C1_SPACE;
    }
    if c == ' ' || c == '\t' || (c.is_whitespace() && !c.is_control() && c > '\u{FF}') {
        class |= C1_BLANK;
    }
    if c.is_control() {
        class |= C1_CNTRL;
    }
    if c.is_ascii_hexdigit() {
        class |= C1_XDIGIT;
    }
    if c.is_alphabetic() {
        class |= C1_ALPHA;
    }
    let punct = if c.is_ascii() {
        c.is_ascii_punctuation()
    } else {
        !(c.is_alphanumeric() || c.is_whitespace() || c.is_control() || combining(c))
    };
    if punct {
        class |= C1_PUNCT;
    }
    class
}

/// A character's `CT_CTYPE2` direction.
fn ctype2(c: char) -> u16 {
    if c.is_ascii_digit() {
        C2_EUROPENUMBER
    } else if c.is_alphabetic() {
        C2_LEFTTORIGHT
    } else if c.is_whitespace() {
        C2_WHITESPACE
    } else {
        C2_OTHERNEUTRAL
    }
}

/// A character's `CT_CTYPE3` class.
fn ctype3(c: char) -> u16 {
    if combining(c) {
        C3_NONSPACING
    } else if c.is_alphabetic() {
        C3_ALPHA
    } else {
        0
    }
}

/// The class of each code unit of `text` (`len` -1: with the NUL); a
/// surrogate has none.
pub extern "win64" fn GetStringTypeW(
    info_type: u32,
    text: *const u16,
    len: i32,
    out: *mut u16,
) -> BOOL {
    let class = match info_type {
        CT_CTYPE1 => ctype1,
        CT_CTYPE2 => ctype2,
        CT_CTYPE3 => ctype3,
        _ => return fail(ERROR_INVALID_FLAGS, FALSE),
    };
    let Some(text) = units(text, len, true).filter(|_| !out.is_null()) else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    for (i, &unit) in text.iter().enumerate() {
        let value = char::from_u32(u32::from(unit)).map_or(0, class);
        // SAFETY: the title's array holds one class per unit.
        unsafe { out.add(i).write_unaligned(value) };
    }
    TRUE
}

/// See [`GetStringTypeW`].
pub extern "win64" fn GetStringTypeExW(
    _locale: u32,
    info_type: u32,
    text: *const u16,
    len: i32,
    out: *mut u16,
) -> BOOL {
    GetStringTypeW(info_type, text, len, out)
}

/// The text as it is (no normalization tables); its length.
pub extern "win64" fn FoldStringW(
    flags: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
    cch: i32,
) -> i32 {
    if flags == 0 {
        return fail(ERROR_INVALID_FLAGS, 0);
    }
    // In place is refused, as on Windows (the copy would overlap).
    if !out.is_null() && std::ptr::eq(src, out) {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    let Some(text) = units(src, len, true).filter(|t| !t.is_empty()) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    put(text, out, cch)
}

// The `lstr` functions.

/// The units of a NUL-terminated string; none for null.
fn lstr<'a, T: Unit>(text: *const T) -> &'a [T] {
    if text.is_null() {
        return &[];
    }
    // SAFETY: the title passes a NUL-terminated string.
    unsafe { terminated(text) }
}

/// -1, 0 or 1, as `lstrcmp` answers.
fn lstr_order(a: &[u16], b: &[u16], ignore_case: bool) -> i32 {
    compare(a, b, ignore_case) - 2
}

/// The units of the string at `text` up to its NUL or `most` units,
/// whichever comes first (the title's source may have no NUL within reach).
fn lstr_bounded<'a, T: Unit>(text: *const T, most: usize) -> &'a [T] {
    if text.is_null() {
        return &[];
    }
    // SAFETY: the title's string, read one unit at a time up to `most`.
    let n = (0..most)
        .take_while(|&i| unsafe { text.add(i).read() } != T::default())
        .count();
    // SAFETY: `n` units were just read there.
    unsafe { std::slice::from_raw_parts(text, n) }
}

/// Copies `src` and a NUL to `dst`, `max` units in all at most (and reads
/// no more of `src` than fit).
fn lstr_copy<T: Unit>(dst: *mut T, src: *const T, max: usize) -> *mut T {
    if dst.is_null() || max == 0 {
        return dst;
    }
    let text = if max == usize::MAX {
        lstr(src)
    } else {
        lstr_bounded(src, max - 1)
    };
    let n = text.len().min(max - 1);
    // SAFETY: the title's buffer holds `max` units (all of `src` and its
    // NUL for the unbounded copies).
    unsafe {
        std::ptr::copy(text.as_ptr(), dst, n);
        dst.add(n).write(T::default());
    }
    dst
}

pub extern "win64" fn lstrlenA(text: *const u8) -> i32 {
    lstr(text).len() as i32
}

pub extern "win64" fn lstrlenW(text: *const u16) -> i32 {
    lstr(text).len() as i32
}

/// Latin-1 bytes as code units.
fn widen(text: &[u8]) -> Vec<u16> {
    text.iter().map(|&b| u16::from(b)).collect()
}

pub extern "win64" fn lstrcmpA(a: *const u8, b: *const u8) -> i32 {
    lstr_order(&widen(lstr(a)), &widen(lstr(b)), false)
}

pub extern "win64" fn lstrcmpW(a: *const u16, b: *const u16) -> i32 {
    lstr_order(lstr(a), lstr(b), false)
}

pub extern "win64" fn lstrcmpiA(a: *const u8, b: *const u8) -> i32 {
    lstr_order(&widen(lstr(a)), &widen(lstr(b)), true)
}

pub extern "win64" fn lstrcmpiW(a: *const u16, b: *const u16) -> i32 {
    lstr_order(lstr(a), lstr(b), true)
}

pub extern "win64" fn lstrcpyA(dst: *mut u8, src: *const u8) -> *mut u8 {
    lstr_copy(dst, src, usize::MAX)
}

pub extern "win64" fn lstrcpyW(dst: *mut u16, src: *const u16) -> *mut u16 {
    lstr_copy(dst, src, usize::MAX)
}

pub extern "win64" fn lstrcpynA(dst: *mut u8, src: *const u8, max: i32) -> *mut u8 {
    lstr_copy(dst, src, max.max(0) as usize)
}

pub extern "win64" fn lstrcpynW(dst: *mut u16, src: *const u16, max: i32) -> *mut u16 {
    lstr_copy(dst, src, max.max(0) as usize)
}

pub extern "win64" fn lstrcatA(dst: *mut u8, src: *const u8) -> *mut u8 {
    if !dst.is_null() {
        // SAFETY: the title's buffer holds both strings and a NUL.
        lstr_copy(unsafe { dst.add(lstr(dst).len()) }, src, usize::MAX);
    }
    dst
}

pub extern "win64" fn lstrcatW(dst: *mut u16, src: *const u16) -> *mut u16 {
    if !dst.is_null() {
        // SAFETY: the title's buffer holds both strings and a NUL.
        lstr_copy(unsafe { dst.add(lstr(dst).len()) }, src, usize::MAX);
    }
    dst
}

// Messages.

const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x100;
const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x200;
const FORMAT_MESSAGE_FROM_STRING: u32 = 0x400;
const FORMAT_MESSAGE_FROM_HMODULE: u32 = 0x800;
const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
const FORMAT_MESSAGE_ARGUMENT_ARRAY: u32 = 0x2000;
const FORMAT_MESSAGE_MAX_WIDTH_MASK: u32 = 0xFF;

/// The texts of the common Win32 errors, as Windows (English) gives them.
fn system_message(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "The operation completed successfully.",
        1 => "Incorrect function.",
        2 => "The system cannot find the file specified.",
        3 => "The system cannot find the path specified.",
        4 => "The system cannot open the file.",
        5 => "Access is denied.",
        6 => "The handle is invalid.",
        8 => "Not enough memory resources are available to process this command.",
        13 => "The data is invalid.",
        14 => "Not enough storage is available to complete this operation.",
        15 => "The system cannot find the drive specified.",
        18 => "There are no more files.",
        32 => "The process cannot access the file because it is being used by another process.",
        33 => {
            "The process cannot access the file because another process has locked a portion \
             of the file."
        }
        38 => "Reached the end of the file.",
        50 => "The request is not supported.",
        80 => "The file exists.",
        87 => "The parameter is incorrect.",
        109 => "The pipe has been ended.",
        112 => "There is not enough space on the disk.",
        120 => "This function is not supported on this system.",
        122 => "The data area passed to a system call is too small.",
        123 => "The filename, directory name, or volume label syntax is incorrect.",
        145 => "The directory is not empty.",
        183 => "Cannot create a file when that file already exists.",
        206 => "The filename or extension is too long.",
        234 => "More data is available.",
        258 => "The wait operation timed out.",
        259 => "No more data is available.",
        317 => {
            "The system cannot find message text for message number 0x%1 in the message file \
             for %2."
        }
        995 => {
            "The I/O operation has been aborted because of either a thread exit or an \
             application request."
        }
        996 => "Overlapped I/O event is not in a signaled state.",
        997 => "Overlapped I/O operation is in progress.",
        998 => "Invalid access to memory location.",
        1004 => "Invalid flags.",
        1113 => "No mapping for the Unicode character exists in the target multi-byte code page.",
        1168 => "Element not found.",
        1223 => "The operation was canceled by the user.",
        1314 => "A required privilege is not held by the client.",
        _ => return None,
    })
}

/// Where the inserts' values are: an array of 8-byte slots (a Win64
/// `va_list` holds them the same way).
struct Arguments {
    slots: *const u64,
    /// Strings are wide unless a spec says otherwise (the W form).
    wide: bool,
}

impl Arguments {
    /// Insert `n`'s slot (from 1).
    fn get(&self, n: usize) -> Option<u64> {
        if self.slots.is_null() || n == 0 {
            return None;
        }
        // SAFETY: the title's arguments, one slot per insert it uses.
        Some(unsafe { self.slots.add(n - 1).read_unaligned() })
    }

    /// A string argument, wide or narrow.
    fn string(&self, pointer: u64, wide_string: bool) -> String {
        if pointer == 0 {
            return "(null)".to_owned();
        }
        if wide_string {
            wide(pointer as *const u16).unwrap_or_default()
        } else {
            narrow(pointer as *const u8).unwrap_or_default()
        }
    }
}

/// One printf-style conversion of an insert (`!...!`).
#[derive(Debug, Default)]
struct Conversion {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alternate: bool,
    width: Option<usize>,
    precision: Option<usize>,
    /// `h` (16 bits; narrow), `l` (32 bits; wide), `ll` or `I64` (64 bits),
    /// `I` (pointer-sized), `I32`; none.
    size: &'static str,
    kind: char,
}

/// Parses a conversion; `*` widths and precisions take the next inserts,
/// counted in `next`.
fn parse_conversion(spec: &str, args: &Arguments, next: &mut usize) -> Option<Conversion> {
    let mut c = Conversion::default();
    let mut rest = spec;
    while let Some(flag) = rest.chars().next() {
        match flag {
            '-' => c.left = true,
            '0' => c.zero = true,
            '+' => c.plus = true,
            ' ' => c.space = true,
            '#' => c.alternate = true,
            _ => break,
        }
        rest = &rest[1..];
    }
    let mut number = |rest: &mut &str| -> Option<usize> {
        if let Some(after) = rest.strip_prefix('*') {
            *rest = after;
            let value = args.get(*next)? as u32 as usize;
            *next += 1;
            return Some(value);
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let value = rest[..digits].parse().ok();
        *rest = &rest[digits..];
        value
    };
    c.width = number(&mut rest);
    if let Some(after) = rest.strip_prefix('.') {
        rest = after;
        c.precision = Some(number(&mut rest).unwrap_or(0));
    }
    for size in ["I64", "I32", "ll", "h", "l", "I", "w"] {
        if let Some(after) = rest.strip_prefix(size) {
            c.size = if size == "w" { "l" } else { size };
            rest = after;
            break;
        }
    }
    let mut chars = rest.chars();
    c.kind = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// An insert's value as text, as its conversion asks.
fn format_insert(c: &Conversion, value: u64, args: &Arguments) -> String {
    let (wide_string, wide_char) = match (c.size, c.kind) {
        ("h", _) => (false, false),
        ("l", _) => (true, true),
        (_, 'S' | 'C') => (!args.wide, !args.wide),
        _ => (args.wide, args.wide),
    };
    let bits = match c.size {
        "h" => 16,
        "ll" | "I64" | "I" => 64,
        _ => 32,
    };
    let unsigned = if bits == 64 {
        value
    } else {
        value & ((1u64 << bits) - 1)
    };
    let (sign, digits, prefix) = match c.kind {
        'd' | 'i' => {
            let signed = match bits {
                16 => i64::from(value as i16),
                32 => i64::from(value as i32),
                _ => value as i64,
            };
            let sign = if signed < 0 {
                "-"
            } else if c.plus {
                "+"
            } else if c.space {
                " "
            } else {
                ""
            };
            (sign, signed.unsigned_abs().to_string(), "")
        }
        'u' => ("", unsigned.to_string(), ""),
        'x' => (
            "",
            format!("{unsigned:x}"),
            if c.alternate { "0x" } else { "" },
        ),
        'X' => (
            "",
            format!("{unsigned:X}"),
            if c.alternate { "0X" } else { "" },
        ),
        'o' => (
            "",
            format!("{unsigned:o}"),
            if c.alternate { "0" } else { "" },
        ),
        'p' => ("", format!("{value:016X}"), ""),
        'c' | 'C' => {
            let unit = if wide_char {
                value as u16
            } else {
                u16::from(value as u8)
            };
            let text = String::from_utf16_lossy(&[unit]);
            return pad(c, "", "", text);
        }
        's' | 'S' => {
            let mut text = args.string(value, wide_string);
            if let Some(max) = c.precision {
                text = text.chars().take(max).collect();
            }
            return pad(c, "", "", text);
        }
        // No floating point (nor does Windows' take any): the spec as it is.
        other => return format!("!{other}!"),
    };
    let digits = match c.precision {
        Some(0) if unsigned == 0 && c.kind != 'p' => String::new(),
        Some(min) if digits.len() < min => format!("{}{digits}", "0".repeat(min - digits.len())),
        _ => digits,
    };
    pad(c, sign, prefix, digits)
}

/// `sign`, `prefix` and `body` padded to the conversion's width.
fn pad(c: &Conversion, sign: &str, prefix: &str, body: String) -> String {
    let len = sign.len() + prefix.len() + body.chars().count();
    let fill = c.width.unwrap_or(0).saturating_sub(len);
    if c.left {
        format!("{sign}{prefix}{body}{}", " ".repeat(fill))
    } else if c.zero && c.precision.is_none() && !matches!(c.kind, 's' | 'S' | 'c' | 'C') {
        format!("{sign}{prefix}{}{body}", "0".repeat(fill))
    } else {
        format!("{}{sign}{prefix}{body}", " ".repeat(fill))
    }
}

/// Expands a message's escapes and inserts. Regular line breaks of the
/// text become spaces when the output has a line width (`width` not 0);
/// `%n` always breaks the line.
fn format_message(
    text: &str,
    args: Option<&Arguments>,
    ignore_inserts: bool,
    width: u32,
) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' if width != 0 => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '%' => match chars.next() {
                None => break,
                Some('0') => return Some(out),
                Some('n') => out.push_str("\r\n"),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(digit @ '1'..='9') => {
                    let mut number = digit.to_digit(10)? as usize;
                    if let Some(second) = chars.peek().and_then(|c| c.to_digit(10)) {
                        number = number * 10 + second as usize;
                        chars.next();
                    }
                    let mut spec = None;
                    if chars.peek() == Some(&'!') {
                        chars.next();
                        let inner: String = chars.by_ref().take_while(|&c| c != '!').collect();
                        spec = Some(inner);
                    }
                    if ignore_inserts {
                        out.push('%');
                        out.push_str(&number.to_string());
                        if let Some(spec) = spec {
                            out.push_str(&format!("!{spec}!"));
                        }
                        continue;
                    }
                    let args = args?;
                    let mut next = number;
                    let spec = spec.unwrap_or_else(|| "s".to_owned());
                    let conversion = parse_conversion(&spec, args, &mut next)?;
                    let value = args.get(next)?;
                    out.push_str(&format_insert(&conversion, value, args));
                }
                // `%%`, `%.`, `%!`, `% ` and the like: the character.
                Some(other) => out.push(other),
            },
            c => out.push(c),
        }
    }
    Some(match width {
        0 | FORMAT_MESSAGE_MAX_WIDTH_MASK => out,
        width => wrap(&out, width as usize),
    })
}

/// Breaks the lines of `text` at the last space before `width` characters
/// (or at `width` where there is none).
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.split("\r\n").enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        let mut rest: Vec<char> = line.chars().collect();
        while rest.len() > width {
            let cut = rest[..=width].iter().rposition(|&c| c == ' ');
            let (head, skip) = match cut {
                Some(at) if at > 0 => (at, 1),
                _ => (width, 0),
            };
            out.extend(&rest[..head]);
            out.push_str("\r\n");
            rest.drain(..head + skip);
        }
        out.extend(&rest);
    }
    out
}

/// `FormatMessage`'s body: the message as text, or the error.
fn message(
    flags: u32,
    source: Option<String>,
    id: u32,
    args: *const c_void,
    wide: bool,
) -> Result<String, u32> {
    let text = if flags & FORMAT_MESSAGE_FROM_STRING != 0 {
        source.ok_or(ERROR_INVALID_PARAMETER)?
    } else if flags & (FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_FROM_HMODULE) != 0 {
        // No module message tables: a module's ids are looked up as the
        // system's.
        let text = system_message(id).ok_or(ERROR_MR_MID_NOT_FOUND)?;
        format!("{text}\r\n")
    } else {
        return Err(ERROR_INVALID_PARAMETER);
    };
    let ignore = flags & FORMAT_MESSAGE_IGNORE_INSERTS != 0;
    let slots = if ignore || args.is_null() || flags & FORMAT_MESSAGE_ARGUMENT_ARRAY != 0 {
        // Ignored inserts read no arguments at all, as on Windows.
        if ignore {
            std::ptr::null()
        } else {
            args.cast::<u64>()
        }
    } else {
        // SAFETY: a `va_list *`: a pointer to the list's first slot.
        unsafe { args.cast::<*const u64>().read_unaligned() }
    };
    let arguments = Arguments { slots, wide };
    let width = flags & FORMAT_MESSAGE_MAX_WIDTH_MASK;
    format_message(&text, Some(&arguments), ignore, width).ok_or(ERROR_INVALID_PARAMETER)
}

/// Writes a message to the title's buffer (or one `LocalAlloc`ed for it,
/// at least `size` units); the units written without the NUL.
fn deliver<T: Unit>(flags: u32, text: Vec<T>, out: *mut c_void, size: u32) -> u32 {
    let len = text.len();
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    let target = if flags & FORMAT_MESSAGE_ALLOCATE_BUFFER != 0 {
        let units = (len + 1).max(size as usize);
        let block = LocalAlloc(0, units * std::mem::size_of::<T>()).cast::<T>();
        if block.is_null() {
            return fail(super::handles::ERROR_NOT_ENOUGH_MEMORY, 0);
        }
        // SAFETY: the title's place for the buffer's address.
        unsafe { out.cast::<*mut T>().write_unaligned(block) };
        block
    } else {
        if (size as usize) < len + 1 {
            return fail(ERROR_INSUFFICIENT_BUFFER, 0);
        }
        out.cast::<T>()
    };
    // SAFETY: the buffer holds the message and its NUL (checked or made so).
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), target, len);
        target.add(len).write(T::default());
    }
    len as u32
}

/// A message from a format string or the system's texts, its inserts
/// filled in; see [`format_message`].
pub extern "win64" fn FormatMessageW(
    flags: u32,
    source: *const c_void,
    id: u32,
    _language: u32,
    out: *mut u16,
    size: u32,
    args: *const c_void,
) -> u32 {
    let source = (flags & FORMAT_MESSAGE_FROM_STRING != 0)
        .then(|| wide(source.cast()))
        .flatten();
    match message(flags, source, id, args, true) {
        Ok(text) => deliver(flags, text.encode_utf16().collect(), out.cast(), size),
        Err(code) => fail(code, 0),
    }
}

/// See [`FormatMessageW`]; strings are narrow (the ANSI page).
pub extern "win64" fn FormatMessageA(
    flags: u32,
    source: *const c_void,
    id: u32,
    _language: u32,
    out: *mut u8,
    size: u32,
    args: *const c_void,
) -> u32 {
    let source = (flags & FORMAT_MESSAGE_FROM_STRING != 0)
        .then(|| narrow(source.cast()))
        .flatten();
    match message(flags, source, id, args, false) {
        Ok(text) => deliver(flags, to_latin1(&text), out.cast(), size),
        Err(code) => fail(code, 0),
    }
}

// Dates and times.

const DATE_SHORTDATE: u32 = 1;
const DATE_LONGDATE: u32 = 2;
const DATE_USE_ALT_CALENDAR: u32 = 4;
const DATE_YEARMONTH: u32 = 8;
const DATE_LTRREADING: u32 = 0x10;
const DATE_RTLREADING: u32 = 0x20;
const DATE_AUTOLAYOUT: u32 = 0x40;
const DATE_MONTHDAY: u32 = 0x80;
const TIME_NOMINUTESORSECONDS: u32 = 1;
const TIME_NOSECONDS: u32 = 2;
const TIME_NOTIMEMARKER: u32 = 4;
const TIME_FORCE24HOURFORMAT: u32 = 8;

/// A piece of a date or time picture.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// A run of one picture letter: the letter and how many.
    Field(char, usize),
    Text(String),
}

/// The pieces of a picture: runs of the letters `fields` takes, quoted
/// text (`''` a quote), and other characters as they are.
fn pieces(picture: &str, fields: &str) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut text = String::new();
    let chars: Vec<char> = picture.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if fields.contains(c) {
            let n = chars[i..].iter().take_while(|&&x| x == c).count();
            if !text.is_empty() {
                out.push(Piece::Text(std::mem::take(&mut text)));
            }
            out.push(Piece::Field(c, n));
            i += n;
        } else if c == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        text.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
        } else {
            text.push(c);
            i += 1;
        }
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    out
}

/// A date's picture fields: `d`, `M`, `y`, `g`.
fn date_text(st: &SYSTEMTIME, picture: &str) -> String {
    let mut out = String::new();
    for piece in pieces(picture, "dMyg") {
        match piece {
            Piece::Text(text) => out.push_str(&text),
            Piece::Field('d', 1) => out.push_str(&st.day.to_string()),
            Piece::Field('d', 2) => out.push_str(&format!("{:02}", st.day)),
            Piece::Field('d', 3) => out.push_str(&weekday(st)[..3]),
            Piece::Field('d', _) => out.push_str(weekday(st)),
            Piece::Field('M', 1) => out.push_str(&st.month.to_string()),
            Piece::Field('M', 2) => out.push_str(&format!("{:02}", st.month)),
            Piece::Field('M', 3) => out.push_str(&MONTHS[usize::from(st.month - 1)][..3]),
            Piece::Field('M', _) => out.push_str(MONTHS[usize::from(st.month - 1)]),
            Piece::Field('y', 1) => out.push_str(&(st.year % 100).to_string()),
            Piece::Field('y', 2) => out.push_str(&format!("{:02}", st.year % 100)),
            Piece::Field('y', _) => out.push_str(&st.year.to_string()),
            Piece::Field(_, _) => out.push_str("A.D."),
        }
    }
    out
}

/// The day of the week's name (`day_of_week` 0 is Sunday).
fn weekday(st: &SYSTEMTIME) -> &'static str {
    DAYS[(usize::from(st.day_of_week) + 6) % 7]
}

/// A time's picture fields: `h`, `H`, `m`, `s`, `t`; `flags` drop the
/// minutes, seconds or marker (with the text before each) or force 24
/// hours.
fn time_text(st: &SYSTEMTIME, picture: &str, flags: u32) -> String {
    let mut list = pieces(picture, "hHmst");
    let drop = |c: char| match c {
        'm' => flags & TIME_NOMINUTESORSECONDS != 0,
        's' => flags & (TIME_NOMINUTESORSECONDS | TIME_NOSECONDS) != 0,
        't' => flags & TIME_NOTIMEMARKER != 0,
        _ => false,
    };
    let mut i = 0;
    while i < list.len() {
        match list[i] {
            Piece::Field(c, _) if drop(c) => {
                list.remove(i);
                if i > 0 && matches!(list[i - 1], Piece::Text(_)) {
                    list.remove(i - 1);
                    i -= 1;
                } else if i < list.len() && matches!(list[i], Piece::Text(_)) {
                    list.remove(i);
                }
            }
            _ => i += 1,
        }
    }
    let marker = if st.hour < 12 { "AM" } else { "PM" };
    let twelve = match st.hour % 12 {
        0 => 12,
        h => h,
    };
    let mut out = String::new();
    for piece in list {
        match piece {
            Piece::Text(text) => out.push_str(&text),
            Piece::Field('h', n) if flags & TIME_FORCE24HOURFORMAT != 0 => {
                out.push_str(&number(st.hour, n));
            }
            Piece::Field('h', n) => out.push_str(&number(twelve, n)),
            Piece::Field('H', n) => out.push_str(&number(st.hour, n)),
            Piece::Field('m', n) => out.push_str(&number(st.minute, n)),
            Piece::Field('s', n) => out.push_str(&number(st.second, n)),
            Piece::Field(_, 1) => out.push_str(&marker[..1]),
            Piece::Field(_, _) => out.push_str(marker),
        }
    }
    out
}

/// `value` with a leading zero for a field of two letters.
fn number(value: u16, letters: usize) -> String {
    if letters >= 2 {
        format!("{value:02}")
    } else {
        value.to_string()
    }
}

/// The title's time, or now (UTC); its date checked (with the day of the
/// week worked out) or its time of day.
fn when(time: *const SYSTEMTIME, date: bool) -> Result<SYSTEMTIME, u32> {
    let mut st = SYSTEMTIME::default();
    if time.is_null() {
        GetSystemTime(&mut st);
        return Ok(st);
    }
    // SAFETY: checked non-null; the title's time.
    st = unsafe { time.read_unaligned() };
    if date {
        let day = SYSTEMTIME {
            hour: 0,
            minute: 0,
            second: 0,
            milliseconds: 0,
            ..st
        };
        let mut ft = FILETIME::default();
        let mut back = SYSTEMTIME::default();
        if SystemTimeToFileTime(&day, &mut ft) == FALSE
            || FileTimeToSystemTime(&ft, &mut back) == FALSE
        {
            return Err(ERROR_INVALID_PARAMETER);
        }
        st.day_of_week = back.day_of_week;
    } else if st.hour > 23 || st.minute > 59 || st.second > 59 || st.milliseconds > 999 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(st)
}

/// `GetDateFormat`'s body: the date as text.
fn date_format(flags: u32, time: *const SYSTEMTIME, format: Option<String>) -> Result<String, u32> {
    let pictures = DATE_SHORTDATE | DATE_LONGDATE | DATE_YEARMONTH | DATE_MONTHDAY;
    let known = pictures
        | DATE_USE_ALT_CALENDAR
        | DATE_LTRREADING
        | DATE_RTLREADING
        | DATE_AUTOLAYOUT
        | LOCALE_NOUSEROVERRIDE;
    let picture_flags = flags & pictures;
    if flags & !known != 0
        || picture_flags.count_ones() > 1
        || (format.is_some() && picture_flags != 0)
    {
        return Err(ERROR_INVALID_FLAGS);
    }
    let st = when(time, true)?;
    let picture = format.unwrap_or_else(|| {
        match picture_flags {
            DATE_LONGDATE => "dddd, MMMM d, yyyy",
            DATE_YEARMONTH => "MMMM yyyy",
            DATE_MONTHDAY => "MMMM d",
            _ => "M/d/yyyy",
        }
        .to_owned()
    });
    Ok(date_text(&st, &picture))
}

/// `GetTimeFormat`'s body: the time as text.
fn time_format(flags: u32, time: *const SYSTEMTIME, format: Option<String>) -> Result<String, u32> {
    let known = TIME_NOMINUTESORSECONDS
        | TIME_NOSECONDS
        | TIME_NOTIMEMARKER
        | TIME_FORCE24HOURFORMAT
        | LOCALE_NOUSEROVERRIDE;
    if flags & !known != 0 {
        return Err(ERROR_INVALID_FLAGS);
    }
    let st = when(time, false)?;
    let picture = format.unwrap_or_else(|| {
        if flags & TIME_FORCE24HOURFORMAT != 0 {
            "HH:mm:ss"
        } else {
            "h:mm:ss tt"
        }
        .to_owned()
    });
    Ok(time_text(&st, &picture, flags))
}

/// A formatted date or time into a wide buffer, with its NUL.
fn put_wide(text: Result<String, u32>, out: *mut u16, cch: i32) -> i32 {
    match text {
        Ok(text) => put(&wide_z(&text), out, cch),
        Err(code) => fail(code, 0),
    }
}

/// See [`put_wide`]: narrow.
fn put_narrow(text: Result<String, u32>, out: *mut u8, cch: i32) -> i32 {
    match text {
        Ok(text) => put(&narrow_z(&text), out, cch),
        Err(code) => fail(code, 0),
    }
}

/// A date in a picture (`format`) or the locale's (`flags`); the units
/// written with the NUL.
pub extern "win64" fn GetDateFormatW(
    _lcid: u32,
    flags: u32,
    time: *const SYSTEMTIME,
    format: *const u16,
    out: *mut u16,
    cch: i32,
) -> i32 {
    put_wide(date_format(flags, time, wide(format)), out, cch)
}

/// See [`GetDateFormatW`].
pub extern "win64" fn GetDateFormatEx(
    _name: *const u16,
    flags: u32,
    time: *const SYSTEMTIME,
    format: *const u16,
    out: *mut u16,
    cch: i32,
    _calendar: *const u16,
) -> i32 {
    put_wide(date_format(flags, time, wide(format)), out, cch)
}

/// See [`GetDateFormatW`].
pub extern "win64" fn GetDateFormatA(
    _lcid: u32,
    flags: u32,
    time: *const SYSTEMTIME,
    format: *const u8,
    out: *mut u8,
    cch: i32,
) -> i32 {
    put_narrow(date_format(flags, time, narrow(format)), out, cch)
}

/// A time in a picture (`format`) or the locale's; `flags` drop parts of
/// either.
pub extern "win64" fn GetTimeFormatW(
    _lcid: u32,
    flags: u32,
    time: *const SYSTEMTIME,
    format: *const u16,
    out: *mut u16,
    cch: i32,
) -> i32 {
    put_wide(time_format(flags, time, wide(format)), out, cch)
}

/// See [`GetTimeFormatW`].
pub extern "win64" fn GetTimeFormatEx(
    _name: *const u16,
    flags: u32,
    time: *const SYSTEMTIME,
    format: *const u16,
    out: *mut u16,
    cch: i32,
) -> i32 {
    put_wide(time_format(flags, time, wide(format)), out, cch)
}

/// See [`GetTimeFormatW`].
pub extern "win64" fn GetTimeFormatA(
    _lcid: u32,
    flags: u32,
    time: *const SYSTEMTIME,
    format: *const u8,
    out: *mut u8,
    cch: i32,
) -> i32 {
    put_narrow(time_format(flags, time, narrow(format)), out, cch)
}

// Time zones: UTC.

/// A time zone with the key it has in the registry (its first part is a
/// `TIME_ZONE_INFORMATION`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DYNAMIC_TIME_ZONE_INFORMATION {
    pub zone: TIME_ZONE_INFORMATION,
    pub key_name: [u16; 128],
    pub dynamic_daylight_time_disabled: u8,
}

const _: () = assert!(std::mem::size_of::<DYNAMIC_TIME_ZONE_INFORMATION>() == 432);

const TIME_ZONE_ID_UNKNOWN: u32 = 0;
const TIME_ZONE_ID_INVALID: u32 = u32::MAX;

/// UTC, named so.
fn utc() -> DYNAMIC_TIME_ZONE_INFORMATION {
    let mut name = [0u16; 32];
    let mut key = [0u16; 128];
    for (i, unit) in "UTC".encode_utf16().enumerate() {
        name[i] = unit;
        key[i] = unit;
    }
    // SAFETY: zero is a value of every field.
    let mut zone: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    zone.standard_name = name;
    zone.daylight_name = name;
    DYNAMIC_TIME_ZONE_INFORMATION {
        zone,
        key_name: key,
        dynamic_daylight_time_disabled: 0,
    }
}

/// `st` moved by `minutes`.
fn shifted(st: *const SYSTEMTIME, minutes: i64, out: *mut SYSTEMTIME) -> BOOL {
    if st.is_null() || out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let mut ft = FILETIME::default();
    if SystemTimeToFileTime(st, &mut ft) == FALSE {
        return FALSE;
    }
    let value = (u64::from(ft.high) << 32 | u64::from(ft.low)) as i64;
    let Some(moved) = value
        .checked_add(minutes * 60 * 10_000_000)
        .filter(|&v| v >= 0)
    else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    let moved = FILETIME {
        low: moved as u32,
        high: (moved as u64 >> 32) as u32,
    };
    FileTimeToSystemTime(&moved, out)
}

/// A zone's offset from UTC in minutes (`Bias + StandardBias`); UTC's for
/// none.
fn bias(tz: *const TIME_ZONE_INFORMATION) -> i64 {
    if tz.is_null() {
        return 0;
    }
    // SAFETY: checked non-null; the title's zone.
    let zone = unsafe { tz.read_unaligned() };
    i64::from(zone.bias) + i64::from(zone.standard_bias)
}

/// A local time to UTC: plus the zone's bias.
pub extern "win64" fn TzSpecificLocalTimeToSystemTime(
    tz: *const TIME_ZONE_INFORMATION,
    local: *const SYSTEMTIME,
    out: *mut SYSTEMTIME,
) -> BOOL {
    shifted(local, bias(tz), out)
}

/// UTC to a local time: less the zone's bias.
pub extern "win64" fn SystemTimeToTzSpecificLocalTime(
    tz: *const TIME_ZONE_INFORMATION,
    system: *const SYSTEMTIME,
    out: *mut SYSTEMTIME,
) -> BOOL {
    shifted(system, -bias(tz), out)
}

/// See [`TzSpecificLocalTimeToSystemTime`].
pub extern "win64" fn TzSpecificLocalTimeToSystemTimeEx(
    tz: *const DYNAMIC_TIME_ZONE_INFORMATION,
    local: *const SYSTEMTIME,
    out: *mut SYSTEMTIME,
) -> BOOL {
    shifted(local, bias(tz.cast()), out)
}

/// See [`SystemTimeToTzSpecificLocalTime`].
pub extern "win64" fn SystemTimeToTzSpecificLocalTimeEx(
    tz: *const DYNAMIC_TIME_ZONE_INFORMATION,
    system: *const SYSTEMTIME,
    out: *mut SYSTEMTIME,
) -> BOOL {
    shifted(system, -bias(tz.cast()), out)
}

/// UTC, with no daylight time.
pub extern "win64" fn GetDynamicTimeZoneInformation(
    out: *mut DYNAMIC_TIME_ZONE_INFORMATION,
) -> u32 {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, TIME_ZONE_ID_INVALID);
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { out.write_unaligned(utc()) };
    TIME_ZONE_ID_UNKNOWN
}

/// The zone given (or UTC) in any year: its rules do not change here.
pub extern "win64" fn GetTimeZoneInformationForYear(
    _year: u16,
    zone: *const DYNAMIC_TIME_ZONE_INFORMATION,
    out: *mut TIME_ZONE_INFORMATION,
) -> BOOL {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let info = if zone.is_null() {
        utc().zone
    } else {
        // SAFETY: checked non-null; the title's zone.
        unsafe { zone.read_unaligned() }.zone
    };
    // SAFETY: checked non-null; the title's output.
    unsafe { out.write_unaligned(info) };
    TRUE
}

/// The zone is not the title's to change.
pub extern "win64" fn SetTimeZoneInformation(_zone: *const TIME_ZONE_INFORMATION) -> BOOL {
    fail(ERROR_ACCESS_DENIED, FALSE)
}

// Processors and nodes.

/// Logical processors, at most 64: one group's.
fn processors() -> usize {
    Host::logical_processors().clamp(1, 64)
}

/// The mask of every processor.
fn all_processors() -> usize {
    match processors() {
        64 => usize::MAX,
        n => (1 << n) - 1,
    }
}

/// One group, 0.
pub extern "win64" fn GetProcessGroupAffinity(
    _process: *mut c_void,
    count: *mut u16,
    groups: *mut u16,
) -> BOOL {
    if count.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's in-out count.
    unsafe {
        let room = count.read_unaligned();
        count.write_unaligned(1);
        if room == 0 || groups.is_null() {
            return fail(ERROR_INSUFFICIENT_BUFFER, FALSE);
        }
        groups.write_unaligned(0);
    }
    TRUE
}

/// Every processor, for the process and the system.
pub extern "win64" fn GetProcessAffinityMask(
    _process: *mut c_void,
    process_mask: *mut usize,
    system_mask: *mut usize,
) -> BOOL {
    if process_mask.is_null() || system_mask.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's outputs.
    unsafe {
        process_mask.write_unaligned(all_processors());
        system_mask.write_unaligned(all_processors());
    }
    TRUE
}

/// Accepted for any set of the system's processors (the host schedules).
pub extern "win64" fn SetProcessAffinityMask(_process: *mut c_void, mask: usize) -> BOOL {
    if mask == 0 || mask & !all_processors() != 0 {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    TRUE
}

/// One group.
pub extern "win64" fn GetMaximumProcessorGroupCount() -> u16 {
    1
}

/// One node, 0.
pub extern "win64" fn GetNumaHighestNodeNumber(out: *mut u32) -> BOOL {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { out.write_unaligned(0) };
    TRUE
}

/// Node 0 has every processor of group 0.
pub extern "win64" fn GetNumaNodeProcessorMaskEx(node: u16, affinity: *mut GROUP_AFFINITY) -> BOOL {
    if node != 0 || affinity.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let mask = GROUP_AFFINITY {
        mask: all_processors(),
        ..Default::default()
    };
    // SAFETY: checked non-null; the title's output.
    unsafe { affinity.write_unaligned(mask) };
    TRUE
}

/// Every processor is on node 0.
pub extern "win64" fn GetNumaProcessorNodeEx(
    processor: *const PROCESSOR_NUMBER,
    node: *mut u16,
) -> BOOL {
    if processor.is_null() || node.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's processor.
    let number = unsafe { processor.read_unaligned() };
    let known = number.group == 0 && usize::from(number.number) < processors();
    // SAFETY: checked non-null; the title's output.
    unsafe { node.write_unaligned(if known { 0 } else { u16::MAX }) };
    if !known {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    TRUE
}

/// Node 0 has the machine's available memory.
pub extern "win64" fn GetNumaAvailableMemoryNodeEx(node: u16, available: *mut u64) -> BOOL {
    if node != 0 || available.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { available.write_unaligned(Host::physical_memory().1) };
    TRUE
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(MultiByteToWideChar),
        hle_export!(WideCharToMultiByte),
        hle_export!(GetACP),
        hle_export!(GetOEMCP),
        hle_export!(GetCPInfo),
        hle_export!(GetCPInfoExW),
        hle_export!(IsValidCodePage),
        hle_export!(IsDBCSLeadByte),
        hle_export!(IsDBCSLeadByteEx),
        hle_export!(GetUserDefaultLocaleName),
        hle_export!(GetSystemDefaultLocaleName),
        hle_export!(GetUserDefaultLCID),
        hle_export!(GetSystemDefaultLCID),
        hle_export!(GetUserDefaultLangID),
        hle_export!(GetSystemDefaultLangID),
        hle_export!(GetUserDefaultUILanguage),
        hle_export!(GetSystemDefaultUILanguage),
        hle_export!(GetThreadUILanguage),
        hle_export!(GetThreadLocale),
        hle_export!(SetThreadLocale),
        hle_export!(SetThreadUILanguage),
        hle_export!(GetThreadPreferredUILanguages),
        hle_export!(GetUserPreferredUILanguages),
        hle_export!(IsValidLocaleName),
        hle_export!(ResolveLocaleName),
        hle_export!(LocaleNameToLCID),
        hle_export!(LCIDToLocaleName),
        hle_export!(GetLocaleInfoEx),
        hle_export!(GetLocaleInfoW),
        hle_export!(GetLocaleInfoA),
        hle_export!(EnumSystemLocalesEx),
        hle_export!(LCMapStringEx),
        hle_export!(LCMapStringW),
        hle_export!(CompareStringEx),
        hle_export!(CompareStringW),
        hle_export!(CompareStringOrdinal),
        hle_export!(GetStringTypeW),
        hle_export!(GetStringTypeExW),
        hle_export!(FoldStringW),
        hle_export!(lstrlenA),
        hle_export!(lstrlenW),
        hle_export!(lstrcmpA),
        hle_export!(lstrcmpW),
        hle_export!(lstrcmpiA),
        hle_export!(lstrcmpiW),
        hle_export!(lstrcpyA),
        hle_export!(lstrcpyW),
        hle_export!(lstrcpynA),
        hle_export!(lstrcpynW),
        hle_export!(lstrcatA),
        hle_export!(lstrcatW),
        hle_export!(FormatMessageW),
        hle_export!(FormatMessageA),
        hle_export!(GetDateFormatW),
        hle_export!(GetDateFormatEx),
        hle_export!(GetDateFormatA),
        hle_export!(GetTimeFormatW),
        hle_export!(GetTimeFormatEx),
        hle_export!(GetTimeFormatA),
        hle_export!(TzSpecificLocalTimeToSystemTime),
        hle_export!(SystemTimeToTzSpecificLocalTime),
        hle_export!(TzSpecificLocalTimeToSystemTimeEx),
        hle_export!(SystemTimeToTzSpecificLocalTimeEx),
        hle_export!(GetDynamicTimeZoneInformation),
        hle_export!(GetTimeZoneInformationForYear),
        hle_export!(SetTimeZoneInformation),
        hle_export!(GetProcessGroupAffinity),
        hle_export!(GetProcessAffinityMask),
        hle_export!(SetProcessAffinityMask),
        hle_export!(GetMaximumProcessorGroupCount),
        hle_export!(GetNumaHighestNodeNumber),
        hle_export!(GetNumaNodeProcessorMaskEx),
        hle_export!(GetNumaProcessorNodeEx),
        hle_export!(GetNumaAvailableMemoryNodeEx),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::handles::last_error;
    use super::*;

    fn w(text: &str) -> Vec<u16> {
        wide_z(text)
    }

    fn attach() {
        victory_loader::teb::attach_current_thread();
    }

    #[test]
    fn utf8_converts_both_ways() {
        attach();
        let text = "héllo €\0";
        let bytes = text.as_bytes();
        let needed = MultiByteToWideChar(CP_UTF8, 0, bytes.as_ptr(), -1, std::ptr::null_mut(), 0);
        assert_eq!(needed, 8);
        let mut out = [0u16; 8];
        assert_eq!(
            MultiByteToWideChar(CP_UTF8, 0, bytes.as_ptr(), -1, out.as_mut_ptr(), 8),
            8
        );
        assert_eq!(String::from_utf16(&out).unwrap(), text);
        let mut small = [0u16; 4];
        assert_eq!(
            MultiByteToWideChar(CP_UTF8, 0, bytes.as_ptr(), -1, small.as_mut_ptr(), 4),
            0
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);

        let bad = [b'a', 0xFF, b'b'];
        assert_eq!(
            MultiByteToWideChar(
                CP_UTF8,
                MB_ERR_INVALID_CHARS,
                bad.as_ptr(),
                3,
                out.as_mut_ptr(),
                8
            ),
            0
        );
        assert_eq!(last_error(), ERROR_NO_UNICODE_TRANSLATION);
        assert_eq!(
            MultiByteToWideChar(CP_UTF8, 0, bad.as_ptr(), 3, out.as_mut_ptr(), 8),
            3
        );
        assert_eq!(&out[..3], &[0x61, 0xFFFD, 0x62]);

        let wide_text = w("héllo €");
        let null = std::ptr::null();
        let none: *mut i32 = std::ptr::null_mut();
        let needed = WideCharToMultiByte(
            CP_UTF8,
            0,
            wide_text.as_ptr(),
            -1,
            none.cast(),
            0,
            null,
            none,
        );
        assert_eq!(needed as usize, bytes.len());
        let mut narrow_out = [0u8; 16];
        let n = WideCharToMultiByte(
            CP_UTF8,
            0,
            wide_text.as_ptr(),
            -1,
            narrow_out.as_mut_ptr(),
            16,
            null,
            none,
        );
        assert_eq!(&narrow_out[..n as usize], bytes);
        let mut used = 0;
        assert_eq!(
            WideCharToMultiByte(
                CP_UTF8,
                0,
                wide_text.as_ptr(),
                -1,
                narrow_out.as_mut_ptr(),
                16,
                null,
                &mut used
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        let lone = [0x61u16, 0xD800, 0x62];
        assert_eq!(
            WideCharToMultiByte(
                CP_UTF8,
                WC_ERR_INVALID_CHARS,
                lone.as_ptr(),
                3,
                narrow_out.as_mut_ptr(),
                16,
                null,
                none
            ),
            0
        );
        assert_eq!(last_error(), ERROR_NO_UNICODE_TRANSLATION);
        assert_eq!(
            WideCharToMultiByte(
                CP_UTF8,
                0,
                lone.as_ptr(),
                3,
                narrow_out.as_mut_ptr(),
                16,
                null,
                none
            ),
            5
        );
        assert_eq!(&narrow_out[..5], b"a\xEF\xBF\xBDb");
    }

    #[test]
    fn latin1_converts_both_ways() {
        attach();
        let bytes = [b'a', 0xE9, 0x80, 0];
        let mut out = [0u16; 4];
        assert_eq!(
            MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), -1, out.as_mut_ptr(), 4),
            4
        );
        assert_eq!(out, [0x61, 0xE9, 0x80, 0]);
        assert_eq!(
            MultiByteToWideChar(1200, 0, bytes.as_ptr(), -1, out.as_mut_ptr(), 4),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(
            MultiByteToWideChar(1252, 0, bytes.as_ptr(), 0, out.as_mut_ptr(), 4),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);

        let text = w("a\u{e9}\u{20ac}");
        let mut narrow_out = [0u8; 4];
        let mut used = 0;
        let null = std::ptr::null();
        assert_eq!(
            WideCharToMultiByte(
                1252,
                0,
                text.as_ptr(),
                -1,
                narrow_out.as_mut_ptr(),
                4,
                null,
                &mut used
            ),
            4
        );
        assert_eq!(narrow_out, [b'a', 0xE9, b'?', 0]);
        assert_eq!(used, 1);
        let star = b'*';
        assert_eq!(
            WideCharToMultiByte(
                437,
                0,
                text.as_ptr(),
                3,
                narrow_out.as_mut_ptr(),
                4,
                &star,
                &mut used
            ),
            3
        );
        assert_eq!(&narrow_out[..3], &[b'a', 0xE9, b'*']);
        assert_eq!(
            WideCharToMultiByte(
                1252,
                0,
                text.as_ptr(),
                3,
                narrow_out.as_mut_ptr(),
                2,
                null,
                &mut used
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(GetACP(), 1252);
        let mut info = CPINFO::default();
        assert_eq!(GetCPInfo(CP_UTF8, &mut info), TRUE);
        assert_eq!(info.max_char_size, 4);
    }

    /// The value of a locale type, as text.
    fn info(lctype: u32) -> String {
        let mut out = [0u16; 64];
        let n = GetLocaleInfoEx(w("en-US").as_ptr(), lctype, out.as_mut_ptr(), 64);
        assert!(n > 0, "{lctype:#x}");
        String::from_utf16(&out[..n as usize - 1]).unwrap()
    }

    #[test]
    fn the_locale_answers_as_en_us() {
        attach();
        for (lctype, expected) in [
            (0x5C, "en-US"),
            (0x1001, "English"),
            (0x1002, "United States"),
            (0x2, "English (United States)"),
            (0xE, "."),
            (0xF, ","),
            (0x1F, "M/d/yyyy"),
            (0x1003, "h:mm:ss tt"),
            (0x14, "$"),
            (0x2A, "Monday"),
            (0x30, "Sunday"),
            (0x37, "Sun"),
            (0x38, "January"),
            (0x4F, "Dec"),
            (0x1004, "1252"),
            (0x100C, "6"),
        ] {
            assert_eq!(info(lctype), expected);
        }
        let mut number = [0u16; 2];
        let null = std::ptr::null();
        assert_eq!(
            GetLocaleInfoEx(null, 0x1004 | LOCALE_RETURN_NUMBER, number.as_mut_ptr(), 2),
            2
        );
        assert_eq!(u32::from(number[0]) | u32::from(number[1]) << 16, 1252);
        assert_eq!(
            GetLocaleInfoEx(null, 0x1 | LOCALE_RETURN_NUMBER, number.as_mut_ptr(), 2),
            2
        );
        assert_eq!(number[0], 0x409);
        assert_eq!(
            GetLocaleInfoEx(null, 0x5C | LOCALE_RETURN_NUMBER, number.as_mut_ptr(), 2),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_FLAGS);
        assert_eq!(GetLocaleInfoEx(null, 0xFFFF, number.as_mut_ptr(), 2), 0);
        assert_eq!(last_error(), ERROR_INVALID_FLAGS);
        assert_eq!(GetLocaleInfoEx(null, 0x5C, std::ptr::null_mut(), 0), 6);
        let mut name = [0u16; 8];
        assert_eq!(GetUserDefaultLocaleName(name.as_mut_ptr(), 5), 0);
        assert_eq!(GetUserDefaultLocaleName(name.as_mut_ptr(), 8), 6);
        assert_eq!(IsValidLocaleName(w("fr-FR").as_ptr()), TRUE);
        assert_eq!(IsValidLocaleName(w("e1-US").as_ptr()), FALSE);
        assert_eq!(LocaleNameToLCID(w("en-US").as_ptr(), 0), 0x409);
    }

    #[test]
    fn strings_map_and_compare() {
        attach();
        let text = w("Straße ok");
        let mut out = [0u16; 16];
        let n = LCMapStringEx(
            null_name(),
            LCMAP_UPPERCASE,
            text.as_ptr(),
            -1,
            out.as_mut_ptr(),
            16,
            nil(),
            nil(),
            0,
        );
        assert_eq!(n, 10);
        assert_eq!(String::from_utf16(&out[..9]).unwrap(), "STRAßE OK");

        let key = |s: &str, flags: u32| {
            let s = w(s);
            let mut key = [0u8; 64];
            let n = LCMapStringW(
                0x409,
                LCMAP_SORTKEY | flags,
                s.as_ptr(),
                -1,
                key.as_mut_ptr().cast(),
                64,
            );
            assert!(n > 0);
            assert!(key[..n as usize - 1].iter().all(|&b| b != 0));
            key[..n as usize].to_vec()
        };
        assert!(key("apple", 0) < key("apricot", 0));
        assert!(key("Zebra", 0) < key("apple", 0));
        assert_eq!(key("APPLE", NORM_IGNORECASE), key("apple", NORM_IGNORECASE));

        let (a, b) = (w("Hello"), w("hELLO"));
        assert_eq!(
            CompareStringEx(
                null_name(),
                NORM_IGNORECASE,
                a.as_ptr(),
                -1,
                b.as_ptr(),
                -1,
                nil(),
                nil(),
                0
            ),
            CSTR_EQUAL
        );
        assert_eq!(
            CompareStringEx(
                null_name(),
                0,
                a.as_ptr(),
                -1,
                b.as_ptr(),
                -1,
                nil(),
                nil(),
                0
            ),
            CSTR_LESS_THAN
        );
        assert_eq!(
            CompareStringOrdinal(b.as_ptr(), -1, a.as_ptr(), 3, FALSE),
            CSTR_GREATER_THAN
        );
        assert_eq!(lstrcmpiW(a.as_ptr(), b.as_ptr()), 0);
        assert_eq!(lstrcmpW(a.as_ptr(), b.as_ptr()), -1);
        assert_eq!(lstrcmpiW(w("abc").as_ptr(), w("ABD").as_ptr()), -1);

        let mut types = [0u16; 4];
        assert_eq!(
            GetStringTypeW(CT_CTYPE1, w("A1 .").as_ptr(), 4, types.as_mut_ptr()),
            TRUE
        );
        assert_eq!(types, [0x381, 0x284, 0x248, 0x210]);
        assert_eq!(
            GetStringTypeW(CT_CTYPE2, w("A1 .").as_ptr(), 4, types.as_mut_ptr()),
            TRUE
        );
        assert_eq!(types, [1, 3, 8, 11]);

        let mut copy = [0u16; 8];
        lstrcpynW(copy.as_mut_ptr(), w("abcdef").as_ptr(), 4);
        assert_eq!(&copy[..4], &w("abc")[..]);
        lstrcatW(copy.as_mut_ptr(), w("XY").as_ptr());
        assert_eq!(lstrlenW(copy.as_ptr()), 5);
    }

    fn null_name() -> *const u16 {
        std::ptr::null()
    }

    fn nil() -> *mut c_void {
        std::ptr::null_mut()
    }

    /// `FormatMessageW` into a buffer of 128.
    fn format(flags: u32, source: &str, id: u32, args: &[u64]) -> Option<String> {
        let source = w(source);
        let mut out = [0u16; 128];
        let args = if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr().cast()
        };
        let n = FormatMessageW(
            flags,
            source.as_ptr().cast(),
            id,
            0,
            out.as_mut_ptr(),
            128,
            args,
        );
        (n > 0).then(|| String::from_utf16(&out[..n as usize]).unwrap())
    }

    #[test]
    fn messages_fill_their_inserts() {
        attach();
        let name = w("disk");
        let narrow_name = b"tape\0";
        let flags = FORMAT_MESSAGE_FROM_STRING | FORMAT_MESSAGE_ARGUMENT_ARRAY;
        let args = [
            name.as_ptr() as u64,
            (-42i32) as u32 as u64,
            0xBEEF,
            narrow_name.as_ptr() as u64,
            7,
        ];
        assert_eq!(
            format(
                flags,
                "%1 has %2!d! (%3!08x!) %4!hs!%n%5!lu!%%%t%0rest",
                0,
                &args
            )
            .as_deref(),
            Some("disk has -42 (0000beef) tape\r\n7%\t")
        );
        assert_eq!(
            format(flags, "[%1!-6s!][%5!5d!][%5!.3u!]", 0, &args).as_deref(),
            Some("[disk  ][    7][007]")
        );
        assert_eq!(
            format(
                FORMAT_MESSAGE_FROM_STRING | FORMAT_MESSAGE_IGNORE_INSERTS,
                "a %1 b %2!d!",
                0,
                &[]
            )
            .as_deref(),
            Some("a %1 b %2!d!")
        );
        // A va_list: a pointer to the slots.
        let list = args.as_ptr();
        let mut out = [0u16; 32];
        let source = w("<%1>");
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_STRING,
            source.as_ptr().cast(),
            0,
            0,
            out.as_mut_ptr(),
            32,
            (&raw const list).cast(),
        );
        assert_eq!(String::from_utf16(&out[..n as usize]).unwrap(), "<disk>");

        let mut allocated: *mut u16 = std::ptr::null_mut();
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM
                | FORMAT_MESSAGE_ALLOCATE_BUFFER
                | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            2,
            0,
            (&raw mut allocated).cast(),
            0,
            std::ptr::null(),
        );
        // SAFETY: `n` units and a NUL in the block made for it.
        let text = unsafe { std::slice::from_raw_parts(allocated, n as usize + 1) };
        assert_eq!(
            String::from_utf16(&text[..n as usize]).unwrap(),
            "The system cannot find the file specified.\r\n"
        );
        assert_eq!(text[n as usize], 0);
        super::super::memory::LocalFree(allocated.cast());
        assert_eq!(
            format(FORMAT_MESSAGE_FROM_SYSTEM | 0xFF, "", 5, &[]).as_deref(),
            Some("Access is denied. ")
        );
        assert_eq!(format(FORMAT_MESSAGE_FROM_SYSTEM, "", 0xFFFF, &[]), None);
        assert_eq!(last_error(), ERROR_MR_MID_NOT_FOUND);
        let mut small = [0u16; 4];
        assert_eq!(
            FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM,
                std::ptr::null(),
                5,
                0,
                small.as_mut_ptr(),
                4,
                std::ptr::null()
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
    }

    #[test]
    fn dates_and_times_follow_their_pictures() {
        attach();
        // Sunday, 5 March 2023, 14:07:09.
        let st = SYSTEMTIME {
            year: 2023,
            month: 3,
            day_of_week: 0,
            day: 5,
            hour: 14,
            minute: 7,
            second: 9,
            milliseconds: 0,
        };
        let date = |flags: u32, picture: Option<&str>| {
            let picture = picture.map(w);
            let format = picture.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
            let mut out = [0u16; 64];
            let n = GetDateFormatW(0x409, flags, &st, format, out.as_mut_ptr(), 64);
            (n > 0).then(|| String::from_utf16(&out[..n as usize - 1]).unwrap())
        };
        assert_eq!(date(DATE_SHORTDATE, None).as_deref(), Some("3/5/2023"));
        assert_eq!(
            date(DATE_LONGDATE, None).as_deref(),
            Some("Sunday, March 5, 2023")
        );
        assert_eq!(date(DATE_YEARMONTH, None).as_deref(), Some("March 2023"));
        assert_eq!(date(DATE_MONTHDAY, None).as_deref(), Some("March 5"));
        assert_eq!(
            date(
                0,
                Some("d dd ddd dddd M MM MMM MMMM y yy yyyy gg 'o''clock'")
            )
            .as_deref(),
            Some("5 05 Sun Sunday 3 03 Mar March 23 23 2023 A.D. o'clock")
        );
        assert_eq!(date(DATE_SHORTDATE, Some("d")), None);
        assert_eq!(last_error(), ERROR_INVALID_FLAGS);
        let time = |flags: u32, picture: Option<&str>| {
            let picture = picture.map(w);
            let format = picture.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
            let mut out = [0u16; 64];
            let n = GetTimeFormatW(0x409, flags, &st, format, out.as_mut_ptr(), 64);
            (n > 0).then(|| String::from_utf16(&out[..n as usize - 1]).unwrap())
        };
        assert_eq!(time(0, None).as_deref(), Some("2:07:09 PM"));
        assert_eq!(time(TIME_NOSECONDS, None).as_deref(), Some("2:07 PM"));
        assert_eq!(
            time(TIME_FORCE24HOURFORMAT, None).as_deref(),
            Some("14:07:09")
        );
        assert_eq!(time(TIME_NOMINUTESORSECONDS, None).as_deref(), Some("2 PM"));
        assert_eq!(time(TIME_NOTIMEMARKER, None).as_deref(), Some("2:07:09"));
        assert_eq!(
            time(0, Some("h hh H HH m mm s ss t tt")).as_deref(),
            Some("2 02 14 14 7 07 9 09 P PM")
        );
        assert_eq!(
            GetTimeFormatW(0x409, 0, &st, std::ptr::null(), std::ptr::null_mut(), 0),
            11
        );
        let mut narrow_out = [0u8; 16];
        assert_eq!(
            GetDateFormatA(0x409, 0, &st, std::ptr::null(), narrow_out.as_mut_ptr(), 16),
            9
        );
        assert_eq!(&narrow_out[..9], b"3/5/2023\0");
        let bad = SYSTEMTIME { month: 13, ..st };
        assert_eq!(
            GetDateFormatW(0x409, 0, &bad, std::ptr::null(), std::ptr::null_mut(), 0),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn zones_apply_their_bias() {
        attach();
        let st = SYSTEMTIME {
            year: 2024,
            month: 1,
            day: 1,
            hour: 1,
            ..Default::default()
        };
        let mut zone = utc().zone;
        zone.bias = 120;
        let mut out = SYSTEMTIME::default();
        assert_eq!(SystemTimeToTzSpecificLocalTime(&zone, &st, &mut out), TRUE);
        assert_eq!((out.year, out.month, out.day, out.hour), (2023, 12, 31, 23));
        let mut back = SYSTEMTIME::default();
        assert_eq!(
            TzSpecificLocalTimeToSystemTime(&zone, &out, &mut back),
            TRUE
        );
        assert_eq!((back.year, back.hour), (2024, 1));
        let mut mask = (0, 0);
        assert_eq!(
            GetProcessAffinityMask(nil(), &mut mask.0, &mut mask.1),
            TRUE
        );
        assert_eq!(mask.0.count_ones() as usize, processors());
        let (mut count, mut group) = (0u16, 9u16);
        assert_eq!(
            GetProcessGroupAffinity(nil(), &mut count, &mut group),
            FALSE
        );
        assert_eq!((count, last_error()), (1, ERROR_INSUFFICIENT_BUFFER));
        assert_eq!(GetProcessGroupAffinity(nil(), &mut count, &mut group), TRUE);
        assert_eq!(group, 0);
    }

    /// A bounded copy reads no further than it copies; title case; the
    /// genitive-names flag; a null-free sort.
    #[test]
    fn bounded_copies_title_case_and_genitive_names() {
        attach();
        // A 16-byte field with no NUL, followed by nothing readable we rely
        // on: only the 7 units that fit are read and copied.
        let field: [u8; 16] = *b"0123456789abcdef";
        let mut dst = [0xAAu8; 8];
        assert_eq!(
            lstrcpynA(dst.as_mut_ptr(), field.as_ptr(), 8),
            dst.as_mut_ptr()
        );
        assert_eq!(&dst, b"0123456\0");
        let text = w("hello wORLD 2nd");
        let mut out = [0u16; 16];
        let n = LCMapStringEx(
            std::ptr::null(),
            LCMAP_TITLECASE,
            text.as_ptr(),
            -1,
            out.as_mut_ptr(),
            16,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        );
        assert_eq!(n, 16);
        assert_eq!(String::from_utf16(&out[..15]).unwrap(), "Hello World 2nd");
        let mut month = [0u16; 16];
        let n = GetLocaleInfoEx(
            std::ptr::null(),
            0x38 | LOCALE_RETURN_GENITIVE_NAMES,
            month.as_mut_ptr(),
            16,
        );
        assert_eq!(n, 8);
        assert_eq!(String::from_utf16(&month[..7]).unwrap(), "January");
        let mut digits = [0u16; 2];
        assert_eq!(
            GetLocaleInfoEx(
                std::ptr::null(),
                0x1A | LOCALE_RETURN_NUMBER,
                digits.as_mut_ptr(),
                2
            ),
            2
        );
        assert_eq!(digits, [2, 0]);
        assert_eq!(IsValidCodePage(65000), FALSE);
        assert_eq!(IsValidCodePage(65001), TRUE);
        // In place is refused.
        let mut same = w("abc");
        assert_eq!(
            FoldStringW(0x40, same.as_ptr(), -1, same.as_mut_ptr(), 4),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 74);
    }
}
