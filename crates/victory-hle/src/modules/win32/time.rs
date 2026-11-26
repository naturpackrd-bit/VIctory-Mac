//! Clocks (the performance counter at 10 MHz as modern Windows', tick
//! counts, file times since 1601, system times), and what Windows tells a
//! title about the machine it runs on (processors, their features, the
//! version of Windows, its directories and the computer's name).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use victory_platform::system::System;
use victory_platform::{Host, Memory};

use crate::{HleExport, hle_export};

type BOOL = i32;
type DWORD = u32;
type UINT = u32;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_BUFFER_OVERFLOW: u32 = 111;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_MORE_DATA: u32 = 234;

/// Sets this thread's last error in its TEB; nothing on a thread without one.
fn set_last_error(code: u32) {
    if let Some(teb) = victory_loader::teb::current_teb() {
        // SAFETY: this thread's own TEB, which only this thread writes.
        unsafe { (*(teb as *mut victory_loader::teb::Teb)).last_error = code };
    }
}

// The clocks.

/// The performance counter's rate: 100-nanosecond ticks, as on modern
/// Windows.
const COUNTER_FREQUENCY: i64 = 10_000_000;

/// When the clocks started: the first time one was read.
fn anchor() -> Instant {
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    *ANCHOR.get_or_init(Instant::now)
}

/// 100-nanosecond units since [`anchor`].
fn hundred_ns_since_anchor() -> u64 {
    (anchor().elapsed().as_nanos() / 100) as u64
}

/// 100-nanosecond ticks since the clocks started.
pub extern "win64" fn QueryPerformanceCounter(out: *mut i64) -> BOOL {
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    // SAFETY: the title's out parameter, checked non-null.
    unsafe { out.write_unaligned(hundred_ns_since_anchor() as i64) };
    TRUE
}

/// 10 MHz.
pub extern "win64" fn QueryPerformanceFrequency(out: *mut i64) -> BOOL {
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    // SAFETY: the title's out parameter, checked non-null.
    unsafe { out.write_unaligned(COUNTER_FREQUENCY) };
    TRUE
}

/// Milliseconds since the clocks started, wrapping at 2^32 as Windows'.
pub extern "win64" fn GetTickCount() -> DWORD {
    anchor().elapsed().as_millis() as u32
}

/// Milliseconds since the clocks started.
pub extern "win64" fn GetTickCount64() -> u64 {
    anchor().elapsed().as_millis() as u64
}

/// 100-nanosecond units since the clocks started (nothing is suspended
/// here, so biased and unbiased time are the same).
pub extern "win64" fn QueryUnbiasedInterruptTime(out: *mut u64) -> BOOL {
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    // SAFETY: the title's out parameter, checked non-null.
    unsafe { out.write_unaligned(hundred_ns_since_anchor()) };
    TRUE
}

/// See [`QueryUnbiasedInterruptTime`] (Windows returns nothing here).
pub extern "win64" fn QueryInterruptTime(out: *mut u64) {
    if !out.is_null() {
        // SAFETY: the title's out parameter, checked non-null.
        unsafe { out.write_unaligned(hundred_ns_since_anchor()) };
    }
}

/// 100-nanosecond intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFFERENCE: u64 = 116_444_736_000_000_000;
/// Days between 1601-01-01 and 1970-01-01.
const EPOCH_DAYS: i64 = 134_774;
const UNITS_PER_SECOND: u64 = 10_000_000;
const UNITS_PER_MILLISECOND: u64 = 10_000;
const SECONDS_PER_DAY: u64 = 86_400;

/// 100-nanosecond intervals since 1601-01-01 (UTC), split in two halves.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FILETIME {
    pub low: u32,
    pub high: u32,
}

impl FILETIME {
    fn new(value: u64) -> Self {
        FILETIME {
            low: value as u32,
            high: (value >> 32) as u32,
        }
    }

    fn value(self) -> u64 {
        u64::from(self.high) << 32 | u64::from(self.low)
    }
}

/// A calendar date and time.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SYSTEMTIME {
    pub year: u16,
    pub month: u16,
    /// 0 is Sunday.
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

/// Now, as a file time.
fn now() -> u64 {
    // A host clock before 1970 reads as 1970.
    let since_1970 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    EPOCH_DIFFERENCE + (since_1970.as_nanos() / 100) as u64
}

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

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// The calendar date and time of a file time; `None` past Windows' limit
/// (year 30827).
fn system_time(file_time: u64) -> Option<SYSTEMTIME> {
    if file_time > i64::MAX as u64 {
        return None;
    }
    let seconds = file_time / UNITS_PER_SECOND;
    let milliseconds = (file_time / UNITS_PER_MILLISECOND % 1000) as u16;
    let days = (seconds / SECONDS_PER_DAY) as i64 - EPOCH_DAYS;
    let in_day = seconds % SECONDS_PER_DAY;
    let (year, month, day) = civil_from_days(days);
    Some(SYSTEMTIME {
        year: u16::try_from(year).ok()?,
        month: month as u16,
        day_of_week: (days + 4).rem_euclid(7) as u16,
        day: day as u16,
        hour: (in_day / 3600) as u16,
        minute: (in_day / 60 % 60) as u16,
        second: (in_day % 60) as u16,
        milliseconds,
    })
}

/// The file time of a calendar date and time; `None` for one that is not
/// a valid date from 1601 to 30827 (the day of the week is not checked, as
/// on Windows).
fn file_time(st: &SYSTEMTIME) -> Option<u64> {
    let year = i64::from(st.year);
    let month = u32::from(st.month);
    let valid = (1601..=30827).contains(&year)
        && (1..=12).contains(&month)
        && st.day >= 1
        && u32::from(st.day) <= days_in_month(year, month)
        && st.hour < 24
        && st.minute < 60
        && st.second < 60
        && st.milliseconds < 1000;
    if !valid {
        return None;
    }
    let days = (days_from_civil(year, month, u32::from(st.day)) + EPOCH_DAYS) as u64;
    let seconds = days * SECONDS_PER_DAY
        + u64::from(st.hour) * 3600
        + u64::from(st.minute) * 60
        + u64::from(st.second);
    Some(seconds * UNITS_PER_SECOND + u64::from(st.milliseconds) * UNITS_PER_MILLISECOND)
}

/// Now (UTC), as a file time.
pub extern "win64" fn GetSystemTimeAsFileTime(out: *mut FILETIME) {
    if !out.is_null() {
        // SAFETY: the title's out parameter, checked non-null.
        unsafe { out.write_unaligned(FILETIME::new(now())) };
    }
}

/// See [`GetSystemTimeAsFileTime`] (the host's clock is as precise).
pub extern "win64" fn GetSystemTimePreciseAsFileTime(out: *mut FILETIME) {
    GetSystemTimeAsFileTime(out);
}

/// Now (UTC), as a calendar date and time.
pub extern "win64" fn GetSystemTime(out: *mut SYSTEMTIME) {
    if out.is_null() {
        return;
    }
    if let Some(st) = system_time(now()) {
        // SAFETY: the title's out parameter, checked non-null.
        unsafe { out.write_unaligned(st) };
    }
}

/// Now in UTC too: there is no time zone yet.
pub extern "win64" fn GetLocalTime(out: *mut SYSTEMTIME) {
    GetSystemTime(out);
}

/// A file time as a calendar date and time.
pub extern "win64" fn FileTimeToSystemTime(ft: *const FILETIME, out: *mut SYSTEMTIME) -> BOOL {
    if ft.is_null() || out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    // SAFETY: the title's file time, checked non-null.
    let value = unsafe { ft.read_unaligned() }.value();
    let Some(st) = system_time(value) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    };
    // SAFETY: the title's out parameter, checked non-null.
    unsafe { out.write_unaligned(st) };
    TRUE
}

/// A calendar date and time as a file time.
pub extern "win64" fn SystemTimeToFileTime(st: *const SYSTEMTIME, out: *mut FILETIME) -> BOOL {
    if st.is_null() || out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    // SAFETY: the title's system time, checked non-null.
    let Some(value) = file_time(&unsafe { st.read_unaligned() }) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    };
    // SAFETY: the title's out parameter, checked non-null.
    unsafe { out.write_unaligned(FILETIME::new(value)) };
    TRUE
}

/// A time zone: its offset from UTC and when daylight time starts and ends.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TIME_ZONE_INFORMATION {
    pub bias: i32,
    pub standard_name: [u16; 32],
    pub standard_date: SYSTEMTIME,
    pub standard_bias: i32,
    pub daylight_name: [u16; 32],
    pub daylight_date: SYSTEMTIME,
    pub daylight_bias: i32,
}

const _: () = assert!(std::mem::size_of::<TIME_ZONE_INFORMATION>() == 172);

/// No time zone is known yet: what it reports is UTC.
const TIME_ZONE_ID_UNKNOWN: u32 = 0;
const TIME_ZONE_ID_INVALID: u32 = u32::MAX;

/// UTC, with no daylight time.
pub extern "win64" fn GetTimeZoneInformation(out: *mut TIME_ZONE_INFORMATION) -> DWORD {
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return TIME_ZONE_ID_INVALID;
    }
    // SAFETY: the title's out parameter, checked non-null; zero is a value
    // of every field.
    unsafe { out.write_unaligned(std::mem::zeroed()) };
    TIME_ZONE_ID_UNKNOWN
}

/// The processors' idle, kernel and user times: not kept, all zero.
pub extern "win64" fn GetSystemTimes(
    idle: *mut FILETIME,
    kernel: *mut FILETIME,
    user: *mut FILETIME,
) -> BOOL {
    for out in [idle, kernel, user] {
        if !out.is_null() {
            // SAFETY: the title's out parameter, checked non-null.
            unsafe { out.write_unaligned(FILETIME::default()) };
        }
    }
    TRUE
}

// The machine.

/// Logical processors, at most 64: one processor group's worth.
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

/// The machine, as `GetSystemInfo` reports it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SYSTEM_INFO {
    pub processor_architecture: u16,
    pub reserved: u16,
    pub page_size: u32,
    pub minimum_application_address: *mut c_void,
    pub maximum_application_address: *mut c_void,
    pub active_processor_mask: usize,
    pub number_of_processors: u32,
    pub processor_type: u32,
    pub allocation_granularity: u32,
    pub processor_level: u16,
    pub processor_revision: u16,
}

const _: () = assert!(std::mem::size_of::<SYSTEM_INFO>() == 48);

const PROCESSOR_ARCHITECTURE_AMD64: u16 = 9;
const PROCESSOR_AMD_X8664: u32 = 8664;

/// An AMD64 machine with the host's processors, as the console's Zen 2
/// (family 23) reports itself.
pub extern "win64" fn GetSystemInfo(out: *mut SYSTEM_INFO) {
    if out.is_null() {
        return;
    }
    let info = SYSTEM_INFO {
        processor_architecture: PROCESSOR_ARCHITECTURE_AMD64,
        reserved: 0,
        page_size: Host::page_size() as u32,
        minimum_application_address: 0x10000 as *mut c_void,
        maximum_application_address: 0x7FFF_FFFE_FFFF as *mut c_void,
        active_processor_mask: all_processors(),
        number_of_processors: processors() as u32,
        processor_type: PROCESSOR_AMD_X8664,
        allocation_granularity: 0x10000,
        processor_level: 23,
        processor_revision: 0x7100,
    };
    // SAFETY: the title's out parameter, checked non-null.
    unsafe { out.write_unaligned(info) };
}

/// See [`GetSystemInfo`]: the title runs natively on it.
pub extern "win64" fn GetNativeSystemInfo(out: *mut SYSTEM_INFO) {
    GetSystemInfo(out);
}

/// Whether the processor running us has an x86 feature; none off x86-64.
#[cfg(target_arch = "x86_64")]
macro_rules! detected {
    ($feature:tt) => {
        std::arch::is_x86_feature_detected!($feature)
    };
}

#[cfg(not(target_arch = "x86_64"))]
macro_rules! detected {
    ($feature:tt) => {
        false
    };
}

/// The features x86-64 always has, and those the processor running us
/// reports.
pub extern "win64" fn IsProcessorFeaturePresent(feature: DWORD) -> BOOL {
    let present = match feature {
        // XMMI (SSE), XMMI64 (SSE2), NX, SSE3, RDTSC, FASTFAIL.
        6 | 10 | 12 | 13 | 8 | 23 => true,
        14 => detected!("cmpxchg16b"),
        // XSAVE, as Windows enables it for AVX.
        17 => detected!("avx"),
        // RDWRFSGSBASE: titles must not write the segment bases.
        22 => false,
        36 => detected!("ssse3"),
        37 => detected!("sse4.1"),
        38 => detected!("sse4.2"),
        39 => detected!("avx"),
        40 => detected!("avx2"),
        41 => detected!("avx512f"),
        _ => false,
    };
    BOOL::from(present)
}

/// Every processor, in group 0 (or all groups, 0xFFFF).
pub extern "win64" fn GetActiveProcessorCount(group: u16) -> DWORD {
    match group {
        0 | 0xFFFF => processors() as u32,
        _ => 0,
    }
}

/// See [`GetActiveProcessorCount`].
pub extern "win64" fn GetMaximumProcessorCount(group: u16) -> DWORD {
    GetActiveProcessorCount(group)
}

/// One processor group.
pub extern "win64" fn GetActiveProcessorGroupCount() -> u16 {
    1
}

/// Always processor 0: the host does not say which one runs us.
pub extern "win64" fn GetCurrentProcessorNumber() -> DWORD {
    0
}

/// A processor's group and number in it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PROCESSOR_NUMBER {
    pub group: u16,
    pub number: u8,
    pub reserved: u8,
}

/// Group 0, processor 0 (see [`GetCurrentProcessorNumber`]).
pub extern "win64" fn GetCurrentProcessorNumberEx(out: *mut PROCESSOR_NUMBER) {
    if !out.is_null() {
        // SAFETY: the title's out parameter, checked non-null.
        unsafe { out.write_unaligned(PROCESSOR_NUMBER::default()) };
    }
}

const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_NUMA_NODE: u32 = 1;
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
const RELATION_GROUP: u32 = 4;
const RELATION_ALL: u32 = 0xFFFF;

/// One relation of processors, as `GetLogicalProcessorInformation` lists
/// them.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SYSTEM_LOGICAL_PROCESSOR_INFORMATION {
    pub processor_mask: usize,
    pub relationship: u32,
    /// The kind's data: a core's flags, a node's number; zero here but for
    /// that.
    pub union: [u64; 2],
}

const _: () = assert!(std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>() == 32);

/// Which processors `relationship` relations cover: one core per processor
/// (each its own), then the one NUMA node and the one package (every
/// processor).
fn relations() -> Vec<(u32, usize)> {
    let mut list: Vec<(u32, usize)> = (0..processors())
        .map(|i| (RELATION_PROCESSOR_CORE, 1 << i))
        .collect();
    list.push((RELATION_NUMA_NODE, all_processors()));
    list.push((RELATION_PROCESSOR_PACKAGE, all_processors()));
    list
}

/// Checks the title's buffer of `*len` bytes against `needed`: false (with
/// `*len` set and the last error) if it is too small.
fn fits(buffer: *mut c_void, len: *mut DWORD, needed: usize, too_small: u32) -> Option<bool> {
    if len.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return None;
    }
    // SAFETY: the title's in-out length, checked non-null.
    let room = unsafe { len.read_unaligned() } as usize;
    // SAFETY: as above.
    unsafe { len.write_unaligned(needed as u32) };
    if (buffer.is_null() && needed > 0) || room < needed {
        set_last_error(too_small);
        return Some(false);
    }
    Some(true)
}

/// The cores, the NUMA node and the package.
pub extern "win64" fn GetLogicalProcessorInformation(
    buffer: *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION,
    len: *mut DWORD,
) -> BOOL {
    let list = relations();
    let entry = std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
    if fits(
        buffer.cast(),
        len,
        list.len() * entry,
        ERROR_INSUFFICIENT_BUFFER,
    ) != Some(true)
    {
        return FALSE;
    }
    for (i, &(relationship, processor_mask)) in list.iter().enumerate() {
        let info = SYSTEM_LOGICAL_PROCESSOR_INFORMATION {
            processor_mask,
            relationship,
            // A core's flags (no SMT pair is told) and node 0's number.
            union: [0; 2],
        };
        // SAFETY: the title's buffer, checked to hold every entry.
        unsafe { buffer.add(i).write_unaligned(info) };
    }
    TRUE
}

/// A set of processors in one group.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GROUP_AFFINITY {
    pub mask: usize,
    pub group: u16,
    pub reserved: [u16; 3],
}

/// A core's or a package's processors.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PROCESSOR_RELATIONSHIP {
    pub flags: u8,
    pub efficiency_class: u8,
    pub reserved: [u8; 20],
    pub group_count: u16,
    pub group_mask: [GROUP_AFFINITY; 1],
}

/// A NUMA node's processors.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NUMA_NODE_RELATIONSHIP {
    pub node_number: u32,
    pub reserved: [u8; 20],
    pub group_mask: GROUP_AFFINITY,
}

/// One `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` entry: its kind, its size
/// in bytes, then the kind's data.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct EntryEx<T> {
    relationship: u32,
    size: u32,
    data: T,
}

const PROCESSOR_ENTRY: usize = std::mem::size_of::<EntryEx<PROCESSOR_RELATIONSHIP>>();
const NODE_ENTRY: usize = std::mem::size_of::<EntryEx<NUMA_NODE_RELATIONSHIP>>();

/// The relations of [`relations`] as `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`
/// entries, filtered by `relationship`. Caches and groups are not
/// described (no entries for them).
pub extern "win64" fn GetLogicalProcessorInformationEx(
    relationship: u32,
    buffer: *mut c_void,
    len: *mut DWORD,
) -> BOOL {
    if ![
        RELATION_PROCESSOR_CORE,
        RELATION_NUMA_NODE,
        RELATION_CACHE,
        RELATION_PROCESSOR_PACKAGE,
        RELATION_GROUP,
        RELATION_ALL,
    ]
    .contains(&relationship)
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    let list: Vec<(u32, usize)> = relations()
        .into_iter()
        .filter(|&(kind, _)| relationship == RELATION_ALL || relationship == kind)
        .collect();
    let size = |kind: u32| match kind {
        RELATION_NUMA_NODE => NODE_ENTRY,
        _ => PROCESSOR_ENTRY,
    };
    let needed = list.iter().map(|&(kind, _)| size(kind)).sum();
    if fits(buffer, len, needed, ERROR_INSUFFICIENT_BUFFER) != Some(true) {
        return FALSE;
    }
    let mut at = buffer.cast::<u8>();
    for (kind, mask) in list {
        let group_mask = GROUP_AFFINITY {
            mask,
            ..Default::default()
        };
        // SAFETY: the title's buffer, checked to hold every entry; entries
        // need not be aligned.
        unsafe {
            if kind == RELATION_NUMA_NODE {
                at.cast::<EntryEx<NUMA_NODE_RELATIONSHIP>>()
                    .write_unaligned(EntryEx {
                        relationship: kind,
                        size: NODE_ENTRY as u32,
                        data: NUMA_NODE_RELATIONSHIP {
                            group_mask,
                            ..Default::default()
                        },
                    });
            } else {
                at.cast::<EntryEx<PROCESSOR_RELATIONSHIP>>()
                    .write_unaligned(EntryEx {
                        relationship: kind,
                        size: PROCESSOR_ENTRY as u32,
                        data: PROCESSOR_RELATIONSHIP {
                            group_count: 1,
                            group_mask: [group_mask],
                            ..Default::default()
                        },
                    });
            }
            at = at.add(size(kind));
        }
    }
    TRUE
}

// Windows itself.

const MAJOR_VERSION: u32 = 10;
const MINOR_VERSION: u32 = 0;
const BUILD_NUMBER: u32 = 19044;
const VER_PLATFORM_WIN32_NT: u32 = 2;
const VER_SUITE_SINGLEUSERTS: u16 = 0x100;
const VER_NT_WORKSTATION: u8 = 1;
const PRODUCT_PROFESSIONAL: u32 = 0x30;
const STATUS_SUCCESS: i32 = 0;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;

/// Windows 10 (build 19044), packed as `GetVersion` does: the major
/// version in the low byte, the minor in the next, the build in the high
/// word.
pub extern "win64" fn GetVersion() -> DWORD {
    MAJOR_VERSION | MINOR_VERSION << 8 | BUILD_NUMBER << 16
}

/// The version of Windows, `OSVERSIONINFOW` and the `EX` fields after it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct OSVERSIONINFOEXW {
    pub length: u32,
    pub major_version: u32,
    pub minor_version: u32,
    pub build_number: u32,
    pub platform_id: u32,
    pub csd_version: [u16; 128],
    pub service_pack_major: u16,
    pub service_pack_minor: u16,
    pub suite_mask: u16,
    pub product_type: u8,
    pub reserved: u8,
}

const VERSION_INFO: usize = 276;
const VERSION_INFO_EX: usize = 284;
const _: () = assert!(std::mem::size_of::<OSVERSIONINFOEXW>() == VERSION_INFO_EX);

/// Fills in Windows 10's version in the title's struct of either size;
/// false for any other size.
fn version_info(out: *mut OSVERSIONINFOEXW) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: the title's struct, whose `length` it set.
    let length = unsafe { (*out).length } as usize;
    if length != VERSION_INFO && length != VERSION_INFO_EX {
        return false;
    }
    let info = OSVERSIONINFOEXW {
        length: length as u32,
        major_version: MAJOR_VERSION,
        minor_version: MINOR_VERSION,
        build_number: BUILD_NUMBER,
        platform_id: VER_PLATFORM_WIN32_NT,
        csd_version: [0; 128],
        service_pack_major: 0,
        service_pack_minor: 0,
        suite_mask: VER_SUITE_SINGLEUSERTS,
        product_type: VER_NT_WORKSTATION,
        reserved: 0,
    };
    // SAFETY: `length` bytes of the title's struct, from a struct at least
    // that long.
    unsafe {
        std::ptr::copy_nonoverlapping((&raw const info).cast::<u8>(), out.cast::<u8>(), length)
    };
    true
}

/// Windows 10, build 19044.
pub extern "win64" fn GetVersionExW(out: *mut OSVERSIONINFOEXW) -> BOOL {
    if version_info(out) {
        TRUE
    } else {
        set_last_error(ERROR_INVALID_PARAMETER);
        FALSE
    }
}

/// See [`GetVersionExW`] (ntdll's, an `NTSTATUS`).
pub extern "win64" fn RtlGetVersion(out: *mut OSVERSIONINFOEXW) -> i32 {
    if version_info(out) {
        STATUS_SUCCESS
    } else {
        STATUS_INVALID_PARAMETER
    }
}

/// Windows 10 Pro, whatever version is asked about.
pub extern "win64" fn GetProductInfo(
    _os_major: DWORD,
    _os_minor: DWORD,
    _sp_major: DWORD,
    _sp_minor: DWORD,
    product_type: *mut DWORD,
) -> BOOL {
    if product_type.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    // SAFETY: the title's out parameter, checked non-null.
    unsafe { product_type.write_unaligned(PRODUCT_PROFESSIONAL) };
    TRUE
}

/// Copies `text` and a NUL into the title's buffer of `len` characters:
/// the length without the NUL, or, if it does not fit, the length with it
/// (nothing written), as the directory functions do.
fn copy_directory(text: &str, buffer: *mut u16, len: u32) -> UINT {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    if buffer.is_null() || (len as usize) < wide.len() {
        return wide.len() as u32;
    }
    // SAFETY: the title's buffer of `len` characters, checked to hold them.
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), buffer, wide.len()) };
    (wide.len() - 1) as u32
}

const SYSTEM_DIRECTORY: &str = r"C:\Windows\System32";
const WINDOWS_DIRECTORY: &str = r"C:\Windows";
const COMPUTER_NAME: &str = "VICTORY";

/// `C:\Windows\System32`.
pub extern "win64" fn GetSystemDirectoryW(buffer: *mut u16, len: UINT) -> UINT {
    copy_directory(SYSTEM_DIRECTORY, buffer, len)
}

/// `C:\Windows`.
pub extern "win64" fn GetWindowsDirectoryW(buffer: *mut u16, len: UINT) -> UINT {
    copy_directory(WINDOWS_DIRECTORY, buffer, len)
}

/// `C:\Windows`.
pub extern "win64" fn GetSystemWindowsDirectoryW(buffer: *mut u16, len: UINT) -> UINT {
    copy_directory(WINDOWS_DIRECTORY, buffer, len)
}

/// Copies the computer's name and a NUL into the title's buffer of `*len`
/// characters: `*len` becomes the length without the NUL, or, if it does
/// not fit, the length with it (and `too_small` the last error).
fn computer_name(buffer: *mut u16, len: *mut DWORD, too_small: u32) -> BOOL {
    let wide: Vec<u16> = COMPUTER_NAME.encode_utf16().chain([0]).collect();
    let Some(fits) = fits(buffer.cast(), len, wide.len(), too_small) else {
        return FALSE;
    };
    if !fits {
        return FALSE;
    }
    // SAFETY: the title's buffer, checked to hold the name and its NUL; then
    // its in-out length, checked non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), buffer, wide.len());
        len.write_unaligned((wide.len() - 1) as u32);
    }
    TRUE
}

/// `VICTORY`.
pub extern "win64" fn GetComputerNameW(buffer: *mut u16, len: *mut DWORD) -> BOOL {
    computer_name(buffer, len, ERROR_BUFFER_OVERFLOW)
}

/// `VICTORY`, for every kind of name.
pub extern "win64" fn GetComputerNameExW(_kind: i32, buffer: *mut u16, len: *mut DWORD) -> BOOL {
    computer_name(buffer, len, ERROR_MORE_DATA)
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(QueryPerformanceCounter),
        hle_export!(QueryPerformanceFrequency),
        hle_export!(GetTickCount),
        hle_export!(GetTickCount64),
        hle_export!(QueryUnbiasedInterruptTime),
        hle_export!(QueryInterruptTime),
        hle_export!(GetSystemTimeAsFileTime),
        hle_export!(GetSystemTimePreciseAsFileTime),
        hle_export!(GetSystemTime),
        hle_export!(GetLocalTime),
        hle_export!(FileTimeToSystemTime),
        hle_export!(SystemTimeToFileTime),
        hle_export!(GetTimeZoneInformation),
        hle_export!(GetSystemTimes),
        hle_export!(GetSystemInfo),
        hle_export!(GetNativeSystemInfo),
        hle_export!(IsProcessorFeaturePresent),
        hle_export!(GetActiveProcessorCount),
        hle_export!(GetMaximumProcessorCount),
        hle_export!(GetActiveProcessorGroupCount),
        hle_export!(GetCurrentProcessorNumber),
        hle_export!(GetCurrentProcessorNumberEx),
        hle_export!(GetLogicalProcessorInformation),
        hle_export!(GetLogicalProcessorInformationEx),
        hle_export!(GetVersion),
        hle_export!(GetVersionExW),
        hle_export!(RtlGetVersion),
        hle_export!(GetProductInfo),
        hle_export!(GetSystemDirectoryW),
        hle_export!(GetWindowsDirectoryW),
        hle_export!(GetSystemWindowsDirectoryW),
        hle_export!(GetComputerNameW),
        hle_export!(GetComputerNameExW),
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

    #[test]
    fn the_counter_runs_at_ten_megahertz() {
        let mut frequency = 0;
        assert_eq!(QueryPerformanceFrequency(&mut frequency), TRUE);
        assert_eq!(frequency, 10_000_000);
        let (mut first, mut second) = (0, 0);
        assert_eq!(QueryPerformanceCounter(&mut first), TRUE);
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(QueryPerformanceCounter(&mut second), TRUE);
        let elapsed = second - first;
        assert!((90_000..1_000_000).contains(&elapsed), "{elapsed}");
        assert!(GetTickCount64() >= 10);
    }

    #[test]
    fn file_times_round_trip() {
        let leap_day_after = SYSTEMTIME {
            year: 2000,
            month: 3,
            day: 1,
            ..Default::default()
        };
        let mut ft = FILETIME::default();
        assert_eq!(SystemTimeToFileTime(&leap_day_after, &mut ft), TRUE);
        let mut back = SYSTEMTIME::default();
        assert_eq!(FileTimeToSystemTime(&ft, &mut back), TRUE);
        assert_eq!(
            back,
            SYSTEMTIME {
                day_of_week: 3,
                ..leap_day_after
            }
        );

        let epoch = SYSTEMTIME {
            year: 1970,
            month: 1,
            day: 1,
            ..Default::default()
        };
        assert_eq!(SystemTimeToFileTime(&epoch, &mut ft), TRUE);
        assert_eq!(ft.value(), EPOCH_DIFFERENCE);

        let precise = SYSTEMTIME {
            year: 2024,
            month: 2,
            day: 29,
            hour: 23,
            minute: 59,
            second: 58,
            milliseconds: 999,
            day_of_week: 4,
        };
        assert_eq!(SystemTimeToFileTime(&precise, &mut ft), TRUE);
        assert_eq!(FileTimeToSystemTime(&ft, &mut back), TRUE);
        assert_eq!(back, precise);

        victory_loader::teb::attach_current_thread();
        let not_a_day = SYSTEMTIME {
            day: 29,
            month: 2,
            year: 2023,
            ..Default::default()
        };
        assert_eq!(SystemTimeToFileTime(&not_a_day, &mut ft), FALSE);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);

        let mut now = SYSTEMTIME::default();
        GetSystemTime(&mut now);
        assert!(now.year >= 2024, "{now:?}");
    }

    #[test]
    fn system_info_is_the_machine() {
        // SAFETY: zero is a value of every field.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        GetSystemInfo(&mut info);
        assert_eq!(info.page_size, 4096);
        assert!(info.number_of_processors >= 1);
        assert_eq!(
            info.active_processor_mask.count_ones(),
            info.number_of_processors
        );
        assert_eq!(info.allocation_granularity, 0x10000);
        assert_eq!(info.processor_architecture, 9);
        assert_eq!(GetActiveProcessorCount(0), info.number_of_processors);
        assert_eq!(GetActiveProcessorCount(1), 0);
        assert_eq!(GetVersion(), 0x4A64_000A);
    }

    #[test]
    fn processor_information_fits_and_lists_every_core() {
        victory_loader::teb::attach_current_thread();
        let n = processors();
        let entry = std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
        let mut len = 0u32;
        assert_eq!(
            GetLogicalProcessorInformation(std::ptr::null_mut(), &mut len),
            FALSE
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(len as usize, (n + 2) * entry);
        let mut list = vec![
            SYSTEM_LOGICAL_PROCESSOR_INFORMATION {
                processor_mask: 0,
                relationship: 0,
                union: [0; 2]
            };
            n + 2
        ];
        assert_eq!(
            GetLogicalProcessorInformation(list.as_mut_ptr(), &mut len),
            TRUE
        );
        let cores: Vec<_> = list
            .iter()
            .filter(|e| e.relationship == RELATION_PROCESSOR_CORE)
            .collect();
        assert_eq!(cores.len(), n);
        let mask = cores.iter().fold(0, |m, e| m | e.processor_mask);
        assert_eq!(mask, all_processors());
        assert_eq!(
            list.iter()
                .filter(|e| e.relationship == RELATION_PROCESSOR_PACKAGE)
                .count(),
            1
        );
        assert_eq!(
            list.iter()
                .filter(|e| e.relationship == RELATION_NUMA_NODE)
                .count(),
            1
        );

        let mut len = 0u32;
        assert_eq!(
            GetLogicalProcessorInformationEx(RELATION_ALL, std::ptr::null_mut(), &mut len),
            FALSE
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        let mut buffer = vec![0u8; len as usize];
        assert_eq!(
            GetLogicalProcessorInformationEx(RELATION_ALL, buffer.as_mut_ptr().cast(), &mut len),
            TRUE
        );
        let mut kinds = Vec::new();
        let mut at = 0;
        while at < len as usize {
            let word = |i: usize| u32::from_le_bytes(buffer[i..i + 4].try_into().unwrap());
            let (kind, size) = (word(at), word(at + 4) as usize);
            assert!(size >= 8);
            // The group mask's processors, at the end of either kind.
            let mask =
                usize::from_le_bytes(buffer[at + size - 16..at + size - 8].try_into().unwrap());
            kinds.push((kind, mask));
            at += size;
        }
        assert_eq!(at, len as usize);
        assert_eq!(kinds.len(), n + 2);
        assert_eq!(
            kinds
                .iter()
                .filter(|(k, _)| *k == RELATION_PROCESSOR_CORE)
                .count(),
            n
        );
        assert!(kinds.contains(&(RELATION_PROCESSOR_PACKAGE, all_processors())));
        assert!(kinds.contains(&(RELATION_NUMA_NODE, all_processors())));

        let mut len = 0u32;
        assert_eq!(
            GetLogicalProcessorInformationEx(RELATION_CACHE, std::ptr::null_mut(), &mut len),
            TRUE
        );
        assert_eq!(len, 0, "no caches described");
    }

    #[test]
    fn features_match_the_processor() {
        assert_eq!(IsProcessorFeaturePresent(10), TRUE);
        assert_eq!(
            IsProcessorFeaturePresent(40),
            BOOL::from(std::arch::is_x86_feature_detected!("avx2"))
        );
        assert_eq!(IsProcessorFeaturePresent(22), FALSE);
        assert_eq!(IsProcessorFeaturePresent(1000), FALSE);
    }

    #[test]
    fn windows_is_windows_10() {
        victory_loader::teb::attach_current_thread();
        // SAFETY: zero is a value of every field.
        let mut info: OSVERSIONINFOEXW = unsafe { std::mem::zeroed() };
        info.length = 284;
        assert_eq!(GetVersionExW(&mut info), TRUE);
        assert_eq!((info.major_version, info.build_number), (10, 19044));
        assert_eq!(info.product_type, 1);
        info.length = 3;
        assert_eq!(RtlGetVersion(&mut info), STATUS_INVALID_PARAMETER);

        let mut buffer = [0u16; 32];
        assert_eq!(GetWindowsDirectoryW(buffer.as_mut_ptr(), 32), 10);
        assert_eq!(String::from_utf16_lossy(&buffer[..10]), r"C:\Windows");
        assert_eq!(buffer[10], 0);
        assert_eq!(GetSystemDirectoryW(buffer.as_mut_ptr(), 5), 20);

        let mut len = 3;
        assert_eq!(GetComputerNameW(buffer.as_mut_ptr(), &mut len), FALSE);
        assert_eq!(len, 8);
        assert_eq!(last_error(), ERROR_BUFFER_OVERFLOW);
        assert_eq!(GetComputerNameW(buffer.as_mut_ptr(), &mut len), TRUE);
        assert_eq!(len, 7);
        assert_eq!(String::from_utf16_lossy(&buffer[..7]), "VICTORY");
    }
}
