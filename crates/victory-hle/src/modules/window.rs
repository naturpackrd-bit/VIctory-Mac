//! The Win32 model of windows off Windows (`user32.dll` and its
//! `ext-ms-win-*ntuser-window/message/sysparams-*` API sets): window
//! classes, windows and their procedures, each thread's message queue, sent
//! messages between threads, timers, and what the system says about the
//! screen, the cursor and the keyboard. It is kept by us; no host window
//! exists yet (M3's SDL3 layer will feed it real events).
//!
//! Limits:
//! - The screen is a fixed 1920 x 1080 at 96 DPI, one monitor; windows have
//!   no frame (the client area is the window), nothing paints, and there is
//!   no input (keys read up, the cursor only moves when the title moves it).
//! - Strings in messages are not converted between the narrow and the wide
//!   forms (`SendMessageA` to a wide window passes its pointers as they
//!   are); the text functions (`SetWindowTextA`, `GetWindowTextA`, ...) and
//!   window creation do convert.
//! - `MsgWaitForMultipleObjects(Ex)` waits for the handles in slices of
//!   5 ms, checking the queue between them: an approximation of Windows'
//!   single wait.
#![allow(non_snake_case, non_camel_case_types)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]
// Windows' signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::win32::handles::{GetCurrentProcessId, GetCurrentThreadId, set_last_error};
use super::win32::sync::{WAIT_OBJECT_0, WAIT_TIMEOUT, WaitForMultipleObjects};
use super::win32::time::GetTickCount;
use crate::{HleExport, HleModule, hle_export, runtime};

type BOOL = i32;
type HWND = usize;
type LRESULT = isize;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_INVALID_WINDOW_HANDLE: u32 = 1400;
const ERROR_CANNOT_FIND_WND_CLASS: u32 = 1407;
const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
const ERROR_CLASS_DOES_NOT_EXIST: u32 = 1411;
const ERROR_CLASS_HAS_WINDOWS: u32 = 1412;
const ERROR_INVALID_INDEX: u32 = 1413;
const ERROR_INVALID_THREAD_ID: u32 = 1444;
const ERROR_TIMEOUT: u32 = 1460;

/// The screen, until M3 reports the host display's.
const SCREEN_WIDTH: i32 = 1920;
const SCREEN_HEIGHT: i32 = 1080;
const DPI: u32 = 96;

/// The desktop window; windows follow it 16 apart.
const DESKTOP: HWND = 0x1_0000;
const HWND_STEP: usize = 16;
const HWND_BROADCAST: HWND = 0xFFFF;
/// The fake device context every window has.
const HDC: usize = 0x2_0000;
/// The one monitor.
const HMONITOR: usize = 0x3_0000;
const DISPLAY: &str = r"\\.\DISPLAY1";

const CW_USEDEFAULT: i32 = 0x8000_0000_u32 as i32;
const WS_CHILD: u32 = 0x4000_0000;
const WS_POPUP: u32 = 0x8000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_DISABLED: u32 = 0x0800_0000;
const CS_GLOBALCLASS: u32 = 0x4000;

pub const WM_NULL: u32 = 0;
pub const WM_CREATE: u32 = 1;
pub const WM_DESTROY: u32 = 2;
pub const WM_MOVE: u32 = 3;
pub const WM_SIZE: u32 = 5;
pub const WM_ACTIVATE: u32 = 6;
pub const WM_SETFOCUS: u32 = 7;
pub const WM_KILLFOCUS: u32 = 8;
pub const WM_ENABLE: u32 = 0xA;
pub const WM_SETTEXT: u32 = 0xC;
pub const WM_GETTEXT: u32 = 0xD;
pub const WM_GETTEXTLENGTH: u32 = 0xE;
pub const WM_PAINT: u32 = 0xF;
pub const WM_CLOSE: u32 = 0x10;
pub const WM_QUIT: u32 = 0x12;
pub const WM_ERASEBKGND: u32 = 0x14;
pub const WM_SHOWWINDOW: u32 = 0x18;
pub const WM_ACTIVATEAPP: u32 = 0x1C;
pub const WM_SETCURSOR: u32 = 0x20;
pub const WM_GETMINMAXINFO: u32 = 0x24;
pub const WM_WINDOWPOSCHANGING: u32 = 0x46;
pub const WM_WINDOWPOSCHANGED: u32 = 0x47;
pub const WM_NCCREATE: u32 = 0x81;
pub const WM_NCDESTROY: u32 = 0x82;
pub const WM_NCCALCSIZE: u32 = 0x83;
pub const WM_NCACTIVATE: u32 = 0x86;
pub const WM_SYSCOMMAND: u32 = 0x112;
pub const WM_TIMER: u32 = 0x113;
pub const WM_USER: u32 = 0x400;
pub const WM_APP: u32 = 0x8000;

const WA_INACTIVE: usize = 0;
const WA_ACTIVE: usize = 1;
const SIZE_RESTORED: usize = 0;

/// A lock, even if a thread panicked holding it: what our locks guard is
/// consistent between our operations.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sets the last error to `code` and gives back `value`.
fn fail<T>(code: u32, value: T) -> T {
    set_last_error(code);
    value
}

fn thread_id() -> u32 {
    GetCurrentThreadId()
}

/// Logs `what` at warn level the first time.
fn warn_once(what: &'static str) {
    static SEEN: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    let mut seen = lock(&SEEN);
    if !seen.contains(&what) {
        tracing::warn!("{what}");
        seen.push(what);
    }
}

// Geometry.

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl RECT {
    fn width(&self) -> i32 {
        self.right - self.left
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }

    fn holds(&self, p: POINT) -> bool {
        (self.left..self.right).contains(&p.x) && (self.top..self.bottom).contains(&p.y)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

/// A `POINT` passed by value (in one register).
fn point(packed: u64) -> POINT {
    POINT {
        x: packed as u32 as i32,
        y: (packed >> 32) as u32 as i32,
    }
}

const SCREEN: RECT = RECT {
    left: 0,
    top: 0,
    right: SCREEN_WIDTH,
    bottom: SCREEN_HEIGHT,
};

/// Two 16-bit values in one parameter, as `MAKELPARAM` packs them.
fn pack(low: i32, high: i32) -> isize {
    (u32::from(low as u16) | u32::from(high as u16) << 16) as isize
}

// Strings.

/// A NUL-terminated string's units.
fn units<'a, T: Copy + Default + PartialEq>(text: *const T) -> &'a [T] {
    if text.is_null() {
        return &[];
    }
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != T::default()).count();
        std::slice::from_raw_parts(text, len)
    }
}

fn wide(text: *const u16) -> String {
    String::from_utf16_lossy(units(text))
}

/// Narrow strings are the ANSI code page's: Latin-1 here.
fn narrow(text: *const u8) -> String {
    units(text).iter().map(|&b| char::from(b)).collect()
}

fn wide_z(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn narrow_z(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .chain(Some(0))
        .collect()
}

/// A class or property name: a string, or an atom (a "pointer" below
/// 0x10000).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Name {
    Atom(u16),
    Text(String),
}

fn name_w(text: *const u16) -> Option<Name> {
    match text as usize {
        0 => None,
        n if n < 0x1_0000 => Some(Name::Atom(n as u16)),
        _ => Some(Name::Text(wide(text))),
    }
}

fn name_a(text: *const u8) -> Option<Name> {
    match text as usize {
        0 => None,
        n if n < 0x1_0000 => Some(Name::Atom(n as u16)),
        _ => Some(Name::Text(narrow(text))),
    }
}

/// The atom table classes and window messages share: names (ignoring
/// case) from 0xC000.
fn atom(name: &str) -> u16 {
    static ATOMS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let name = name.to_lowercase();
    let mut atoms = lock(&ATOMS);
    let index = match atoms.iter().position(|a| *a == name) {
        Some(i) => i,
        None => {
            atoms.push(name);
            atoms.len() - 1
        }
    };
    0xC000 + index as u16
}

/// A name's text: an atom as `#n`.
fn name_text(name: &Name) -> String {
    match name {
        Name::Atom(n) => format!("#{n}"),
        Name::Text(t) => t.clone(),
    }
}

// Classes.

struct Class {
    atom: u16,
    name: String,
    instance: usize,
    wndproc: usize,
    style: u32,
    cls_extra: i32,
    wnd_extra: i32,
    icon: usize,
    cursor: usize,
    background: usize,
    menu_name: usize,
    icon_small: usize,
    /// Registered through a wide function: its windows take wide strings.
    unicode: bool,
    /// The class's extra bytes (`cbClsExtra`).
    extra: Vec<u8>,
    /// False once unregistered (the record stays: windows name it by
    /// index).
    registered: bool,
}

static CLASSES: Mutex<Vec<Class>> = Mutex::new(Vec::new());

/// The classes, with the system's few that titles create windows of
/// (`DefWindowProc` their procedure: none of their behaviour is here).
fn classes() -> MutexGuard<'static, Vec<Class>> {
    let mut classes = lock(&CLASSES);
    if classes.is_empty() {
        for name in ["Message", "STATIC", "BUTTON"] {
            classes.push(Class {
                atom: atom(name),
                name: name.to_owned(),
                instance: 0,
                wndproc: DefWindowProcW as *const () as usize,
                style: CS_GLOBALCLASS,
                cls_extra: 0,
                wnd_extra: 0,
                icon: 0,
                cursor: 0,
                background: 0,
                menu_name: 0,
                icon_small: 0,
                unicode: true,
                extra: Vec::new(),
                registered: true,
            });
        }
    }
    classes
}

/// An instance of 0 is the title's executable, as on Windows.
fn instance_of(instance: usize) -> usize {
    match instance {
        0 => runtime::main_image().unwrap_or(0),
        i => i,
    }
}

fn matches_name(class: &Class, name: &Name) -> bool {
    match name {
        Name::Atom(a) => class.atom == *a,
        Name::Text(t) => class.name.to_lowercase() == t.to_lowercase(),
    }
}

/// The registered class of `name` for `instance` (its own, else a global
/// one), as an index.
fn find_class(classes: &[Class], name: &Name, instance: usize) -> Option<usize> {
    let instance = instance_of(instance);
    let live = |c: &&Class| c.registered && matches_name(c, name);
    let own = classes
        .iter()
        .position(|c| live(&c) && instance_of(c.instance) == instance);
    own.or_else(|| {
        classes
            .iter()
            .position(|c| live(&c) && c.style & CS_GLOBALCLASS != 0)
    })
}

/// A window class as `RegisterClass` takes it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WNDCLASSW {
    pub style: u32,
    pub wndproc: usize,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub instance: usize,
    pub icon: usize,
    pub cursor: usize,
    pub background: usize,
    pub menu_name: usize,
    pub class_name: usize,
}

/// A window class as `RegisterClassEx` takes it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WNDCLASSEXW {
    pub size: u32,
    pub style: u32,
    pub wndproc: usize,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub instance: usize,
    pub icon: usize,
    pub cursor: usize,
    pub background: usize,
    pub menu_name: usize,
    pub class_name: usize,
    pub icon_small: usize,
}

const _: () = assert!(std::mem::size_of::<WNDCLASSW>() == 72);
const _: () = assert!(std::mem::size_of::<WNDCLASSEXW>() == 80);

/// The narrow forms have the same layout (their strings narrow).
type WNDCLASSA = WNDCLASSW;
type WNDCLASSEXA = WNDCLASSEXW;

impl WNDCLASSW {
    fn ex(&self) -> WNDCLASSEXW {
        WNDCLASSEXW {
            size: 80,
            style: self.style,
            wndproc: self.wndproc,
            cls_extra: self.cls_extra,
            wnd_extra: self.wnd_extra,
            instance: self.instance,
            icon: self.icon,
            cursor: self.cursor,
            background: self.background,
            menu_name: self.menu_name,
            class_name: self.class_name,
            icon_small: 0,
        }
    }
}

/// `RegisterClass`'s body: the class's atom, or 0 and the error.
fn register_class(class: &WNDCLASSEXW, unicode: bool) -> u16 {
    let name = if unicode {
        name_w(class.class_name as *const u16)
    } else {
        name_a(class.class_name as *const u8)
    };
    let Some(name) = name else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    if class.cls_extra < 0 || class.wnd_extra < 0 {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    let mut classes = classes();
    let instance = instance_of(class.instance);
    let taken = classes
        .iter()
        .any(|c| c.registered && matches_name(c, &name) && instance_of(c.instance) == instance);
    if taken {
        return fail(ERROR_CLASS_ALREADY_EXISTS, 0);
    }
    let atom = match &name {
        Name::Atom(a) => *a,
        Name::Text(t) => atom(t),
    };
    classes.push(Class {
        atom,
        name: name_text(&name),
        instance,
        wndproc: class.wndproc,
        style: class.style,
        cls_extra: class.cls_extra,
        wnd_extra: class.wnd_extra,
        icon: class.icon,
        cursor: class.cursor,
        background: class.background,
        menu_name: class.menu_name,
        icon_small: class.icon_small,
        unicode,
        extra: vec![0; class.cls_extra as usize],
        registered: true,
    });
    atom
}

/// Reads the title's class.
fn read<T: Copy>(value: *const T) -> Option<T> {
    // SAFETY: checked non-null; the title's structure.
    (!value.is_null()).then(|| unsafe { value.read_unaligned() })
}

pub extern "win64" fn RegisterClassW(class: *const WNDCLASSW) -> u16 {
    match read(class) {
        Some(class) => register_class(&class.ex(), true),
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

pub extern "win64" fn RegisterClassA(class: *const WNDCLASSA) -> u16 {
    match read(class) {
        Some(class) => register_class(&class.ex(), false),
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

pub extern "win64" fn RegisterClassExW(class: *const WNDCLASSEXW) -> u16 {
    match read(class).filter(|c| c.size == 80) {
        Some(class) => register_class(&class, true),
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

pub extern "win64" fn RegisterClassExA(class: *const WNDCLASSEXA) -> u16 {
    match read(class).filter(|c| c.size == 80) {
        Some(class) => register_class(&class, false),
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

/// `UnregisterClass`'s body: refused while windows of the class live.
fn unregister_class(name: Option<Name>, instance: usize) -> BOOL {
    let Some(name) = name else {
        return fail(ERROR_CLASS_DOES_NOT_EXIST, FALSE);
    };
    let mut classes = classes();
    let instance = instance_of(instance);
    let Some(index) = classes.iter().position(|c| {
        c.registered && matches_name(c, &name) && instance_of(c.instance) == instance
    }) else {
        return fail(ERROR_CLASS_DOES_NOT_EXIST, FALSE);
    };
    if windows().iter().any(|w| w.class == index && !w.destroyed) {
        return fail(ERROR_CLASS_HAS_WINDOWS, FALSE);
    }
    classes[index].registered = false;
    TRUE
}

pub extern "win64" fn UnregisterClassW(name: *const u16, instance: usize) -> BOOL {
    unregister_class(name_w(name), instance)
}

pub extern "win64" fn UnregisterClassA(name: *const u8, instance: usize) -> BOOL {
    unregister_class(name_a(name), instance)
}

/// `GetClassInfo`'s body: the class as registered (its name pointer the
/// one asked with); its atom.
fn class_info(instance: usize, name: Option<Name>, given: usize, out: *mut WNDCLASSEXW) -> u16 {
    let classes = classes();
    let found = name.and_then(|name| find_class(&classes, &name, instance));
    let Some(class) = found.map(|i| &classes[i]) else {
        return fail(ERROR_CLASS_DOES_NOT_EXIST, 0);
    };
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    let info = WNDCLASSEXW {
        size: 80,
        style: class.style,
        wndproc: class.wndproc,
        cls_extra: class.cls_extra,
        wnd_extra: class.wnd_extra,
        instance: class.instance,
        icon: class.icon,
        cursor: class.cursor,
        background: class.background,
        menu_name: class.menu_name,
        class_name: given,
        icon_small: class.icon_small,
    };
    // SAFETY: checked non-null; the title's structure.
    unsafe { out.write_unaligned(info) };
    class.atom
}

/// [`class_info`] into the shorter `WNDCLASS`.
fn class_info_short(
    instance: usize,
    name: Option<Name>,
    given: usize,
    out: *mut WNDCLASSW,
) -> BOOL {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let mut ex = std::mem::MaybeUninit::<WNDCLASSEXW>::zeroed();
    if class_info(instance, name, given, ex.as_mut_ptr()) == 0 {
        return FALSE;
    }
    // SAFETY: filled in above.
    let ex = unsafe { ex.assume_init() };
    let short = WNDCLASSW {
        style: ex.style,
        wndproc: ex.wndproc,
        cls_extra: ex.cls_extra,
        wnd_extra: ex.wnd_extra,
        instance: ex.instance,
        icon: ex.icon,
        cursor: ex.cursor,
        background: ex.background,
        menu_name: ex.menu_name,
        class_name: ex.class_name,
    };
    // SAFETY: checked non-null; the title's structure.
    unsafe { out.write_unaligned(short) };
    TRUE
}

pub extern "win64" fn GetClassInfoW(
    instance: usize,
    name: *const u16,
    out: *mut WNDCLASSW,
) -> BOOL {
    class_info_short(instance, name_w(name), name as usize, out)
}

pub extern "win64" fn GetClassInfoA(instance: usize, name: *const u8, out: *mut WNDCLASSA) -> BOOL {
    class_info_short(instance, name_a(name), name as usize, out)
}

pub extern "win64" fn GetClassInfoExW(
    instance: usize,
    name: *const u16,
    out: *mut WNDCLASSEXW,
) -> BOOL {
    BOOL::from(class_info(instance, name_w(name), name as usize, out))
}

pub extern "win64" fn GetClassInfoExA(
    instance: usize,
    name: *const u8,
    out: *mut WNDCLASSEXA,
) -> BOOL {
    BOOL::from(class_info(instance, name_a(name), name as usize, out))
}

/// A window's class name; the desktop's is `#32769`.
fn class_name_of(hwnd: HWND) -> Option<String> {
    let class = live(hwnd, |w| w.class)?;
    if class == usize::MAX {
        return Some("#32769".to_owned());
    }
    classes().get(class).map(|c| c.name.clone())
}

/// Copies `text` and a NUL to a buffer of `cch` units, truncating; the
/// units copied without the NUL.
fn copy_truncated<T: Copy + Default>(text: &[T], out: *mut T, cch: i32) -> i32 {
    if out.is_null() || cch <= 0 {
        return 0;
    }
    let n = text.len().min(cch as usize - 1);
    // SAFETY: the title's buffer holds `cch` units.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), out, n);
        out.add(n).write(T::default());
    }
    n as i32
}

pub extern "win64" fn GetClassNameW(hwnd: HWND, out: *mut u16, cch: i32) -> i32 {
    match class_name_of(hwnd) {
        Some(name) => copy_truncated(&name.encode_utf16().collect::<Vec<_>>(), out, cch),
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
    }
}

pub extern "win64" fn GetClassNameA(hwnd: HWND, out: *mut u8, cch: i32) -> i32 {
    match class_name_of(hwnd) {
        Some(name) => {
            let bytes = narrow_z(&name);
            copy_truncated(&bytes[..bytes.len() - 1], out, cch)
        }
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
    }
}

const GCLP_MENUNAME: i32 = -8;
const GCLP_HBRBACKGROUND: i32 = -10;
const GCLP_HCURSOR: i32 = -12;
const GCLP_HICON: i32 = -14;
const GCLP_HMODULE: i32 = -16;
const GCL_CBWNDEXTRA: i32 = -18;
const GCL_CBCLSEXTRA: i32 = -20;
const GCLP_WNDPROC: i32 = -24;
const GCL_STYLE: i32 = -26;
const GCW_ATOM: i32 = -32;
const GCLP_HICONSM: i32 = -34;

/// `Get/SetClassLongPtr`'s body: the old value, `new` stored if given.
fn class_long(hwnd: HWND, index: i32, new: Option<usize>) -> usize {
    let Some(class) = live(hwnd, |w| w.class) else {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    };
    let mut classes = classes();
    let Some(c) = classes.get_mut(class) else {
        return fail(ERROR_INVALID_INDEX, 0);
    };
    if index >= 0 {
        let at = index as usize;
        let Some(bytes) = c.extra.get_mut(at..at + 8) else {
            return fail(ERROR_INVALID_INDEX, 0);
        };
        let old = usize::from_le_bytes(bytes.try_into().unwrap_or_default());
        if let Some(new) = new {
            bytes.copy_from_slice(&new.to_le_bytes());
        }
        return old;
    }
    let field: &mut usize = match index {
        GCLP_MENUNAME => &mut c.menu_name,
        GCLP_HBRBACKGROUND => &mut c.background,
        GCLP_HCURSOR => &mut c.cursor,
        GCLP_HICON => &mut c.icon,
        GCLP_HMODULE => &mut c.instance,
        GCLP_WNDPROC => &mut c.wndproc,
        GCLP_HICONSM => &mut c.icon_small,
        GCL_STYLE => {
            let old = c.style as usize;
            if let Some(new) = new {
                c.style = new as u32;
            }
            return old;
        }
        GCL_CBWNDEXTRA => return c.wnd_extra as usize,
        GCL_CBCLSEXTRA => return c.cls_extra as usize,
        GCW_ATOM => return usize::from(c.atom),
        _ => return fail(ERROR_INVALID_INDEX, 0),
    };
    let old = *field;
    if let Some(new) = new {
        *field = new;
    }
    old
}

pub extern "win64" fn GetClassLongPtrW(hwnd: HWND, index: i32) -> usize {
    class_long(hwnd, index, None)
}

pub extern "win64" fn GetClassLongPtrA(hwnd: HWND, index: i32) -> usize {
    class_long(hwnd, index, None)
}

pub extern "win64" fn SetClassLongPtrW(hwnd: HWND, index: i32, value: usize) -> usize {
    class_long(hwnd, index, Some(value))
}

pub extern "win64" fn SetClassLongPtrA(hwnd: HWND, index: i32, value: usize) -> usize {
    class_long(hwnd, index, Some(value))
}

// Windows.

struct Window {
    hwnd: HWND,
    /// An index into the classes; `usize::MAX` for the desktop's.
    class: usize,
    thread: u32,
    instance: usize,
    /// The parent of a child window; the owner of a top-level one.
    parent: HWND,
    /// A top-level window's menu (kept as Windows keeps it; nothing here
    /// reads it yet).
    #[allow(dead_code)]
    menu: usize,
    style: u32,
    exstyle: u32,
    /// On the screen.
    rect: RECT,
    /// The client area: the whole window (no frame here).
    client: RECT,
    title: String,
    user_data: isize,
    wndproc: usize,
    id: isize,
    extra: Vec<u8>,
    visible: bool,
    enabled: bool,
    destroyed: bool,
    /// `DestroyWindow` is under way (it is not started twice).
    destroying: bool,
    props: Vec<(String, usize)>,
    unicode: bool,
}

impl Window {
    fn child(&self) -> bool {
        self.style & WS_CHILD != 0
    }
}

static WINDOWS: Mutex<Vec<Window>> = Mutex::new(Vec::new());

/// The windows, the desktop first.
fn windows() -> MutexGuard<'static, Vec<Window>> {
    let mut windows = lock(&WINDOWS);
    if windows.is_empty() {
        windows.push(Window {
            hwnd: DESKTOP,
            class: usize::MAX,
            thread: 0,
            instance: 0,
            parent: 0,
            menu: 0,
            style: WS_VISIBLE,
            exstyle: 0,
            rect: SCREEN,
            client: SCREEN,
            title: String::new(),
            user_data: 0,
            wndproc: 0,
            id: 0,
            extra: Vec::new(),
            visible: true,
            enabled: true,
            destroyed: false,
            destroying: false,
            props: Vec::new(),
            unicode: true,
        });
    }
    windows
}

/// The index of the record a handle names.
fn index(hwnd: HWND) -> Option<usize> {
    let offset = hwnd.checked_sub(DESKTOP)?;
    offset
        .is_multiple_of(HWND_STEP)
        .then_some(offset / HWND_STEP)
}

/// Runs `f` on a live window's record (`WINDOWS` locked meanwhile: `f`
/// calls no procedure).
fn live<R>(hwnd: HWND, f: impl FnOnce(&mut Window) -> R) -> Option<R> {
    let i = index(hwnd)?;
    let mut windows = windows();
    windows.get_mut(i).filter(|w| !w.destroyed).map(f)
}

/// Calls a window procedure (none: 0).
fn call(proc: usize, hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> LRESULT {
    if proc == 0 {
        return 0;
    }
    type Procedure = extern "win64" fn(HWND, u32, usize, isize) -> LRESULT;
    // SAFETY: a window procedure the title registered (or ours), called as
    // the system calls one.
    let proc: Procedure = unsafe { std::mem::transmute(proc) };
    proc(hwnd, msg, wparam, lparam)
}

/// A window and its descendants.
fn family(hwnd: HWND) -> Vec<HWND> {
    let windows = windows();
    let mut out = vec![hwnd];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        out.extend(
            windows
                .iter()
                .filter(|w| !w.destroyed && w.child() && w.parent == parent)
                .map(|w| w.hwnd),
        );
        i += 1;
    }
    out
}

/// The window's parent chain is all visible.
fn visible_chain(hwnd: HWND) -> bool {
    let mut at = hwnd;
    loop {
        match live(at, |w| (w.visible, w.child(), w.parent)) {
            Some((true, true, parent)) => at = parent,
            Some((true, false, _)) => return true,
            _ => return false,
        }
    }
}

/// The arguments of `CreateWindowEx`, as the procedure sees them.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CREATESTRUCTW {
    pub create_params: usize,
    pub instance: usize,
    pub menu: usize,
    pub parent: HWND,
    pub cy: i32,
    pub cx: i32,
    pub y: i32,
    pub x: i32,
    pub style: i32,
    pub name: usize,
    pub class: usize,
    pub exstyle: u32,
}

const _: () = assert!(std::mem::size_of::<CREATESTRUCTW>() == 80);

/// A window's place, as `WM_WINDOWPOSCHANGING` and `-CHANGED` carry it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WINDOWPOS {
    pub hwnd: HWND,
    pub insert_after: HWND,
    pub x: i32,
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub flags: u32,
}

const _: () = assert!(std::mem::size_of::<WINDOWPOS>() == 40);

/// What `CreateWindowEx` was given, its strings in either form.
struct Create {
    exstyle: u32,
    class: Option<Name>,
    class_pointer: usize,
    title: Option<String>,
    title_pointer: usize,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: HWND,
    menu: usize,
    instance: usize,
    param: usize,
    /// Called through the wide function.
    unicode: bool,
}

/// `CreateWindowEx`'s body.
fn create_window(args: Create) -> HWND {
    let Some(class_name) = args.class.clone() else {
        return fail(ERROR_CANNOT_FIND_WND_CLASS, 0);
    };
    let (class, wndproc, wnd_extra, unicode) = {
        let classes = classes();
        let Some(i) = find_class(&classes, &class_name, args.instance) else {
            return fail(ERROR_CANNOT_FIND_WND_CLASS, 0);
        };
        let c = &classes[i];
        (i, c.wndproc, c.wnd_extra, c.unicode)
    };
    let child = args.style & WS_CHILD != 0;
    if child && live(args.parent, |_| ()).is_none() {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    }
    let (mut x, mut y) = (args.x, args.y);
    if x == CW_USEDEFAULT {
        (x, y) = (0, 0);
    }
    let (w, h) = if args.w == CW_USEDEFAULT {
        (SCREEN_WIDTH, SCREEN_HEIGHT)
    } else {
        (args.w.max(0), args.h.max(0))
    };
    let origin = if child {
        live(args.parent, |p| (p.client.left, p.client.top)).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let rect = RECT {
        left: origin.0 + x,
        top: origin.1 + y,
        right: origin.0 + x + w,
        bottom: origin.1 + y + h,
    };
    let hwnd = {
        let mut windows = windows();
        let hwnd = DESKTOP + HWND_STEP * windows.len();
        windows.push(Window {
            hwnd,
            class,
            thread: thread_id(),
            instance: args.instance,
            parent: args.parent,
            menu: if child { 0 } else { args.menu },
            style: args.style & !WS_VISIBLE,
            exstyle: args.exstyle,
            rect,
            client: rect,
            title: String::new(),
            user_data: 0,
            wndproc,
            id: if child { args.menu as isize } else { 0 },
            extra: vec![0; wnd_extra.max(0) as usize],
            visible: false,
            enabled: args.style & WS_DISABLED == 0,
            destroyed: false,
            destroying: false,
            props: Vec::new(),
            unicode,
        });
        hwnd
    };
    // The procedure's strings are in its own form; the pointers given are
    // passed when they already are.
    let mut keep_wide: Vec<Vec<u16>> = Vec::new();
    let mut keep_narrow: Vec<Vec<u8>> = Vec::new();
    let mut string = |text: Option<String>, pointer: usize, atom: bool| -> usize {
        if atom || pointer == 0 || unicode == args.unicode {
            return pointer;
        }
        let text = text.unwrap_or_default();
        if unicode {
            keep_wide.push(wide_z(&text));
            keep_wide.last().map_or(0, |t| t.as_ptr() as usize)
        } else {
            keep_narrow.push(narrow_z(&text));
            keep_narrow.last().map_or(0, |t| t.as_ptr() as usize)
        }
    };
    let class_atom = matches!(class_name, Name::Atom(_));
    let class_text = Some(name_text(&class_name));
    let cs = CREATESTRUCTW {
        create_params: args.param,
        instance: args.instance,
        menu: args.menu,
        parent: args.parent,
        cy: h,
        cx: w,
        y,
        x,
        style: args.style as i32,
        name: string(args.title.clone(), args.title_pointer, false),
        class: string(class_text, args.class_pointer, class_atom),
        exstyle: args.exstyle,
    };
    let cs_pointer = (&raw const cs) as isize;
    if call(wndproc, hwnd, WM_NCCREATE, 0, cs_pointer) == 0 {
        live(hwnd, |w| w.destroyed = true);
        return 0;
    }
    if call(wndproc, hwnd, WM_CREATE, 0, cs_pointer) == -1 {
        DestroyWindow(hwnd);
        return 0;
    }
    drop((keep_wide, keep_narrow));
    if args.style & WS_VISIBLE != 0 {
        show(hwnd, SW_SHOW);
    }
    hwnd
}

pub extern "win64" fn CreateWindowExW(
    exstyle: u32,
    class: *const u16,
    title: *const u16,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: HWND,
    menu: usize,
    instance: usize,
    param: *mut c_void,
) -> HWND {
    create_window(Create {
        exstyle,
        class: name_w(class),
        class_pointer: class as usize,
        title: (!title.is_null()).then(|| wide(title)),
        title_pointer: title as usize,
        style,
        x,
        y,
        w,
        h,
        parent,
        menu,
        instance,
        param: param as usize,
        unicode: true,
    })
}

pub extern "win64" fn CreateWindowExA(
    exstyle: u32,
    class: *const u8,
    title: *const u8,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: HWND,
    menu: usize,
    instance: usize,
    param: *mut c_void,
) -> HWND {
    create_window(Create {
        exstyle,
        class: name_a(class),
        class_pointer: class as usize,
        title: (!title.is_null()).then(|| narrow(title)),
        title_pointer: title as usize,
        style,
        x,
        y,
        w,
        h,
        parent,
        menu,
        instance,
        param: param as usize,
        unicode: false,
    })
}

// Showing, activating, placing.

const SW_HIDE: i32 = 0;
const SW_SHOWNORMAL: i32 = 1;
const SW_SHOWMINIMIZED: i32 = 2;
const SW_SHOWMAXIMIZED: i32 = 3;
const SW_SHOWNOACTIVATE: i32 = 4;
const SW_SHOW: i32 = 5;
const SW_MINIMIZE: i32 = 6;
const SW_SHOWMINNOACTIVE: i32 = 7;
const SW_SHOWNA: i32 = 8;
const SW_RESTORE: i32 = 9;
const SW_SHOWDEFAULT: i32 = 10;
const SW_FORCEMINIMIZE: i32 = 11;

const SWP_NOSIZE: u32 = 1;
const SWP_NOMOVE: u32 = 2;
const SWP_NOZORDER: u32 = 4;
const SWP_NOACTIVATE: u32 = 0x10;
const SWP_SHOWWINDOW: u32 = 0x40;
const SWP_HIDEWINDOW: u32 = 0x80;

/// Who has the focus, the activation and the cursor.
struct Input {
    foreground: HWND,
    /// Each thread's active and focus windows.
    active: HashMap<u32, HWND>,
    focus: HashMap<u32, HWND>,
    capture: HWND,
    cursor_pos: POINT,
    clip: Option<RECT>,
    cursor: usize,
    show_count: i32,
}

static INPUT: LazyLock<Mutex<Input>> = LazyLock::new(|| {
    Mutex::new(Input {
        foreground: 0,
        active: HashMap::new(),
        focus: HashMap::new(),
        capture: 0,
        cursor_pos: POINT { x: 960, y: 540 },
        clip: None,
        cursor: 0,
        show_count: 0,
    })
});

fn input() -> MutexGuard<'static, Input> {
    lock(&INPUT)
}

/// Sends `msg` to a window and ignores its answer.
fn notify(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) {
    let _ = send(hwnd, msg, wparam, lparam, None);
}

/// Makes a top-level window the active, focus and foreground window of its
/// thread, with the messages Windows sends.
fn activate(hwnd: HWND) {
    let Some(thread) = live(hwnd, |w| w.thread) else {
        return;
    };
    let (previous, focus, app_active) = {
        let mut input = input();
        let previous = input.active.insert(thread, hwnd).unwrap_or(0);
        let focus = input.focus.insert(thread, hwnd).unwrap_or(0);
        let foreground = std::mem::replace(&mut input.foreground, hwnd);
        (previous, focus, foreground != 0 && foreground != hwnd)
    };
    let previous = if live(previous, |_| ()).is_some() {
        previous
    } else {
        0
    };
    if previous == hwnd {
        return;
    }
    if focus != 0 && focus != hwnd {
        notify(focus, WM_KILLFOCUS, hwnd, 0);
    }
    if previous != 0 {
        notify(previous, WM_NCACTIVATE, 0, 0);
        notify(previous, WM_ACTIVATE, WA_INACTIVE, hwnd as isize);
    }
    if !app_active {
        notify(hwnd, WM_ACTIVATEAPP, 1, 0);
    }
    notify(hwnd, WM_NCACTIVATE, 1, 0);
    notify(hwnd, WM_ACTIVATE, WA_ACTIVE, previous as isize);
    notify(hwnd, WM_SETFOCUS, focus, 0);
}

/// `WM_SIZE` and `WM_MOVE` of the window's client area.
fn size_and_move(hwnd: HWND, sized: bool, moved: bool) {
    let Some((client, child, parent)) = live(hwnd, |w| (w.client, w.child(), w.parent)) else {
        return;
    };
    let origin = if child {
        live(parent, |p| (p.client.left, p.client.top)).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    if sized {
        notify(
            hwnd,
            WM_SIZE,
            SIZE_RESTORED,
            pack(client.width(), client.height()),
        );
    }
    if moved {
        notify(
            hwnd,
            WM_MOVE,
            0,
            pack(client.left - origin.0, client.top - origin.1),
        );
    }
}

/// The window's `WINDOWPOS` now.
fn window_pos(hwnd: HWND, flags: u32) -> WINDOWPOS {
    let rect = live(hwnd, |w| w.rect).unwrap_or_default();
    WINDOWPOS {
        hwnd,
        insert_after: 0,
        x: rect.left,
        y: rect.top,
        cx: rect.width(),
        cy: rect.height(),
        flags,
    }
}

/// `ShowWindow`'s body: whether the window was visible.
fn show(hwnd: HWND, cmd: i32) -> BOOL {
    let Some((was, child)) = live(hwnd, |w| (w.visible, w.child())) else {
        return fail(ERROR_INVALID_WINDOW_HANDLE, FALSE);
    };
    if hwnd == DESKTOP {
        return TRUE;
    }
    match cmd {
        SW_HIDE => {
            if was {
                notify(hwnd, WM_SHOWWINDOW, 0, 0);
                live(hwnd, |w| {
                    w.visible = false;
                    w.style &= !WS_VISIBLE;
                });
                let pos = window_pos(hwnd, SWP_HIDEWINDOW | SWP_NOSIZE | SWP_NOMOVE);
                notify(hwnd, WM_WINDOWPOSCHANGED, 0, (&raw const pos) as isize);
                let mut input = input();
                if input.foreground == hwnd {
                    input.foreground = 0;
                }
                input.active.retain(|_, w| *w != hwnd);
                input.focus.retain(|_, w| *w != hwnd);
            }
        }
        SW_SHOWNORMAL | SW_SHOWMINIMIZED | SW_SHOWMAXIMIZED | SW_SHOWNOACTIVATE | SW_SHOW
        | SW_MINIMIZE | SW_SHOWMINNOACTIVE | SW_SHOWNA | SW_RESTORE | SW_SHOWDEFAULT
        | SW_FORCEMINIMIZE => {
            if !was {
                live(hwnd, |w| {
                    w.visible = true;
                    w.style |= WS_VISIBLE;
                });
                notify(hwnd, WM_SHOWWINDOW, 1, 0);
                let pos = window_pos(hwnd, SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE);
                notify(hwnd, WM_WINDOWPOSCHANGED, 0, (&raw const pos) as isize);
                size_and_move(hwnd, true, true);
            }
            let passive = matches!(
                cmd,
                SW_SHOWNOACTIVATE | SW_SHOWNA | SW_SHOWMINNOACTIVE | SW_MINIMIZE | SW_FORCEMINIMIZE
            );
            if !passive && !child {
                activate(hwnd);
            }
        }
        _ => return fail(ERROR_INVALID_PARAMETER, FALSE),
    }
    BOOL::from(was)
}

pub extern "win64" fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL {
    show(hwnd, cmd)
}

/// As [`ShowWindow`] (the window's thread is this process's).
pub extern "win64" fn ShowWindowAsync(hwnd: HWND, cmd: i32) -> BOOL {
    show(hwnd, cmd)
}

/// Nothing paints.
pub extern "win64" fn UpdateWindow(hwnd: HWND) -> BOOL {
    BOOL::from(live(hwnd, |_| ()).is_some())
}

pub extern "win64" fn IsWindow(hwnd: HWND) -> BOOL {
    BOOL::from(live(hwnd, |_| ()).is_some())
}

/// The window and its parents are visible.
pub extern "win64" fn IsWindowVisible(hwnd: HWND) -> BOOL {
    BOOL::from(visible_chain(hwnd))
}

pub extern "win64" fn IsWindowEnabled(hwnd: HWND) -> BOOL {
    BOOL::from(live(hwnd, |w| w.enabled).unwrap_or(false))
}

/// Enables or disables input to the window (`WM_ENABLE` on a change);
/// whether it was disabled.
pub extern "win64" fn EnableWindow(hwnd: HWND, enable: BOOL) -> BOOL {
    let enable = enable != 0;
    let Some(was) = live(hwnd, |w| {
        let was = w.enabled;
        w.enabled = enable;
        if enable {
            w.style &= !WS_DISABLED;
        } else {
            w.style |= WS_DISABLED;
        }
        was
    }) else {
        return fail(ERROR_INVALID_WINDOW_HANDLE, FALSE);
    };
    if was != enable {
        notify(hwnd, WM_ENABLE, usize::from(enable), 0);
    }
    BOOL::from(!was)
}

/// Never minimized: there is no desktop to minimize to.
pub extern "win64" fn IsIconic(_hwnd: HWND) -> BOOL {
    FALSE
}

/// Never maximized.
pub extern "win64" fn IsZoomed(_hwnd: HWND) -> BOOL {
    FALSE
}

/// Timers of a window or a thread, posting `WM_TIMER`.
struct Timer {
    hwnd: HWND,
    id: usize,
    thread: u32,
    interval: Duration,
    due: Instant,
    proc: usize,
}

static TIMERS: Mutex<Vec<Timer>> = Mutex::new(Vec::new());
static TIMERS_CHANGED: Condvar = Condvar::new();

/// Sends `WM_DESTROY` to the window, destroys its children, sends
/// `WM_NCDESTROY`, and marks it destroyed (its handle is never used
/// again).
fn destroy(hwnd: HWND) {
    let Some(proc) = live(hwnd, |w| {
        let started = w.destroying;
        w.destroying = true;
        (!started).then_some(w.wndproc)
    })
    .flatten() else {
        return;
    };
    call(proc, hwnd, WM_DESTROY, 0, 0);
    let children: Vec<HWND> = windows()
        .iter()
        .filter(|w| !w.destroyed && w.child() && w.parent == hwnd)
        .map(|w| w.hwnd)
        .collect();
    for child in children {
        destroy(child);
    }
    let proc = live(hwnd, |w| w.wndproc).unwrap_or(proc);
    call(proc, hwnd, WM_NCDESTROY, 0, 0);
    live(hwnd, |w| w.destroyed = true);
    lock(&TIMERS).retain(|t| t.hwnd != hwnd);
    let mut input = input();
    if input.foreground == hwnd {
        input.foreground = 0;
    }
    if input.capture == hwnd {
        input.capture = 0;
    }
    input.active.retain(|_, w| *w != hwnd);
    input.focus.retain(|_, w| *w != hwnd);
}

/// Destroys a window of the calling thread (another thread's is refused).
pub extern "win64" fn DestroyWindow(hwnd: HWND) -> BOOL {
    match live(hwnd, |w| w.thread) {
        None => fail(ERROR_INVALID_WINDOW_HANDLE, FALSE),
        Some(_) if hwnd == DESKTOP => fail(ERROR_ACCESS_DENIED, FALSE),
        Some(thread) if thread != thread_id() => fail(ERROR_ACCESS_DENIED, FALSE),
        Some(_) => {
            destroy(hwnd);
            TRUE
        }
    }
}

/// Minimizing: nothing to do here.
pub extern "win64" fn CloseWindow(hwnd: HWND) -> BOOL {
    BOOL::from(live(hwnd, |_| ()).is_some())
}

/// Writes a rectangle for the title.
fn put_rect(out: *mut RECT, rect: Option<RECT>) -> BOOL {
    match rect {
        None => fail(ERROR_INVALID_WINDOW_HANDLE, FALSE),
        Some(_) if out.is_null() => fail(ERROR_INVALID_PARAMETER, FALSE),
        Some(rect) => {
            // SAFETY: checked non-null; the title's rectangle.
            unsafe { out.write_unaligned(rect) };
            TRUE
        }
    }
}

/// `(0, 0)` to the client area's size.
pub extern "win64" fn GetClientRect(hwnd: HWND, out: *mut RECT) -> BOOL {
    let rect = live(hwnd, |w| RECT {
        left: 0,
        top: 0,
        right: w.client.width(),
        bottom: w.client.height(),
    });
    put_rect(out, rect)
}

/// The window on the screen.
pub extern "win64" fn GetWindowRect(hwnd: HWND, out: *mut RECT) -> BOOL {
    put_rect(out, live(hwnd, |w| w.rect))
}

/// Windows have no frame here: the rectangle stays as it is.
pub extern "win64" fn AdjustWindowRect(rect: *mut RECT, _style: u32, _menu: BOOL) -> BOOL {
    BOOL::from(!rect.is_null())
}

/// See [`AdjustWindowRect`].
pub extern "win64" fn AdjustWindowRectEx(
    rect: *mut RECT,
    _style: u32,
    _menu: BOOL,
    _exstyle: u32,
) -> BOOL {
    BOOL::from(!rect.is_null())
}

/// See [`AdjustWindowRect`].
pub extern "win64" fn AdjustWindowRectExForDpi(
    rect: *mut RECT,
    _style: u32,
    _menu: BOOL,
    _exstyle: u32,
    _dpi: u32,
) -> BOOL {
    BOOL::from(!rect.is_null())
}

/// Moves, sizes, shows or hides the window, with `WM_WINDOWPOSCHANGING`
/// (whose changes to the place are taken), `WM_WINDOWPOSCHANGED`, then
/// `WM_SIZE` and `WM_MOVE` for what changed. The z-order is not kept, and
/// `SWP_FRAMECHANGED` changes nothing (there is no frame).
pub extern "win64" fn SetWindowPos(
    hwnd: HWND,
    after: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: u32,
) -> BOOL {
    let Some((rect, child, parent)) = live(hwnd, |w| (w.rect, w.child(), w.parent)) else {
        return fail(ERROR_INVALID_WINDOW_HANDLE, FALSE);
    };
    if hwnd == DESKTOP {
        return TRUE;
    }
    let origin = if child {
        live(parent, |p| (p.client.left, p.client.top)).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let mut pos = WINDOWPOS {
        hwnd,
        insert_after: after,
        x: if flags & SWP_NOMOVE != 0 {
            rect.left - origin.0
        } else {
            x
        },
        y: if flags & SWP_NOMOVE != 0 {
            rect.top - origin.1
        } else {
            y
        },
        cx: if flags & SWP_NOSIZE != 0 {
            rect.width()
        } else {
            w.max(0)
        },
        cy: if flags & SWP_NOSIZE != 0 {
            rect.height()
        } else {
            h.max(0)
        },
        flags,
    };
    notify(hwnd, WM_WINDOWPOSCHANGING, 0, (&raw mut pos) as isize);
    let new = RECT {
        left: origin.0 + pos.x,
        top: origin.1 + pos.y,
        right: origin.0 + pos.x + pos.cx,
        bottom: origin.1 + pos.y + pos.cy,
    };
    let moved = (new.left, new.top) != (rect.left, rect.top);
    let sized = (new.width(), new.height()) != (rect.width(), rect.height());
    live(hwnd, |w| {
        w.rect = new;
        w.client = new;
        if pos.flags & SWP_SHOWWINDOW != 0 {
            w.visible = true;
            w.style |= WS_VISIBLE;
        }
        if pos.flags & SWP_HIDEWINDOW != 0 {
            w.visible = false;
            w.style &= !WS_VISIBLE;
        }
    });
    notify(hwnd, WM_WINDOWPOSCHANGED, 0, (&raw const pos) as isize);
    size_and_move(hwnd, sized, moved);
    let visible = live(hwnd, |w| w.visible).unwrap_or(false);
    if pos.flags & SWP_NOACTIVATE == 0 && visible && !child {
        activate(hwnd);
    }
    TRUE
}

/// [`SetWindowPos`] of the place and size.
pub extern "win64" fn MoveWindow(
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    _repaint: BOOL,
) -> BOOL {
    SetWindowPos(hwnd, 0, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE)
}

const GWLP_WNDPROC: i32 = -4;
const GWLP_HINSTANCE: i32 = -6;
const GWLP_HWNDPARENT: i32 = -8;
const GWLP_ID: i32 = -12;
const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;
const GWLP_USERDATA: i32 = -21;

/// `Get/SetWindowLong(Ptr)`'s body: the old value, `new` stored if given;
/// `width` bytes of the extra bytes at a positive index.
fn window_long(hwnd: HWND, index: i32, new: Option<isize>, width: usize) -> isize {
    let result = live(hwnd, |w| {
        if index >= 0 {
            let at = index as usize;
            let bytes = w.extra.get_mut(at..at + width).ok_or(ERROR_INVALID_INDEX)?;
            let mut value = [0u8; 8];
            value[..width].copy_from_slice(bytes);
            let old = i64::from_le_bytes(value);
            let old = if width == 4 {
                i64::from(old as i32)
            } else {
                old
            };
            if let Some(new) = new {
                bytes.copy_from_slice(&new.to_le_bytes()[..width]);
            }
            return Ok(old as isize);
        }
        let field: &mut isize = match index {
            GWLP_USERDATA => &mut w.user_data,
            GWLP_ID => &mut w.id,
            _ => {
                let (old, set): (isize, fn(&mut Window, isize)) = match index {
                    GWLP_WNDPROC => (w.wndproc as isize, |w, v| w.wndproc = v as usize),
                    GWLP_HINSTANCE => (w.instance as isize, |w, v| w.instance = v as usize),
                    GWLP_HWNDPARENT => (w.parent as isize, |w, v| w.parent = v as usize),
                    GWL_STYLE => (w.style as i32 as isize, |w, v| w.style = v as u32),
                    GWL_EXSTYLE => (w.exstyle as i32 as isize, |w, v| w.exstyle = v as u32),
                    _ => return Err(ERROR_INVALID_INDEX),
                };
                if let Some(new) = new {
                    set(w, new);
                }
                return Ok(old);
            }
        };
        let old = *field;
        if let Some(new) = new {
            *field = new;
        }
        Ok(old)
    });
    match result {
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
        Some(Err(code)) => fail(code, 0),
        Some(Ok(old)) => old,
    }
}

pub extern "win64" fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize {
    window_long(hwnd, index, None, 8)
}

pub extern "win64" fn GetWindowLongPtrA(hwnd: HWND, index: i32) -> isize {
    window_long(hwnd, index, None, 8)
}

pub extern "win64" fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize {
    window_long(hwnd, index, Some(value), 8)
}

pub extern "win64" fn SetWindowLongPtrA(hwnd: HWND, index: i32, value: isize) -> isize {
    window_long(hwnd, index, Some(value), 8)
}

/// The 32-bit forms: a pointer-sized index (the procedure, the instance,
/// the parent) is not one of theirs on 64-bit Windows.
fn window_long_32(hwnd: HWND, index: i32, new: Option<i32>) -> i32 {
    if matches!(index, GWLP_WNDPROC | GWLP_HINSTANCE | GWLP_HWNDPARENT) {
        return fail(ERROR_INVALID_INDEX, 0);
    }
    window_long(hwnd, index, new.map(|v| v as isize), 4) as i32
}

pub extern "win64" fn GetWindowLongW(hwnd: HWND, index: i32) -> i32 {
    window_long_32(hwnd, index, None)
}

pub extern "win64" fn GetWindowLongA(hwnd: HWND, index: i32) -> i32 {
    window_long_32(hwnd, index, None)
}

pub extern "win64" fn SetWindowLongW(hwnd: HWND, index: i32, value: i32) -> i32 {
    window_long_32(hwnd, index, Some(value))
}

pub extern "win64" fn SetWindowLongA(hwnd: HWND, index: i32, value: i32) -> i32 {
    window_long_32(hwnd, index, Some(value))
}

/// Whether the window takes wide strings.
fn unicode(hwnd: HWND) -> Option<bool> {
    live(hwnd, |w| w.unicode)
}

/// `WM_SETTEXT` in the window's own form.
fn set_text(hwnd: HWND, text: &str) -> BOOL {
    let result = match unicode(hwnd) {
        None => return fail(ERROR_INVALID_WINDOW_HANDLE, FALSE),
        Some(true) => send_wait(hwnd, WM_SETTEXT, 0, wide_z(text).as_ptr() as isize),
        Some(false) => send_wait(hwnd, WM_SETTEXT, 0, narrow_z(text).as_ptr() as isize),
    };
    BOOL::from(result != 0)
}

pub extern "win64" fn SetWindowTextW(hwnd: HWND, text: *const u16) -> BOOL {
    set_text(hwnd, &wide(text))
}

pub extern "win64" fn SetWindowTextA(hwnd: HWND, text: *const u8) -> BOOL {
    set_text(hwnd, &narrow(text))
}

/// `WM_GETTEXT` in the window's own form, as text.
fn get_text(hwnd: HWND, room: usize) -> Option<String> {
    let room = room.max(1);
    Some(if unicode(hwnd)? {
        let mut buffer = vec![0u16; room];
        let n = send_wait(hwnd, WM_GETTEXT, room, buffer.as_mut_ptr() as isize).max(0);
        String::from_utf16_lossy(&buffer[..(n as usize).min(room - 1)])
    } else {
        let mut buffer = vec![0u8; room];
        let n = send_wait(hwnd, WM_GETTEXT, room, buffer.as_mut_ptr() as isize).max(0);
        buffer[..(n as usize).min(room - 1)]
            .iter()
            .map(|&b| char::from(b))
            .collect()
    })
}

pub extern "win64" fn GetWindowTextW(hwnd: HWND, out: *mut u16, cch: i32) -> i32 {
    if cch <= 0 || out.is_null() {
        return 0;
    }
    match get_text(hwnd, cch as usize) {
        Some(text) => copy_truncated(&text.encode_utf16().collect::<Vec<_>>(), out, cch),
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
    }
}

pub extern "win64" fn GetWindowTextA(hwnd: HWND, out: *mut u8, cch: i32) -> i32 {
    if cch <= 0 || out.is_null() {
        return 0;
    }
    match get_text(hwnd, cch as usize) {
        Some(text) => {
            let bytes = narrow_z(&text);
            copy_truncated(&bytes[..bytes.len() - 1], out, cch)
        }
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
    }
}

pub extern "win64" fn GetWindowTextLengthW(hwnd: HWND) -> i32 {
    if unicode(hwnd).is_none() {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    }
    send_wait(hwnd, WM_GETTEXTLENGTH, 0, 0) as i32
}

pub extern "win64" fn GetWindowTextLengthA(hwnd: HWND) -> i32 {
    GetWindowTextLengthW(hwnd)
}

/// The window's top-level window: itself, or its parents' last.
fn top_level(hwnd: HWND) -> HWND {
    let mut at = hwnd;
    while let Some((true, parent)) = live(at, |w| (w.child(), w.parent)) {
        at = parent;
    }
    at
}

pub extern "win64" fn SetForegroundWindow(hwnd: HWND) -> BOOL {
    if live(hwnd, |_| ()).is_none() {
        return FALSE;
    }
    activate(top_level(hwnd));
    TRUE
}

pub extern "win64" fn GetForegroundWindow() -> HWND {
    input().foreground
}

/// Activates a top-level window of the calling thread; the one active
/// before.
pub extern "win64" fn SetActiveWindow(hwnd: HWND) -> HWND {
    let previous = GetActiveWindow();
    if live(hwnd, |_| ()).is_none() {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    }
    activate(top_level(hwnd));
    previous
}

pub extern "win64" fn GetActiveWindow() -> HWND {
    input().active.get(&thread_id()).copied().unwrap_or(0)
}

/// Gives the window the keyboard focus (`WM_KILLFOCUS`, `WM_SETFOCUS`;
/// its top-level window activated first); the window that had it.
pub extern "win64" fn SetFocus(hwnd: HWND) -> HWND {
    let me = thread_id();
    let previous = GetFocus();
    if hwnd == 0 {
        input().focus.remove(&me);
        if previous != 0 {
            notify(previous, WM_KILLFOCUS, 0, 0);
        }
        return previous;
    }
    if live(hwnd, |_| ()).is_none() {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    }
    let top = top_level(hwnd);
    if GetActiveWindow() != top {
        activate(top);
    }
    let focus = input().focus.insert(me, hwnd).unwrap_or(0);
    if focus != hwnd {
        if focus != 0 {
            notify(focus, WM_KILLFOCUS, hwnd, 0);
        }
        notify(hwnd, WM_SETFOCUS, focus, 0);
    }
    previous
}

pub extern "win64" fn GetFocus() -> HWND {
    input().focus.get(&thread_id()).copied().unwrap_or(0)
}

pub extern "win64" fn GetDesktopWindow() -> HWND {
    DESKTOP
}

/// A child window's parent, a pop-up's owner; else none.
pub extern "win64" fn GetParent(hwnd: HWND) -> HWND {
    match live(hwnd, |w| (w.style, w.parent)) {
        Some((style, parent)) if style & (WS_CHILD | WS_POPUP) != 0 => parent,
        Some(_) => 0,
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
    }
}

/// A new parent (none: the desktop); the old one.
pub extern "win64" fn SetParent(hwnd: HWND, parent: HWND) -> HWND {
    let parent = if parent == 0 { DESKTOP } else { parent };
    match live(hwnd, |w| std::mem::replace(&mut w.parent, parent)) {
        Some(0) => DESKTOP,
        Some(old) => old,
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
    }
}

const GW_OWNER: u32 = 4;

/// `GW_OWNER`: the owner (the parent recorded); the other relations are
/// not kept.
pub extern "win64" fn GetWindow(hwnd: HWND, cmd: u32) -> HWND {
    match live(hwnd, |w| (w.child(), w.parent)) {
        Some((false, parent)) if cmd == GW_OWNER => parent,
        Some(_) => 0,
        None => fail(ERROR_INVALID_WINDOW_HANDLE, 0),
    }
}

const GA_PARENT: u32 = 1;
const GA_ROOT: u32 = 2;
const GA_ROOTOWNER: u32 = 3;

/// The parent (the desktop for a top-level window), the root, or the root's
/// last owner.
pub extern "win64" fn GetAncestor(hwnd: HWND, flags: u32) -> HWND {
    let Some((child, parent)) = live(hwnd, |w| (w.child(), w.parent)) else {
        return 0;
    };
    if hwnd == DESKTOP {
        return 0;
    }
    match flags {
        GA_PARENT if child => parent,
        GA_PARENT => DESKTOP,
        GA_ROOT => top_level(hwnd),
        GA_ROOTOWNER => {
            let mut at = top_level(hwnd);
            while let Some(owner) = live(at, |w| w.parent).filter(|&o| o != 0 && o != DESKTOP) {
                at = top_level(owner);
            }
            at
        }
        _ => 0,
    }
}

/// The window's thread, and this process.
pub extern "win64" fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32 {
    let Some(thread) = live(hwnd, |w| w.thread) else {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    };
    if !pid.is_null() {
        // SAFETY: checked non-null; the title's output.
        unsafe { pid.write_unaligned(GetCurrentProcessId()) };
    }
    thread
}

/// The live windows `filter` takes, in creation order (not the desktop).
fn select(filter: impl Fn(&Window) -> bool) -> Vec<HWND> {
    windows()
        .iter()
        .skip(1)
        .filter(|w| !w.destroyed && filter(w))
        .map(|w| w.hwnd)
        .collect()
}

/// Whether a window's class (by index) and title are those asked (either
/// none: any; case is ignored).
fn named(class_index: usize, text: &str, class: &Option<Name>, title: &Option<String>) -> bool {
    let class_ok = class.as_ref().is_none_or(|name| {
        let classes = classes();
        classes
            .get(class_index)
            .is_some_and(|c| matches_name(c, name))
    });
    let title_ok = title
        .as_ref()
        .is_none_or(|t| t.to_lowercase() == text.to_lowercase());
    class_ok && title_ok
}

/// `FindWindowEx`'s body: children of `parent` (none: top-level windows)
/// after `after`.
fn find_window(parent: HWND, after: HWND, class: Option<Name>, title: Option<String>) -> HWND {
    let top = parent == 0 || parent == DESKTOP;
    let list = select(|w| {
        if top {
            !w.child()
        } else {
            w.child() && w.parent == parent
        }
    });
    let start = list.iter().position(|&h| h == after).map_or(0, |i| i + 1);
    let list = &list[if after == 0 { 0 } else { start }..];
    let found = list.iter().copied().find(|&h| {
        live(h, |w| (w.class, w.title.clone()))
            .is_some_and(|(index, text)| named(index, &text, &class, &title))
    });
    found.unwrap_or(0)
}

pub extern "win64" fn FindWindowW(class: *const u16, title: *const u16) -> HWND {
    let title = (!title.is_null()).then(|| wide(title));
    find_window(0, 0, name_w(class), title)
}

pub extern "win64" fn FindWindowA(class: *const u8, title: *const u8) -> HWND {
    let title = (!title.is_null()).then(|| narrow(title));
    find_window(0, 0, name_a(class), title)
}

pub extern "win64" fn FindWindowExW(
    parent: HWND,
    after: HWND,
    class: *const u16,
    title: *const u16,
) -> HWND {
    let title = (!title.is_null()).then(|| wide(title));
    find_window(parent, after, name_w(class), title)
}

pub extern "win64" fn FindWindowExA(
    parent: HWND,
    after: HWND,
    class: *const u8,
    title: *const u8,
) -> HWND {
    let title = (!title.is_null()).then(|| narrow(title));
    find_window(parent, after, name_a(class), title)
}

type EnumProc = Option<extern "win64" fn(HWND, isize) -> BOOL>;

/// Each top-level window to the callback, until it answers FALSE.
pub extern "win64" fn EnumWindows(callback: EnumProc, param: isize) -> BOOL {
    let Some(callback) = callback else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    for hwnd in select(|w| !w.child()) {
        if callback(hwnd, param) == FALSE {
            return FALSE;
        }
    }
    TRUE
}

/// Each top-level window of a thread; FALSE when it has none.
pub extern "win64" fn EnumThreadWindows(thread: u32, callback: EnumProc, param: isize) -> BOOL {
    let Some(callback) = callback else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    let list = select(|w| !w.child() && w.thread == thread);
    if list.is_empty() {
        return FALSE;
    }
    for hwnd in list {
        if callback(hwnd, param) == FALSE {
            return FALSE;
        }
    }
    TRUE
}

/// The window's client area origin on the screen.
fn client_origin(hwnd: HWND) -> Option<(i32, i32)> {
    if hwnd == 0 {
        return Some((0, 0));
    }
    live(hwnd, |w| (w.client.left, w.client.top))
}

/// Moves points by `(dx, dy)`.
fn offset_points(points: *mut POINT, count: usize, dx: i32, dy: i32) {
    for i in 0..count {
        // SAFETY: the title's `count` points.
        unsafe {
            let p = points.add(i);
            let mut value = p.read_unaligned();
            value.x += dx;
            value.y += dy;
            p.write_unaligned(value);
        }
    }
}

pub extern "win64" fn ClientToScreen(hwnd: HWND, p: *mut POINT) -> BOOL {
    match client_origin(hwnd) {
        Some((x, y)) if !p.is_null() => {
            offset_points(p, 1, x, y);
            TRUE
        }
        _ => FALSE,
    }
}

pub extern "win64" fn ScreenToClient(hwnd: HWND, p: *mut POINT) -> BOOL {
    match client_origin(hwnd) {
        Some((x, y)) if !p.is_null() => {
            offset_points(p, 1, -x, -y);
            TRUE
        }
        _ => FALSE,
    }
}

/// Points from one window's client coordinates to another's (none: the
/// screen's); the offset, x in the low word and y in the high.
pub extern "win64" fn MapWindowPoints(from: HWND, to: HWND, points: *mut POINT, count: u32) -> i32 {
    let (Some(a), Some(b)) = (client_origin(from), client_origin(to)) else {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    };
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    if !points.is_null() {
        offset_points(points, count as usize, dx, dy);
    }
    pack(dx, dy) as i32
}

/// The last-made visible window holding the point; the desktop if none.
pub extern "win64" fn WindowFromPoint(p: u64) -> HWND {
    let p = point(p);
    let list = select(|w| w.rect.holds(p));
    list.into_iter()
        .rev()
        .find(|&h| visible_chain(h))
        .unwrap_or(DESKTOP)
}

/// The figures of the fixed screen (M3 replaces them with the host's).
pub extern "win64" fn GetSystemMetrics(index: i32) -> i32 {
    match index {
        0 | 16 | 78 => SCREEN_WIDTH,
        1 | 79 => SCREEN_HEIGHT,
        2 | 3 => 17,
        4 => 23,
        5 | 6 => 1,
        17 => 1057,
        19 | 80 => 1,
        28 => 136,
        29 => 39,
        32 | 33 => 8,
        43 => 3,
        _ => 0,
    }
}

/// See [`GetSystemMetrics`]: at 96 DPI.
pub extern "win64" fn GetSystemMetricsForDpi(index: i32, _dpi: u32) -> i32 {
    GetSystemMetrics(index)
}

const SPI_GETBEEP: u32 = 1;
const SPI_GETSCREENSAVEACTIVE: u32 = 0x10;
const SPI_GETWORKAREA: u32 = 0x30;
const SPI_GETMOUSESPEED: u32 = 0x70;

/// The `SPI_SET*` actions titles use.
const SPI_SETTERS: [u32; 15] = [
    0x02, 0x04, 0x06, 0x0B, 0x0F, 0x11, 0x14, 0x17, 0x2F, 0x33, 0x35, 0x3B, 0x71, 0x1001, 0x2001,
];

/// `SystemParametersInfo`'s body.
fn parameters(action: u32, out: *mut c_void) -> BOOL {
    if SPI_SETTERS.contains(&action) {
        return TRUE;
    }
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's output of the action's type.
    unsafe {
        match action {
            SPI_GETWORKAREA => out.cast::<RECT>().write_unaligned(SCREEN),
            SPI_GETMOUSESPEED => out.cast::<i32>().write_unaligned(10),
            SPI_GETSCREENSAVEACTIVE | SPI_GETBEEP => out.cast::<BOOL>().write_unaligned(FALSE),
            _ => return fail(ERROR_INVALID_PARAMETER, FALSE),
        }
    }
    TRUE
}

pub extern "win64" fn SystemParametersInfoW(
    action: u32,
    _param: u32,
    out: *mut c_void,
    _winini: u32,
) -> BOOL {
    parameters(action, out)
}

pub extern "win64" fn SystemParametersInfoA(
    action: u32,
    _param: u32,
    out: *mut c_void,
    _winini: u32,
) -> BOOL {
    parameters(action, out)
}

pub extern "win64" fn GetDpiForWindow(hwnd: HWND) -> u32 {
    live(hwnd, |_| DPI).unwrap_or(0)
}

pub extern "win64" fn GetDpiForSystem() -> u32 {
    DPI
}

pub extern "win64" fn SetProcessDpiAwarenessContext(_context: isize) -> BOOL {
    TRUE
}

pub extern "win64" fn SetProcessDPIAware() -> BOOL {
    TRUE
}

/// The one fake device context.
pub extern "win64" fn GetDC(_hwnd: HWND) -> usize {
    HDC
}

pub extern "win64" fn GetWindowDC(_hwnd: HWND) -> usize {
    HDC
}

pub extern "win64" fn ReleaseDC(_hwnd: HWND, _hdc: usize) -> i32 {
    1
}

// The monitor.

const MONITOR_DEFAULTTONULL: u32 = 0;

pub extern "win64" fn MonitorFromWindow(_hwnd: HWND, _flags: u32) -> usize {
    HMONITOR
}

/// The monitor, unless the point is off it and `MONITOR_DEFAULTTONULL`.
pub extern "win64" fn MonitorFromPoint(p: u64, flags: u32) -> usize {
    if flags == MONITOR_DEFAULTTONULL && !SCREEN.holds(point(p)) {
        return 0;
    }
    HMONITOR
}

/// See [`MonitorFromPoint`].
pub extern "win64" fn MonitorFromRect(rect: *const RECT, flags: u32) -> usize {
    let off = read(rect).is_none_or(|r| {
        r.right <= 0 || r.bottom <= 0 || r.left >= SCREEN_WIDTH || r.top >= SCREEN_HEIGHT
    });
    if flags == MONITOR_DEFAULTTONULL && off {
        return 0;
    }
    HMONITOR
}

/// `MONITORINFO`, and the device name of the `-EX` forms after it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MONITORINFO {
    pub size: u32,
    pub monitor: RECT,
    pub work: RECT,
    pub flags: u32,
}

const _: () = assert!(std::mem::size_of::<MONITORINFO>() == 40);
const MONITORINFOF_PRIMARY: u32 = 1;

/// `GetMonitorInfo`'s body; `unit` the bytes of a device-name character.
fn monitor_info(monitor: usize, info: *mut MONITORINFO, unit: usize) -> BOOL {
    if monitor != HMONITOR || info.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's structure of `size` bytes.
    let size = unsafe { info.cast::<u32>().read_unaligned() } as usize;
    let ex = 40 + 32 * unit;
    if size != 40 && size != ex {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let base = MONITORINFO {
        size: size as u32,
        monitor: SCREEN,
        work: SCREEN,
        flags: MONITORINFOF_PRIMARY,
    };
    // SAFETY: as above.
    unsafe {
        info.write_unaligned(base);
        if size == ex {
            let device = info.cast::<u8>().add(40);
            std::ptr::write_bytes(device, 0, 32 * unit);
            for (i, c) in DISPLAY.bytes().enumerate() {
                if unit == 2 {
                    device.cast::<u16>().add(i).write_unaligned(u16::from(c));
                } else {
                    device.add(i).write(c);
                }
            }
        }
    }
    TRUE
}

pub extern "win64" fn GetMonitorInfoW(monitor: usize, info: *mut MONITORINFO) -> BOOL {
    monitor_info(monitor, info, 2)
}

pub extern "win64" fn GetMonitorInfoA(monitor: usize, info: *mut MONITORINFO) -> BOOL {
    monitor_info(monitor, info, 1)
}

type MonitorProc = Option<extern "win64" fn(usize, usize, *mut RECT, isize) -> BOOL>;

/// The one monitor to the callback.
pub extern "win64" fn EnumDisplayMonitors(
    hdc: usize,
    _clip: *const RECT,
    callback: MonitorProc,
    param: isize,
) -> BOOL {
    let Some(callback) = callback else {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    };
    let mut rect = SCREEN;
    callback(HMONITOR, hdc, &mut rect, param);
    TRUE
}

const ENUM_CURRENT_SETTINGS: u32 = u32::MAX;
const ENUM_REGISTRY_SETTINGS: u32 = u32::MAX - 1;
const DM_POSITION: u32 = 0x20;
const DM_BITSPERPEL: u32 = 0x4_0000;
const DM_PELSWIDTH: u32 = 0x8_0000;
const DM_PELSHEIGHT: u32 = 0x10_0000;
const DM_DISPLAYFLAGS: u32 = 0x20_0000;
const DM_DISPLAYFREQUENCY: u32 = 0x40_0000;

/// Writes `text` as `unit`-byte characters at `at` (room for `chars`).
///
/// # Safety
/// `at` holds `chars * unit` bytes.
unsafe fn put_name(at: *mut u8, chars: usize, unit: usize, text: &str) {
    // SAFETY: the caller's room.
    unsafe {
        std::ptr::write_bytes(at, 0, chars * unit);
        for (i, c) in text.chars().take(chars - 1).enumerate() {
            if unit == 2 {
                at.cast::<u16>().add(i).write_unaligned(c as u16);
            } else {
                at.add(i).write(u8::try_from(u32::from(c)).unwrap_or(b'?'));
            }
        }
    }
}

/// Whether a display device name is the one display's (none: it).
fn our_display(device: Option<String>) -> bool {
    device.is_none_or(|d| d.is_empty() || d.eq_ignore_ascii_case(DISPLAY))
}

/// `EnumDisplaySettings`'s body: the one mode, 1920 x 1080 at 32 bits and
/// 60 Hz, into a `DEVMODEA` (`unit` 1) or `DEVMODEW` (2).
fn display_settings(device: Option<String>, mode: u32, out: *mut u8, unit: usize) -> BOOL {
    if !our_display(device)
        || out.is_null()
        || !matches!(mode, 0 | ENUM_CURRENT_SETTINGS | ENUM_REGISTRY_SETTINGS)
    {
        return FALSE;
    }
    let names = 32 * unit;
    let size = if unit == 2 { 220 } else { 156 };
    // SAFETY: checked non-null; the title's DEVMODE of `size` bytes (its
    // offsets follow the two names' width).
    unsafe {
        let driver_extra = out.add(names + 6).cast::<u16>().read_unaligned();
        std::ptr::write_bytes(out, 0, size);
        put_name(out, 32, unit, DISPLAY);
        let field = |offset: usize| out.add(offset);
        field(names).cast::<u16>().write_unaligned(0x0401);
        field(names + 4).cast::<u16>().write_unaligned(size as u16);
        field(names + 6).cast::<u16>().write_unaligned(driver_extra);
        let fields = DM_POSITION
            | DM_BITSPERPEL
            | DM_PELSWIDTH
            | DM_PELSHEIGHT
            | DM_DISPLAYFLAGS
            | DM_DISPLAYFREQUENCY;
        field(names + 8).cast::<u32>().write_unaligned(fields);
        let after_form = names + 38 + names;
        field(after_form).cast::<u16>().write_unaligned(DPI as u16);
        field(after_form + 2).cast::<u32>().write_unaligned(32);
        field(after_form + 6)
            .cast::<u32>()
            .write_unaligned(SCREEN_WIDTH as u32);
        field(after_form + 10)
            .cast::<u32>()
            .write_unaligned(SCREEN_HEIGHT as u32);
        field(after_form + 18).cast::<u32>().write_unaligned(60);
    }
    TRUE
}

pub extern "win64" fn EnumDisplaySettingsW(
    device: *const u16,
    mode: u32,
    out: *mut c_void,
) -> BOOL {
    let device = (!device.is_null()).then(|| wide(device));
    display_settings(device, mode, out.cast(), 2)
}

pub extern "win64" fn EnumDisplaySettingsA(device: *const u8, mode: u32, out: *mut c_void) -> BOOL {
    let device = (!device.is_null()).then(|| narrow(device));
    display_settings(device, mode, out.cast(), 1)
}

const DISPLAY_DEVICE_ATTACHED_TO_DESKTOP: u32 = 1;
const DISPLAY_DEVICE_PRIMARY_DEVICE: u32 = 4;
const DISPLAY_DEVICE_ACTIVE: u32 = 1;
const DISPLAY_DEVICE_ATTACHED: u32 = 2;

/// `EnumDisplayDevices`'s body: the one adapter (device none) or its one
/// monitor, into a `DISPLAY_DEVICEA` (`unit` 1) or `-W` (2).
fn display_devices(device: Option<String>, index: u32, out: *mut u8, unit: usize) -> BOOL {
    if index != 0 || out.is_null() {
        return FALSE;
    }
    let (name, text, state) = match device {
        None => (
            DISPLAY.to_owned(),
            "Display Adapter",
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP | DISPLAY_DEVICE_PRIMARY_DEVICE,
        ),
        Some(d) if d.eq_ignore_ascii_case(DISPLAY) => (
            format!(r"{DISPLAY}\Monitor0"),
            "Generic PnP Monitor",
            DISPLAY_DEVICE_ACTIVE | DISPLAY_DEVICE_ATTACHED,
        ),
        Some(_) => return FALSE,
    };
    // SAFETY: checked non-null; the title's DISPLAY_DEVICE: its size, a name
    // of 32 characters, a description of 128, the state, two more of 128.
    unsafe {
        put_name(out.add(4), 32, unit, &name);
        put_name(out.add(4 + 32 * unit), 128, unit, text);
        let state_at = 4 + 160 * unit;
        out.add(state_at).cast::<u32>().write_unaligned(state);
        put_name(out.add(state_at + 4), 128, unit, "");
        put_name(out.add(state_at + 4 + 128 * unit), 128, unit, "");
    }
    TRUE
}

pub extern "win64" fn EnumDisplayDevicesW(
    device: *const u16,
    index: u32,
    out: *mut c_void,
    _flags: u32,
) -> BOOL {
    let device = (!device.is_null()).then(|| wide(device));
    display_devices(device, index, out.cast(), 2)
}

pub extern "win64" fn EnumDisplayDevicesA(
    device: *const u8,
    index: u32,
    out: *mut c_void,
    _flags: u32,
) -> BOOL {
    let device = (!device.is_null()).then(|| narrow(device));
    display_devices(device, index, out.cast(), 1)
}

const DISP_CHANGE_SUCCESSFUL: i32 = 0;

/// Accepted; the screen stays as it is.
pub extern "win64" fn ChangeDisplaySettingsW(_mode: *const c_void, _flags: u32) -> i32 {
    DISP_CHANGE_SUCCESSFUL
}

pub extern "win64" fn ChangeDisplaySettingsA(_mode: *const c_void, _flags: u32) -> i32 {
    DISP_CHANGE_SUCCESSFUL
}

pub extern "win64" fn ChangeDisplaySettingsExW(
    _device: *const u16,
    _mode: *const c_void,
    _hwnd: HWND,
    _flags: u32,
    _param: *mut c_void,
) -> i32 {
    DISP_CHANGE_SUCCESSFUL
}

pub extern "win64" fn ChangeDisplaySettingsExA(
    _device: *const u8,
    _mode: *const c_void,
    _hwnd: HWND,
    _flags: u32,
    _param: *mut c_void,
) -> i32 {
    DISP_CHANGE_SUCCESSFUL
}

// The cursor and the keyboard.

pub extern "win64" fn GetCursorPos(out: *mut POINT) -> BOOL {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's point.
    unsafe { out.write_unaligned(input().cursor_pos) };
    TRUE
}

/// Moves the stored position (within the clip rectangle, if any).
pub extern "win64" fn SetCursorPos(x: i32, y: i32) -> BOOL {
    let mut input = input();
    let clip = input.clip.unwrap_or(SCREEN);
    input.cursor_pos = POINT {
        x: x.clamp(clip.left, (clip.right - 1).max(clip.left)),
        y: y.clamp(clip.top, (clip.bottom - 1).max(clip.top)),
    };
    TRUE
}

/// The display counter: up with TRUE, down with FALSE; its new value.
pub extern "win64" fn ShowCursor(show: BOOL) -> i32 {
    let mut input = input();
    input.show_count += if show != 0 { 1 } else { -1 };
    input.show_count
}

pub extern "win64" fn SetCursor(cursor: usize) -> usize {
    std::mem::replace(&mut input().cursor, cursor)
}

pub extern "win64" fn GetCursor() -> usize {
    input().cursor
}

/// A new handle with nothing behind it (`0x4_0000 + n`); a system resource
/// (no instance, an ordinal) gets the same one each time, as on Windows.
fn resource_handle(instance: usize, name: usize) -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    static SHARED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    let new = || 0x4_0000 + NEXT.fetch_add(1, Ordering::Relaxed);
    if instance != 0 || name >= 0x1_0000 {
        return new();
    }
    let mut shared = lock(&SHARED);
    match shared.iter().find(|(n, _)| *n == name) {
        Some(&(_, handle)) => handle,
        None => {
            let handle = new();
            shared.push((name, handle));
            handle
        }
    }
}

pub extern "win64" fn LoadCursorW(instance: usize, name: *const u16) -> usize {
    resource_handle(instance, name as usize)
}

pub extern "win64" fn LoadCursorA(instance: usize, name: *const u8) -> usize {
    resource_handle(instance, name as usize)
}

pub extern "win64" fn LoadCursorFromFileW(_name: *const u16) -> usize {
    resource_handle(1, 0)
}

pub extern "win64" fn LoadIconW(instance: usize, name: *const u16) -> usize {
    resource_handle(instance, name as usize)
}

pub extern "win64" fn LoadIconA(instance: usize, name: *const u8) -> usize {
    resource_handle(instance, name as usize)
}

pub extern "win64" fn LoadImageW(
    instance: usize,
    name: *const u16,
    _kind: u32,
    _cx: i32,
    _cy: i32,
    _flags: u32,
) -> usize {
    resource_handle(instance, name as usize)
}

pub extern "win64" fn LoadImageA(
    instance: usize,
    name: *const u8,
    _kind: u32,
    _cx: i32,
    _cy: i32,
    _flags: u32,
) -> usize {
    resource_handle(instance, name as usize)
}

pub extern "win64" fn DestroyCursor(_cursor: usize) -> BOOL {
    TRUE
}

pub extern "win64" fn DestroyIcon(_icon: usize) -> BOOL {
    TRUE
}

/// Keeps the cursor in the rectangle (none: anywhere).
pub extern "win64" fn ClipCursor(rect: *const RECT) -> BOOL {
    input().clip = read(rect);
    TRUE
}

/// The clip rectangle; the screen when there is none.
pub extern "win64" fn GetClipCursor(out: *mut RECT) -> BOOL {
    let clip = input().clip.unwrap_or(SCREEN);
    put_rect(out, Some(clip))
}

pub extern "win64" fn SetCapture(hwnd: HWND) -> HWND {
    std::mem::replace(&mut input().capture, hwnd)
}

pub extern "win64" fn ReleaseCapture() -> BOOL {
    input().capture = 0;
    TRUE
}

pub extern "win64" fn GetCapture() -> HWND {
    input().capture
}

/// A handle; nothing is hooked (there is no input to hook).
pub extern "win64" fn SetWindowsHookExW(
    _id: i32,
    _proc: usize,
    _instance: usize,
    _thread: u32,
) -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    warn_once("a windows hook: nothing is hooked here");
    0x5_0000 + NEXT.fetch_add(1, Ordering::Relaxed)
}

pub extern "win64" fn SetWindowsHookExA(
    id: i32,
    proc: usize,
    instance: usize,
    thread: u32,
) -> usize {
    SetWindowsHookExW(id, proc, instance, thread)
}

pub extern "win64" fn UnhookWindowsHookEx(_hook: usize) -> BOOL {
    TRUE
}

pub extern "win64" fn CallNextHookEx(
    _hook: usize,
    _code: i32,
    _wparam: usize,
    _lparam: isize,
) -> LRESULT {
    0
}

/// `SetProp`, `GetProp` and `RemoveProp`'s body, on the window's list.
fn prop(hwnd: HWND, name: Option<Name>, change: Option<Option<usize>>) -> Option<usize> {
    let key = name_text(&name?).to_lowercase();
    live(hwnd, |w| {
        let found = w.props.iter().position(|(k, _)| *k == key);
        let old = found.map(|i| w.props[i].1);
        match (change, found) {
            (None, _) => old,
            (Some(None), Some(i)) => Some(w.props.remove(i).1),
            (Some(None), None) => None,
            (Some(Some(value)), Some(i)) => {
                w.props[i].1 = value;
                Some(value)
            }
            (Some(Some(value)), None) => {
                w.props.push((key, value));
                Some(value)
            }
        }
    })
    .flatten()
}

pub extern "win64" fn SetPropW(hwnd: HWND, name: *const u16, value: usize) -> BOOL {
    BOOL::from(prop(hwnd, name_w(name), Some(Some(value))).is_some())
}

pub extern "win64" fn SetPropA(hwnd: HWND, name: *const u8, value: usize) -> BOOL {
    BOOL::from(prop(hwnd, name_a(name), Some(Some(value))).is_some())
}

pub extern "win64" fn GetPropW(hwnd: HWND, name: *const u16) -> usize {
    prop(hwnd, name_w(name), None).unwrap_or(0)
}

pub extern "win64" fn GetPropA(hwnd: HWND, name: *const u8) -> usize {
    prop(hwnd, name_a(name), None).unwrap_or(0)
}

pub extern "win64" fn RemovePropW(hwnd: HWND, name: *const u16) -> usize {
    prop(hwnd, name_w(name), Some(None)).unwrap_or(0)
}

pub extern "win64" fn RemovePropA(hwnd: HWND, name: *const u8) -> usize {
    prop(hwnd, name_a(name), Some(None)).unwrap_or(0)
}

/// A message number of the atom table: the same name, the same number.
pub extern "win64" fn RegisterWindowMessageW(name: *const u16) -> u32 {
    if name.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    u32::from(atom(&wide(name)))
}

pub extern "win64" fn RegisterWindowMessageA(name: *const u8) -> u32 {
    if name.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    u32::from(atom(&narrow(name)))
}

/// No key is down: input is M3's.
pub extern "win64" fn GetKeyState(_key: i32) -> i16 {
    0
}

pub extern "win64" fn GetAsyncKeyState(_key: i32) -> i16 {
    0
}

pub extern "win64" fn GetKeyboardState(out: *mut u8) -> BOOL {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    // SAFETY: checked non-null; the title's 256 key states.
    unsafe { std::ptr::write_bytes(out, 0, 256) };
    TRUE
}

/// No keyboard layout here.
pub extern "win64" fn MapVirtualKeyW(_code: u32, _kind: u32) -> u32 {
    0
}

pub extern "win64" fn MapVirtualKeyA(_code: u32, _kind: u32) -> u32 {
    0
}

pub extern "win64" fn MapVirtualKeyExW(_code: u32, _kind: u32, _layout: usize) -> u32 {
    0
}

pub extern "win64" fn MapVirtualKeyExA(_code: u32, _kind: u32, _layout: usize) -> u32 {
    0
}

pub extern "win64" fn ToUnicode(
    _key: u32,
    _scan: u32,
    _state: *const u8,
    _out: *mut u16,
    _cch: i32,
    _flags: u32,
) -> i32 {
    0
}

pub extern "win64" fn GetKeyNameTextW(_lparam: i32, _out: *mut u16, _cch: i32) -> i32 {
    0
}

pub extern "win64" fn GetKeyNameTextA(_lparam: i32, _out: *mut u8, _cch: i32) -> i32 {
    0
}

/// A raw input device a title registers.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RAWINPUTDEVICE {
    pub usage_page: u16,
    pub usage: u16,
    pub flags: u32,
    pub target: HWND,
}

const _: () = assert!(std::mem::size_of::<RAWINPUTDEVICE>() == 16);
const RIDEV_REMOVE: u32 = 1;

static RAW_DEVICES: Mutex<Vec<RAWINPUTDEVICE>> = Mutex::new(Vec::new());

/// Kept (by usage), for `GetRegisteredRawInputDevices`; nothing ever
/// arrives.
pub extern "win64" fn RegisterRawInputDevices(
    devices: *const RAWINPUTDEVICE,
    count: u32,
    size: u32,
) -> BOOL {
    if devices.is_null() || size as usize != std::mem::size_of::<RAWINPUTDEVICE>() {
        return fail(ERROR_INVALID_PARAMETER, FALSE);
    }
    let mut list = lock(&RAW_DEVICES);
    for i in 0..count as usize {
        // SAFETY: the title's `count` devices.
        let device = unsafe { devices.add(i).read_unaligned() };
        list.retain(|d| (d.usage_page, d.usage) != (device.usage_page, device.usage));
        if device.flags & RIDEV_REMOVE == 0 {
            list.push(device);
        }
    }
    TRUE
}

/// No raw input arrives.
pub extern "win64" fn GetRawInputData(
    _raw: usize,
    _command: u32,
    _data: *mut c_void,
    size: *mut u32,
    _header: u32,
) -> u32 {
    if !size.is_null() {
        // SAFETY: checked non-null; the title's size.
        unsafe { size.write_unaligned(0) };
    }
    0
}

pub extern "win64" fn GetRawInputBuffer(_data: *mut c_void, size: *mut u32, _header: u32) -> u32 {
    if !size.is_null() {
        // SAFETY: checked non-null; the title's size.
        unsafe { size.write_unaligned(0) };
    }
    0
}

/// What was registered; the count, or -1 when it does not fit.
pub extern "win64" fn GetRegisteredRawInputDevices(
    devices: *mut RAWINPUTDEVICE,
    count: *mut u32,
    size: u32,
) -> u32 {
    if count.is_null() || size as usize != std::mem::size_of::<RAWINPUTDEVICE>() {
        return fail(ERROR_INVALID_PARAMETER, u32::MAX);
    }
    let list = lock(&RAW_DEVICES);
    // SAFETY: checked non-null; the title's in-out count.
    unsafe {
        let room = count.read_unaligned() as usize;
        count.write_unaligned(list.len() as u32);
        if devices.is_null() {
            return 0;
        }
        if room < list.len() {
            return fail(ERROR_INSUFFICIENT_BUFFER, u32::MAX);
        }
        for (i, device) in list.iter().enumerate() {
            devices.add(i).write_unaligned(*device);
        }
    }
    list.len() as u32
}

const IDOK: i32 = 1;

/// Logged; answered `IDOK` (there is no one to ask).
pub extern "win64" fn MessageBoxW(
    _hwnd: HWND,
    text: *const u16,
    caption: *const u16,
    _kind: u32,
) -> i32 {
    tracing::warn!(caption = %wide(caption), text = %wide(text), "message box");
    IDOK
}

pub extern "win64" fn MessageBoxA(
    _hwnd: HWND,
    text: *const u8,
    caption: *const u8,
    _kind: u32,
) -> i32 {
    tracing::warn!(caption = %narrow(caption), text = %narrow(text), "message box");
    IDOK
}

pub extern "win64" fn MessageBeep(_kind: u32) -> BOOL {
    TRUE
}

/// The window was not active before (nothing flashes).
pub extern "win64" fn FlashWindow(_hwnd: HWND, _invert: BOOL) -> BOOL {
    FALSE
}

pub extern "win64" fn FlashWindowEx(_info: *const c_void) -> BOOL {
    FALSE
}

pub extern "win64" fn SetLayeredWindowAttributes(
    _hwnd: HWND,
    _key: u32,
    _alpha: u8,
    _flags: u32,
) -> BOOL {
    TRUE
}

// Painting: nobody paints.

pub extern "win64" fn InvalidateRect(_hwnd: HWND, _rect: *const RECT, _erase: BOOL) -> BOOL {
    TRUE
}

pub extern "win64" fn ValidateRect(_hwnd: HWND, _rect: *const RECT) -> BOOL {
    TRUE
}

pub extern "win64" fn RedrawWindow(
    _hwnd: HWND,
    _rect: *const RECT,
    _region: usize,
    _flags: u32,
) -> BOOL {
    TRUE
}

/// What `BeginPaint` fills in.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PAINTSTRUCT {
    pub hdc: usize,
    pub erase: BOOL,
    pub paint: RECT,
    pub restore: BOOL,
    pub inc_update: BOOL,
    pub reserved: [u8; 32],
}

const _: () = assert!(std::mem::size_of::<PAINTSTRUCT>() == 72);

/// The client area and the fake device context.
pub extern "win64" fn BeginPaint(hwnd: HWND, paint: *mut PAINTSTRUCT) -> usize {
    let Some(client) = live(hwnd, |w| w.client) else {
        return fail(ERROR_INVALID_WINDOW_HANDLE, 0);
    };
    if !paint.is_null() {
        let ps = PAINTSTRUCT {
            hdc: HDC,
            erase: FALSE,
            paint: RECT {
                left: 0,
                top: 0,
                right: client.width(),
                bottom: client.height(),
            },
            restore: FALSE,
            inc_update: FALSE,
            reserved: [0; 32],
        };
        // SAFETY: checked non-null; the title's structure.
        unsafe { paint.write_unaligned(ps) };
    }
    HDC
}

pub extern "win64" fn EndPaint(_hwnd: HWND, _paint: *const PAINTSTRUCT) -> BOOL {
    TRUE
}

/// Nothing is ever out of date.
pub extern "win64" fn GetUpdateRect(_hwnd: HWND, rect: *mut RECT, _erase: BOOL) -> BOOL {
    if !rect.is_null() {
        // SAFETY: checked non-null; the title's rectangle.
        unsafe { rect.write_unaligned(RECT::default()) };
    }
    FALSE
}

// Timers.

const USER_TIMER_MINIMUM: u32 = 10;

/// The timer thread: posts each timer's `WM_TIMER` when it is due (one
/// waiting at most per timer), sleeping until the next one.
fn timer_thread() {
    let mut timers = lock(&TIMERS);
    loop {
        let now = Instant::now();
        for timer in timers.iter_mut().filter(|t| t.due <= now) {
            let queue = queue_of(timer.thread);
            let (hwnd, id) = (timer.hwnd, timer.id);
            let waiting = lock(&queue.state)
                .posted
                .iter()
                .any(|m| m.message == WM_TIMER && m.hwnd == hwnd && m.wparam == id);
            if !waiting {
                queue.post(Msg::new(hwnd, WM_TIMER, id, timer.proc as isize));
            }
            timer.due = now + timer.interval;
        }
        let next = timers.iter().map(|t| t.due).min();
        timers = match next {
            Some(due) => {
                let wait = due.saturating_duration_since(Instant::now());
                TIMERS_CHANGED
                    .wait_timeout(timers, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => TIMERS_CHANGED
                .wait(timers)
                .unwrap_or_else(PoisonError::into_inner),
        };
    }
}

/// A timer posting `WM_TIMER` every `ms` (10 at least) to the window's
/// thread, or with no window to the calling thread under a new id.
pub extern "win64" fn SetTimer(hwnd: HWND, id: usize, ms: u32, proc: usize) -> usize {
    static STARTED: AtomicBool = AtomicBool::new(false);
    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
    let (thread, id) = if hwnd == 0 {
        (thread_id(), NEXT_ID.fetch_add(1, Ordering::Relaxed))
    } else {
        match live(hwnd, |w| w.thread) {
            Some(thread) => (thread, id),
            None => return fail(ERROR_INVALID_WINDOW_HANDLE, 0),
        }
    };
    let interval = Duration::from_millis(u64::from(ms.max(USER_TIMER_MINIMUM)));
    {
        let mut timers = lock(&TIMERS);
        timers.retain(|t| !(t.hwnd == hwnd && t.id == id && (hwnd != 0 || t.thread == thread)));
        timers.push(Timer {
            hwnd,
            id,
            thread,
            interval,
            due: Instant::now() + interval,
            proc,
        });
    }
    if !STARTED.swap(true, Ordering::SeqCst) {
        std::thread::Builder::new()
            .name("victory-timers".into())
            .spawn(timer_thread)
            .map_err(|e| tracing::error!(%e, "no timer thread"))
            .ok();
    }
    TIMERS_CHANGED.notify_all();
    id
}

/// Stops a timer, and drops its `WM_TIMER` waiting in the queue.
pub extern "win64" fn KillTimer(hwnd: HWND, id: usize) -> BOOL {
    let removed = {
        let mut timers = lock(&TIMERS);
        let before = timers.len();
        timers.retain(|t| !(t.hwnd == hwnd && t.id == id));
        timers.len() != before
    };
    TIMERS_CHANGED.notify_all();
    if !removed {
        return FALSE;
    }
    let thread = if hwnd == 0 {
        thread_id()
    } else {
        live(hwnd, |w| w.thread).unwrap_or(0)
    };
    let queue = queue_of(thread);
    lock(&queue.state)
        .posted
        .retain(|m| !(m.message == WM_TIMER && m.hwnd == hwnd && m.wparam == id));
    TRUE
}

// The default procedure.

/// The text a `WM_SETTEXT` or a `CREATESTRUCT` carries.
fn message_text(pointer: usize, wide_text: bool) -> String {
    if pointer < 0x1_0000 {
        return String::new();
    }
    if wide_text {
        wide(pointer as *const u16)
    } else {
        narrow(pointer as *const u8)
    }
}

/// `DefWindowProc`'s body; `wide_text` for the W form.
fn default_procedure(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    wide_text: bool,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            if lparam != 0 {
                // SAFETY: the CREATESTRUCT `CreateWindowEx` passes.
                let cs = unsafe { (lparam as *const CREATESTRUCTW).read_unaligned() };
                let title = message_text(cs.name, wide_text);
                live(hwnd, |w| w.title = title);
            }
            1
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_SETTEXT => {
            let title = message_text(lparam as usize, wide_text);
            LRESULT::from(live(hwnd, |w| w.title = title).is_some())
        }
        WM_GETTEXT => {
            let title = live(hwnd, |w| w.title.clone()).unwrap_or_default();
            let room = wparam.min(i32::MAX as usize) as i32;
            if wide_text {
                let units: Vec<u16> = title.encode_utf16().collect();
                copy_truncated(&units, lparam as *mut u16, room) as LRESULT
            } else {
                let bytes = narrow_z(&title);
                copy_truncated(&bytes[..bytes.len() - 1], lparam as *mut u8, room) as LRESULT
            }
        }
        WM_GETTEXTLENGTH => {
            let title = live(hwnd, |w| w.title.clone()).unwrap_or_default();
            let len = if wide_text {
                title.encode_utf16().count()
            } else {
                title.chars().count()
            };
            len as LRESULT
        }
        WM_ERASEBKGND | WM_SETCURSOR => 1,
        _ => 0,
    }
}

pub extern "win64" fn DefWindowProcW(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> LRESULT {
    default_procedure(hwnd, msg, wparam, lparam, true)
}

pub extern "win64" fn DefWindowProcA(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> LRESULT {
    default_procedure(hwnd, msg, wparam, lparam, false)
}

pub extern "win64" fn CallWindowProcW(
    proc: usize,
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> LRESULT {
    call(proc, hwnd, msg, wparam, lparam)
}

pub extern "win64" fn CallWindowProcA(
    proc: usize,
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> LRESULT {
    call(proc, hwnd, msg, wparam, lparam)
}

// Message queues.

/// A message, as `MSG`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Msg {
    pub hwnd: HWND,
    pub message: u32,
    pub wparam: usize,
    pub lparam: isize,
    pub time: u32,
    pub pt: POINT,
}

const _: () = assert!(std::mem::size_of::<Msg>() == 48);

impl Msg {
    /// Stamped with the time and the cursor's position.
    fn new(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> Msg {
        Msg {
            hwnd,
            message,
            wparam,
            lparam,
            time: GetTickCount(),
            pt: input().cursor_pos,
        }
    }
}

/// A message another thread sent, and where its answer goes.
struct SentMessage {
    msg: Msg,
    result: Arc<(Mutex<Option<isize>>, Condvar)>,
}

/// What a queue holds.
#[derive(Default)]
struct QueueState {
    posted: VecDeque<Msg>,
    sent: VecDeque<SentMessage>,
    quit: Option<i32>,
    last_time: u32,
    last_pos: POINT,
    /// Counts every change, so a wait sees one made since it looked.
    generation: u64,
}

/// A thread's message queue.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    wake: Condvar,
}

impl Queue {
    fn post(&self, msg: Msg) {
        let mut state = lock(&self.state);
        state.posted.push_back(msg);
        state.generation += 1;
        self.wake.notify_all();
    }
}

static QUEUES: LazyLock<Mutex<HashMap<u32, Arc<Queue>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Removes the thread's queue when it ends, as Windows does.
struct QueueOwner(u32);

impl Drop for QueueOwner {
    fn drop(&mut self) {
        lock(&QUEUES).remove(&self.0);
    }
}

thread_local! {
    static OWNER: Cell<bool> = const { Cell::new(false) };
    static OWNER_GUARD: std::cell::RefCell<Option<QueueOwner>> = const { std::cell::RefCell::new(None) };
    static EXTRA_INFO: Cell<isize> = const { Cell::new(0) };
}

/// A thread's queue, made when first needed.
fn queue_of(thread: u32) -> Arc<Queue> {
    lock(&QUEUES).entry(thread).or_default().clone()
}

/// The calling thread's queue.
fn own_queue() -> Arc<Queue> {
    let me = thread_id();
    if !OWNER.with(Cell::get) {
        OWNER.with(|o| o.set(true));
        OWNER_GUARD.with(|g| *g.borrow_mut() = Some(QueueOwner(me)));
    }
    queue_of(me)
}

/// Runs the messages other threads sent to this one.
fn run_sent(queue: &Queue) {
    loop {
        let Some(sent) = lock(&queue.state).sent.pop_front() else {
            return;
        };
        let m = sent.msg;
        let proc = live(m.hwnd, |w| w.wndproc).unwrap_or(0);
        let result = call(proc, m.hwnd, m.message, m.wparam, m.lparam);
        *lock(&sent.result.0) = Some(result);
        sent.result.1.notify_all();
    }
}

/// `SendMessage`'s body: the procedure's answer; called now on the
/// window's own thread, else queued for that thread (whose answer is
/// waited for, `timeout` at most, running messages sent to this one).
fn send(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    timeout: Option<Duration>,
) -> Result<LRESULT, u32> {
    let (thread, proc) =
        live(hwnd, |w| (w.thread, w.wndproc)).ok_or(ERROR_INVALID_WINDOW_HANDLE)?;
    if thread == thread_id() || hwnd == DESKTOP {
        return Ok(call(proc, hwnd, msg, wparam, lparam));
    }
    let target = queue_of(thread);
    let result = Arc::new((Mutex::new(None), Condvar::new()));
    {
        let mut state = lock(&target.state);
        state.sent.push_back(SentMessage {
            msg: Msg::new(hwnd, msg, wparam, lparam),
            result: result.clone(),
        });
        state.generation += 1;
        target.wake.notify_all();
    }
    let own = own_queue();
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        if let Some(answer) = *lock(&result.0) {
            return Ok(answer);
        }
        run_sent(&own);
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(ERROR_TIMEOUT);
        }
        let guard = lock(&result.0);
        if guard.is_none() {
            let _ = result.1.wait_timeout(guard, Duration::from_millis(2));
        }
    }
}

/// [`send`] without a bound; 0 for a window that is not there.
fn send_wait(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> LRESULT {
    send(hwnd, msg, wparam, lparam, None).unwrap_or_else(|code| fail(code, 0))
}

/// `PostMessage`'s body.
fn post(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> BOOL {
    if hwnd == 0 {
        own_queue().post(Msg::new(0, msg, wparam, lparam));
        return TRUE;
    }
    if hwnd == HWND_BROADCAST {
        for (target, thread) in windows()
            .iter()
            .skip(1)
            .filter(|w| !w.destroyed && !w.child())
            .map(|w| (w.hwnd, w.thread))
            .collect::<Vec<_>>()
        {
            queue_of(thread).post(Msg::new(target, msg, wparam, lparam));
        }
        return TRUE;
    }
    match live(hwnd, |w| w.thread) {
        Some(thread) => {
            queue_of(thread).post(Msg::new(hwnd, msg, wparam, lparam));
            TRUE
        }
        None => fail(ERROR_INVALID_WINDOW_HANDLE, FALSE),
    }
}

pub extern "win64" fn PostMessageW(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> BOOL {
    post(hwnd, msg, wparam, lparam)
}

pub extern "win64" fn PostMessageA(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> BOOL {
    post(hwnd, msg, wparam, lparam)
}

/// To a thread's queue (any thread id of ours: its queue is made).
pub extern "win64" fn PostThreadMessageW(
    thread: u32,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> BOOL {
    if thread == 0 {
        return fail(ERROR_INVALID_THREAD_ID, FALSE);
    }
    queue_of(thread).post(Msg::new(0, msg, wparam, lparam));
    TRUE
}

pub extern "win64" fn PostThreadMessageA(
    thread: u32,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> BOOL {
    PostThreadMessageW(thread, msg, wparam, lparam)
}

/// `WM_QUIT` with `code` comes once the posted messages are taken.
pub extern "win64" fn PostQuitMessage(code: i32) {
    let queue = own_queue();
    let mut state = lock(&queue.state);
    state.quit = Some(code);
    state.generation += 1;
    queue.wake.notify_all();
}

pub extern "win64" fn SendMessageW(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> LRESULT {
    send(hwnd, msg, wparam, lparam, None).unwrap_or(0)
}

pub extern "win64" fn SendMessageA(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> LRESULT {
    send(hwnd, msg, wparam, lparam, None).unwrap_or(0)
}

/// [`SendMessageW`] waiting `timeout` ms at most; 0 and `ERROR_TIMEOUT`
/// when that runs out.
pub extern "win64" fn SendMessageTimeoutW(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _flags: u32,
    timeout: u32,
    result: *mut usize,
) -> LRESULT {
    let bound = Some(Duration::from_millis(u64::from(timeout)));
    match send(hwnd, msg, wparam, lparam, bound) {
        Ok(answer) => {
            if !result.is_null() {
                // SAFETY: checked non-null; the title's output.
                unsafe { result.write_unaligned(answer as usize) };
            }
            1
        }
        Err(code) => fail(code, 0),
    }
}

pub extern "win64" fn SendMessageTimeoutA(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    flags: u32,
    timeout: u32,
    result: *mut usize,
) -> LRESULT {
    SendMessageTimeoutW(hwnd, msg, wparam, lparam, flags, timeout, result)
}

/// Called now on the window's own thread, posted to another's.
pub extern "win64" fn SendNotifyMessageW(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> BOOL {
    match live(hwnd, |w| w.thread) {
        Some(thread) if thread == thread_id() => {
            notify(hwnd, msg, wparam, lparam);
            TRUE
        }
        Some(_) => post(hwnd, msg, wparam, lparam),
        None => fail(ERROR_INVALID_WINDOW_HANDLE, FALSE),
    }
}

pub extern "win64" fn SendNotifyMessageA(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> BOOL {
    SendNotifyMessageW(hwnd, msg, wparam, lparam)
}

type SendAsyncProc = Option<extern "win64" fn(HWND, u32, usize, LRESULT)>;

/// Sends, then gives the answer to the callback.
pub extern "win64" fn SendMessageCallbackW(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    callback: SendAsyncProc,
    data: usize,
) -> BOOL {
    match send(hwnd, msg, wparam, lparam, None) {
        Ok(answer) => {
            if let Some(callback) = callback {
                callback(hwnd, msg, data, answer);
            }
            TRUE
        }
        Err(code) => fail(code, FALSE),
    }
}

pub extern "win64" fn SendMessageCallbackA(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
    callback: SendAsyncProc,
    data: usize,
) -> BOOL {
    SendMessageCallbackW(hwnd, msg, wparam, lparam, callback, data)
}

pub extern "win64" fn ReplyMessage(_result: LRESULT) -> BOOL {
    FALSE
}

pub extern "win64" fn InSendMessage() -> BOOL {
    FALSE
}

/// `ISMEX_NOSEND`.
pub extern "win64" fn InSendMessageEx(_reserved: *mut c_void) -> u32 {
    0
}

const PM_REMOVE: u32 = 1;
/// `PeekMessage`'s window filter for thread messages only.
const THREAD_MESSAGES: HWND = usize::MAX;

/// `PeekMessage`'s body: the sent messages run, then the first posted one
/// the filters take (or `WM_QUIT`).
fn peek(hwnd: HWND, min: u32, max: u32, remove: bool) -> Option<Msg> {
    let queue = own_queue();
    run_sent(&queue);
    let targets = match hwnd {
        0 | THREAD_MESSAGES => Vec::new(),
        hwnd => family(hwnd),
    };
    let takes = |m: &Msg| {
        let window = match hwnd {
            0 => true,
            THREAD_MESSAGES => m.hwnd == 0,
            _ => targets.contains(&m.hwnd),
        };
        window && ((min == 0 && max == 0) || (min..=max).contains(&m.message))
    };
    let mut state = lock(&queue.state);
    if let Some(i) = state.posted.iter().position(takes) {
        let msg = if remove {
            state.posted.remove(i)?
        } else {
            state.posted[i]
        };
        state.last_time = msg.time;
        state.last_pos = msg.pt;
        return Some(msg);
    }
    let code = state.quit?;
    if remove {
        state.quit = None;
    }
    Some(Msg {
        message: WM_QUIT,
        wparam: code as usize,
        time: GetTickCount(),
        ..Msg::default()
    })
}

/// Writes a message for the title.
fn put_msg(out: *mut Msg, msg: Msg) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title's MSG.
        unsafe { out.write_unaligned(msg) };
    }
}

pub extern "win64" fn PeekMessageW(
    out: *mut Msg,
    hwnd: HWND,
    min: u32,
    max: u32,
    flags: u32,
) -> BOOL {
    match peek(hwnd, min, max, flags & PM_REMOVE != 0) {
        Some(msg) => {
            put_msg(out, msg);
            TRUE
        }
        None => FALSE,
    }
}

pub extern "win64" fn PeekMessageA(
    out: *mut Msg,
    hwnd: HWND,
    min: u32,
    max: u32,
    flags: u32,
) -> BOOL {
    PeekMessageW(out, hwnd, min, max, flags)
}

/// Waits on the calling thread's queue until it changes from
/// `generation` (`timeout` at most).
fn wait_change(queue: &Queue, generation: u64, timeout: Option<Duration>) {
    let state = lock(&queue.state);
    if state.generation != generation {
        return;
    }
    match timeout {
        Some(t) => drop(
            queue
                .wake
                .wait_timeout_while(state, t, |s| s.generation == generation),
        ),
        None => drop(queue.wake.wait_while(state, |s| s.generation == generation)),
    }
}

/// As [`PeekMessageW`] removing, but waiting; 0 for `WM_QUIT`, -1 for a
/// window that is not there.
pub extern "win64" fn GetMessageW(out: *mut Msg, hwnd: HWND, min: u32, max: u32) -> BOOL {
    if hwnd != 0 && hwnd != THREAD_MESSAGES && live(hwnd, |_| ()).is_none() {
        return fail(ERROR_INVALID_WINDOW_HANDLE, -1);
    }
    let queue = own_queue();
    loop {
        let generation = lock(&queue.state).generation;
        if let Some(msg) = peek(hwnd, min, max, true) {
            put_msg(out, msg);
            return BOOL::from(msg.message != WM_QUIT);
        }
        wait_change(&queue, generation, None);
    }
}

pub extern "win64" fn GetMessageA(out: *mut Msg, hwnd: HWND, min: u32, max: u32) -> BOOL {
    GetMessageW(out, hwnd, min, max)
}

/// No keyboard yet: nothing to translate.
pub extern "win64" fn TranslateMessage(_msg: *const Msg) -> BOOL {
    FALSE
}

type TimerProc = extern "win64" fn(HWND, u32, usize, u32);

/// A `WM_TIMER` with a procedure to it; anything else to its window's
/// procedure (none, or a window that is gone: 0).
pub extern "win64" fn DispatchMessageW(msg: *const Msg) -> LRESULT {
    let Some(m) = read(msg) else {
        return fail(ERROR_INVALID_PARAMETER, 0);
    };
    if m.message == WM_TIMER && m.lparam != 0 {
        // SAFETY: the TIMERPROC the title gave `SetTimer`.
        let proc: TimerProc = unsafe { std::mem::transmute(m.lparam as usize) };
        proc(m.hwnd, WM_TIMER, m.wparam, GetTickCount());
        return 0;
    }
    match live(m.hwnd, |w| w.wndproc) {
        Some(proc) => call(proc, m.hwnd, m.message, m.wparam, m.lparam),
        None => 0,
    }
}

pub extern "win64" fn DispatchMessageA(msg: *const Msg) -> LRESULT {
    DispatchMessageW(msg)
}

const QS_POSTMESSAGE: u32 = 8;
const QS_TIMER: u32 = 0x10;
const QS_SENDMESSAGE: u32 = 0x40;
const QS_ALLPOSTMESSAGE: u32 = 0x100;

/// What waits in the calling thread's queue, as `QS_*` bits.
fn queue_bits(queue: &Queue) -> u32 {
    let state = lock(&queue.state);
    let mut bits = 0;
    if state.posted.iter().any(|m| m.message != WM_TIMER) || state.quit.is_some() {
        bits |= QS_POSTMESSAGE | QS_ALLPOSTMESSAGE;
    }
    if state.posted.iter().any(|m| m.message == WM_TIMER) {
        bits |= QS_TIMER;
    }
    if !state.sent.is_empty() {
        bits |= QS_SENDMESSAGE;
    }
    bits
}

/// What waits, in both halves (what is there, and what is new).
pub extern "win64" fn GetQueueStatus(flags: u32) -> u32 {
    let bits = queue_bits(&own_queue()) & flags;
    bits << 16 | bits
}

const MWMO_WAITALL: u32 = 1;
const INFINITE: u32 = u32::MAX;

/// Waits for the handles or for the queue to hold what `wake_mask` asks:
/// `WAIT_OBJECT_0 + count` for the queue. The handles are waited for in
/// slices of 5 ms, the queue checked between them.
pub extern "win64" fn MsgWaitForMultipleObjectsEx(
    count: u32,
    handles: *const *mut c_void,
    timeout: u32,
    wake_mask: u32,
    flags: u32,
) -> u32 {
    let queue = own_queue();
    let start = Instant::now();
    let wait_all = i32::from(flags & MWMO_WAITALL != 0);
    loop {
        run_sent(&queue);
        let generation = lock(&queue.state).generation;
        if queue_bits(&queue) & wake_mask != 0 {
            return WAIT_OBJECT_0 + count;
        }
        let elapsed = start.elapsed().as_millis() as u64;
        if timeout != INFINITE && elapsed >= u64::from(timeout) {
            return WAIT_TIMEOUT;
        }
        let left = if timeout == INFINITE {
            5
        } else {
            (u64::from(timeout) - elapsed).min(5)
        };
        if count == 0 {
            wait_change(&queue, generation, Some(Duration::from_millis(left)));
            continue;
        }
        let result = WaitForMultipleObjects(count, handles, wait_all, left as u32);
        if result != WAIT_TIMEOUT {
            return result;
        }
    }
}

pub extern "win64" fn MsgWaitForMultipleObjects(
    count: u32,
    handles: *const *mut c_void,
    wait_all: BOOL,
    timeout: u32,
    wake_mask: u32,
) -> u32 {
    let flags = if wait_all != 0 { MWMO_WAITALL } else { 0 };
    MsgWaitForMultipleObjectsEx(count, handles, timeout, wake_mask, flags)
}

/// Waits until a message is posted (sent ones run meanwhile).
pub extern "win64" fn WaitMessage() -> BOOL {
    let queue = own_queue();
    loop {
        run_sent(&queue);
        let generation = {
            let state = lock(&queue.state);
            if !state.posted.is_empty() || state.quit.is_some() {
                return TRUE;
            }
            state.generation
        };
        wait_change(&queue, generation, None);
    }
}

/// The time of the last message taken.
pub extern "win64" fn GetMessageTime() -> i32 {
    lock(&own_queue().state).last_time as i32
}

/// The cursor's position at the last message taken: x low, y high.
pub extern "win64" fn GetMessagePos() -> u32 {
    let pos = lock(&own_queue().state).last_pos;
    pack(pos.x, pos.y) as u32
}

pub extern "win64" fn GetMessageExtraInfo() -> isize {
    EXTRA_INFO.with(Cell::get)
}

pub extern "win64" fn SetMessageExtraInfo(value: isize) -> isize {
    EXTRA_INFO.with(|e| e.replace(value))
}

/// No input waits: there is none.
pub extern "win64" fn GetInputState() -> BOOL {
    FALSE
}

/// Every thread may have windows (its queue is made).
pub extern "win64" fn IsGUIThread(_convert: BOOL) -> BOOL {
    own_queue();
    TRUE
}

pub extern "win64" fn AttachThreadInput(_from: u32, _to: u32, _attach: BOOL) -> BOOL {
    TRUE
}

pub extern "win64" fn ChangeWindowMessageFilter(_msg: u32, _flag: u32) -> BOOL {
    TRUE
}

pub extern "win64" fn ChangeWindowMessageFilterEx(
    _hwnd: HWND,
    _msg: u32,
    _action: u32,
    _filter: *mut c_void,
) -> BOOL {
    TRUE
}

/// Posts `WM_CLOSE` to every live top-level window (the title's end, so a
/// title that leaves through its message loop gets its chance).
pub(crate) fn post_close_all() {
    for hwnd in select(|w| !w.child()) {
        post(hwnd, WM_CLOSE, 0, 0);
    }
}

fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(RegisterClassA),
        hle_export!(RegisterClassW),
        hle_export!(RegisterClassExA),
        hle_export!(RegisterClassExW),
        hle_export!(UnregisterClassA),
        hle_export!(UnregisterClassW),
        hle_export!(GetClassInfoA),
        hle_export!(GetClassInfoW),
        hle_export!(GetClassInfoExA),
        hle_export!(GetClassInfoExW),
        hle_export!(GetClassNameA),
        hle_export!(GetClassNameW),
        hle_export!(GetClassLongPtrA),
        hle_export!(GetClassLongPtrW),
        hle_export!(SetClassLongPtrA),
        hle_export!(SetClassLongPtrW),
        hle_export!(CreateWindowExA),
        hle_export!(CreateWindowExW),
        hle_export!(ShowWindow),
        hle_export!(ShowWindowAsync),
        hle_export!(UpdateWindow),
        hle_export!(IsWindow),
        hle_export!(IsWindowVisible),
        hle_export!(IsWindowEnabled),
        hle_export!(EnableWindow),
        hle_export!(IsIconic),
        hle_export!(IsZoomed),
        hle_export!(DestroyWindow),
        hle_export!(CloseWindow),
        hle_export!(GetClientRect),
        hle_export!(GetWindowRect),
        hle_export!(AdjustWindowRect),
        hle_export!(AdjustWindowRectEx),
        hle_export!(AdjustWindowRectExForDpi),
        hle_export!(SetWindowPos),
        hle_export!(MoveWindow),
        hle_export!(GetWindowLongPtrA),
        hle_export!(GetWindowLongPtrW),
        hle_export!(SetWindowLongPtrA),
        hle_export!(SetWindowLongPtrW),
        hle_export!(GetWindowLongA),
        hle_export!(GetWindowLongW),
        hle_export!(SetWindowLongA),
        hle_export!(SetWindowLongW),
        hle_export!(SetWindowTextA),
        hle_export!(SetWindowTextW),
        hle_export!(GetWindowTextA),
        hle_export!(GetWindowTextW),
        hle_export!(GetWindowTextLengthA),
        hle_export!(GetWindowTextLengthW),
        hle_export!(SetForegroundWindow),
        hle_export!(GetForegroundWindow),
        hle_export!(SetActiveWindow),
        hle_export!(GetActiveWindow),
        hle_export!(SetFocus),
        hle_export!(GetFocus),
        hle_export!(GetDesktopWindow),
        hle_export!(GetParent),
        hle_export!(SetParent),
        hle_export!(GetWindow),
        hle_export!(GetAncestor),
        hle_export!(GetWindowThreadProcessId),
        hle_export!(FindWindowA),
        hle_export!(FindWindowW),
        hle_export!(FindWindowExA),
        hle_export!(FindWindowExW),
        hle_export!(EnumWindows),
        hle_export!(EnumThreadWindows),
        hle_export!(ClientToScreen),
        hle_export!(ScreenToClient),
        hle_export!(MapWindowPoints),
        hle_export!(WindowFromPoint),
        hle_export!(GetSystemMetrics),
        hle_export!(GetSystemMetricsForDpi),
        hle_export!(SystemParametersInfoA),
        hle_export!(SystemParametersInfoW),
        hle_export!(GetDpiForWindow),
        hle_export!(GetDpiForSystem),
        hle_export!(SetProcessDpiAwarenessContext),
        hle_export!(SetProcessDPIAware),
        hle_export!(GetDC),
        hle_export!(GetWindowDC),
        hle_export!(ReleaseDC),
        hle_export!(MonitorFromWindow),
        hle_export!(MonitorFromPoint),
        hle_export!(MonitorFromRect),
        hle_export!(GetMonitorInfoA),
        hle_export!(GetMonitorInfoW),
        hle_export!(EnumDisplayMonitors),
        hle_export!(EnumDisplaySettingsA),
        hle_export!(EnumDisplaySettingsW),
        hle_export!(EnumDisplayDevicesA),
        hle_export!(EnumDisplayDevicesW),
        hle_export!(ChangeDisplaySettingsA),
        hle_export!(ChangeDisplaySettingsW),
        hle_export!(ChangeDisplaySettingsExA),
        hle_export!(ChangeDisplaySettingsExW),
        hle_export!(GetCursorPos),
        hle_export!(SetCursorPos),
        hle_export!(ShowCursor),
        hle_export!(SetCursor),
        hle_export!(GetCursor),
        hle_export!(LoadCursorA),
        hle_export!(LoadCursorW),
        hle_export!(LoadCursorFromFileW),
        hle_export!(LoadImageA),
        hle_export!(LoadImageW),
        hle_export!(LoadIconA),
        hle_export!(LoadIconW),
        hle_export!(DestroyCursor),
        hle_export!(DestroyIcon),
        hle_export!(ClipCursor),
        hle_export!(GetClipCursor),
        hle_export!(SetCapture),
        hle_export!(ReleaseCapture),
        hle_export!(GetCapture),
        hle_export!(SetWindowsHookExA),
        hle_export!(SetWindowsHookExW),
        hle_export!(UnhookWindowsHookEx),
        hle_export!(CallNextHookEx),
        hle_export!(SetPropA),
        hle_export!(SetPropW),
        hle_export!(GetPropA),
        hle_export!(GetPropW),
        hle_export!(RemovePropA),
        hle_export!(RemovePropW),
        hle_export!(RegisterWindowMessageA),
        hle_export!(RegisterWindowMessageW),
        hle_export!(GetKeyState),
        hle_export!(GetAsyncKeyState),
        hle_export!(GetKeyboardState),
        hle_export!(MapVirtualKeyA),
        hle_export!(MapVirtualKeyW),
        hle_export!(MapVirtualKeyExA),
        hle_export!(MapVirtualKeyExW),
        hle_export!(ToUnicode),
        hle_export!(GetKeyNameTextA),
        hle_export!(GetKeyNameTextW),
        hle_export!(RegisterRawInputDevices),
        hle_export!(GetRawInputData),
        hle_export!(GetRawInputBuffer),
        hle_export!(GetRegisteredRawInputDevices),
        hle_export!(MessageBoxA),
        hle_export!(MessageBoxW),
        hle_export!(MessageBeep),
        hle_export!(FlashWindow),
        hle_export!(FlashWindowEx),
        hle_export!(SetLayeredWindowAttributes),
        hle_export!(InvalidateRect),
        hle_export!(ValidateRect),
        hle_export!(RedrawWindow),
        hle_export!(BeginPaint),
        hle_export!(EndPaint),
        hle_export!(GetUpdateRect),
        hle_export!(SetTimer),
        hle_export!(KillTimer),
        hle_export!(DefWindowProcA),
        hle_export!(DefWindowProcW),
        hle_export!(CallWindowProcA),
        hle_export!(CallWindowProcW),
        hle_export!(PostMessageA),
        hle_export!(PostMessageW),
        hle_export!(PostThreadMessageA),
        hle_export!(PostThreadMessageW),
        hle_export!(PostQuitMessage),
        hle_export!(SendMessageA),
        hle_export!(SendMessageW),
        hle_export!(SendMessageTimeoutA),
        hle_export!(SendMessageTimeoutW),
        hle_export!(SendNotifyMessageA),
        hle_export!(SendNotifyMessageW),
        hle_export!(SendMessageCallbackA),
        hle_export!(SendMessageCallbackW),
        hle_export!(ReplyMessage),
        hle_export!(InSendMessage),
        hle_export!(InSendMessageEx),
        hle_export!(PeekMessageA),
        hle_export!(PeekMessageW),
        hle_export!(GetMessageA),
        hle_export!(GetMessageW),
        hle_export!(TranslateMessage),
        hle_export!(DispatchMessageA),
        hle_export!(DispatchMessageW),
        hle_export!(GetQueueStatus),
        hle_export!(MsgWaitForMultipleObjects),
        hle_export!(MsgWaitForMultipleObjectsEx),
        hle_export!(WaitMessage),
        hle_export!(GetMessageTime),
        hle_export!(GetMessagePos),
        hle_export!(GetMessageExtraInfo),
        hle_export!(SetMessageExtraInfo),
        hle_export!(GetInputState),
        hle_export!(IsGUIThread),
        hle_export!(AttachThreadInput),
        hle_export!(ChangeWindowMessageFilter),
        hle_export!(ChangeWindowMessageFilterEx),
    ]
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "window",
        dlls: &[
            "ext-ms-win-rtcore-ntuser-window-*",
            "ext-ms-win-rtcore-ntuser-message-*",
            "ext-ms-win-rtcore-ntuser-sysparams-*",
            "ext-ms-win-ntuser-window-*",
            "ext-ms-win-ntuser-message-*",
            "ext-ms-win-ntuser-sysparams-*",
            "user32.dll",
        ],
        exports: exports(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a test window's procedure saw: (window, message, wparam,
    /// lparam), and for the creation messages the `CREATESTRUCTW`'s size,
    /// style and title.
    type Seen = (HWND, u32, usize, isize, Option<(i32, i32, i32, String)>);

    static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());

    extern "win64" fn recording(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> LRESULT {
        let created = matches!(msg, WM_NCCREATE | WM_CREATE).then(|| {
            // SAFETY: the creation messages carry a CREATESTRUCTW.
            let cs = unsafe { (lparam as *const CREATESTRUCTW).read_unaligned() };
            (cs.cx, cs.cy, cs.style, wide(cs.name as *const u16))
        });
        lock(&SEEN).push((hwnd, msg, wparam, lparam, created));
        if msg == WM_USER + 7 {
            return 77;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// The messages `hwnd` saw, in order.
    fn seen(hwnd: HWND) -> Vec<Seen> {
        lock(&SEEN)
            .iter()
            .filter(|s| s.0 == hwnd)
            .cloned()
            .collect()
    }

    fn w(text: &str) -> Vec<u16> {
        wide_z(text)
    }

    /// Registers a class of the test's own name with [`recording`].
    fn class(name: &str) -> Vec<u16> {
        let name = w(name);
        let class = WNDCLASSEXW {
            size: 80,
            style: 0,
            wndproc: recording as *const () as usize,
            cls_extra: 0,
            wnd_extra: 16,
            instance: 0,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: 0,
            class_name: name.as_ptr() as usize,
            icon_small: 0,
        };
        assert_ne!(RegisterClassExW(&class), 0);
        name
    }

    /// A window of the class, `CW_USEDEFAULT` in size.
    fn window(class: &[u16], title: &str, style: u32) -> HWND {
        let title = w(title);
        let d = CW_USEDEFAULT;
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            style,
            d,
            d,
            d,
            d,
            0,
            0,
            0,
            std::ptr::null_mut(),
        );
        assert_ne!(hwnd, 0);
        hwnd
    }

    fn attach() {
        victory_loader::teb::attach_current_thread();
    }

    #[test]
    fn creating_a_visible_window_sends_what_windows_sends() {
        attach();
        let class = class("victory.test.create");
        let hwnd = window(&class, "Created", WS_VISIBLE);
        let messages: Vec<u32> = seen(hwnd).iter().map(|s| s.1).collect();
        let order = [
            WM_NCCREATE,
            WM_CREATE,
            WM_SHOWWINDOW,
            WM_SIZE,
            WM_MOVE,
            WM_ACTIVATE,
            WM_SETFOCUS,
        ];
        let mut at = 0;
        for m in &messages {
            if at < order.len() && *m == order[at] {
                at += 1;
            }
        }
        assert_eq!(at, order.len(), "{messages:x?}");
        let all = seen(hwnd);
        let created = all.iter().find(|s| s.1 == WM_CREATE).unwrap().4.clone();
        assert_eq!(
            created,
            Some((1920, 1080, WS_VISIBLE as i32, "Created".to_owned()))
        );
        let size = all.iter().find(|s| s.1 == WM_SIZE).unwrap();
        assert_eq!(size.3, pack(1920, 1080));
        let mut rect = RECT::default();
        assert_eq!(GetClientRect(hwnd, &mut rect), TRUE);
        assert_eq!(rect, SCREEN);
        assert_eq!(IsWindowVisible(hwnd), TRUE);
        assert_eq!(GetFocus(), hwnd);
        assert_eq!(
            GetWindowThreadProcessId(hwnd, std::ptr::null_mut()),
            thread_id()
        );

        assert_eq!(SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0x1234), 0);
        assert_eq!(GetWindowLongPtrW(hwnd, GWLP_USERDATA), 0x1234);
        assert_eq!(SetWindowLongPtrW(hwnd, 8, 99), 0);
        assert_eq!(GetWindowLongPtrW(hwnd, 8), 99);
        assert_eq!(GetWindowLongPtrW(hwnd, 9), 0);
        assert_eq!(
            super::super::win32::handles::last_error(),
            ERROR_INVALID_INDEX
        );

        let mut text = [0u16; 32];
        assert_eq!(GetWindowTextW(hwnd, text.as_mut_ptr(), 32), 7);
        assert_eq!(SetWindowTextW(hwnd, w("Renamed").as_ptr()), TRUE);
        assert_eq!(GetWindowTextW(hwnd, text.as_mut_ptr(), 32), 7);
        assert_eq!(String::from_utf16(&text[..7]).unwrap(), "Renamed");
        assert_eq!(GetWindowTextLengthW(hwnd), 7);
        let mut narrow_text = [0u8; 4];
        assert_eq!(GetWindowTextA(hwnd, narrow_text.as_mut_ptr(), 4), 3);
        assert_eq!(&narrow_text, b"Ren\0");
        assert_eq!(FindWindowW(class.as_ptr(), w("renamed").as_ptr()), hwnd);
        assert_eq!(FindWindowW(std::ptr::null(), w("Renamed").as_ptr()), hwnd);

        assert_eq!(DestroyWindow(hwnd), TRUE);
        let tail: Vec<u32> = seen(hwnd).iter().rev().take(2).map(|s| s.1).collect();
        assert_eq!(tail, [WM_NCDESTROY, WM_DESTROY]);
        assert_eq!(IsWindow(hwnd), FALSE);
        assert_eq!(FindWindowW(class.as_ptr(), std::ptr::null()), 0);
    }

    #[test]
    fn posted_messages_come_in_order_then_quit() {
        attach();
        let class = class("victory.test.post");
        let hwnd = window(&class, "Post", 0);
        let mut msg = Msg::default();
        for i in 1..=4 {
            assert_eq!(PostMessageW(hwnd, WM_USER, i, 0), TRUE);
        }
        for i in 1..=3 {
            assert_eq!(PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE), TRUE);
            assert_eq!((msg.hwnd, msg.message, msg.wparam), (hwnd, WM_USER, i));
        }
        assert_eq!(PeekMessageW(&mut msg, hwnd, 0, 0, 0), TRUE);
        assert_eq!(msg.wparam, 4);
        assert_eq!(GetQueueStatus(0x1FF) & QS_POSTMESSAGE, QS_POSTMESSAGE);
        assert_eq!(GetMessageW(&mut msg, 0, 0, 0), TRUE);
        assert_eq!(msg.wparam, 4);
        assert_eq!(PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE), FALSE);

        assert_eq!(PostMessageW(hwnd, WM_USER + 1, 5, 6), TRUE);
        assert_eq!(PostMessageW(hwnd, WM_APP, 0, 0), TRUE);
        PostQuitMessage(3);
        assert_eq!(PeekMessageW(&mut msg, 0, WM_APP, WM_APP, PM_REMOVE), TRUE);
        assert_eq!(msg.message, WM_APP);
        assert_eq!(GetMessageW(&mut msg, 0, 0, 0), TRUE);
        assert_eq!(DispatchMessageW(&msg), 0);
        assert!(
            seen(hwnd)
                .iter()
                .any(|s| s.1 == WM_USER + 1 && s.2 == 5 && s.3 == 6)
        );
        assert_eq!(GetMessageW(&mut msg, 0, 0, 0), 0);
        assert_eq!((msg.message, msg.wparam), (WM_QUIT, 3));
        assert_eq!(PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE), FALSE);
        assert_eq!(GetMessageW(&mut msg, 0x7FFF_0000, 0, 0), -1);
        assert_eq!(DestroyWindow(hwnd), TRUE);
        assert_eq!(PostMessageW(hwnd, WM_USER, 0, 0), FALSE);
    }

    #[test]
    fn another_thread_is_answered_when_this_one_peeks() {
        attach();
        let class = class("victory.test.send");
        let hwnd = window(&class, "Send", 0);
        let sender = std::thread::spawn(move || {
            victory_loader::teb::attach_current_thread();
            SendMessageW(hwnd, WM_USER + 7, 1, 2)
        });
        let started = Instant::now();
        let mut msg = Msg::default();
        while !sender.is_finished() {
            PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(sender.join().unwrap(), 77);
        assert!(
            seen(hwnd)
                .iter()
                .any(|s| s.1 == WM_USER + 7 && s.2 == 1 && s.3 == 2)
        );
        let late = std::thread::spawn(move || {
            victory_loader::teb::attach_current_thread();
            let mut result = 0;
            let answer = SendMessageTimeoutW(hwnd, WM_USER + 7, 0, 0, 0, 20, &mut result);
            (answer, super::super::win32::handles::last_error())
        });
        assert_eq!(late.join().unwrap(), (0, ERROR_TIMEOUT));
        DestroyWindow(hwnd);
    }

    #[test]
    fn timers_post_until_killed() {
        attach();
        let class = class("victory.test.timer");
        let hwnd = window(&class, "Timer", 0);
        assert_eq!(SetTimer(hwnd, 7, 10, 0), 7);
        let started = Instant::now();
        let mut msg = Msg::default();
        assert_eq!(GetMessageW(&mut msg, hwnd, 0, 0), TRUE);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!((msg.message, msg.wparam), (WM_TIMER, 7));
        assert_eq!(KillTimer(hwnd, 7), TRUE);
        assert_eq!(KillTimer(hwnd, 7), FALSE);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            PeekMessageW(&mut msg, hwnd, WM_TIMER, WM_TIMER, PM_REMOVE),
            FALSE
        );
        DestroyWindow(hwnd);
    }

    #[test]
    fn classes_are_registered_once_and_close_destroys() {
        attach();
        let class = class("victory.test.class");
        let name = class.clone();
        let again = WNDCLASSEXW {
            size: 80,
            style: 0,
            wndproc: recording as *const () as usize,
            cls_extra: 0,
            wnd_extra: 0,
            instance: 0,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: 0,
            class_name: name.as_ptr() as usize,
            icon_small: 0,
        };
        assert_eq!(RegisterClassExW(&again), 0);
        assert_eq!(
            super::super::win32::handles::last_error(),
            ERROR_CLASS_ALREADY_EXISTS
        );
        let mut info = std::mem::MaybeUninit::<WNDCLASSEXW>::zeroed();
        assert_ne!(GetClassInfoExW(0, class.as_ptr(), info.as_mut_ptr()), 0);
        // SAFETY: filled in above.
        assert_eq!(unsafe { info.assume_init() }.wnd_extra, 16);
        let hwnd = window(&class, "Close", 0);
        let mut text = [0u16; 32];
        assert_eq!(GetClassNameW(hwnd, text.as_mut_ptr(), 32), 18);
        assert_eq!(UnregisterClassW(class.as_ptr(), 0), FALSE);
        assert_eq!(DefWindowProcW(hwnd, WM_CLOSE, 0, 0), 0);
        assert_eq!(IsWindow(hwnd), FALSE);
        assert_eq!(UnregisterClassW(class.as_ptr(), 0), TRUE);
        assert_eq!(GetSystemMetrics(0), 1920);
        assert_eq!(GetSystemMetrics(1), 1080);
        assert_eq!(
            RegisterWindowMessageW(w("victory.msg").as_ptr()),
            RegisterWindowMessageW(w("VICTORY.MSG").as_ptr())
        );
        let missing = w("victory.test.none");
        let d = CW_USEDEFAULT;
        let none = CreateWindowExW(
            0,
            missing.as_ptr(),
            missing.as_ptr(),
            0,
            d,
            d,
            d,
            d,
            0,
            0,
            0,
            std::ptr::null_mut(),
        );
        assert_eq!(none, 0);
        assert_eq!(
            super::super::win32::handles::last_error(),
            ERROR_CANNOT_FIND_WND_CLASS
        );
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 196);
    }
}
