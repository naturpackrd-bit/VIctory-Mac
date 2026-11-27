//! COM's runtime off Windows (`ole32.dll`, `combase.dll`, the
//! `api-ms-win-core-com-*` sets): apartments per thread, task memory (the C
//! runtime's heap), GUIDs and their strings, and `PROPVARIANT`s. There is
//! no class registry: every class a title asks for is not registered (the
//! census tells which ones titles want, if any).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]
// Windows' signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use super::ucrt::heap;
use super::win32::sync::{WAIT_OBJECT_0, WAIT_TIMEOUT, WaitForMultipleObjects};
use super::xgameruntime::Guid;
use crate::{HleExport, HleModule, hle_export};

type HRESULT = i32;

const S_OK: HRESULT = 0;
const S_FALSE: HRESULT = 1;
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
/// A null output pointer.
const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
#[allow(dead_code)]
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const REGDB_E_CLASSNOTREG: HRESULT = 0x8004_0154_u32 as i32;
const RPC_E_CHANGED_MODE: HRESULT = 0x8001_0106_u32 as i32;
const CO_E_NOTINITIALIZED: HRESULT = 0x8004_01F0_u32 as i32;
const CO_E_CLASSSTRING: HRESULT = 0x8004_01F3_u32 as i32;
const RPC_S_CALLPENDING: HRESULT = 0x8001_0115_u32 as i32;

const COINIT_APARTMENTTHREADED: u32 = 2;
const COWAIT_WAITALL: u32 = 1;

const APTTYPE_STA: i32 = 0;
const APTTYPE_MTA: i32 = 1;
const APTTYPE_MAINSTA: i32 = 3;
const APTTYPEQUALIFIER_NONE: i32 = 0;
const APTTYPEQUALIFIER_IMPLICIT_MTA: i32 = 1;

/// The apartment a thread joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Apartment {
    Single,
    Multi,
}

thread_local! {
    /// The thread's `CoInitialize` count and its apartment.
    static COM: Cell<(u32, Option<Apartment>)> = const { Cell::new((0, None)) };
}

/// The thread of the process's main single-threaded apartment (its first),
/// 0 for none.
static MAIN_STA: AtomicU32 = AtomicU32::new(0);

/// `CoIncrementMTAUsage`'s count: the implicit multi-threaded apartment
/// lives while it is above 0.
static MTA_USAGE: AtomicUsize = AtomicUsize::new(0);

/// Logs `what` at `message` the first time it is seen.
fn warn_once(what: &str, message: &str) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    if !seen.iter().any(|s| s == what) {
        tracing::warn!(what, "{message}");
        seen.push(what.to_owned());
    }
}

fn thread_id() -> u32 {
    super::win32::handles::GetCurrentThreadId()
}

// Apartments.

/// Joins the thread to an apartment: `S_OK` the first time, `S_FALSE` again
/// (counted), `RPC_E_CHANGED_MODE` for the other kind.
pub extern "win64" fn CoInitializeEx(_reserved: *mut c_void, coinit: u32) -> HRESULT {
    let wanted = if coinit & COINIT_APARTMENTTHREADED != 0 {
        Apartment::Single
    } else {
        Apartment::Multi
    };
    COM.with(|com| match com.get() {
        (count, Some(apartment)) if count > 0 => {
            if apartment != wanted {
                return RPC_E_CHANGED_MODE;
            }
            com.set((count + 1, Some(apartment)));
            S_FALSE
        }
        _ => {
            com.set((1, Some(wanted)));
            if wanted == Apartment::Single {
                let _ =
                    MAIN_STA.compare_exchange(0, thread_id(), Ordering::AcqRel, Ordering::Relaxed);
            }
            S_OK
        }
    })
}

/// [`CoInitializeEx`] into a single-threaded apartment.
pub extern "win64" fn CoInitialize(reserved: *mut c_void) -> HRESULT {
    CoInitializeEx(reserved, COINIT_APARTMENTTHREADED)
}

/// Undoes one [`CoInitializeEx`]; the last one leaves the apartment.
pub extern "win64" fn CoUninitialize() {
    COM.with(|com| match com.get() {
        (0, _) => {}
        (1, _) => {
            com.set((0, None));
            let _ = MAIN_STA.compare_exchange(thread_id(), 0, Ordering::AcqRel, Ordering::Relaxed);
        }
        (count, apartment) => com.set((count - 1, apartment)),
    });
}

/// The calling thread's apartment: its own, else the implicit
/// multi-threaded one while `CoIncrementMTAUsage` holds it.
pub extern "win64" fn CoGetApartmentType(kind: *mut i32, qualifier: *mut i32) -> HRESULT {
    if kind.is_null() || qualifier.is_null() {
        return E_INVALIDARG;
    }
    let (apartment, how) = match COM.with(Cell::get) {
        (count, Some(Apartment::Single)) if count > 0 => {
            let main = MAIN_STA.load(Ordering::Acquire) == thread_id();
            let kind = if main { APTTYPE_MAINSTA } else { APTTYPE_STA };
            (kind, APTTYPEQUALIFIER_NONE)
        }
        (count, Some(Apartment::Multi)) if count > 0 => (APTTYPE_MTA, APTTYPEQUALIFIER_NONE),
        _ if MTA_USAGE.load(Ordering::Acquire) > 0 => (APTTYPE_MTA, APTTYPEQUALIFIER_IMPLICIT_MTA),
        _ => {
            // SAFETY: checked non-null; the title's outputs.
            unsafe {
                kind.write(APTTYPE_STA);
                qualifier.write(APTTYPEQUALIFIER_NONE);
            }
            return CO_E_NOTINITIALIZED;
        }
    };
    // SAFETY: checked non-null; the title's outputs.
    unsafe {
        kind.write(apartment);
        qualifier.write(how);
    }
    S_OK
}

/// Keeps the implicit multi-threaded apartment; a cookie to give back.
pub extern "win64" fn CoIncrementMTAUsage(cookie: *mut usize) -> HRESULT {
    if cookie.is_null() {
        return E_INVALIDARG;
    }
    let n = MTA_USAGE.fetch_add(1, Ordering::AcqRel) + 1;
    // SAFETY: checked non-null; the title's output.
    unsafe { cookie.write(n) };
    S_OK
}

/// Gives back a cookie of [`CoIncrementMTAUsage`].
pub extern "win64" fn CoDecrementMTAUsage(cookie: usize) -> HRESULT {
    if cookie == 0 {
        return E_INVALIDARG;
    }
    let _ = MTA_USAGE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    S_OK
}

/// Waits for the handles (all of them with `COWAIT_WAITALL`); `*index` the
/// one signaled; `RPC_S_CALLPENDING` when the time runs out.
pub extern "win64" fn CoWaitForMultipleHandles(
    flags: u32,
    timeout: u32,
    count: u32,
    handles: *const *mut c_void,
    index: *mut u32,
) -> HRESULT {
    if handles.is_null() || count == 0 || index.is_null() {
        return E_INVALIDARG;
    }
    let wait_all = i32::from(flags & COWAIT_WAITALL != 0);
    let result = WaitForMultipleObjects(count, handles, wait_all, timeout);
    if result == WAIT_TIMEOUT {
        return RPC_S_CALLPENDING;
    }
    if (WAIT_OBJECT_0..WAIT_OBJECT_0 + count).contains(&result) {
        // SAFETY: checked non-null; the title's output.
        unsafe { index.write(result - WAIT_OBJECT_0) };
        return S_OK;
    }
    E_INVALIDARG
}

// Classes: none is registered.

/// The registry form of a GUID, `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
fn guid_text(guid: &Guid) -> String {
    format!("{{{}}}", guid.to_string().to_ascii_uppercase())
}

/// A class that is not registered: logged once.
fn class_not_registered(clsid: *const Guid) -> HRESULT {
    if !clsid.is_null() {
        // SAFETY: checked non-null; the title's CLSID.
        let text = guid_text(&unsafe { clsid.read_unaligned() });
        warn_once(&text, "a COM class that is not here (no class registry)");
    }
    REGDB_E_CLASSNOTREG
}

/// No class is registered: `REGDB_E_CLASSNOTREG`, `*out` null.
pub extern "win64" fn CoCreateInstance(
    clsid: *const Guid,
    _outer: *mut c_void,
    _context: u32,
    _iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { out.write(std::ptr::null_mut()) };
    class_not_registered(clsid)
}

/// An interface `CoCreateInstanceEx` is asked for, and its result.
#[repr(C)]
pub struct MULTI_QI {
    pub iid: *const Guid,
    pub itf: *mut c_void,
    pub hr: HRESULT,
}

const _: () = assert!(std::mem::size_of::<MULTI_QI>() == 24);

/// As [`CoCreateInstance`], each result failing the same way.
pub extern "win64" fn CoCreateInstanceEx(
    clsid: *const Guid,
    _outer: *mut c_void,
    _context: u32,
    _server: *mut c_void,
    count: u32,
    results: *mut MULTI_QI,
) -> HRESULT {
    if count == 0 || results.is_null() {
        return E_INVALIDARG;
    }
    for i in 0..count as usize {
        // SAFETY: the title's array of `count` entries, checked non-null.
        unsafe {
            let entry = results.add(i);
            (*entry).itf = std::ptr::null_mut();
            (*entry).hr = REGDB_E_CLASSNOTREG;
        }
    }
    class_not_registered(clsid)
}

/// Accepted; nothing is kept (no other code asks for the class here).
pub extern "win64" fn CoRegisterClassObject(
    _clsid: *const Guid,
    _unknown: *mut c_void,
    _context: u32,
    _flags: u32,
    cookie: *mut u32,
) -> HRESULT {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    if cookie.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { cookie.write(NEXT.fetch_add(1, Ordering::Relaxed)) };
    S_OK
}

/// See [`CoRegisterClassObject`].
pub extern "win64" fn CoRevokeClassObject(_cookie: u32) -> HRESULT {
    S_OK
}

/// No DLL of classes is loaded.
pub extern "win64" fn CoFreeUnusedLibraries() {}

/// See [`CoFreeUnusedLibraries`].
pub extern "win64" fn CoFreeUnusedLibrariesEx(_delay: u32, _reserved: u32) {}

/// There are no proxies here.
pub extern "win64" fn CoSetProxyBlanket(
    _proxy: *mut c_void,
    _authentication: u32,
    _authorization: u32,
    _principal: *const u16,
    _level: u32,
    _impersonation: u32,
    _identity: *mut c_void,
    _capabilities: u32,
) -> HRESULT {
    S_OK
}

/// There is one user here.
pub extern "win64" fn CoRevertToSelf() -> HRESULT {
    S_OK
}

/// See [`CoRevertToSelf`].
pub extern "win64" fn CoImpersonateClient() -> HRESULT {
    S_OK
}

// Task memory.

/// The C runtime's heap.
pub extern "win64" fn CoTaskMemAlloc(size: usize) -> *mut c_void {
    heap::malloc(size)
}

/// See [`CoTaskMemAlloc`].
pub extern "win64" fn CoTaskMemRealloc(block: *mut c_void, size: usize) -> *mut c_void {
    heap::realloc(block, size)
}

/// See [`CoTaskMemAlloc`].
pub extern "win64" fn CoTaskMemFree(block: *mut c_void) {
    heap::free(block);
}

// GUIDs.

/// A new random GUID (rpcrt4's `UuidCreate`).
pub extern "win64" fn CoCreateGuid(out: *mut Guid) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    match super::rpcrt4::UuidCreate(out) {
        0 => S_OK,
        _ => E_OUTOFMEMORY,
    }
}

/// `{...}` (38 characters) and a NUL: 39, or 0 when `cch` is less.
pub extern "win64" fn StringFromGUID2(guid: *const Guid, out: *mut u16, cch: i32) -> i32 {
    if guid.is_null() || out.is_null() || cch < 39 {
        return 0;
    }
    // SAFETY: checked non-null; the title's GUID.
    let text = guid_text(&unsafe { guid.read_unaligned() });
    for (i, unit) in text.encode_utf16().chain(Some(0)).enumerate() {
        // SAFETY: the title's buffer holds `cch` (at least 39) units.
        unsafe { out.add(i).write(unit) };
    }
    39
}

/// A GUID's string in task memory.
fn string_from_guid(guid: *const Guid, out: *mut *mut u16) -> HRESULT {
    if guid.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let text = CoTaskMemAlloc(39 * 2).cast::<u16>();
    if text.is_null() {
        // SAFETY: checked non-null; the title's output.
        unsafe { out.write(std::ptr::null_mut()) };
        return E_OUTOFMEMORY;
    }
    StringFromGUID2(guid, text, 39);
    // SAFETY: checked non-null; the title's output.
    unsafe { out.write(text) };
    S_OK
}

/// See [`string_from_guid`].
pub extern "win64" fn StringFromCLSID(clsid: *const Guid, out: *mut *mut u16) -> HRESULT {
    string_from_guid(clsid, out)
}

/// See [`string_from_guid`].
pub extern "win64" fn StringFromIID(iid: *const Guid, out: *mut *mut u16) -> HRESULT {
    string_from_guid(iid, out)
}

/// The GUID of `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, in braces or not.
fn parse_guid(text: &str) -> Option<Guid> {
    let inner = match text.strip_prefix('{') {
        Some(rest) => rest.strip_suffix('}')?,
        None => text,
    };
    let groups: Vec<&str> = inner.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];
    if groups.len() != 5
        || groups
            .iter()
            .zip(lengths)
            .any(|(g, n)| g.len() != n || !g.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let tail = format!("{}{}", groups[3], groups[4]);
    let mut data4 = [0u8; 8];
    for (i, byte) in data4.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&tail[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(Guid::new(
        u32::from_str_radix(groups[0], 16).ok()?,
        u16::from_str_radix(groups[1], 16).ok()?,
        u16::from_str_radix(groups[2], 16).ok()?,
        data4,
    ))
}

/// A NUL-terminated UTF-16 string from the title.
fn wide(text: *const u16) -> String {
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

/// A GUID from its string; null is `GUID_NULL`; `CO_E_CLASSSTRING` for
/// anything else (no ProgIDs: no class registry).
fn guid_from_string(text: *const u16, out: *mut Guid) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let guid = if text.is_null() {
        Some(Guid::new(0, 0, 0, [0; 8]))
    } else {
        parse_guid(&wide(text))
    };
    match guid {
        Some(guid) => {
            // SAFETY: checked non-null; the title's output.
            unsafe { out.write_unaligned(guid) };
            S_OK
        }
        None => CO_E_CLASSSTRING,
    }
}

/// See [`guid_from_string`].
pub extern "win64" fn CLSIDFromString(text: *const u16, out: *mut Guid) -> HRESULT {
    guid_from_string(text, out)
}

/// See [`guid_from_string`].
pub extern "win64" fn IIDFromString(text: *const u16, out: *mut Guid) -> HRESULT {
    guid_from_string(text, out)
}

// PROPVARIANTs.

const VT_EMPTY: u16 = 0;
const VT_I2: u16 = 2;
const VT_I4: u16 = 3;
const VT_R4: u16 = 4;
const VT_R8: u16 = 5;
const VT_CY: u16 = 6;
const VT_DATE: u16 = 7;
const VT_BSTR: u16 = 8;
const VT_DISPATCH: u16 = 9;
const VT_ERROR: u16 = 10;
const VT_BOOL: u16 = 11;
const VT_VARIANT: u16 = 12;
const VT_UNKNOWN: u16 = 13;
const VT_I1: u16 = 16;
const VT_UI1: u16 = 17;
const VT_UI2: u16 = 18;
const VT_UI4: u16 = 19;
const VT_I8: u16 = 20;
const VT_UI8: u16 = 21;
const VT_INT: u16 = 22;
const VT_UINT: u16 = 23;
const VT_LPSTR: u16 = 30;
const VT_LPWSTR: u16 = 31;
const VT_FILETIME: u16 = 64;
const VT_BLOB: u16 = 65;
const VT_CLSID: u16 = 72;
const VT_VECTOR: u16 = 0x1000;

/// A tagged value: its type, then the value (a pointer at `data[0]`; a
/// counted value, `BLOB` or vector, has its count at `data[0]` and its
/// pointer at `data[1]`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PropVariant {
    pub vt: u16,
    pub reserved: [u16; 3],
    pub data: [u64; 2],
}

const _: () = assert!(std::mem::size_of::<PropVariant>() == 24);

/// A `BSTR`'s block starts 4 bytes before its text, at its byte length.
fn sys_free_string(text: u64) {
    if text != 0 {
        CoTaskMemFree((text - 4) as *mut c_void);
    }
}

/// Calls an interface's `AddRef` (slot 1) or `Release` (slot 2).
fn interface_call(object: u64, slot: usize) {
    if object == 0 {
        return;
    }
    type Method = extern "win64" fn(*mut c_void) -> u32;
    // SAFETY: a COM interface pointer of the title's: its first word is its
    // method table, whose slots 1 and 2 are `AddRef` and `Release`.
    unsafe {
        let table = *(object as *const *const usize);
        let method: Method = std::mem::transmute(*table.add(slot));
        method(object as *mut c_void);
    }
}

/// The bytes of one element of a vector of `vt`; none for a type vectors
/// do not hold.
fn element_size(vt: u16) -> Option<usize> {
    Some(match vt {
        VT_I1 | VT_UI1 => 1,
        VT_I2 | VT_UI2 | VT_BOOL => 2,
        VT_I4 | VT_UI4 | VT_R4 | VT_ERROR | VT_INT | VT_UINT => 4,
        VT_I8 | VT_UI8 | VT_R8 | VT_CY | VT_DATE | VT_FILETIME => 8,
        VT_LPSTR | VT_LPWSTR | VT_BSTR => 8,
        VT_CLSID | VT_BLOB => 16,
        VT_VARIANT => 24,
        _ => return None,
    })
}

/// Frees what one element of type `vt` at `at` owns (not the element).
///
/// # Safety
/// `at` points at a live element of that type.
unsafe fn clear_element(vt: u16, at: *mut u8) {
    // SAFETY: the caller's element.
    unsafe {
        match vt {
            VT_LPSTR | VT_LPWSTR => CoTaskMemFree(at.cast::<*mut c_void>().read_unaligned()),
            VT_BSTR => sys_free_string(at.cast::<u64>().read_unaligned()),
            VT_BLOB => CoTaskMemFree(at.add(8).cast::<*mut c_void>().read_unaligned()),
            VT_VARIANT => {
                PropVariantClear(at.cast());
            }
            _ => {}
        }
    }
}

/// Frees what the value owns by its type, then makes it `VT_EMPTY`.
pub extern "win64" fn PropVariantClear(pv: *mut PropVariant) -> HRESULT {
    if pv.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title's value.
    let value = unsafe { pv.read_unaligned() };
    let (vt, data) = (value.vt, value.data);
    match vt {
        VT_LPSTR | VT_LPWSTR | VT_CLSID => CoTaskMemFree(data[0] as *mut c_void),
        VT_BSTR => sys_free_string(data[0]),
        VT_BLOB => CoTaskMemFree(data[1] as *mut c_void),
        VT_UNKNOWN | VT_DISPATCH => interface_call(data[0], 2),
        _ if vt & VT_VECTOR != 0 => {
            let element = vt & !VT_VECTOR;
            let (count, array) = (data[0] as u32 as usize, data[1] as *mut u8);
            if let Some(size) = element_size(element)
                && !array.is_null()
            {
                for i in 0..count {
                    // SAFETY: the vector's `count` elements of `size` bytes.
                    unsafe { clear_element(element, array.add(i * size)) };
                }
            }
            CoTaskMemFree(array.cast());
        }
        _ => {}
    }
    let empty = PropVariant {
        vt: VT_EMPTY,
        ..Default::default()
    };
    // SAFETY: as above.
    unsafe { pv.write_unaligned(empty) };
    S_OK
}

/// `len` bytes of `from` in task memory; null for null.
fn copy_bytes(from: u64, len: usize) -> Option<u64> {
    if from == 0 {
        return Some(0);
    }
    let to = CoTaskMemAlloc(len.max(1)).cast::<u8>();
    if to.is_null() {
        return None;
    }
    // SAFETY: `from` holds `len` bytes (the value's own); `to` is new.
    unsafe { std::ptr::copy_nonoverlapping(from as *const u8, to, len) };
    Some(to as u64)
}

/// The bytes of a NUL-terminated string of `unit`-byte units, NUL counted.
fn string_bytes(text: u64, unit: usize) -> usize {
    if text == 0 {
        return 0;
    }
    let mut len = 0;
    // SAFETY: a NUL-terminated string the value owns.
    unsafe {
        while (0..unit).any(|b| *(text as *const u8).add(len * unit + b) != 0) {
            len += 1;
        }
    }
    (len + 1) * unit
}

/// A copy of a `BSTR` (its length prefix, text and NUL).
fn copy_bstr(text: u64) -> Option<u64> {
    if text == 0 {
        return Some(0);
    }
    // SAFETY: a BSTR's byte length is the 4 bytes before its text.
    let len = unsafe { ((text - 4) as *const u32).read_unaligned() } as usize;
    copy_bytes(text - 4, len + 6).map(|block| block + 4)
}

/// Copies what one element of type `vt` at `from` owns into the element at
/// `to` (already a bitwise copy).
///
/// # Safety
/// Both point at elements of that type.
unsafe fn copy_element(vt: u16, from: *const u8, to: *mut u8) -> bool {
    // SAFETY: the caller's elements; the 8-byte pointer is read only for
    // the types that hold one (a 1, 2 or 4-byte element is narrower).
    unsafe {
        let pointer = || from.cast::<u64>().read_unaligned();
        let copied = match vt {
            VT_LPSTR => copy_bytes(pointer(), string_bytes(pointer(), 1)),
            VT_LPWSTR => copy_bytes(pointer(), string_bytes(pointer(), 2)),
            VT_BSTR => copy_bstr(pointer()),
            VT_BLOB => {
                let len = from.cast::<u32>().read_unaligned() as usize;
                let data = from.add(8).cast::<u64>().read_unaligned();
                return match copy_bytes(data, len) {
                    Some(copy) => {
                        to.add(8).cast::<u64>().write_unaligned(copy);
                        true
                    }
                    None => false,
                };
            }
            VT_VARIANT => return PropVariantCopy(to.cast(), from.cast()) == S_OK,
            _ => return true,
        };
        match copied {
            Some(copy) => {
                to.cast::<u64>().write_unaligned(copy);
                true
            }
            None => false,
        }
    }
}

/// A deep copy of `src` into `dest` (whose old value is not freed, as on
/// Windows); an interface is `AddRef`ed.
pub extern "win64" fn PropVariantCopy(dest: *mut PropVariant, src: *const PropVariant) -> HRESULT {
    if dest.is_null() || src.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title's values.
    let value = unsafe { src.read_unaligned() };
    let mut copy = value;
    let vt = value.vt;
    let done = match vt {
        VT_CLSID => copy_bytes(value.data[0], 16).map(|p| copy.data[0] = p),
        VT_LPSTR | VT_LPWSTR | VT_BSTR | VT_BLOB => {
            let from = (&raw const value.data).cast::<u8>();
            let to = (&raw mut copy.data).cast::<u8>();
            // SAFETY: the values' own data, read as the element they hold.
            unsafe { copy_element(vt, from, to) }.then_some(())
        }
        VT_UNKNOWN | VT_DISPATCH => {
            interface_call(value.data[0], 1);
            Some(())
        }
        _ if vt & VT_VECTOR != 0 => {
            let element = vt & !VT_VECTOR;
            let (count, array) = (value.data[0] as u32 as usize, value.data[1]);
            match element_size(element) {
                None => return E_INVALIDARG,
                Some(size) => copy_bytes(array, count * size).and_then(|new| {
                    copy.data[1] = new;
                    let all = (0..count).all(|i| {
                        // SAFETY: both arrays hold `count` elements.
                        unsafe {
                            copy_element(
                                element,
                                (array as *const u8).add(i * size),
                                (new as *mut u8).add(i * size),
                            )
                        }
                    });
                    all.then_some(())
                }),
            }
        }
        _ => Some(()),
    };
    if done.is_none() {
        return E_OUTOFMEMORY;
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { dest.write_unaligned(copy) };
    S_OK
}

fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(CoInitializeEx),
        hle_export!(CoInitialize),
        hle_export!(CoUninitialize),
        hle_export!(CoGetApartmentType),
        hle_export!(CoIncrementMTAUsage),
        hle_export!(CoDecrementMTAUsage),
        hle_export!(CoWaitForMultipleHandles),
        hle_export!(CoCreateInstance),
        hle_export!(CoCreateInstanceEx),
        hle_export!(CoRegisterClassObject),
        hle_export!(CoRevokeClassObject),
        hle_export!(CoFreeUnusedLibraries),
        hle_export!(CoFreeUnusedLibrariesEx),
        hle_export!(CoSetProxyBlanket),
        hle_export!(CoRevertToSelf),
        hle_export!(CoImpersonateClient),
        hle_export!(CoTaskMemAlloc),
        hle_export!(CoTaskMemRealloc),
        hle_export!(CoTaskMemFree),
        hle_export!(CoCreateGuid),
        hle_export!(StringFromGUID2),
        hle_export!(StringFromCLSID),
        hle_export!(StringFromIID),
        hle_export!(CLSIDFromString),
        hle_export!(IIDFromString),
        hle_export!(PropVariantClear),
        hle_export!(PropVariantCopy),
    ]
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "com",
        dlls: &["api-ms-win-core-com-*", "ole32.dll", "combase.dll"],
        exports: exports(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn apartments_follow_the_rules() {
        std::thread::spawn(|| {
            let null = std::ptr::null_mut();
            let (mut kind, mut qualifier) = (-1, -1);
            if MTA_USAGE.load(Ordering::Acquire) == 0 {
                assert_eq!(
                    CoGetApartmentType(&mut kind, &mut qualifier),
                    CO_E_NOTINITIALIZED
                );
            }
            assert_eq!(CoInitializeEx(null, 0), S_OK);
            assert_eq!(CoInitializeEx(null, 0), S_FALSE);
            assert_eq!(CoInitialize(null), RPC_E_CHANGED_MODE);
            assert_eq!(CoGetApartmentType(&mut kind, &mut qualifier), S_OK);
            assert_eq!((kind, qualifier), (APTTYPE_MTA, APTTYPEQUALIFIER_NONE));
            CoUninitialize();
            CoUninitialize();
            // Left: now the other kind may join.
            assert_eq!(CoInitializeEx(null, COINIT_APARTMENTTHREADED), S_OK);
            assert_eq!(CoGetApartmentType(&mut kind, &mut qualifier), S_OK);
            assert!(kind == APTTYPE_MAINSTA || kind == APTTYPE_STA);
            assert_eq!(CoInitializeEx(null, 0), RPC_E_CHANGED_MODE);
            CoUninitialize();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn guids_have_their_string_form() {
        let guid = Guid::new(
            0x0123_4567,
            0x89AB,
            0xCDEF,
            [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF],
        );
        let mut text = [0u16; 39];
        assert_eq!(StringFromGUID2(&guid, text.as_mut_ptr(), 38), 0);
        assert_eq!(StringFromGUID2(&guid, text.as_mut_ptr(), 39), 39);
        assert_eq!(
            String::from_utf16(&text[..38]).unwrap(),
            "{01234567-89AB-CDEF-0123-456789ABCDEF}"
        );
        assert_eq!(text[38], 0);

        let mut back = Guid::new(0, 0, 0, [0; 8]);
        assert_eq!(CLSIDFromString(text.as_ptr(), &mut back), S_OK);
        assert_eq!(back, guid);
        let bare = w("01234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(IIDFromString(bare.as_ptr(), &mut back), S_OK);
        assert_eq!(back, guid);
        let bad = w("{01234567-89AB-CDEF-0123-456789ABCDE}");
        assert_eq!(CLSIDFromString(bad.as_ptr(), &mut back), CO_E_CLASSSTRING);

        let mut allocated = std::ptr::null_mut();
        assert_eq!(StringFromCLSID(&guid, &mut allocated), S_OK);
        // SAFETY: 39 units from `StringFromCLSID`.
        let copy = unsafe { std::slice::from_raw_parts(allocated, 39) };
        assert_eq!(copy, &text);
        CoTaskMemFree(allocated.cast());

        let (mut a, mut b) = (guid, guid);
        assert_eq!(CoCreateGuid(&mut a), S_OK);
        assert_eq!(CoCreateGuid(&mut b), S_OK);
        assert_ne!(a, b);
    }

    #[test]
    fn no_class_is_registered() {
        let mut out = std::ptr::dangling_mut::<c_void>();
        let clsid = Guid::new(1, 2, 3, [4; 8]);
        let null = std::ptr::null_mut();
        assert_eq!(
            CoCreateInstance(&clsid, null, 1, &clsid, &mut out),
            REGDB_E_CLASSNOTREG
        );
        assert!(out.is_null());
        let mut results = [
            MULTI_QI {
                iid: &clsid,
                itf: std::ptr::dangling_mut::<c_void>(),
                hr: 0,
            },
            MULTI_QI {
                iid: &clsid,
                itf: std::ptr::dangling_mut::<c_void>(),
                hr: 0,
            },
        ];
        assert_eq!(
            CoCreateInstanceEx(&clsid, null, 1, null, 2, results.as_mut_ptr()),
            REGDB_E_CLASSNOTREG
        );
        assert!(
            results
                .iter()
                .all(|r| r.itf.is_null() && r.hr == REGDB_E_CLASSNOTREG)
        );
    }

    #[test]
    fn clearing_frees_and_empties() {
        let text = w("value");
        let block = CoTaskMemAlloc(text.len() * 2).cast::<u16>();
        // SAFETY: a block of `text.len()` units.
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), block, text.len()) };
        let mut pv = PropVariant {
            vt: VT_LPWSTR,
            data: [block as u64, 0],
            ..Default::default()
        };
        let mut copy = PropVariant::default();
        assert_eq!(PropVariantCopy(&mut copy, &pv), S_OK);
        assert_ne!(copy.data[0], pv.data[0]);
        assert_eq!(wide(copy.data[0] as *const u16), "value");
        assert_eq!(PropVariantClear(&mut pv), S_OK);
        assert_eq!((pv.vt, pv.data), (VT_EMPTY, [0, 0]));
        assert_eq!(PropVariantClear(&mut copy), S_OK);
        assert_eq!(copy.vt, VT_EMPTY);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 27);
    }
}
