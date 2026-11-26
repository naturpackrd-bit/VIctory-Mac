//! What a C program's start and end call (`api-ms-win-crt-runtime`): the
//! initializer tables, the exit hooks, the program's arguments, command line
//! and environment; the per-thread `errno` and its messages; signals; the
//! floating-point control word; threads started through the C runtime; and
//! the pieces of `vcruntime140.dll` that are not exception dispatch (type
//! information, RTTI casts, `setjmp`/`longjmp`).
//!
//! Limits:
//! - C++ exceptions are not dispatched yet: `_CxxThrowException` ends the
//!   process, and the frame handlers are inert (they let the search go on).
//! - `_seh_filter_exe` does not run the title's `signal(SIGSEGV)` (or
//!   `SIGILL`, `SIGFPE`) handlers for a hardware exception: it lets the
//!   search go on.
//! - `abort` is `kernel.rs`'s: it does not run a title's `SIGABRT` handler.
//! - `longjmp` ignores the buffer's frame: no `__finally` block between the
//!   two calls runs (Windows unwinds through `RtlUnwindEx` there).
//! - `__RTCastToVoid` and `__RTDynamicCast` handle classes whose complete
//!   object locator has no constructor-displacement offset (`cd_offset` 0)
//!   only; others end the process with a message. `bad_cast` and
//!   `bad_typeid` cannot be thrown: where they would be, the process ends.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::{Cell, UnsafeCell};
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, Once, OnceLock, PoisonError};

use victory_loader::seh::__C_specific_handler;
use victory_platform::system::System;
use victory_platform::{Host, Thread};

use super::heap;
use crate::{HleExport, hle_export};

/// `errno`: operation not permitted.
pub(crate) const EPERM: i32 = 1;
/// `errno`: no such file or directory.
pub(crate) const ENOENT: i32 = 2;
/// `errno`: bad file descriptor (for the stdio slice).
#[allow(dead_code)]
pub(crate) const EBADF: i32 = 9;
/// `errno`: resource temporarily unavailable.
const EAGAIN: i32 = 11;
/// `errno`: not enough memory.
pub(crate) const ENOMEM: i32 = 12;
/// `errno`: permission denied (for the stdio slice).
#[allow(dead_code)]
pub(crate) const EACCES: i32 = 13;
/// `errno`: the file exists (for the stdio slice).
#[allow(dead_code)]
pub(crate) const EEXIST: i32 = 17;
/// `errno`: invalid argument.
pub(crate) const EINVAL: i32 = 22;
/// `errno`: too many open files (for the stdio slice).
#[allow(dead_code)]
pub(crate) const EMFILE: i32 = 24;
/// `errno`: a math argument out of the function's domain.
pub(crate) const EDOM: i32 = 33;
/// `errno`: a result out of range.
pub(crate) const ERANGE: i32 = 34;
/// `errno`: an illegal byte sequence.
pub(crate) const EILSEQ: i32 = 42;
/// What the `_s` functions return when they truncated as asked.
pub(crate) const STRUNCATE: i32 = 80;

/// Locks a mutex, taking it over from a thread that panicked holding it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Initializers and exit hooks.

/// An entry of an initializer table.
pub type Pvfv = Option<unsafe extern "win64" fn()>;
/// An entry of an initializer table that can fail.
pub type Pifv = Option<unsafe extern "win64" fn() -> i32>;
/// A function the title registers to run at exit.
pub type ExitFunction = Option<extern "win64" fn()>;

/// Calls each non-null entry from `begin` up to `end` (exclusive).
pub extern "win64" fn _initterm(begin: *const Pvfv, end: *const Pvfv) {
    if begin.is_null() || end.is_null() {
        return;
    }
    let mut entry = begin;
    while entry < end {
        // SAFETY: an entry of the title's table, which lies between `begin`
        // and `end`; the title's own initializer, called as it expects.
        unsafe {
            if let Some(f) = entry.read_unaligned() {
                f();
            }
        }
        entry = entry.wrapping_add(1);
    }
}

/// [`_initterm`] with initializers that return a status: stops at the first
/// non-zero one and returns it, else 0.
pub extern "win64" fn _initterm_e(begin: *const Pifv, end: *const Pifv) -> i32 {
    if begin.is_null() || end.is_null() {
        return 0;
    }
    let mut entry = begin;
    while entry < end {
        // SAFETY: as in `_initterm`.
        let status = unsafe { entry.read_unaligned().map_or(0, |f| f()) };
        if status != 0 {
            return status;
        }
        entry = entry.wrapping_add(1);
    }
    0
}

/// The CRT's `_onexit_table_t`. Ours keeps a leaked `Mutex<Vec<usize>>` in
/// `first` once anything was registered; `last` and `end` stay null.
#[repr(C)]
pub struct OnexitTable {
    first: *mut usize,
    last: *mut usize,
    end: *mut usize,
}

const _: () = assert!(std::mem::size_of::<OnexitTable>() == 24);

/// One table's list of functions.
type OnexitList = Mutex<Vec<usize>>;

/// Held while a table's list is made, so that two threads do not both.
static ONEXIT_TABLES: Mutex<()> = Mutex::new(());

/// Empties a table; -1 for null.
pub extern "win64" fn _initialize_onexit_table(table: *mut OnexitTable) -> i32 {
    if table.is_null() {
        return -1;
    }
    // SAFETY: the title's table.
    unsafe {
        table.write(OnexitTable {
            first: std::ptr::null_mut(),
            last: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
        })
    };
    0
}

/// Adds a function to a table; -1 for a null table or function.
pub extern "win64" fn _register_onexit_function(
    table: *mut OnexitTable,
    function: ExitFunction,
) -> i32 {
    let (false, Some(function)) = (table.is_null(), function) else {
        return -1;
    };
    let list = {
        let _made = lock(&ONEXIT_TABLES);
        // SAFETY: the title's table, whose `first` is null or a list of ours.
        unsafe {
            if (*table).first.is_null() {
                (*table).first = Box::into_raw(Box::new(OnexitList::default())).cast();
            }
            &*(*table).first.cast::<OnexitList>()
        }
    };
    lock(list).push(function as usize);
    0
}

/// Runs the functions of `list` in reverse order of registration, each
/// taken out before it runs, so that one a function registers runs too.
fn run_reversed(list: &Mutex<Vec<usize>>) {
    loop {
        // The lock is let go before the function runs: it may register more.
        let next = lock(list).pop();
        let Some(function) = next else { break };
        // SAFETY: registered as an `extern "win64" fn()`.
        let function = unsafe { std::mem::transmute::<usize, extern "win64" fn()>(function) };
        function();
    }
}

/// Runs a table's functions in reverse order, then frees its list.
pub extern "win64" fn _execute_onexit_table(table: *mut OnexitTable) -> i32 {
    if table.is_null() {
        return -1;
    }
    // SAFETY: the title's table, whose `first` is null or a list of ours.
    let first = unsafe { (*table).first };
    if first.is_null() {
        return 0;
    }
    // SAFETY: a list of ours, alive until it is freed below.
    run_reversed(unsafe { &*first.cast::<OnexitList>() });
    // SAFETY: as above; the table forgets it before it is freed.
    unsafe {
        (*table).first = std::ptr::null_mut();
        drop(Box::from_raw(first.cast::<OnexitList>()));
    }
    0
}

/// The process's `atexit` functions.
static ATEXIT: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// The process's `at_quick_exit` functions.
static AT_QUICK_EXIT: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Whether the exit hooks ran (or are running).
static EXIT_HOOKS_RAN: AtomicBool = AtomicBool::new(false);

/// Registers a function to run at exit; -1 for null.
pub extern "win64" fn _crt_atexit(function: ExitFunction) -> i32 {
    match function {
        Some(f) => {
            lock(&ATEXIT).push(f as usize);
            0
        }
        None => -1,
    }
}

/// Registers a function to run at `quick_exit`; -1 for null.
pub extern "win64" fn _crt_at_quick_exit(function: ExitFunction) -> i32 {
    match function {
        Some(f) => {
            lock(&AT_QUICK_EXIT).push(f as usize);
            0
        }
        None => -1,
    }
}

/// Runs the `atexit` functions in reverse order, once: an `exit` from inside
/// one of them does not run them again.
pub(crate) fn run_exit_hooks() {
    if EXIT_HOOKS_RAN.swap(true, Ordering::AcqRel) {
        return;
    }
    run_reversed(&ATEXIT);
}

/// Runs the exit hooks without ending the process.
pub extern "win64" fn _cexit() {
    run_exit_hooks();
}

/// Would end the C runtime without the exit hooks: nothing to do.
pub extern "win64" fn _c_exit() {}

/// Ends the process at once, without the exit hooks.
pub extern "win64" fn _exit(code: i32) -> ! {
    std::process::exit(code)
}

/// See [`_exit`].
pub extern "win64" fn _Exit(code: i32) -> ! {
    std::process::exit(code)
}

/// Runs the `at_quick_exit` functions in reverse order, then ends the
/// process without the exit hooks.
pub extern "win64" fn quick_exit(code: i32) -> ! {
    run_reversed(&AT_QUICK_EXIT);
    std::process::exit(code)
}

// ---------------------------------------------------------------------------
// The program's settings, arguments and environment.

static APP_TYPE: AtomicI32 = AtomicI32::new(0);
static COMMODE: AtomicI32 = AtomicI32::new(0);
static FMODE: AtomicI32 = AtomicI32::new(0);

/// Records whether the program is a console or a windowed one.
pub extern "win64" fn _set_app_type(kind: i32) {
    APP_TYPE.store(kind, Ordering::Relaxed);
}

/// See [`_set_app_type`].
pub extern "win64" fn _get_app_type() -> i32 {
    APP_TYPE.load(Ordering::Relaxed)
}

/// The default commit mode of streams.
pub extern "win64" fn __p__commode() -> *mut i32 {
    COMMODE.as_ptr()
}

/// Sets the default translation mode of files (kept for the stdio slice).
pub extern "win64" fn _set_fmode(mode: i32) -> i32 {
    FMODE.store(mode, Ordering::Relaxed);
    0
}

/// See [`_set_fmode`]; `EINVAL` for null.
pub extern "win64" fn _get_fmode(out: *mut i32) -> i32 {
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: the title's place for the mode.
    unsafe { out.write(FMODE.load(Ordering::Relaxed)) };
    0
}

/// See [`_set_fmode`].
pub extern "win64" fn __p__fmode() -> *mut i32 {
    FMODE.as_ptr()
}

/// The arguments are made on first use: nothing to do.
pub extern "win64" fn _configure_narrow_argv(_mode: i32) -> i32 {
    0
}

/// See [`_configure_narrow_argv`].
pub extern "win64" fn _configure_wide_argv(_mode: i32) -> i32 {
    0
}

/// The environment is made on first use: nothing to do.
pub extern "win64" fn _initialize_narrow_environment() -> i32 {
    0
}

/// See [`_initialize_narrow_environment`].
pub extern "win64" fn _initialize_wide_environment() -> i32 {
    0
}

/// The title's environment, name and value: what a Windows machine would
/// have that a title may look at. `_putenv` changes it.
pub(crate) fn environment() -> &'static Mutex<Vec<(String, String)>> {
    static ENVIRONMENT: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    ENVIRONMENT.get_or_init(|| {
        let processors = Host::logical_processors().to_string();
        let list = [
            ("USERPROFILE", r"C:\Users\victory"),
            ("TEMP", r"T:\"),
            ("TMP", r"T:\"),
            ("PATH", r"C:\Windows\System32"),
            ("SystemRoot", r"C:\Windows"),
            ("COMPUTERNAME", "VICTORY"),
            ("NUMBER_OF_PROCESSORS", &processors),
            ("PROCESSOR_ARCHITECTURE", "AMD64"),
        ];
        Mutex::new(
            list.iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        )
    })
}

/// A NUL-terminated copy of `text`, kept for the process's life (a NUL in
/// it ends it early).
pub(crate) fn leak_narrow(text: &str) -> *mut c_char {
    let mut bytes: Vec<u8> = text.bytes().take_while(|&b| b != 0).collect();
    bytes.push(0);
    Box::leak(bytes.into_boxed_slice()).as_mut_ptr().cast()
}

/// A NUL-terminated UTF-16 copy of `text`, kept for the process's life.
pub(crate) fn leak_wide(text: &str) -> *mut u16 {
    let mut units: Vec<u16> = text.encode_utf16().take_while(|&u| u != 0).collect();
    units.push(0);
    Box::leak(units.into_boxed_slice()).as_mut_ptr()
}

/// A null-terminated array of `items`, kept for the process's life.
pub(crate) fn leak_list<T>(items: Vec<*mut T>) -> *mut *mut T {
    let list: Box<[*mut T]> = items
        .into_iter()
        .chain(Some(std::ptr::null_mut()))
        .collect();
    Box::leak(list).as_mut_ptr()
}

/// The environment's entries as `NAME=value`.
pub(crate) fn environment_entries() -> Vec<String> {
    lock(environment())
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

/// The environment as the program started with it, made at the first call.
pub extern "win64" fn _get_initial_narrow_environment() -> *mut *mut c_char {
    static INITIAL: OnceLock<usize> = OnceLock::new();
    *INITIAL.get_or_init(|| {
        let entries = environment_entries();
        leak_list(entries.iter().map(|e| leak_narrow(e)).collect()) as usize
    }) as *mut *mut c_char
}

/// See [`_get_initial_narrow_environment`].
pub extern "win64" fn _get_initial_wide_environment() -> *mut *mut u16 {
    static INITIAL: OnceLock<usize> = OnceLock::new();
    *INITIAL.get_or_init(|| {
        let entries = environment_entries();
        leak_list(entries.iter().map(|e| leak_wide(e)).collect()) as usize
    }) as *mut *mut u16
}

/// The program's path as the title sees it ([`program_path`]), then the
/// title's arguments.
fn arguments() -> Vec<String> {
    std::iter::once(program_path())
        .chain(crate::runtime::title_args().iter().cloned())
        .collect()
}

/// Arguments joined by spaces into a command line, each one with a space in
/// it quoted.
fn command_line(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|a| {
            if a.contains(' ') || a.contains('\t') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

static WARGV: AtomicPtr<*mut u16> = AtomicPtr::new(std::ptr::null_mut());

/// The program's arguments as UTF-16 (`__wargv`), made once.
pub extern "win64" fn __p___wargv() -> *mut *mut *mut u16 {
    static MADE: Once = Once::new();
    MADE.call_once(|| {
        let list = arguments().iter().map(|a| leak_wide(a)).collect();
        WARGV.store(leak_list(list), Ordering::Release);
    });
    WARGV.as_ptr()
}

static ACMDLN: AtomicPtr<c_char> = AtomicPtr::new(std::ptr::null_mut());
static WCMDLN: AtomicPtr<u16> = AtomicPtr::new(std::ptr::null_mut());

/// The command line (`_acmdln`): the program and its arguments.
pub extern "win64" fn __p__acmdln() -> *mut *mut c_char {
    static MADE: Once = Once::new();
    MADE.call_once(|| ACMDLN.store(leak_narrow(&command_line(&arguments())), Ordering::Release));
    ACMDLN.as_ptr()
}

/// See [`__p__acmdln`].
pub extern "win64" fn __p__wcmdln() -> *mut *mut u16 {
    static MADE: Once = Once::new();
    MADE.call_once(|| WCMDLN.store(leak_wide(&command_line(&arguments())), Ordering::Release));
    WCMDLN.as_ptr()
}

/// The command line after the program's name, as `WinMain` gets it.
pub extern "win64" fn _get_narrow_winmain_command_line() -> *mut c_char {
    static LINE: OnceLock<usize> = OnceLock::new();
    *LINE.get_or_init(|| leak_narrow(&command_line(&arguments()[1..])) as usize) as *mut c_char
}

/// See [`_get_narrow_winmain_command_line`].
pub extern "win64" fn _get_wide_winmain_command_line() -> *mut u16 {
    static LINE: OnceLock<usize> = OnceLock::new();
    *LINE.get_or_init(|| leak_wide(&command_line(&arguments()[1..])) as usize) as *mut u16
}

/// The program's path as the title sees it: the main image's recorded
/// path (the runner records `G:\game.exe`, the console's own form) or,
/// before a title is mapped, `C:\title\game.exe`.
fn program_path() -> String {
    crate::runtime::main_image()
        .and_then(crate::runtime::image_path)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| r"C:\title\game.exe".to_owned())
}

static PGMPTR: AtomicPtr<c_char> = AtomicPtr::new(std::ptr::null_mut());
static WPGMPTR: AtomicPtr<u16> = AtomicPtr::new(std::ptr::null_mut());

/// `_pgmptr`: see [`program_path`].
pub extern "win64" fn __p__pgmptr() -> *mut *mut c_char {
    static MADE: Once = Once::new();
    MADE.call_once(|| PGMPTR.store(leak_narrow(&program_path()), Ordering::Release));
    PGMPTR.as_ptr()
}

/// `_wpgmptr`: see [`program_path`].
pub extern "win64" fn __p__wpgmptr() -> *mut *mut u16 {
    static MADE: Once = Once::new();
    MADE.call_once(|| WPGMPTR.store(leak_wide(&program_path()), Ordering::Release));
    WPGMPTR.as_ptr()
}

/// See [`__p__pgmptr`]; `EINVAL` for null.
pub extern "win64" fn _get_pgmptr(out: *mut *mut c_char) -> i32 {
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: checked non-null; the title's place for the pointer, and the
    // pointer `__p__pgmptr` gives.
    unsafe { out.write(*__p__pgmptr()) };
    0
}

/// See [`__p__wpgmptr`]; `EINVAL` for null.
pub extern "win64" fn _get_wpgmptr(out: *mut *mut u16) -> i32 {
    if out.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: as in `_get_pgmptr`.
    unsafe { out.write(*__p__wpgmptr()) };
    0
}

// ---------------------------------------------------------------------------
// Handlers: invalid parameters, pure calls, `new`, aborts.

/// The title's handler of invalid parameters: expression, function, file,
/// line, reserved.
pub type InvalidParameterHandler =
    Option<extern "win64" fn(*const u16, *const u16, *const u16, u32, usize)>;

static INVALID_PARAMETER: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static THREAD_INVALID_PARAMETER: Cell<usize> = const { Cell::new(0) };
}

/// A handler kept as an address back as the handler.
fn as_handler(address: usize) -> InvalidParameterHandler {
    // SAFETY: 0 or the address of a handler the title gave; `Option` of a
    // function pointer is a nullable pointer.
    unsafe { std::mem::transmute::<usize, InvalidParameterHandler>(address) }
}

/// Sets the process's handler; returns the previous one.
pub extern "win64" fn _set_invalid_parameter_handler(
    handler: InvalidParameterHandler,
) -> InvalidParameterHandler {
    let address = handler.map_or(0, |h| h as usize);
    as_handler(INVALID_PARAMETER.swap(address, Ordering::AcqRel))
}

/// The process's handler.
pub extern "win64" fn _get_invalid_parameter_handler() -> InvalidParameterHandler {
    as_handler(INVALID_PARAMETER.load(Ordering::Acquire))
}

/// Sets the calling thread's handler, which comes before the process's;
/// returns the previous one.
pub extern "win64" fn _set_thread_local_invalid_parameter_handler(
    handler: InvalidParameterHandler,
) -> InvalidParameterHandler {
    let address = handler.map_or(0, |h| h as usize);
    as_handler(
        THREAD_INVALID_PARAMETER
            .try_with(|h| h.replace(address))
            .unwrap_or(0),
    )
}

/// The calling thread's handler.
pub extern "win64" fn _get_thread_local_invalid_parameter_handler() -> InvalidParameterHandler {
    as_handler(THREAD_INVALID_PARAMETER.try_with(Cell::get).unwrap_or(0))
}

/// Reports an invalid parameter: to the thread's handler, else the
/// process's; with neither, the process ends (as Windows' default handler
/// ends it).
pub extern "win64" fn _invalid_parameter(
    expression: *const u16,
    function: *const u16,
    file: *const u16,
    line: u32,
    reserved: usize,
) {
    let handler =
        _get_thread_local_invalid_parameter_handler().or_else(|| _get_invalid_parameter_handler());
    match handler {
        Some(handler) => handler(expression, function, file, line, reserved),
        None => {
            tracing::error!("the title passed an invalid parameter to the C runtime");
            std::process::abort()
        }
    }
}

/// [`_invalid_parameter`] without details.
pub extern "win64" fn _invalid_parameter_noinfo() {
    _invalid_parameter(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, 0);
}

/// A handler without arguments (`_purecall_handler`).
pub type PurecallHandler = Option<extern "win64" fn()>;

static PURECALL: AtomicUsize = AtomicUsize::new(0);

/// Sets the pure-call handler; returns the previous one.
pub extern "win64" fn _set_purecall_handler(handler: PurecallHandler) -> PurecallHandler {
    let previous = PURECALL.swap(handler.map_or(0, |h| h as usize), Ordering::AcqRel);
    // SAFETY: 0 or a handler the title gave.
    unsafe { std::mem::transmute::<usize, PurecallHandler>(previous) }
}

/// See [`_set_purecall_handler`].
pub extern "win64" fn _get_purecall_handler() -> PurecallHandler {
    // SAFETY: 0 or a handler the title gave.
    unsafe { std::mem::transmute::<usize, PurecallHandler>(PURECALL.load(Ordering::Acquire)) }
}

/// A pure virtual function was called: the handler runs, if there is one,
/// and the process ends.
pub extern "win64" fn _purecall() -> ! {
    if let Some(handler) = _get_purecall_handler() {
        handler();
    }
    tracing::error!("pure virtual function called");
    std::process::abort()
}

/// The handler `new` calls when it has no memory (`_PNH`).
pub type NewHandler = Option<extern "win64" fn(usize) -> i32>;

static NEW_HANDLER: AtomicUsize = AtomicUsize::new(0);
static NEW_MODE: AtomicI32 = AtomicI32::new(0);

/// Sets the `new` handler; returns the previous one.
pub extern "win64" fn _set_new_handler(handler: NewHandler) -> NewHandler {
    let previous = NEW_HANDLER.swap(handler.map_or(0, |h| h as usize), Ordering::AcqRel);
    // SAFETY: 0 or a handler the title gave.
    unsafe { std::mem::transmute::<usize, NewHandler>(previous) }
}

/// Whether `malloc` calls the `new` handler (kept only); returns the
/// previous mode.
pub extern "win64" fn _set_new_mode(mode: i32) -> i32 {
    NEW_MODE.swap(mode, Ordering::AcqRel)
}

/// See [`_set_new_mode`].
pub extern "win64" fn _query_new_mode() -> i32 {
    NEW_MODE.load(Ordering::Acquire)
}

/// Calls the `new` handler, if there is one, and returns its result; 0
/// without one.
pub extern "win64" fn _callnewh(size: usize) -> i32 {
    // SAFETY: 0 or a handler the title gave.
    let handler =
        unsafe { std::mem::transmute::<usize, NewHandler>(NEW_HANDLER.load(Ordering::Acquire)) };
    handler.map_or(0, |h| h(size))
}

/// `_WRITE_ABORT_MSG | _CALL_REPORTFAULT`, the CRT's default.
static ABORT_BEHAVIOR: AtomicU32 = AtomicU32::new(3);

/// Changes the bits of `mask` of the abort behaviour to those of `flags`
/// (kept only); returns the previous behaviour.
pub extern "win64" fn _set_abort_behavior(flags: u32, mask: u32) -> u32 {
    let mut previous = ABORT_BEHAVIOR.load(Ordering::Acquire);
    loop {
        let new = (previous & !mask) | (flags & mask);
        match ABORT_BEHAVIOR.compare_exchange(previous, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return previous,
            Err(now) => previous = now,
        }
    }
}

static ERROR_MODE: AtomicI32 = AtomicI32::new(0);

/// Where error messages go (kept only): `_OUT_TO_DEFAULT` 0,
/// `_OUT_TO_STDERR` 1, `_OUT_TO_MSGBOX` 2; `_REPORT_ERRMODE` 3 asks. Returns
/// the previous mode; -1 and `EINVAL` for another.
pub extern "win64" fn _set_error_mode(mode: i32) -> i32 {
    match mode {
        0..=2 => ERROR_MODE.swap(mode, Ordering::AcqRel),
        3 => ERROR_MODE.load(Ordering::Acquire),
        _ => {
            set_errno(EINVAL);
            -1
        }
    }
}

/// The filter around `main`: lets the search go on
/// (`EXCEPTION_CONTINUE_SEARCH`); the title's `signal` handlers for
/// hardware exceptions are not run.
pub extern "win64" fn _seh_filter_exe(_code: u32, _pointers: *mut c_void) -> i32 {
    0
}

/// See [`_seh_filter_exe`].
pub extern "win64" fn _seh_filter_dll(_code: u32, _pointers: *mut c_void) -> i32 {
    0
}

static USER_MATHERR: AtomicUsize = AtomicUsize::new(0);

/// The title's math error handler: kept only (no math function reports
/// through it).
pub extern "win64" fn __setusermatherr(handler: usize) {
    USER_MATHERR.store(handler, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Signals.

const SIGINT: i32 = 2;
const SIGILL: i32 = 4;
const SIGFPE: i32 = 8;
const SIGSEGV: i32 = 11;
const SIGTERM: i32 = 15;
const SIGBREAK: i32 = 21;
const SIGABRT: i32 = 22;
/// The default action.
const SIG_DFL: usize = 0;
/// Ignore the signal.
const SIG_IGN: usize = 1;
/// Handler values `signal` refuses.
const SIG_SGE: usize = 3;
const SIG_ACK: usize = 4;
/// `signal`'s failure.
const SIG_ERR: usize = usize::MAX;
/// The second argument a `SIGFPE` handler gets from `raise`.
const FPE_EXPLICITGEN: i32 = 0x8C;

/// The signals the CRT knows, in the order of [`SIGNALS`].
const KNOWN_SIGNALS: [i32; 7] = [SIGINT, SIGILL, SIGFPE, SIGSEGV, SIGTERM, SIGBREAK, SIGABRT];

/// Each known signal's handler.
static SIGNALS: Mutex<[usize; 7]> = Mutex::new([SIG_DFL; 7]);

fn signal_index(sig: i32) -> Option<usize> {
    KNOWN_SIGNALS.iter().position(|&s| s == sig)
}

/// Sets a signal's handler (kept: only `raise` and `abort_signal` call
/// them); returns the previous one, or `SIG_ERR` and `EINVAL` for a signal
/// the CRT does not know.
pub extern "win64" fn signal(sig: i32, handler: usize) -> usize {
    let Some(index) = signal_index(sig) else {
        set_errno(EINVAL);
        return SIG_ERR;
    };
    if handler == SIG_SGE || handler == SIG_ACK {
        set_errno(EINVAL);
        return SIG_ERR;
    }
    std::mem::replace(&mut lock(&SIGNALS)[index], handler)
}

/// Runs the handler kept for `sig`, after resetting it to the default (as
/// the CRT does); whether one ran (`SIG_IGN` counts as one).
fn run_signal_handler(sig: i32) -> Option<bool> {
    let index = signal_index(sig)?;
    let handler = {
        let mut signals = lock(&SIGNALS);
        let handler = signals[index];
        if handler != SIG_DFL && handler != SIG_IGN {
            signals[index] = SIG_DFL;
        }
        handler
    };
    match handler {
        SIG_DFL => Some(false),
        SIG_IGN => Some(true),
        handler => {
            // SAFETY: a handler the title gave `signal`; a `SIGFPE` handler
            // takes a second argument, which the others do not read.
            let handler =
                unsafe { std::mem::transmute::<usize, extern "win64" fn(i32, i32)>(handler) };
            handler(sig, FPE_EXPLICITGEN);
            Some(true)
        }
    }
}

/// Raises a signal: its handler runs (0 is returned); without one, the
/// default action ends the process with code 3, as Windows' does. -1 and
/// `EINVAL` for a signal the CRT does not know.
pub extern "win64" fn raise(sig: i32) -> i32 {
    match run_signal_handler(sig) {
        Some(true) => 0,
        Some(false) => {
            tracing::warn!(sig, "the title raised a signal it has no handler for");
            _exit(3)
        }
        None => {
            set_errno(EINVAL);
            -1
        }
    }
}

/// What `abort` would do before ending: run the title's `SIGABRT` handler if
/// it has one. `kernel.rs`'s `abort` does not call it yet.
#[allow(dead_code)]
pub(crate) fn abort_signal() {
    let _ = run_signal_handler(SIGABRT);
}

// ---------------------------------------------------------------------------
// errno.

thread_local! {
    static ERRNO: Cell<i32> = const { Cell::new(0) };
    static DOSERRNO: Cell<u32> = const { Cell::new(0) };
}

/// Sets the calling thread's `errno`.
pub(crate) fn set_errno(code: i32) {
    let _ = ERRNO.try_with(|e| e.set(code));
}

/// The calling thread's `errno`.
pub(crate) fn errno() -> i32 {
    ERRNO.try_with(Cell::get).unwrap_or(0)
}

/// The address of the calling thread's `errno`.
pub extern "win64" fn _errno() -> *mut i32 {
    ERRNO.with(Cell::as_ptr)
}

/// Sets `errno`.
pub extern "win64" fn _set_errno(value: i32) -> i32 {
    set_errno(value);
    0
}

/// Reads `errno`; `EINVAL` for null.
pub extern "win64" fn _get_errno(out: *mut i32) -> i32 {
    if out.is_null() {
        return EINVAL;
    }
    // SAFETY: the title's place for the value.
    unsafe { out.write(errno()) };
    0
}

/// The address of the calling thread's operating-system error
/// (`_doserrno`).
pub extern "win64" fn __doserrno() -> *mut u32 {
    DOSERRNO.with(Cell::as_ptr)
}

/// Sets `_doserrno`.
pub extern "win64" fn _set_doserrno(value: u32) -> i32 {
    let _ = DOSERRNO.try_with(|e| e.set(value));
    0
}

/// Reads `_doserrno`; `EINVAL` for null.
pub extern "win64" fn _get_doserrno(out: *mut u32) -> i32 {
    if out.is_null() {
        return EINVAL;
    }
    // SAFETY: the title's place for the value.
    unsafe { out.write(DOSERRNO.try_with(Cell::get).unwrap_or(0)) };
    0
}

/// The Windows CRT's messages for `errno` values 0 to 42.
const MESSAGES: [&str; 43] = [
    "No error",
    "Operation not permitted",
    "No such file or directory",
    "No such process",
    "Interrupted function call",
    "Input/output error",
    "No such device or address",
    "Arg list too long",
    "Exec format error",
    "Bad file descriptor",
    "No child processes",
    "Resource temporarily unavailable",
    "Not enough space",
    "Permission denied",
    "Bad address",
    "Unknown error",
    "Resource device",
    "File exists",
    "Improper link",
    "No such device",
    "Not a directory",
    "Is a directory",
    "Invalid argument",
    "Too many open files in system",
    "Too many open files",
    "Inappropriate I/O control operation",
    "Unknown error",
    "File too large",
    "No space left on device",
    "Invalid seek",
    "Read-only file system",
    "Too many links",
    "Broken pipe",
    "Domain error",
    "Result too large",
    "Unknown error",
    "Resource deadlock avoided",
    "Unknown error",
    "Filename too long",
    "No locks available",
    "Function not implemented",
    "Directory not empty",
    "Illegal byte sequence",
];

/// The message of an `errno` value.
fn message(code: i32) -> &'static str {
    usize::try_from(code)
        .ok()
        .and_then(|i| MESSAGES.get(i))
        .copied()
        .unwrap_or("Unknown error")
}

/// The size of the CRT's per-thread message buffers.
const MESSAGE_BUFFER: usize = 134;

thread_local! {
    static NARROW_MESSAGE: UnsafeCell<[u8; MESSAGE_BUFFER]> =
        const { UnsafeCell::new([0; MESSAGE_BUFFER]) };
    static WIDE_MESSAGE: UnsafeCell<[u16; MESSAGE_BUFFER]> =
        const { UnsafeCell::new([0; MESSAGE_BUFFER]) };
}

/// `text`, truncated, in the calling thread's narrow message buffer.
fn narrow_message(text: &str) -> *mut c_char {
    NARROW_MESSAGE.with(|buffer| {
        let buffer = buffer.get();
        let bytes = text.as_bytes();
        let n = bytes.len().min(MESSAGE_BUFFER - 1);
        // SAFETY: the calling thread's own buffer, which no reference names.
        unsafe {
            (&mut *buffer)[..n].copy_from_slice(&bytes[..n]);
            (*buffer)[n] = 0;
        }
        buffer.cast()
    })
}

/// `text`, truncated, in the calling thread's wide message buffer.
fn wide_message(text: &str) -> *mut u16 {
    WIDE_MESSAGE.with(|buffer| {
        let buffer = buffer.get();
        let units: Vec<u16> = text.encode_utf16().take(MESSAGE_BUFFER - 1).collect();
        // SAFETY: the calling thread's own buffer, which no reference names.
        unsafe {
            (&mut *buffer)[..units.len()].copy_from_slice(&units);
            (*buffer)[units.len()] = 0;
        }
        buffer.cast()
    })
}

/// Copies `text` into the title's buffer of `size` units, truncated to fit.
fn copy_message<T: Copy + Default>(text: &[T], buffer: *mut T, size: usize) -> i32 {
    if buffer.is_null() || size == 0 {
        set_errno(EINVAL);
        return EINVAL;
    }
    let n = text.len().min(size - 1);
    // SAFETY: the title's buffer of `size` units; `n + 1 <= size`.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, n);
        buffer.add(n).write(T::default());
    }
    0
}

/// The message of an `errno` value, in a per-thread buffer.
pub extern "win64" fn strerror(code: i32) -> *mut c_char {
    narrow_message(message(code))
}

/// The message of an `errno` value, truncated into the title's buffer.
pub extern "win64" fn strerror_s(buffer: *mut c_char, size: usize, code: i32) -> i32 {
    copy_message(message(code).as_bytes(), buffer.cast::<u8>(), size)
}

/// The text of a C string, lossily.
fn narrow_text(text: *const c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the title's NUL-terminated string.
    Some(
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// The text of a wide C string, lossily.
fn wide_text(text: *const u16) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the title's NUL-terminated string.
    let n = unsafe { super::string::wide_len(text) };
    // SAFETY: `n` units before its NUL.
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, n)
    }))
}

/// `prefix: message` of the current `errno` with a newline, or the message
/// alone for a null or empty prefix.
fn prefixed(prefix: Option<String>) -> String {
    let message = message(errno());
    match prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => format!("{prefix}: {message}\n"),
        None => format!("{message}\n"),
    }
}

/// The current `errno`'s message after `prefix`, in a per-thread buffer.
pub extern "win64" fn _strerror(prefix: *const c_char) -> *mut c_char {
    narrow_message(&prefixed(narrow_text(prefix)))
}

/// The message of an `errno` value, in a per-thread wide buffer.
pub extern "win64" fn _wcserror(code: i32) -> *mut u16 {
    wide_message(message(code))
}

/// The message of an `errno` value, truncated into the title's wide buffer.
pub extern "win64" fn _wcserror_s(buffer: *mut u16, size: usize, code: i32) -> i32 {
    let units: Vec<u16> = message(code).encode_utf16().collect();
    copy_message(&units, buffer, size)
}

/// Writes the current `errno`'s message after `prefix` to standard error.
pub extern "win64" fn perror(prefix: *const c_char) {
    eprint!("{}", prefixed(narrow_text(prefix)));
}

/// The wide form of [`perror`] (`_wperror`).
pub extern "win64" fn _wperror(prefix: *const u16) {
    eprint!("{}", prefixed(wide_text(prefix)));
}

static ERRLIST: OnceLock<usize> = OnceLock::new();
static NERR: AtomicI32 = AtomicI32::new(MESSAGES.len() as i32);

/// The messages as an array (`_sys_errlist`).
pub extern "win64" fn __sys_errlist() -> *mut *mut c_char {
    *ERRLIST.get_or_init(|| leak_list(MESSAGES.iter().map(|m| leak_narrow(m)).collect()) as usize)
        as *mut *mut c_char
}

/// The number of messages (`_sys_nerr`).
pub extern "win64" fn __sys_nerr() -> *mut i32 {
    NERR.as_ptr()
}

// ---------------------------------------------------------------------------
// Floating point.

/// The control word's exception masks.
const MCW_EM: u32 = 0x0008_001F;
const EM_INEXACT: u32 = 0x1;
const EM_UNDERFLOW: u32 = 0x2;
const EM_OVERFLOW: u32 = 0x4;
const EM_ZERODIVIDE: u32 = 0x8;
const EM_INVALID: u32 = 0x10;
const EM_DENORMAL: u32 = 0x8_0000;
/// The control word's rounding mode.
const MCW_RC: u32 = 0x300;
/// The control word's denormal mode.
const MCW_DN: u32 = 0x300_0000;
/// Denormals flushed to zero, in and out.
const DN_FLUSH: u32 = 0x100_0000;
/// Denormal operands flushed, results kept.
const DN_FLUSH_OPERANDS_SAVE_RESULTS: u32 = 0x200_0000;
/// Denormal operands kept, results flushed.
const DN_SAVE_OPERANDS_FLUSH_RESULTS: u32 = 0x300_0000;
/// The control word's precision (x87 only).
const MCW_PC: u32 = 0x3_0000;
/// The status word's flags.
const SW_INEXACT: u32 = 0x1;
const SW_UNDERFLOW: u32 = 0x2;
const SW_OVERFLOW: u32 = 0x4;
const SW_ZERODIVIDE: u32 = 0x8;
const SW_INVALID: u32 = 0x10;
const SW_DENORMAL: u32 = 0x8_0000;

/// MXCSR's bits.
const MXCSR_DAZ: u32 = 1 << 6;
const MXCSR_FZ: u32 = 1 << 15;
const MXCSR_STATUS: u32 = 0x3F;
/// MXCSR's state at process start: every exception masked, round to
/// nearest.
const MXCSR_DEFAULT: u32 = 0x1F80;

/// MXCSR's exception bits in order (invalid, denormal, divide by zero,
/// overflow, underflow, precision), as the CRT's flags: the status flags
/// at bits 0..6 and the masks at 7..13 map to the same flags.
const EXCEPTIONS: [u32; 6] = [
    EM_INVALID,
    EM_DENORMAL,
    EM_ZERODIVIDE,
    EM_OVERFLOW,
    EM_UNDERFLOW,
    EM_INEXACT,
];
const STATUS: [u32; 6] = [
    SW_INVALID,
    SW_DENORMAL,
    SW_ZERODIVIDE,
    SW_OVERFLOW,
    SW_UNDERFLOW,
    SW_INEXACT,
];

/// The CRT's control word that an MXCSR value means.
fn control_from_mxcsr(mxcsr: u32) -> u32 {
    let mut control = 0;
    for (bit, flag) in EXCEPTIONS.iter().enumerate() {
        if mxcsr & (1 << (7 + bit)) != 0 {
            control |= flag;
        }
    }
    control |= ((mxcsr >> 13) & 3) << 8;
    control |= match (mxcsr & MXCSR_FZ != 0, mxcsr & MXCSR_DAZ != 0) {
        (true, true) => DN_FLUSH,
        (false, true) => DN_FLUSH_OPERANDS_SAVE_RESULTS,
        (true, false) => DN_SAVE_OPERANDS_FLUSH_RESULTS,
        (false, false) => 0,
    };
    control
}

/// The MXCSR value that a CRT control word means, keeping `mxcsr`'s status
/// flags.
fn mxcsr_from_control(control: u32, mxcsr: u32) -> u32 {
    let mut out = mxcsr & MXCSR_STATUS;
    for (bit, flag) in EXCEPTIONS.iter().enumerate() {
        if control & flag != 0 {
            out |= 1 << (7 + bit);
        }
    }
    out |= ((control >> 8) & 3) << 13;
    out |= match control & MCW_DN {
        DN_FLUSH => MXCSR_FZ | MXCSR_DAZ,
        DN_FLUSH_OPERANDS_SAVE_RESULTS => MXCSR_DAZ,
        DN_SAVE_OPERANDS_FLUSH_RESULTS => MXCSR_FZ,
        _ => 0,
    };
    out
}

/// The CRT's status flags that an MXCSR value holds.
fn status_from_mxcsr(mxcsr: u32) -> u32 {
    STATUS
        .iter()
        .enumerate()
        .filter(|(bit, _)| mxcsr & (1 << bit) != 0)
        .fold(0, |status, (_, flag)| status | flag)
}

/// The calling thread's MXCSR.
fn read_mxcsr() -> u32 {
    let mut value: u32 = 0;
    // SAFETY: stores MXCSR into a local.
    unsafe {
        core::arch::asm!(
            "stmxcsr [{}]",
            in(reg) &raw mut value,
            options(nostack, preserves_flags),
        )
    };
    value
}

/// Sets the calling thread's MXCSR.
fn write_mxcsr(value: u32) {
    // SAFETY: loads MXCSR from a local; reserved bits are never set by the
    // callers.
    unsafe {
        core::arch::asm!(
            "ldmxcsr [{}]",
            in(reg) &raw const value,
            options(nostack, readonly, preserves_flags),
        )
    };
}

/// Changes the bits of `mask` of the control word to those of `new`;
/// returns the control word after the change. The precision bits are read
/// as `_PC_64` and change nothing (SSE has none).
fn change_control(new: u32, mask: u32) -> u32 {
    let mxcsr = read_mxcsr();
    let control = control_from_mxcsr(mxcsr);
    let mask = mask & (MCW_EM | MCW_RC | MCW_DN);
    if mask == 0 {
        return control;
    }
    let changed = (control & !mask) | (new & mask);
    write_mxcsr(mxcsr_from_control(changed, mxcsr));
    control_from_mxcsr(read_mxcsr())
}

/// [`change_control`], with the control word after it in `*current`;
/// `EINVAL` for mask bits outside the control word.
pub extern "win64" fn _controlfp_s(current: *mut u32, new: u32, mask: u32) -> i32 {
    let valid = mask & !(MCW_EM | MCW_RC | MCW_DN | MCW_PC) == 0;
    let control = if valid {
        change_control(new, mask)
    } else {
        change_control(0, 0)
    };
    if !current.is_null() {
        // SAFETY: the title's place for the control word.
        unsafe { current.write(control) };
    }
    if valid {
        0
    } else {
        set_errno(EINVAL);
        EINVAL
    }
}

/// [`change_control`]; the denormal exception's mask is left as it is (as
/// the CRT does).
pub extern "win64" fn _controlfp(new: u32, mask: u32) -> u32 {
    change_control(new, mask & !EM_DENORMAL)
}

/// [`change_control`].
pub extern "win64" fn _control87(new: u32, mask: u32) -> u32 {
    change_control(new, mask)
}

/// [`change_control`], with the result in `*sse2_cw`; the x87 unit is not
/// used by title code here, and `*x86_cw` gets the same value. 1.
pub extern "win64" fn __control87_2(
    new: u32,
    mask: u32,
    x86_cw: *mut u32,
    sse2_cw: *mut u32,
) -> i32 {
    let control = if sse2_cw.is_null() {
        change_control(0, 0)
    } else {
        change_control(new, mask)
    };
    for out in [x86_cw, sse2_cw] {
        if !out.is_null() {
            // SAFETY: the title's place for a control word.
            unsafe { out.write(control) };
        }
    }
    1
}

/// The status flags, which are then cleared.
pub extern "win64" fn _clearfp() -> u32 {
    let mxcsr = read_mxcsr();
    write_mxcsr(mxcsr & !MXCSR_STATUS);
    status_from_mxcsr(mxcsr)
}

/// The status flags.
pub extern "win64" fn _statusfp() -> u32 {
    status_from_mxcsr(read_mxcsr())
}

/// The status flags of the x87 unit (none: title code does not use it here)
/// and of SSE.
pub extern "win64" fn _statusfp2(x86: *mut u32, sse2: *mut u32) {
    if !x86.is_null() {
        // SAFETY: the title's place for the flags.
        unsafe { x86.write(0) };
    }
    if !sse2.is_null() {
        // SAFETY: as above.
        unsafe { sse2.write(_statusfp()) };
    }
}

/// Resets the floating-point state: MXCSR as at process start.
pub extern "win64" fn _fpreset() {
    write_mxcsr(MXCSR_DEFAULT);
}

/// `FLT_ROUNDS` of the current rounding mode: 1 to nearest, 3 down, 2 up, 0
/// toward zero.
pub extern "win64" fn __fpe_flt_rounds() -> i32 {
    match (read_mxcsr() >> 13) & 3 {
        0 => 1,
        1 => 3,
        2 => 2,
        _ => 0,
    }
}

const FPCLASS_SNAN: i32 = 0x1;
const FPCLASS_QNAN: i32 = 0x2;
const FPCLASS_NINF: i32 = 0x4;
const FPCLASS_NN: i32 = 0x8;
const FPCLASS_ND: i32 = 0x10;
const FPCLASS_NZ: i32 = 0x20;
const FPCLASS_PZ: i32 = 0x40;
const FPCLASS_PD: i32 = 0x80;
const FPCLASS_PN: i32 = 0x100;
const FPCLASS_PINF: i32 = 0x200;

/// The class of a value, from its sign and category; `quiet` tells a NaN's
/// kind.
fn fp_class(category: std::num::FpCategory, negative: bool, quiet: bool) -> i32 {
    use std::num::FpCategory::*;
    match (category, negative) {
        (Nan, _) if quiet => FPCLASS_QNAN,
        (Nan, _) => FPCLASS_SNAN,
        (Infinite, true) => FPCLASS_NINF,
        (Infinite, false) => FPCLASS_PINF,
        (Normal, true) => FPCLASS_NN,
        (Normal, false) => FPCLASS_PN,
        (Subnormal, true) => FPCLASS_ND,
        (Subnormal, false) => FPCLASS_PD,
        (Zero, true) => FPCLASS_NZ,
        (Zero, false) => FPCLASS_PZ,
    }
}

/// The class of a double (`_FPCLASS_*`).
pub extern "win64" fn _fpclass(x: f64) -> i32 {
    let quiet = x.to_bits() & (1 << 51) != 0;
    fp_class(x.classify(), x.is_sign_negative(), quiet)
}

/// The class of a float (`_FPCLASS_*`).
pub extern "win64" fn _fpclassf(x: f32) -> i32 {
    let quiet = x.to_bits() & (1 << 22) != 0;
    fp_class(x.classify(), x.is_sign_negative(), quiet)
}

// ---------------------------------------------------------------------------
// Threads.

/// A thread's start routine as `_beginthreadex` takes it.
pub type ThreadStartEx = Option<unsafe extern "win64" fn(*mut c_void) -> u32>;
/// A thread's start routine as `_beginthread` takes it.
pub type ThreadStart = Option<unsafe extern "win64" fn(*mut c_void)>;

/// A new thread running `start(arg)` (`CreateThread`); its handle, or 0 and
/// `errno` set.
pub extern "win64" fn _beginthreadex(
    _security: *mut c_void,
    stack: u32,
    start: ThreadStartEx,
    arg: *mut c_void,
    flags: u32,
    thread_id: *mut u32,
) -> usize {
    let Some(start) = start else {
        set_errno(EINVAL);
        return 0;
    };
    let handle = super::super::win32::threads::CreateThread(
        std::ptr::null_mut(),
        stack as usize,
        start as *mut c_void,
        arg,
        flags,
        thread_id,
    );
    if handle.is_null() {
        set_errno(EAGAIN);
    }
    handle as usize
}

/// The start routine of a thread from `_beginthread`: the title's routine
/// and its argument, which ours takes over.
extern "win64" fn begin_thread_routine(parameter: *mut c_void) -> u32 {
    // SAFETY: the box `_beginthread` leaked for this thread alone.
    let (start, arg) = *unsafe { Box::from_raw(parameter.cast::<(usize, usize)>()) };
    // SAFETY: the title's start routine, an `extern "win64" fn(*mut c_void)`.
    unsafe {
        let start = std::mem::transmute::<usize, unsafe extern "win64" fn(*mut c_void)>(start);
        start(arg as *mut c_void);
    }
    0
}

/// A new thread running `start(arg)`; -1 and `errno` set on failure. The
/// handle is closed at once, as Windows' CRT closes it when the thread ends:
/// the value returned names no thread to wait for.
pub extern "win64" fn _beginthread(start: ThreadStart, stack: u32, arg: *mut c_void) -> usize {
    let Some(start) = start else {
        set_errno(EINVAL);
        return usize::MAX;
    };
    let pair = Box::into_raw(Box::new((start as usize, arg as usize)));
    let handle = super::super::win32::threads::CreateThread(
        std::ptr::null_mut(),
        stack as usize,
        begin_thread_routine as *mut c_void,
        pair.cast(),
        0,
        std::ptr::null_mut(),
    );
    if handle.is_null() {
        // SAFETY: no thread took the box; it is ours again.
        drop(unsafe { Box::from_raw(pair) });
        set_errno(EAGAIN);
        return usize::MAX;
    }
    super::super::win32::handles::CloseHandle(handle);
    handle as usize
}

/// Ends the calling thread with `code` (`ExitThread`).
pub extern "win64" fn _endthreadex(code: u32) -> ! {
    super::super::win32::threads::ExitThread(code)
}

/// Ends the calling thread (`ExitThread(0)`).
pub extern "win64" fn _endthread() -> ! {
    super::super::win32::threads::ExitThread(0)
}

/// The process's id.
pub extern "win64" fn _getpid() -> i32 {
    Host::process_id() as i32
}

// ---------------------------------------------------------------------------
// vcruntime: type information.

/// The layout of `std::type_info`: its vtable, the cached undecorated name
/// and the decorated name (`.?AV...`), which starts at offset 16.
#[repr(C)]
pub struct TypeInfo {
    vtable: usize,
    undecorated: *mut c_char,
    decorated: [c_char; 0],
}

/// The decorated name of a `type_info`.
///
/// # Safety
/// `data` is a live `type_info`.
unsafe fn decorated(data: *const TypeInfo) -> *const c_char {
    // SAFETY: the name starts at offset 16 of the caller's `type_info`.
    unsafe { (&raw const (*data).decorated).cast() }
}

/// Frees the cached undecorated names: we cache none.
pub extern "win64" fn __std_type_info_destroy_list(_root: *mut c_void) {}

/// The name of a type: the decorated name without its leading `.`, a stand
/// in for the undecorated one (no undecorator here).
pub extern "win64" fn __std_type_info_name(
    data: *const TypeInfo,
    _root: *mut c_void,
) -> *const c_char {
    if data.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the title's `type_info`, whose name starts with a `.`.
    unsafe { decorated(data).add(1) }
}

/// Compares two types by their decorated names (as `strcmp`).
pub extern "win64" fn __std_type_info_compare(a: *const TypeInfo, b: *const TypeInfo) -> i32 {
    if a == b {
        return 0;
    }
    if a.is_null() || b.is_null() {
        return if a.is_null() { -1 } else { 1 };
    }
    // SAFETY: the title's `type_info`s, whose names are NUL-terminated.
    unsafe { CStr::from_ptr(decorated(a)).cmp(CStr::from_ptr(decorated(b))) as i32 }
}

/// A hash of a type's decorated name (FNV-1a).
pub extern "win64" fn __std_type_info_hash(data: *const TypeInfo) -> usize {
    if data.is_null() {
        return 0;
    }
    // SAFETY: the title's `type_info`, whose name is NUL-terminated.
    let name = unsafe { CStr::from_ptr(decorated(data)) };
    name.to_bytes()
        .iter()
        .fold(0xCBF2_9CE4_8422_2325u64, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
        }) as usize
}

/// `std::exception`'s data: its message, and whether the exception owns it.
#[repr(C)]
pub struct ExceptionData {
    what: *const c_char,
    do_free: u8,
}

const _: () = assert!(std::mem::size_of::<ExceptionData>() == 16);

/// Copies an exception's data: an owned message is copied (`malloc`), a
/// borrowed one shared.
pub extern "win64" fn __std_exception_copy(from: *const ExceptionData, to: *mut ExceptionData) {
    if from.is_null() || to.is_null() {
        return;
    }
    // SAFETY: the title's two exception data, the message NUL-terminated.
    unsafe {
        let what = (*from).what;
        if (*from).do_free == 0 || what.is_null() {
            to.write(ExceptionData { what, do_free: 0 });
            return;
        }
        let bytes = CStr::from_ptr(what).to_bytes_with_nul();
        let copy = heap::malloc(bytes.len()).cast::<u8>();
        if copy.is_null() {
            to.write(ExceptionData {
                what: std::ptr::null(),
                do_free: 0,
            });
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy, bytes.len());
        to.write(ExceptionData {
            what: copy.cast(),
            do_free: 1,
        });
    }
}

/// Frees an exception's message if it owns it; the data is emptied.
pub extern "win64" fn __std_exception_destroy(data: *mut ExceptionData) {
    if data.is_null() {
        return;
    }
    // SAFETY: the title's exception data; an owned message is a block of
    // ours.
    unsafe {
        if (*data).do_free != 0 {
            heap::free((*data).what.cast_mut().cast());
        }
        data.write(ExceptionData {
            what: std::ptr::null(),
            do_free: 0,
        });
    }
}

/// `std::terminate`: `kernel.rs`'s `terminate`.
pub extern "win64" fn __std_terminate() -> ! {
    crate::modules::kernel::terminate()
}

/// Throws a C++ exception: not dispatched yet, so the process ends.
pub extern "win64" fn _CxxThrowException(object: *mut c_void, throw_info: *const c_void) -> ! {
    tracing::error!(
        object = ?object,
        throw_info = ?throw_info,
        "C++ exception thrown: not dispatched yet, the process ends"
    );
    std::process::abort()
}

/// The C++ frame handler: inert, it lets the search go on
/// (`ExceptionContinueSearch`).
pub extern "win64" fn __CxxFrameHandler3(
    _record: *mut c_void,
    _frame: u64,
    _context: *mut c_void,
    _dispatcher: *mut c_void,
) -> u32 {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| tracing::warn!("C++ frame handlers are inert"));
    1
}

/// See [`__CxxFrameHandler3`].
pub extern "win64" fn __CxxFrameHandler4(
    record: *mut c_void,
    frame: u64,
    context: *mut c_void,
    dispatcher: *mut c_void,
) -> u32 {
    __CxxFrameHandler3(record, frame, context, dispatcher)
}

thread_local! {
    static CURRENT_EXCEPTION: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
    static CURRENT_EXCEPTION_CONTEXT: Cell<*mut c_void> =
        const { Cell::new(std::ptr::null_mut()) };
    static PROCESSING_THROW: Cell<i32> = const { Cell::new(0) };
}

/// The calling thread's slot for the exception being handled.
pub extern "win64" fn __current_exception() -> *mut *mut c_void {
    CURRENT_EXCEPTION.with(Cell::as_ptr)
}

/// The calling thread's slot for the context of the exception being handled.
pub extern "win64" fn __current_exception_context() -> *mut *mut c_void {
    CURRENT_EXCEPTION_CONTEXT.with(Cell::as_ptr)
}

/// The calling thread's count of throws in progress.
pub extern "win64" fn __processing_throw() -> *mut i32 {
    PROCESSING_THROW.with(Cell::as_ptr)
}

/// Whether the current exception is of a type: never (none is dispatched).
pub extern "win64" fn _is_exception_typeof(_ty: *const c_void, _pointers: *mut c_void) -> i32 {
    0
}

/// Whether an exception object is to be destroyed: yes.
pub extern "win64" fn _IsExceptionObjectToBeDestroyed(_object: *mut c_void) -> i32 {
    1
}

// ---------------------------------------------------------------------------
// vcruntime: RTTI.

/// The complete object locator stored just before a vtable.
#[repr(C)]
struct CompleteObjectLocator {
    signature: u32,
    offset: u32,
    cd_offset: u32,
    type_descriptor: i32,
    class_descriptor: i32,
    self_rva: i32,
}

/// A class's hierarchy: its base classes.
#[repr(C)]
struct ClassHierarchyDescriptor {
    signature: u32,
    attributes: u32,
    num_base_classes: u32,
    base_class_array: i32,
}

/// One base class: its type and where it is in the complete object.
#[repr(C)]
struct BaseClassDescriptor {
    type_descriptor: i32,
    num_contained_bases: u32,
    mdisp: i32,
    pdisp: i32,
    vdisp: i32,
    attributes: u32,
    class_descriptor: i32,
}

/// Ends the process for an RTTI operation we cannot do.
fn rtti_failure(what: &str) -> ! {
    tracing::error!("{what}: the process ends");
    std::process::abort()
}

/// The locator of a polymorphic object and its image's base.
///
/// # Safety
/// `object` is a live polymorphic object of the title's.
unsafe fn locator(object: *const c_void) -> (CompleteObjectLocator, usize) {
    // SAFETY: the object's first field is its vtable, and the locator's
    // address is stored just before the vtable.
    let (locator, address) = unsafe {
        let vtable = object.cast::<usize>().read_unaligned();
        let address = ((vtable - 8) as *const usize).read_unaligned();
        (
            (address as *const CompleteObjectLocator).read_unaligned(),
            address,
        )
    };
    if locator.signature != 1 {
        rtti_failure("an absolute (32-bit) complete object locator, not seen on x64");
    }
    let base = (address as isize - locator.self_rva as isize) as usize;
    (locator, base)
}

/// The type descriptor (`type_info`) of a polymorphic object; the process
/// ends for null, as `bad_typeid` cannot be thrown.
pub extern "win64" fn __RTtypeid(object: *mut c_void) -> *const c_void {
    if object.is_null() {
        rtti_failure("typeid of a null pointer (bad_typeid cannot be thrown)");
    }
    // SAFETY: the title's polymorphic object.
    let (locator, base) = unsafe { locator(object) };
    (base as isize + locator.type_descriptor as isize) as *const c_void
}

/// The complete object a polymorphic object is part of.
///
/// # Safety
/// `object` is a live polymorphic object of the title's.
unsafe fn complete_object(object: *mut c_void) -> (*mut c_void, CompleteObjectLocator, usize) {
    // SAFETY: the caller's object.
    let (locator, base) = unsafe { locator(object) };
    if locator.cd_offset != 0 {
        rtti_failure("an object in a virtual base with a constructor displacement");
    }
    let complete = (object as isize - locator.offset as isize) as *mut c_void;
    (complete, locator, base)
}

/// The complete object a polymorphic object is part of (`dynamic_cast<void*>`);
/// null for null.
pub extern "win64" fn __RTCastToVoid(object: *mut c_void) -> *mut c_void {
    if object.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the title's polymorphic object.
    unsafe { complete_object(object).0 }
}

/// The name of a type descriptor (offset 16).
///
/// # Safety
/// `descriptor` is a live type descriptor.
unsafe fn descriptor_name<'a>(descriptor: usize) -> &'a CStr {
    // SAFETY: the caller's descriptor's NUL-terminated name.
    unsafe { CStr::from_ptr((descriptor + 16) as *const c_char) }
}

/// `dynamic_cast`: the base class of `object`'s complete object whose type
/// is named as `dst_type`; null when there is none (and the process ends
/// for a reference, as `bad_cast` cannot be thrown). `vfdelta` and
/// `src_type` are not used, and neither is a base's accessibility or
/// ambiguity checked: the first base of that name is the answer.
pub extern "win64" fn __RTDynamicCast(
    object: *mut c_void,
    _vfdelta: i32,
    _src_type: *const c_void,
    dst_type: *const c_void,
    is_reference: i32,
) -> *mut c_void {
    if object.is_null() || dst_type.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the title's polymorphic object, its RTTI and its target type.
    let found = unsafe {
        let (complete, locator, base) = complete_object(object);
        let at = |rva: i32| (base as isize + rva as isize) as usize;
        let hierarchy =
            (at(locator.class_descriptor) as *const ClassHierarchyDescriptor).read_unaligned();
        let target = descriptor_name(dst_type as usize);
        let array = at(hierarchy.base_class_array) as *const i32;
        (0..hierarchy.num_base_classes as usize).find_map(|i| {
            let descriptor =
                (at(array.add(i).read_unaligned()) as *const BaseClassDescriptor).read_unaligned();
            if descriptor_name(at(descriptor.type_descriptor)) != target {
                return None;
            }
            let complete = complete as isize;
            let mut offset = descriptor.mdisp as isize;
            if descriptor.pdisp >= 0 {
                // A virtual base: its offset is in the vbtable.
                let vbtable =
                    ((complete + descriptor.pdisp as isize) as *const usize).read_unaligned();
                let displacement =
                    ((vbtable as isize + descriptor.vdisp as isize) as *const i32).read_unaligned();
                offset += descriptor.pdisp as isize + displacement as isize;
            }
            Some((complete + offset) as *mut c_void)
        })
    };
    match found {
        Some(found) => found,
        None if is_reference != 0 => {
            rtti_failure("a failed dynamic_cast of a reference (bad_cast cannot be thrown)")
        }
        None => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// vcruntime: what it asks the system for, and the rest.

/// `InitializeCriticalSectionEx`.
pub extern "win64" fn __vcrt_InitializeCriticalSectionEx(
    cs: *mut c_void,
    spin: u32,
    flags: u32,
) -> i32 {
    super::super::win32::sync::InitializeCriticalSectionEx(cs.cast(), spin, flags)
}

/// `GetModuleFileNameW`.
pub extern "win64" fn __vcrt_GetModuleFileNameW(
    module: *mut c_void,
    buffer: *mut u16,
    size: u32,
) -> u32 {
    crate::modules::kernel::GetModuleFileNameW(module, buffer, size)
}

/// `GetModuleHandleW`.
pub extern "win64" fn __vcrt_GetModuleHandleW(name: *const u16) -> *mut c_void {
    crate::modules::kernel::GetModuleHandleW(name)
}

/// `LoadLibraryExW`.
pub extern "win64" fn __vcrt_LoadLibraryExW(
    name: *const u16,
    file: *mut c_void,
    flags: u32,
) -> *mut c_void {
    crate::modules::kernel::LoadLibraryExW(name, file, flags)
}

/// Telemetry at `main`'s start: nothing.
pub extern "win64" fn __telemetry_main_invoke_trigger(_module: *const c_void) {}

/// Telemetry at `main`'s end: nothing.
pub extern "win64" fn __telemetry_main_return_trigger(_module: *const c_void) {}

/// Saves the callee-saved registers, the stack pointer, the return address
/// and the floating-point control in the 256-byte `_JUMP_BUFFER` at `rcx`
/// (`rdx`, the frame, is stored); returns 0, and again with
/// [`longjmp`]'s value.
///
/// # Safety
/// `rcx` is a writable `_JUMP_BUFFER`; the caller is title code that expects
/// this function to return twice.
#[unsafe(naked)]
pub unsafe extern "win64" fn _setjmp(buffer: *mut c_void, frame: *mut c_void) -> i32 {
    core::arch::naked_asm!(
        "mov [rcx], rdx",
        "mov [rcx + 0x08], rbx",
        // rsp once we have returned.
        "lea rax, [rsp + 8]",
        "mov [rcx + 0x10], rax",
        "mov [rcx + 0x18], rbp",
        "mov [rcx + 0x20], rsi",
        "mov [rcx + 0x28], rdi",
        "mov [rcx + 0x30], r12",
        "mov [rcx + 0x38], r13",
        "mov [rcx + 0x40], r14",
        "mov [rcx + 0x48], r15",
        // Where we return to.
        "mov rax, [rsp]",
        "mov [rcx + 0x50], rax",
        "stmxcsr [rcx + 0x58]",
        "fnstcw [rcx + 0x5C]",
        "movdqu [rcx + 0x60], xmm6",
        "movdqu [rcx + 0x70], xmm7",
        "movdqu [rcx + 0x80], xmm8",
        "movdqu [rcx + 0x90], xmm9",
        "movdqu [rcx + 0xA0], xmm10",
        "movdqu [rcx + 0xB0], xmm11",
        "movdqu [rcx + 0xC0], xmm12",
        "movdqu [rcx + 0xD0], xmm13",
        "movdqu [rcx + 0xE0], xmm14",
        "movdqu [rcx + 0xF0], xmm15",
        "xor eax, eax",
        "ret",
    )
}

/// Returns from the [`_setjmp`] that filled the buffer at `rcx`, with
/// `value` (1 for 0). The buffer's frame is ignored: nothing between is
/// unwound.
///
/// # Safety
/// The buffer was filled by a `_setjmp` whose caller's frame is still live
/// on this thread.
#[unsafe(naked)]
pub unsafe extern "win64" fn longjmp(buffer: *const c_void, value: i32) -> ! {
    core::arch::naked_asm!(
        "mov rbx, [rcx + 0x08]",
        "mov rbp, [rcx + 0x18]",
        "mov rsi, [rcx + 0x20]",
        "mov rdi, [rcx + 0x28]",
        "mov r12, [rcx + 0x30]",
        "mov r13, [rcx + 0x38]",
        "mov r14, [rcx + 0x40]",
        "mov r15, [rcx + 0x48]",
        "ldmxcsr [rcx + 0x58]",
        "fldcw [rcx + 0x5C]",
        "movdqu xmm6, [rcx + 0x60]",
        "movdqu xmm7, [rcx + 0x70]",
        "movdqu xmm8, [rcx + 0x80]",
        "movdqu xmm9, [rcx + 0x90]",
        "movdqu xmm10, [rcx + 0xA0]",
        "movdqu xmm11, [rcx + 0xB0]",
        "movdqu xmm12, [rcx + 0xC0]",
        "movdqu xmm13, [rcx + 0xD0]",
        "movdqu xmm14, [rcx + 0xE0]",
        "movdqu xmm15, [rcx + 0xF0]",
        "mov eax, edx",
        "test eax, eax",
        "jnz 2f",
        "mov eax, 1",
        "2:",
        "mov rsp, [rcx + 0x10]",
        "jmp qword ptr [rcx + 0x50]",
    )
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(_initterm),
        hle_export!(_initterm_e),
        hle_export!(_initialize_onexit_table),
        hle_export!(_register_onexit_function),
        hle_export!(_execute_onexit_table),
        hle_export!(_crt_atexit),
        hle_export!(_crt_at_quick_exit),
        hle_export!(_cexit),
        hle_export!(_c_exit),
        hle_export!(_exit),
        hle_export!(_Exit),
        hle_export!(quick_exit),
        hle_export!(_set_app_type),
        hle_export!(_get_app_type),
        hle_export!(__p__commode),
        hle_export!(_set_fmode),
        hle_export!(_get_fmode),
        hle_export!(__p__fmode),
        hle_export!(_configure_narrow_argv),
        hle_export!(_configure_wide_argv),
        hle_export!(_initialize_narrow_environment),
        hle_export!(_initialize_wide_environment),
        hle_export!(_get_initial_narrow_environment),
        hle_export!(_get_initial_wide_environment),
        hle_export!(__p___wargv),
        hle_export!(__p__acmdln),
        hle_export!(__p__wcmdln),
        hle_export!(_get_narrow_winmain_command_line),
        hle_export!(_get_wide_winmain_command_line),
        hle_export!(__p__pgmptr),
        hle_export!(__p__wpgmptr),
        hle_export!(_get_pgmptr),
        hle_export!(_get_wpgmptr),
        hle_export!(_set_invalid_parameter_handler),
        hle_export!(_get_invalid_parameter_handler),
        hle_export!(_set_thread_local_invalid_parameter_handler),
        hle_export!(_get_thread_local_invalid_parameter_handler),
        hle_export!(_invalid_parameter),
        hle_export!(_invalid_parameter_noinfo),
        hle_export!(_set_purecall_handler),
        hle_export!(_get_purecall_handler),
        hle_export!(_purecall),
        hle_export!(_set_new_handler),
        hle_export!(_set_new_mode),
        hle_export!(_query_new_mode),
        hle_export!(_callnewh),
        hle_export!(_set_abort_behavior),
        hle_export!(_set_error_mode),
        hle_export!(_seh_filter_exe),
        hle_export!(_seh_filter_dll),
        hle_export!(__setusermatherr),
        hle_export!(signal),
        hle_export!(raise),
        hle_export!(_errno),
        hle_export!(_set_errno),
        hle_export!(_get_errno),
        hle_export!(__doserrno),
        hle_export!(_set_doserrno),
        hle_export!(_get_doserrno),
        hle_export!(strerror),
        hle_export!(strerror_s),
        hle_export!(_strerror),
        hle_export!(_wcserror),
        hle_export!(_wcserror_s),
        hle_export!(perror),
        hle_export!(_wperror),
        hle_export!(__sys_errlist),
        hle_export!(__sys_nerr),
        hle_export!(_controlfp_s),
        hle_export!(_controlfp),
        hle_export!(_control87),
        hle_export!(__control87_2),
        hle_export!(_clearfp),
        hle_export!(_statusfp),
        hle_export!(_statusfp2),
        hle_export!(_fpreset),
        hle_export!(__fpe_flt_rounds),
        hle_export!(_fpclass),
        hle_export!(_fpclassf),
        hle_export!(_beginthreadex),
        hle_export!(_beginthread),
        hle_export!(_endthreadex),
        hle_export!(_endthread),
        hle_export!(_getpid),
        hle_export!(__std_type_info_destroy_list),
        hle_export!(__std_type_info_name),
        hle_export!(__std_type_info_compare),
        hle_export!(__std_type_info_hash),
        hle_export!(__std_exception_copy),
        hle_export!(__std_exception_destroy),
        hle_export!(__std_terminate),
        hle_export!(_CxxThrowException),
        hle_export!(__CxxFrameHandler3),
        hle_export!(__CxxFrameHandler4),
        hle_export!(__current_exception),
        hle_export!(__current_exception_context),
        hle_export!(__processing_throw),
        hle_export!(_is_exception_typeof),
        hle_export!(_IsExceptionObjectToBeDestroyed),
        hle_export!(__RTtypeid),
        hle_export!(__RTCastToVoid),
        hle_export!(__RTDynamicCast),
        hle_export!(__vcrt_InitializeCriticalSectionEx),
        hle_export!(__vcrt_GetModuleFileNameW),
        hle_export!(__vcrt_GetModuleHandleW),
        hle_export!(__vcrt_LoadLibraryExW),
        hle_export!(__telemetry_main_invoke_trigger),
        hle_export!(__telemetry_main_return_trigger),
        hle_export!(_setjmp),
        HleExport {
            name: "_setjmpex",
            address: _setjmp as *const () as usize,
        },
        HleExport {
            name: "__intrinsic_setjmp",
            address: _setjmp as *const () as usize,
        },
        HleExport {
            name: "__intrinsic_setjmpex",
            address: _setjmp as *const () as usize,
        },
        // MSVC's `__try` handler, imported from vcruntime140.dll (ntdll's
        // copy is `win32::exceptions`'s).
        hle_export!(__C_specific_handler),
        hle_export!(longjmp),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    static ORDER: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    extern "win64" fn first() {
        lock(&ORDER).push(1);
    }

    extern "win64" fn second() {
        lock(&ORDER).push(2);
    }

    static TABLE: Mutex<usize> = Mutex::new(0);

    /// Registers [`first`] in the table under test while it is run.
    extern "win64" fn registers_another() {
        lock(&ORDER).push(3);
        let table = *lock(&TABLE) as *mut OnexitTable;
        _register_onexit_function(table, Some(first));
    }

    #[test]
    fn onexit_tables_run_in_reverse_and_take_late_comers() {
        let mut table = OnexitTable {
            first: std::ptr::dangling_mut(),
            last: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
        };
        assert_eq!(_initialize_onexit_table(&mut table), 0);
        assert!(table.first.is_null());
        *lock(&TABLE) = &raw mut table as usize;
        assert_eq!(_register_onexit_function(&mut table, Some(first)), 0);
        assert_eq!(
            _register_onexit_function(&mut table, Some(registers_another)),
            0
        );
        assert_eq!(_register_onexit_function(&mut table, Some(second)), 0);
        assert_eq!(_register_onexit_function(&mut table, None), -1);
        assert_eq!(_execute_onexit_table(&mut table), 0);
        assert_eq!(*lock(&ORDER), [2, 3, 1, 1]);
        assert!(table.first.is_null());
        assert_eq!(_initialize_onexit_table(std::ptr::null_mut()), -1);
    }

    static COUNT: AtomicI32 = AtomicI32::new(0);

    unsafe extern "win64" fn count() {
        COUNT.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "win64" fn fails() -> i32 {
        7
    }

    unsafe extern "win64" fn succeeds() -> i32 {
        COUNT.fetch_add(10, Ordering::Relaxed);
        0
    }

    #[test]
    fn initializer_tables_skip_nulls_and_stop_at_failure() {
        let table: [Pvfv; 3] = [Some(count), None, Some(count)];
        _initterm(table.as_ptr(), table.as_ptr().wrapping_add(3));
        _initterm(std::ptr::null(), std::ptr::null());
        assert_eq!(COUNT.load(Ordering::Relaxed), 2);
        let table: [Pifv; 3] = [Some(succeeds), Some(fails), Some(succeeds)];
        assert_eq!(
            _initterm_e(table.as_ptr(), table.as_ptr().wrapping_add(3)),
            7
        );
        assert_eq!(COUNT.load(Ordering::Relaxed), 12);
    }

    #[test]
    fn errno_is_per_thread_and_has_messages() {
        _set_errno(ERANGE);
        // SAFETY: this thread's errno.
        assert_eq!(unsafe { *_errno() }, ERANGE);
        std::thread::spawn(|| assert_eq!(errno(), 0))
            .join()
            .unwrap();
        let mut out = 0;
        assert_eq!(_get_errno(&mut out), 0);
        assert_eq!(out, ERANGE);
        // SAFETY: a per-thread NUL-terminated buffer.
        let text = unsafe { CStr::from_ptr(strerror(EINVAL)) };
        assert_eq!(text.to_str(), Ok("Invalid argument"));
        // SAFETY: as above.
        assert_eq!(
            unsafe { CStr::from_ptr(strerror(99)) }.to_str(),
            Ok("Unknown error")
        );
        let mut small = [1 as c_char; 8];
        assert_eq!(strerror_s(small.as_mut_ptr(), 8, ENOENT), 0);
        // SAFETY: NUL-terminated by `strerror_s`.
        assert_eq!(
            unsafe { CStr::from_ptr(small.as_ptr()) }.to_str(),
            Ok("No such")
        );
        let prefix = c"open";
        // SAFETY: as above.
        let text = unsafe { CStr::from_ptr(_strerror(prefix.as_ptr())) };
        assert_eq!(text.to_str(), Ok("open: Result too large\n"));
    }

    #[test]
    fn control_words_convert_both_ways() {
        // Process start: every exception masked, round to nearest.
        assert_eq!(control_from_mxcsr(MXCSR_DEFAULT), MCW_EM);
        assert_eq!(mxcsr_from_control(MCW_EM, 0), MXCSR_DEFAULT);
        // Toward zero is RC 3.
        let chop = mxcsr_from_control(MCW_EM | 0x300, 0);
        assert_eq!((chop >> 13) & 3, 3);
        assert_eq!(control_from_mxcsr(chop), MCW_EM | 0x300);
        // `_EM_INVALID` is IM.
        assert_eq!(mxcsr_from_control(EM_INVALID, 0), 1 << 7);
        // `_DN_FLUSH` sets FZ and DAZ.
        let flush = mxcsr_from_control(DN_FLUSH, 0);
        assert_eq!(flush, MXCSR_FZ | MXCSR_DAZ);
        assert_eq!(control_from_mxcsr(flush), DN_FLUSH);
        // The status flags are kept.
        assert_eq!(mxcsr_from_control(0, 0x3F) & 0x3F, 0x3F);
        for control in [
            0,
            MCW_EM,
            EM_ZERODIVIDE | 0x100,
            0x200 | DN_FLUSH,
            EM_DENORMAL,
        ] {
            assert_eq!(control_from_mxcsr(mxcsr_from_control(control, 0)), control);
        }
        assert_eq!(status_from_mxcsr(1 | 4), SW_INVALID | SW_ZERODIVIDE);
    }

    #[test]
    fn the_control_word_changes_and_is_restored() {
        let before = read_mxcsr();
        let mut current = 0;
        assert_eq!(_controlfp_s(&mut current, 0x300, MCW_RC), 0);
        assert_eq!(current & MCW_RC, 0x300);
        assert_eq!(__fpe_flt_rounds(), 0);
        assert_eq!(_controlfp_s(&mut current, 0, 0x4000_0000), EINVAL);
        write_mxcsr(before);
        assert_eq!(_fpclass(f64::NEG_INFINITY), FPCLASS_NINF);
        assert_eq!(_fpclass(f64::NAN), FPCLASS_QNAN);
        assert_eq!(
            _fpclass(f64::from_bits(0x7FF0_0000_0000_0001)),
            FPCLASS_SNAN
        );
        assert_eq!(_fpclass(-0.0), FPCLASS_NZ);
        assert_eq!(_fpclass(f64::MIN_POSITIVE / 2.0), FPCLASS_PD);
        assert_eq!(_fpclassf(1.0), FPCLASS_PN);
    }

    static SIGNALLED: AtomicI32 = AtomicI32::new(0);

    extern "win64" fn on_signal(sig: i32) {
        SIGNALLED.store(sig, Ordering::Relaxed);
    }

    #[test]
    fn signals_keep_handlers_and_raise_runs_them_once() {
        let handler = on_signal as *const () as usize;
        assert_eq!(signal(SIGTERM, handler), SIG_DFL);
        assert_eq!(raise(SIGTERM), 0);
        assert_eq!(SIGNALLED.load(Ordering::Relaxed), SIGTERM);
        // Reset to the default by `raise`.
        assert_eq!(signal(SIGTERM, SIG_IGN), SIG_DFL);
        assert_eq!(raise(SIGTERM), 0);
        assert_eq!(signal(SIGTERM, SIG_DFL), SIG_IGN);
        assert_eq!(signal(99, handler), SIG_ERR);
        assert_eq!(errno(), EINVAL);
        assert_eq!(raise(99), -1);
    }

    #[test]
    fn type_info_names_compare_and_hash() {
        #[repr(C)]
        struct Named {
            vtable: usize,
            undecorated: usize,
            name: [u8; 16],
        }
        let a = Named {
            vtable: 0,
            undecorated: 0,
            name: *b".?AVWidget@@\0\0\0\0",
        };
        let b = Named {
            vtable: 0,
            undecorated: 0,
            name: *b".?AVGadget@@\0\0\0\0",
        };
        let pa = (&raw const a).cast::<TypeInfo>();
        let pb = (&raw const b).cast::<TypeInfo>();
        // SAFETY: NUL-terminated within `a`.
        let name = unsafe { CStr::from_ptr(__std_type_info_name(pa, std::ptr::null_mut())) };
        assert_eq!(name.to_str(), Ok("?AVWidget@@"));
        assert!(__std_type_info_compare(pa, pb) > 0);
        assert_eq!(__std_type_info_compare(pa, pa), 0);
        assert_ne!(__std_type_info_hash(pa), __std_type_info_hash(pb));
    }

    #[test]
    fn exception_data_is_copied_and_destroyed() {
        let owned = heap::malloc(6).cast::<u8>();
        // SAFETY: 6 bytes of ours.
        unsafe { std::ptr::copy_nonoverlapping(c"oops!".as_ptr().cast(), owned, 6) };
        let from = ExceptionData {
            what: owned.cast(),
            do_free: 1,
        };
        let mut to = ExceptionData {
            what: std::ptr::null(),
            do_free: 0,
        };
        __std_exception_copy(&from, &mut to);
        assert_eq!(to.do_free, 1);
        assert_ne!(to.what, from.what);
        // SAFETY: a NUL-terminated copy.
        assert_eq!(unsafe { CStr::from_ptr(to.what) }.to_str(), Ok("oops!"));
        __std_exception_destroy(&mut to);
        assert!(to.what.is_null());
        let mut from = from;
        __std_exception_destroy(&mut from);
        let shared = ExceptionData {
            what: c"static".as_ptr(),
            do_free: 0,
        };
        __std_exception_copy(&shared, &mut to);
        assert_eq!((to.what, to.do_free), (shared.what, 0));
    }

    /// A fake image: a class `Derived` with a base `Base` at offset 16, the
    /// locators of both vtables, the hierarchy and the type descriptors.
    struct FakeImage {
        image: Vec<u64>,
    }

    impl FakeImage {
        const COL: usize = 0x00;
        const VTABLE: usize = 0x30;
        const HIERARCHY: usize = 0x40;
        const BASES: usize = 0x60;
        const DERIVED_BCD: usize = 0x70;
        const BASE_BCD: usize = 0x90;
        const DERIVED_TD: usize = 0x100;
        const BASE_TD: usize = 0x140;
        const SUB_COL: usize = 0x1A0;
        const SUB_VTABLE: usize = 0x1D0;

        fn new() -> FakeImage {
            let mut fake = FakeImage {
                image: vec![0; 0x200 / 8],
            };
            let base = fake.base();
            // The complete object's locator, before its vtable.
            fake.put32(Self::COL, &[1, 0, 0, Self::DERIVED_TD as u32]);
            fake.put32(Self::COL + 16, &[Self::HIERARCHY as u32, Self::COL as u32]);
            fake.put64(Self::VTABLE - 8, (base + Self::COL) as u64);
            // The base sub-object's locator: 16 bytes into the object.
            fake.put32(Self::SUB_COL, &[1, 16, 0, Self::DERIVED_TD as u32]);
            fake.put32(
                Self::SUB_COL + 16,
                &[Self::HIERARCHY as u32, Self::SUB_COL as u32],
            );
            fake.put64(Self::SUB_VTABLE - 8, (base + Self::SUB_COL) as u64);
            fake.put32(Self::HIERARCHY, &[0, 0, 2, Self::BASES as u32]);
            fake.put32(
                Self::BASES,
                &[Self::DERIVED_BCD as u32, Self::BASE_BCD as u32],
            );
            let none = u32::MAX; // pdisp -1: not a virtual base.
            fake.put32(
                Self::DERIVED_BCD,
                &[Self::DERIVED_TD as u32, 1, 0, none, 0, 0, 0],
            );
            fake.put32(
                Self::BASE_BCD,
                &[Self::BASE_TD as u32, 0, 16, none, 0, 0, 0],
            );
            fake.put_name(Self::DERIVED_TD + 16, b".?AVDerived@@\0");
            fake.put_name(Self::BASE_TD + 16, b".?AVBase@@\0");
            fake
        }

        fn base(&self) -> usize {
            self.image.as_ptr() as usize
        }

        fn bytes(&mut self) -> &mut [u8] {
            // SAFETY: the image's own words as bytes.
            unsafe {
                std::slice::from_raw_parts_mut(self.image.as_mut_ptr().cast(), self.image.len() * 8)
            }
        }

        fn put32(&mut self, at: usize, values: &[u32]) {
            for (i, v) in values.iter().enumerate() {
                self.bytes()[at + 4 * i..at + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
            }
        }

        fn put64(&mut self, at: usize, value: u64) {
            self.bytes()[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }

        fn put_name(&mut self, at: usize, name: &[u8]) {
            self.bytes()[at..at + name.len()].copy_from_slice(name);
        }
    }

    #[test]
    fn rtti_finds_types_complete_objects_and_bases() {
        let fake = FakeImage::new();
        let base = fake.base();
        // The object: the complete vtable, then the base's at offset 16.
        let mut object = [0usize; 4];
        object[0] = base + FakeImage::VTABLE;
        object[2] = base + FakeImage::SUB_VTABLE;
        let complete = object.as_mut_ptr().cast::<c_void>();
        let sub = object[2..].as_mut_ptr().cast::<c_void>();
        assert_eq!(__RTtypeid(complete) as usize, base + FakeImage::DERIVED_TD);
        assert_eq!(__RTCastToVoid(complete), complete);
        assert_eq!(__RTCastToVoid(sub), complete);
        assert!(__RTCastToVoid(std::ptr::null_mut()).is_null());
        // The target named as the image's `Base`, from another descriptor.
        let mut other = [0u8; 32];
        other[16..27].copy_from_slice(b".?AVBase@@\0");
        let target = other.as_ptr().cast::<c_void>();
        let source = (base + FakeImage::DERIVED_TD) as *const c_void;
        assert_eq!(__RTDynamicCast(complete, 0, source, target, 0), sub);
        assert_eq!(__RTDynamicCast(sub, 0, source, target, 0), sub);
        other[16..27].copy_from_slice(b".?AVNone@@\0");
        assert!(__RTDynamicCast(complete, 0, source, target, 0).is_null());
        assert!(__RTDynamicCast(std::ptr::null_mut(), 0, source, target, 0).is_null());
    }

    static mut JUMP_BUFFER: [u8; 256] = [0; 256];
    static mut SEEN: i32 = -1;

    /// Calls `_setjmp`, records what it returned, and on its first return
    /// jumps back with 7.
    #[unsafe(naked)]
    unsafe extern "win64" fn jump_twice() {
        core::arch::naked_asm!(
            "sub rsp, 40",
            "lea rcx, [rip + {buffer}]",
            "xor edx, edx",
            "call {setjmp}",
            "mov [rip + {seen}], eax",
            "test eax, eax",
            "jnz 2f",
            "lea rcx, [rip + {buffer}]",
            "mov edx, 7",
            "call {longjmp}",
            "2:",
            "add rsp, 40",
            "ret",
            buffer = sym JUMP_BUFFER,
            seen = sym SEEN,
            setjmp = sym _setjmp,
            longjmp = sym longjmp,
        )
    }

    #[test]
    fn longjmp_returns_from_setjmp_again() {
        // SAFETY: the test function keeps the stack balanced and returns
        // normally; the buffer and the result are statics only it uses.
        unsafe {
            jump_twice();
            assert_eq!((&raw const SEEN).read(), 7);
        }
    }

    #[test]
    fn environment_and_arguments_are_made() {
        let list = _get_initial_narrow_environment();
        // SAFETY: a null-terminated array of NUL-terminated strings.
        let first = unsafe { CStr::from_ptr(*list) };
        assert_eq!(first.to_str(), Ok(r"USERPROFILE=C:\Users\victory"));
        // SAFETY: as above.
        assert!(!unsafe { *__p___wargv() }.is_null());
        assert_eq!(command_line(&["a b".into(), "c".into()]), "\"a b\" c");
        // SAFETY: the pointer `__p__pgmptr` made.
        let path = unsafe { CStr::from_ptr(*__p__pgmptr()) };
        assert!(path.to_str().is_ok_and(|p| p.starts_with(r"C:\title\")));
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 114);
    }
}
