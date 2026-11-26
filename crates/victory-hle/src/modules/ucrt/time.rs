//! Time (`api-ms-win-crt-time`): the clock, calendar conversions,
//! `strftime` and the time-zone settings.
//!
//! There is no time zone yet: local time is UTC (`_localtime64_s` is
//! `_gmtime64_s`, `_mktime64` is `_mkgmtime64`), with no daylight saving,
//! and the zone's name is "UTC". The civil-date arithmetic is Howard
//! Hinnant's, copied from `win32/time.rs` (private there).
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::{c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicPtr};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::runtime::{EINVAL, EPERM, ERANGE, set_errno};
use crate::{HleExport, hle_export};

/// The CRT's `struct tm`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tm {
    pub sec: i32,
    pub min: i32,
    pub hour: i32,
    pub mday: i32,
    pub mon: i32,
    pub year: i32,
    pub wday: i32,
    pub yday: i32,
    pub isdst: i32,
}

const _: () = assert!(std::mem::size_of::<Tm>() == 36);

/// The CRT's `__timeb64`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timeb {
    pub time: i64,
    pub millitm: u16,
    pub timezone: i16,
    pub dstflag: i16,
}

/// The CRT's `__timeb32`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timeb32 {
    pub time: i32,
    pub millitm: u16,
    pub timezone: i16,
    pub dstflag: i16,
}

/// The last second the 64-bit functions take: 3000-12-31 23:59:59.
const MAX_TIME64: i64 = 32_535_244_799;
/// The last second the 32-bit functions take: 2038-01-18 19:14:07.
const MAX_TIME32: i64 = 0x7FFF_D27F;

/// Howard Hinnant's `civil_from_days`: the (year, month, day) of a day
/// counted from 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Howard Hinnant's `days_from_civil`: the day, counted from 1970-01-01, of
/// a (year, month, day).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The broken-down UTC time of seconds since 1970.
fn broken_down(time: i64) -> Tm {
    let days = time.div_euclid(86_400);
    let secs = time.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let yday = days - days_from_civil(year, 1, 1);
    Tm {
        sec: (secs % 60) as i32,
        min: (secs / 60 % 60) as i32,
        hour: (secs / 3600) as i32,
        mday: day as i32,
        mon: month as i32 - 1,
        year: (year - 1900) as i32,
        // 1970-01-01 was a Thursday.
        wday: (days + 4).rem_euclid(7) as i32,
        yday: yday as i32,
        isdst: 0,
    }
}

/// Seconds since 1970 of a broken-down UTC time, its fields normalized
/// (a month of 13 is the next year's first, and so on).
fn seconds(tm: &Tm) -> i64 {
    let months = i64::from(tm.year) * 12 + i64::from(tm.mon);
    let year = months.div_euclid(12) + 1900;
    let month = months.rem_euclid(12) as u32 + 1;
    let days = days_from_civil(year, month, 1) + i64::from(tm.mday) - 1;
    days * 86_400 + i64::from(tm.hour) * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec)
}

/// Seconds since 1970 now.
fn now() -> (i64, u32) {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (since.as_secs() as i64, since.subsec_millis())
}

/// Seconds since 1970, also in `*out`.
pub extern "win64" fn _time64(out: *mut i64) -> i64 {
    let (time, _) = now();
    if !out.is_null() {
        // SAFETY: the title's place for the time.
        unsafe { out.write_unaligned(time) };
    }
    time
}

/// See [`_time64`]; -1 past 2038.
pub extern "win64" fn _time32(out: *mut i32) -> i32 {
    let (time, _) = now();
    let time = if time > MAX_TIME32 { -1 } else { time as i32 };
    if !out.is_null() {
        // SAFETY: the title's place for the time.
        unsafe { out.write_unaligned(time) };
    }
    time
}

/// When the process started, for [`clock`].
static ANCHOR: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the process started (`CLOCKS_PER_SEC` 1000).
pub extern "win64" fn clock() -> i32 {
    ANCHOR.get_or_init(Instant::now).elapsed().as_millis() as i32
}

/// The broken-down time of `time` in `*out`; `EINVAL` for null, negative or
/// too late a time (the fields all -1 then).
fn to_tm(out: *mut Tm, time: Option<i64>, max: i64) -> i32 {
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    let invalid = Tm {
        sec: -1,
        min: -1,
        hour: -1,
        mday: -1,
        mon: -1,
        year: -1,
        wday: -1,
        yday: -1,
        isdst: -1,
    };
    let result = match time {
        Some(t) if (0..=max).contains(&t) => broken_down(t),
        _ => {
            // SAFETY: the title's `tm`.
            unsafe { out.write_unaligned(invalid) };
            set_errno(EINVAL);
            return EINVAL;
        }
    };
    // SAFETY: as above.
    unsafe { out.write_unaligned(result) };
    0
}

/// The time at `time`, or `None` for null.
fn read<T: Copy + Into<i64>>(time: *const T) -> Option<i64> {
    // SAFETY: the title's time, if non-null.
    (!time.is_null()).then(|| unsafe { time.read_unaligned() }.into())
}

/// The UTC time of `*time`.
pub extern "win64" fn _gmtime64_s(out: *mut Tm, time: *const i64) -> i32 {
    to_tm(out, read(time), MAX_TIME64)
}

/// The local time of `*time`: UTC (no time zone yet).
pub extern "win64" fn _localtime64_s(out: *mut Tm, time: *const i64) -> i32 {
    _gmtime64_s(out, time)
}

/// See [`_gmtime64_s`].
pub extern "win64" fn _gmtime32_s(out: *mut Tm, time: *const i32) -> i32 {
    to_tm(out, read(time), MAX_TIME32)
}

/// See [`_localtime64_s`].
pub extern "win64" fn _localtime32_s(out: *mut Tm, time: *const i32) -> i32 {
    _gmtime32_s(out, time)
}

thread_local! {
    static THREAD_TM: Cell<Tm> = const { Cell::new(Tm {
        sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0,
    }) };
}

/// The UTC time of `*time` in a per-thread `tm`; null on failure.
pub extern "win64" fn _gmtime64(time: *const i64) -> *mut Tm {
    THREAD_TM.with(|tm| {
        if _gmtime64_s(tm.as_ptr(), time) == 0 {
            tm.as_ptr()
        } else {
            std::ptr::null_mut()
        }
    })
}

/// See [`_gmtime64`] (local time is UTC).
pub extern "win64" fn _localtime64(time: *const i64) -> *mut Tm {
    _gmtime64(time)
}

/// Normalizes a broken-down UTC time and fills its weekday and day of the
/// year; its seconds since 1970, -1 (and `EINVAL`) out of `0..=max`.
fn from_tm(tm: *mut Tm, max: i64) -> i64 {
    if tm.is_null() {
        set_errno(EINVAL);
        return -1;
    }
    // SAFETY: the title's `tm`.
    let fields = unsafe { tm.read_unaligned() };
    let time = seconds(&fields);
    if !(0..=max).contains(&time) {
        set_errno(EINVAL);
        return -1;
    }
    // SAFETY: as above.
    unsafe { tm.write_unaligned(broken_down(time)) };
    time
}

/// See [`from_tm`] (local time is UTC).
pub extern "win64" fn _mktime64(tm: *mut Tm) -> i64 {
    from_tm(tm, MAX_TIME64)
}

/// See [`from_tm`].
pub extern "win64" fn _mkgmtime64(tm: *mut Tm) -> i64 {
    from_tm(tm, MAX_TIME64)
}

/// See [`from_tm`], up to 2038.
pub extern "win64" fn _mktime32(tm: *mut Tm) -> i32 {
    from_tm(tm, MAX_TIME32) as i32
}

/// See [`from_tm`], up to 2038.
pub extern "win64" fn _mkgmtime32(tm: *mut Tm) -> i32 {
    from_tm(tm, MAX_TIME32) as i32
}

/// `a - b` in seconds.
pub extern "win64" fn _difftime64(a: i64, b: i64) -> f64 {
    (a as f64) - (b as f64)
}

/// See [`_difftime64`].
pub extern "win64" fn _difftime32(a: i32, b: i32) -> f64 {
    f64::from(a) - f64::from(b)
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
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

/// A name from `names`, or "?" for a field out of range.
fn name(names: &[&'static str], index: i32) -> &'static str {
    usize::try_from(index)
        .ok()
        .and_then(|i| names.get(i))
        .copied()
        .unwrap_or("?")
}

/// Whether a year has 53 ISO weeks: its first day a Thursday, or a
/// Wednesday in a leap year.
fn iso_weeks(year: i64) -> i64 {
    let jan1 = (days_from_civil(year, 1, 1) + 4).rem_euclid(7);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    if jan1 == 4 || (leap && jan1 == 3) {
        53
    } else {
        52
    }
}

/// The ISO 8601 year and week of a broken-down time.
fn iso_week(tm: &Tm) -> (i64, i64) {
    let year = i64::from(tm.year) + 1900;
    let monday_based = i64::from((tm.wday + 6).rem_euclid(7));
    let week = (i64::from(tm.yday) - monday_based + 10).div_euclid(7);
    if week < 1 {
        (year - 1, iso_weeks(year - 1))
    } else if week > iso_weeks(year) {
        (year + 1, 1)
    } else {
        (year, week)
    }
}

/// `strftime`'s text for `format`, or `None` for an unknown conversion.
fn format_time(format: &[u32], tm: &Tm) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    let at = |i: usize| format.get(i).and_then(|&c| char::from_u32(c));
    while i < format.len() {
        let c = at(i).unwrap_or(char::REPLACEMENT_CHARACTER);
        i += 1;
        if c != '%' {
            out.push(c);
            continue;
        }
        // `%#x`: MSVC's alternate form, which drops leading zeros.
        let alternate = at(i) == Some('#');
        if alternate {
            i += 1;
        }
        let spec = at(i)?;
        i += 1;
        let year = i64::from(tm.year) + 1900;
        let hour12 = if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 };
        let number = |n: i64, width: usize| {
            if alternate {
                n.to_string()
            } else {
                format!("{n:0width$}")
            }
        };
        let text = match spec {
            'a' => name(&DAYS, tm.wday).chars().take(3).collect(),
            'A' => name(&DAYS, tm.wday).to_owned(),
            'b' | 'h' => name(&MONTHS, tm.mon).chars().take(3).collect(),
            'B' => name(&MONTHS, tm.mon).to_owned(),
            'c' => format_time(&wide("%m/%d/%y %H:%M:%S"), tm)?,
            'C' => number(year.div_euclid(100), 2),
            'd' => number(tm.mday.into(), 2),
            'D' | 'x' => format_time(&wide("%m/%d/%y"), tm)?,
            'e' => format!("{:2}", tm.mday),
            'F' => format_time(&wide("%Y-%m-%d"), tm)?,
            'g' => number(iso_week(tm).0.rem_euclid(100), 2),
            'G' => iso_week(tm).0.to_string(),
            'H' => number(tm.hour.into(), 2),
            'I' => number(hour12.into(), 2),
            'j' => number(i64::from(tm.yday) + 1, 3),
            'm' => number(i64::from(tm.mon) + 1, 2),
            'M' => number(tm.min.into(), 2),
            'n' => "\n".to_owned(),
            'p' => (if tm.hour < 12 { "AM" } else { "PM" }).to_owned(),
            'R' => format_time(&wide("%H:%M"), tm)?,
            's' => seconds(tm).to_string(),
            'S' => number(tm.sec.into(), 2),
            't' => "\t".to_owned(),
            'T' | 'X' => format_time(&wide("%H:%M:%S"), tm)?,
            'u' => (if tm.wday == 0 { 7 } else { tm.wday }).to_string(),
            'U' => number(i64::from(tm.yday + 7 - tm.wday).div_euclid(7), 2),
            'V' => number(iso_week(tm).1, 2),
            'w' => tm.wday.to_string(),
            'W' => number(
                i64::from(tm.yday + 7 - (tm.wday + 6).rem_euclid(7)).div_euclid(7),
                2,
            ),
            'y' => number(year.rem_euclid(100), 2),
            'Y' => year.to_string(),
            'z' => "+0000".to_owned(),
            'Z' => String::new(),
            '%' => "%".to_owned(),
            _ => return None,
        };
        out.push_str(&text);
    }
    Some(out)
}

/// A string's code points, for [`format_time`].
fn wide(text: &str) -> Vec<u32> {
    text.chars().map(u32::from).collect()
}

/// [`format_time`] into a buffer of `max` units with its NUL; the units
/// written without it, or 0 (the buffer emptied, `ERANGE`) when it does not
/// fit, or `EINVAL` for null or an unknown conversion.
///
/// # Safety
/// `buffer` has `max` units; `format` is a NUL-terminated string of units
/// `T`, read as code points by `value`.
unsafe fn strftime_into<T: Copy + Default>(
    buffer: *mut T,
    max: usize,
    format: &[u32],
    tm: *const Tm,
    encode: impl Fn(&str) -> Vec<T>,
) -> usize {
    if buffer.is_null() || max == 0 || tm.is_null() {
        set_errno(EINVAL);
        return 0;
    }
    // SAFETY: the title's `tm`.
    let fields = unsafe { tm.read_unaligned() };
    let Some(text) = format_time(format, &fields) else {
        // SAFETY: the buffer has `max` (non-zero) units.
        unsafe { buffer.write(T::default()) };
        set_errno(EINVAL);
        return 0;
    };
    let units = encode(&text);
    // SAFETY: as above; `units.len() < max` is checked before the copy.
    unsafe {
        if units.len() >= max {
            buffer.write(T::default());
            set_errno(ERANGE);
            return 0;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len());
        buffer.add(units.len()).write(T::default());
    }
    units.len()
}

/// Formats a time: `%Y %m %d %H %M %S %y %j %a %A %b %B %p %I %e %Z %z %%
/// %c %x %X %D %F %T %R %n %t %u %w %U %W %C %G %g %V %s` (the "C"
/// locale's; `%Z` empty, `%z` +0000).
pub extern "win64" fn strftime(
    buffer: *mut c_char,
    max: usize,
    format: *const c_char,
    tm: *const Tm,
) -> usize {
    if format.is_null() {
        set_errno(EINVAL);
        return 0;
    }
    // SAFETY: the title's format string.
    let format: Vec<u32> = String::from_utf8_lossy(unsafe { super::string::narrow_bytes(format) })
        .chars()
        .map(u32::from)
        .collect();
    // SAFETY: the title's buffer of `max` bytes and `tm`.
    unsafe {
        strftime_into(buffer.cast::<u8>(), max, &format, tm, |t| {
            t.as_bytes().to_vec()
        })
    }
}

/// The wide form of [`strftime`].
pub extern "win64" fn wcsftime(
    buffer: *mut u16,
    max: usize,
    format: *const u16,
    tm: *const Tm,
) -> usize {
    if format.is_null() {
        set_errno(EINVAL);
        return 0;
    }
    // SAFETY: the title's format string.
    let format: Vec<u32> =
        char::decode_utf16(unsafe { super::string::wide_units(format) }.iter().copied())
            .map(|c| u32::from(c.unwrap_or(char::REPLACEMENT_CHARACTER)))
            .collect();
    // SAFETY: the title's buffer of `max` units and `tm`.
    unsafe { strftime_into(buffer, max, &format, tm, |t| t.encode_utf16().collect()) }
}

/// See [`strftime`]; the locale is not used.
pub extern "win64" fn _strftime_l(
    buffer: *mut c_char,
    max: usize,
    format: *const c_char,
    tm: *const Tm,
    _locale: *mut c_void,
) -> usize {
    strftime(buffer, max, format, tm)
}

/// See [`wcsftime`]; the locale is not used.
pub extern "win64" fn _wcsftime_l(
    buffer: *mut u16,
    max: usize,
    format: *const u16,
    tm: *const Tm,
    _locale: *mut c_void,
) -> usize {
    wcsftime(buffer, max, format, tm)
}

/// `asctime`'s text: `Www Mmm dd hh:mm:ss yyyy\n`; `None` for fields out of
/// range.
fn asctime_text(tm: *const Tm) -> Option<String> {
    if tm.is_null() {
        return None;
    }
    // SAFETY: the title's `tm`.
    let tm = unsafe { tm.read_unaligned() };
    let valid = (0..7).contains(&tm.wday)
        && (0..12).contains(&tm.mon)
        && (1..=31).contains(&tm.mday)
        && (0..24).contains(&tm.hour)
        && (0..60).contains(&tm.min)
        && (0..=60).contains(&tm.sec)
        && tm.year >= 0;
    valid.then(|| {
        format!(
            "{} {} {:02} {:02}:{:02}:{:02} {}\n",
            &name(&DAYS, tm.wday)[..3],
            &name(&MONTHS, tm.mon)[..3],
            tm.mday,
            tm.hour,
            tm.min,
            tm.sec,
            i64::from(tm.year) + 1900
        )
    })
}

/// The size `asctime`'s text needs, its NUL included.
const ASCBUFSIZE: usize = 26;

/// Copies `text` (or fails with `EINVAL`) into a buffer of `size` units
/// that must hold 26.
///
/// # Safety
/// `buffer` is null or has `size` units.
unsafe fn write_asctime<T: Copy + Default>(
    buffer: *mut T,
    size: usize,
    text: Option<Vec<T>>,
) -> i32 {
    if buffer.is_null() || size == 0 {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: as the caller says.
    unsafe {
        buffer.write(T::default());
        let Some(text) = text.filter(|t| size >= ASCBUFSIZE && t.len() < size) else {
            set_errno(EINVAL);
            return EINVAL;
        };
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
        buffer.add(text.len()).write(T::default());
    }
    0
}

/// `asctime`'s text of a broken-down time into a buffer of `size` bytes.
pub extern "win64" fn asctime_s(buffer: *mut c_char, size: usize, tm: *const Tm) -> i32 {
    let text = asctime_text(tm).map(String::into_bytes);
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { write_asctime(buffer.cast::<u8>(), size, text) }
}

/// The wide form of [`asctime_s`].
pub extern "win64" fn _wasctime_s(buffer: *mut u16, size: usize, tm: *const Tm) -> i32 {
    let text = asctime_text(tm).map(|t| t.encode_utf16().collect());
    // SAFETY: the title's buffer of `size` units.
    unsafe { write_asctime(buffer, size, text) }
}

/// The broken-down time of `*time`, or `None`.
fn tm_of(time: *const i64) -> Option<Tm> {
    let mut tm = Tm::default();
    (_gmtime64_s(&mut tm, time) == 0).then_some(tm)
}

/// [`asctime_s`] of the local time (UTC) of `*time`.
pub extern "win64" fn _ctime64_s(buffer: *mut c_char, size: usize, time: *const i64) -> i32 {
    let text = tm_of(time)
        .and_then(|tm| asctime_text(&tm))
        .map(String::into_bytes);
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { write_asctime(buffer.cast::<u8>(), size, text) }
}

/// The wide form of [`_ctime64_s`].
pub extern "win64" fn _wctime64_s(buffer: *mut u16, size: usize, time: *const i64) -> i32 {
    let text = tm_of(time)
        .and_then(|tm| asctime_text(&tm))
        .map(|t| t.encode_utf16().collect());
    // SAFETY: the title's buffer of `size` units.
    unsafe { write_asctime(buffer, size, text) }
}

/// Reads the time zone from the environment: there is none (UTC).
pub extern "win64" fn _tzset() {}

static TIMEZONE: AtomicI32 = AtomicI32::new(0);
static DAYLIGHT: AtomicI32 = AtomicI32::new(0);
static DSTBIAS: AtomicI32 = AtomicI32::new(0);
static TZ_NAME: [u8; 4] = *b"UTC\0";
static TZNAME: [AtomicPtr<c_char>; 2] = [
    AtomicPtr::new(TZ_NAME.as_ptr() as *mut c_char),
    AtomicPtr::new(TZ_NAME.as_ptr() as *mut c_char),
];

/// Writes `value` to `out`; `EINVAL` for null.
fn get(out: *mut i32, value: &AtomicI32) -> i32 {
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: the title's place for the value.
    unsafe { out.write_unaligned(value.load(std::sync::atomic::Ordering::Relaxed)) };
    0
}

/// Seconds west of UTC: 0.
pub extern "win64" fn _get_timezone(out: *mut i32) -> i32 {
    get(out, &TIMEZONE)
}

/// Whether daylight saving applies: no.
pub extern "win64" fn _get_daylight(out: *mut i32) -> i32 {
    get(out, &DAYLIGHT)
}

/// The daylight-saving offset in seconds: 0.
pub extern "win64" fn _get_dstbias(out: *mut i32) -> i32 {
    get(out, &DSTBIAS)
}

/// The zone's name ("UTC" for both the standard and daylight one) in a
/// buffer of `size` bytes, its length with the NUL in `*len`.
pub extern "win64" fn _get_tzname(
    len: *mut usize,
    buffer: *mut c_char,
    size: usize,
    index: i32,
) -> i32 {
    if (buffer.is_null() && size != 0) || !(0..=1).contains(&index) {
        set_errno(EINVAL);
        return EINVAL;
    }
    if len.is_null() && buffer.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    if !len.is_null() {
        // SAFETY: the title's place for the length.
        unsafe { len.write_unaligned(TZ_NAME.len()) };
    }
    if buffer.is_null() {
        return 0;
    }
    if size < TZ_NAME.len() {
        set_errno(ERANGE);
        return ERANGE;
    }
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(TZ_NAME.as_ptr(), buffer.cast(), TZ_NAME.len()) };
    0
}

/// `_daylight`.
pub extern "win64" fn __daylight() -> *mut i32 {
    DAYLIGHT.as_ptr()
}

/// `_timezone`.
pub extern "win64" fn __timezone() -> *mut i32 {
    TIMEZONE.as_ptr()
}

/// `_dstbias`.
pub extern "win64" fn __dstbias() -> *mut i32 {
    DSTBIAS.as_ptr()
}

/// `_tzname`: two pointers to "UTC" (which the title must not write).
pub extern "win64" fn __tzname() -> *mut *mut c_char {
    TZNAME.as_ptr().cast_mut().cast()
}

/// The time now with milliseconds; `EINVAL` for null.
pub extern "win64" fn _ftime64_s(out: *mut Timeb) -> i32 {
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    let (time, millis) = now();
    // SAFETY: the title's place for the time.
    unsafe {
        out.write_unaligned(Timeb {
            time,
            millitm: millis as u16,
            timezone: 0,
            dstflag: 0,
        })
    };
    0
}

/// See [`_ftime64_s`].
pub extern "win64" fn _ftime32_s(out: *mut Timeb32) -> i32 {
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    let (time, millis) = now();
    // SAFETY: the title's place for the time.
    unsafe {
        out.write_unaligned(Timeb32 {
            time: time as i32,
            millitm: millis as u16,
            timezone: 0,
            dstflag: 0,
        })
    };
    0
}

/// The time now (UTC) in `*tm`; its milliseconds.
pub extern "win64" fn _getsystime(tm: *mut Tm) -> u32 {
    let (time, millis) = now();
    if !tm.is_null() {
        // SAFETY: the title's `tm`.
        unsafe { tm.write_unaligned(broken_down(time)) };
    }
    millis
}

/// Would set the system's clock: not allowed (`EPERM`).
pub extern "win64" fn _setsystime(_tm: *mut Tm, _millis: u32) -> i32 {
    EPERM
}

pub(crate) fn exports() -> Vec<HleExport> {
    // The process's start, for `clock`.
    ANCHOR.get_or_init(Instant::now);
    vec![
        hle_export!(_time64),
        hle_export!(_time32),
        hle_export!(clock),
        hle_export!(_localtime64_s),
        hle_export!(_gmtime64_s),
        hle_export!(_localtime64),
        hle_export!(_gmtime64),
        hle_export!(_localtime32_s),
        hle_export!(_gmtime32_s),
        hle_export!(_mktime64),
        hle_export!(_mkgmtime64),
        hle_export!(_mktime32),
        hle_export!(_mkgmtime32),
        hle_export!(_difftime64),
        hle_export!(_difftime32),
        hle_export!(strftime),
        hle_export!(wcsftime),
        hle_export!(_strftime_l),
        hle_export!(_wcsftime_l),
        hle_export!(asctime_s),
        hle_export!(_ctime64_s),
        hle_export!(_wasctime_s),
        hle_export!(_wctime64_s),
        hle_export!(_tzset),
        hle_export!(_get_timezone),
        hle_export!(_get_daylight),
        hle_export!(_get_dstbias),
        hle_export!(_get_tzname),
        hle_export!(__daylight),
        hle_export!(__timezone),
        hle_export!(__tzname),
        hle_export!(__dstbias),
        hle_export!(_ftime64_s),
        hle_export!(_ftime32_s),
        hle_export!(_getsystime),
        hle_export!(_setsystime),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn gm(time: i64) -> Tm {
        let mut tm = Tm::default();
        assert_eq!(_gmtime64_s(&mut tm, &time), 0);
        tm
    }

    #[test]
    fn broken_down_times_are_utc() {
        let epoch = gm(0);
        assert_eq!(
            (epoch.year, epoch.mon, epoch.mday, epoch.wday, epoch.yday),
            (70, 0, 1, 4, 0)
        );
        let leap = gm(951_782_400);
        assert_eq!((leap.year, leap.mon, leap.mday), (100, 1, 29));
        assert_eq!((leap.wday, leap.yday), (2, 59));
        let mut tm = Tm::default();
        assert_eq!(_gmtime64_s(&mut tm, &-1), EINVAL);
        assert_eq!(tm.year, -1);
        assert_eq!(_gmtime64_s(&mut tm, &(MAX_TIME64 + 1)), EINVAL);
        assert_eq!(_gmtime64_s(&mut tm, std::ptr::null()), EINVAL);
        assert_eq!(_gmtime64_s(std::ptr::null_mut(), &0), EINVAL);
        // SAFETY: the per-thread `tm`.
        assert_eq!(unsafe { *_localtime64(&86_400) }.mday, 2);
    }

    #[test]
    fn mktime_round_trips_and_normalizes() {
        for t in [0, 951_782_400, 1_700_000_000, MAX_TIME64] {
            let mut tm = gm(t);
            assert_eq!(_mktime64(&mut tm), t);
        }
        let mut tm = Tm {
            year: 99,
            mon: 12,
            mday: 1,
            ..Tm::default()
        };
        let t = _mktime64(&mut tm);
        assert_eq!((tm.year, tm.mon, tm.mday), (100, 0, 1));
        assert_eq!(t, 946_684_800);
        assert_eq!(tm.wday, 6);
        let mut tm = Tm {
            year: 100,
            mon: 0,
            mday: 1,
            sec: -1,
            ..Tm::default()
        };
        assert_eq!(_mkgmtime64(&mut tm), 946_684_799);
        assert_eq!((tm.year, tm.mon, tm.mday, tm.hour), (99, 11, 31, 23));
        let mut early = Tm {
            year: 60,
            mday: 1,
            ..Tm::default()
        };
        assert_eq!(_mktime64(&mut early), -1);
        assert_eq!(_difftime64(10, 4), 6.0);
    }

    fn format(spec: &CStr, t: i64) -> String {
        let tm = gm(t);
        let mut buffer = [0 as c_char; 128];
        let n = strftime(buffer.as_mut_ptr(), 128, spec.as_ptr(), &tm);
        // SAFETY: NUL-terminated by `strftime`.
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(n, text.len());
        text
    }

    #[test]
    fn strftime_formats() {
        let t = 951_782_400 + 13 * 3600 + 5 * 60 + 9;
        assert_eq!(format(c"%Y-%m-%d %H:%M:%S", t), "2000-02-29 13:05:09");
        assert_eq!(
            format(c"%a %A %b %B %p %I", t),
            "Tue Tuesday Feb February PM 01"
        );
        assert_eq!(format(c"%j %e %y %C %u %w", t), "060 29 00 20 2 2");
        assert_eq!(
            format(c"%c|%x|%X", t),
            "02/29/00 13:05:09|02/29/00|13:05:09"
        );
        assert_eq!(
            format(c"%F %T %R %D %%", t),
            "2000-02-29 13:05:09 13:05 02/29/00 %"
        );
        assert_eq!(format(c"[%Z] %z %s", t), format!("[] +0000 {t}"));
        // 2005-01-01 is in ISO week 53 of 2004.
        assert_eq!(format(c"%G %g %V %U %W", 1_104_537_600), "2004 04 53 00 00");
        assert_eq!(format(c"%#d", 86_400), "2");
        let tm = gm(0);
        let mut small = [1 as c_char; 4];
        assert_eq!(strftime(small.as_mut_ptr(), 4, c"%Y".as_ptr(), &tm), 0);
        assert_eq!(small[0], 0);
        let mut buffer = [0u16; 16];
        let spec: Vec<u16> = "%H:%M".encode_utf16().chain(Some(0)).collect();
        assert_eq!(wcsftime(buffer.as_mut_ptr(), 16, spec.as_ptr(), &tm), 5);
    }

    #[test]
    fn asctime_and_zone_settings() {
        let mut buffer = [0 as c_char; 26];
        assert_eq!(_ctime64_s(buffer.as_mut_ptr(), 26, &951_782_400), 0);
        // SAFETY: NUL-terminated by `_ctime64_s`.
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        assert_eq!(text.to_str(), Ok("Tue Feb 29 00:00:00 2000\n"));
        assert_eq!(asctime_s(buffer.as_mut_ptr(), 25, &gm(0)), EINVAL);
        let mut zone = 5;
        assert_eq!(_get_timezone(&mut zone), 0);
        assert_eq!(zone, 0);
        let mut len = 0;
        let mut name = [0 as c_char; 8];
        assert_eq!(_get_tzname(&mut len, name.as_mut_ptr(), 8, 0), 0);
        assert_eq!(len, 4);
        // SAFETY: two pointers to "UTC".
        assert_eq!(
            unsafe { CStr::from_ptr(*__tzname().add(1)) }.to_str(),
            Ok("UTC")
        );
        assert!(_time64(std::ptr::null_mut()) > 1_700_000_000);
        assert!(clock() >= 0);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 36);
    }
}
