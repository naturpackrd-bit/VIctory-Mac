//! Threads a title creates, as objects a handle names and a wait ends with
//! (signaled when the thread ended); their ids, exit codes, descriptions,
//! affinities and times. A thread we start gets a TEB and tells the loaded
//! images (`DLL_THREAD_ATTACH`) before its start routine runs; at its end it
//! runs its FLS callbacks and tells them again (`DLL_THREAD_DETACH`).
//! `ExitThread` from title code returns from the start routine's call
//! without unwinding. TLS and FLS slots.
//!
//! Limits:
//! - `ExitThread` called from inside a callback our Rust code invoked (an
//!   `InitOnce` callback, an FLS callback, a TLS callback) skips that Rust
//!   frame, which must hold nothing that needs dropping.
//! - `SuspendThread` works only on a thread that has not begun running
//!   (one created suspended); a running thread cannot be stopped.
//!   `TerminateThread`, `Get`/`SetThreadContext` and `QueueUserAPC` fail.
//! - `TlsAlloc` zeroes the new slot in the calling thread only (Windows
//!   zeroes it in every thread), and `TlsFree` leaves the other threads'
//!   values as they are. A thread that uses an expansion slot (index 64 and
//!   up) gets an 8 KiB array of them, kept for the process's life.
//! - `FlsFree` runs the slot's callback for the calling thread's value only
//!   (Windows runs it for every thread's).
//! - A thread's priority is only kept: `SetThreadPriority` (`kernel.rs`)
//!   does not store it here, and no setting reaches the host's scheduler.
#![allow(non_snake_case, non_camel_case_types)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Once, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use victory_loader::teb::{Teb, attach_current_thread, current_teb};
use victory_loader::{DLL_THREAD_ATTACH, DLL_THREAD_DETACH};
use victory_platform::system::System;
use victory_platform::{Host, Thread};

use super::handles::{
    self, CURRENT_THREAD, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY,
    ERROR_SUCCESS, INVALID_HANDLE_VALUE, Object, set_last_error,
};
use super::sync::Waitable;
use super::time::FILETIME;
use crate::{HleExport, hle_export};

type HANDLE = *mut c_void;
type LPVOID = *mut c_void;
type BOOL = i32;
type DWORD = u32;
type SIZE_T = usize;
type HRESULT = i32;

/// Access denied: what `TerminateThread` answers.
const ERROR_ACCESS_DENIED: u32 = 5;
/// The request is not supported.
const ERROR_NOT_SUPPORTED: u32 = 50;
/// No free TLS or FLS index is left.
const ERROR_NO_MORE_ITEMS: u32 = 259;
/// `GetExitCodeThread` of a thread that has not ended.
const STILL_ACTIVE: u32 = 259;
/// `CreateThread`: the thread waits for `ResumeThread` before it runs.
const CREATE_SUSPENDED: u32 = 4;
/// `GetThreadPriority` of a bad handle.
const THREAD_PRIORITY_ERROR_RETURN: i32 = 0x7FFF_FFFF;
/// `HRESULT` for a bad argument.
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
/// `HRESULT` for an allocation that failed.
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
/// TLS slots in the TEB itself.
const TLS_MINIMUM_AVAILABLE: usize = 64;
/// TLS slots behind the TEB's `tls_expansion_slots`.
const TLS_EXPANSION_SLOTS: usize = 1024;
/// `TlsAlloc` with no free index.
const TLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
/// FLS slots a process has.
const FLS_MAXIMUM_AVAILABLE: usize = 128;
/// `FlsAlloc` with no free index.
const FLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
/// Stacks are reserved in these units, as Windows' allocation granularity.
const STACK_GRANULE: usize = 64 << 10;
/// 100-nanosecond intervals between 1601-01-01 and 1970-01-01 (as
/// `time.rs`).
const EPOCH_DIFFERENCE: u64 = 116_444_736_000_000_000;

/// A processor: its group and its number in the group.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PROCESSOR_NUMBER {
    group: u16,
    number: u8,
    reserved: u8,
}

/// An affinity mask within a processor group.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GROUP_AFFINITY {
    mask: usize,
    group: u16,
    reserved: [u16; 3],
}

/// A lock, even if a thread panicked holding it: what our locks guard is
/// consistent between our operations.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Logs `message` the first time it is reached.
fn warn_once(once: &Once, message: &str) {
    once.call_once(|| tracing::warn!("win32: {message}"));
}

/// A thread: ours when `CreateThread` made it, else the host's (the main
/// thread), known from its first call here.
pub(crate) struct ThreadObject {
    state: Mutex<State>,
    /// For the id handshake and the suspended gate.
    cv: Condvar,
    created: SystemTime,
    description: Mutex<Option<Vec<u16>>>,
    affinity: AtomicUsize,
    ideal: AtomicU32,
    priority: AtomicI32,
}

struct State {
    /// 0 until the thread published it.
    id: u32,
    suspend_count: u32,
    /// The start routine began.
    running: bool,
    ended: bool,
    exit_code: u32,
    exited_at: Option<SystemTime>,
}

impl ThreadObject {
    fn new(suspend_count: u32) -> ThreadObject {
        ThreadObject {
            state: Mutex::new(State {
                id: 0,
                suspend_count,
                running: false,
                ended: false,
                exit_code: 0,
                exited_at: None,
            }),
            cv: Condvar::new(),
            created: SystemTime::now(),
            description: Mutex::new(None),
            affinity: AtomicUsize::new(all_processors()),
            ideal: AtomicU32::new(0),
            priority: AtomicI32::new(0),
        }
    }
}

impl Object for ThreadObject {
    fn kind(&self) -> &'static str {
        "thread"
    }
    fn waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}

impl Waitable for ThreadObject {
    fn is_signaled(&self, _thread: u32) -> bool {
        lock(&self.state).ended
    }
    fn take(&self, _thread: u32) {}
}

/// Every processor's bit: `System::logical_processors` of them, 64 at most.
fn all_processors() -> usize {
    let count = Host::logical_processors().clamp(1, 64);
    if count == 64 {
        usize::MAX
    } else {
        (1 << count) - 1
    }
}

/// Threads by id (only live ones).
///
/// Lock order: a thread's `state`, then this; never the other way.
static THREADS: LazyLock<Mutex<HashMap<u32, Arc<ThreadObject>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// This thread's object, kept by the thread; when the thread's locals are
/// destroyed, it leaves [`THREADS`] if it is still there (a host thread that
/// never ran `ExitThread`).
struct Registered(Arc<ThreadObject>);

impl Drop for Registered {
    fn drop(&mut self) {
        let id = lock(&self.0.state).id;
        let mut threads = lock(&THREADS);
        if threads.get(&id).is_some_and(|t| Arc::ptr_eq(t, &self.0)) {
            threads.remove(&id);
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Registered>> = const { RefCell::new(None) };
    /// Where `ExitThread` returns to: the innermost `run_on_this_thread`.
    static EXIT_JUMP: Cell<*mut [u64; 8]> = const { Cell::new(std::ptr::null_mut()) };
    /// This thread's FLS values, by index.
    static FLS_VALUES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// This thread's object, made now if it was not created by `CreateThread`
/// (the main thread): registered under `Host::thread_id()`, `running` true,
/// suspend 0.
pub(crate) fn current() -> Arc<ThreadObject> {
    let known = CURRENT
        .try_with(|slot| slot.borrow().as_ref().map(|r| r.0.clone()))
        .ok()
        .flatten();
    if let Some(object) = known {
        return object;
    }
    let object = Arc::new(ThreadObject::new(0));
    let id = Host::thread_id();
    {
        let mut state = lock(&object.state);
        state.id = id;
        state.running = true;
    }
    lock(&THREADS).insert(id, object.clone());
    // While the thread's locals are being destroyed there is no slot: the
    // object then stays in the table, under an id the host may reuse.
    let _ = CURRENT.try_with(|slot| *slot.borrow_mut() = Some(Registered(object.clone())));
    object
}

/// What a thread start and end tell the loaded images.
static NOTIFIER: Mutex<Option<fn(u32)>> = Mutex::new(None);

/// What a thread start and end tell the loaded images: set by the runner
/// (`fn(reason: u32)` with `DLL_THREAD_ATTACH` / `DLL_THREAD_DETACH`). None
/// until set.
pub fn set_thread_notifier(f: fn(u32)) {
    *lock(&NOTIFIER) = Some(f);
}

fn notify(reason: u32) {
    let notifier = *lock(&NOTIFIER);
    if let Some(notifier) = notifier {
        notifier(reason);
    }
}

/// The stack size of a thread created with 0.
static DEFAULT_STACK_SIZE: AtomicUsize = AtomicUsize::new(1 << 20);

/// The stack size of a thread created with 0, from the title's image header
/// (`SizeOfStackReserve`); 1 MiB until set.
pub fn set_default_stack_size(bytes: usize) {
    DEFAULT_STACK_SIZE.store(bytes, Ordering::Relaxed);
}

/// Calls `start(parameter)` with the Win64 convention; saves the registers a
/// return needs in `jump` (rbx, rbp, r12..r15, rsp after this returns, this
/// call's return address), so that [`exit_to`] can return from here later.
///
/// # Safety
/// `start` is a function taking one pointer-sized argument and returning a
/// 32-bit value in the Win64 convention; `jump` is writable and outlives the
/// call.
#[unsafe(naked)]
unsafe extern "C" fn run_start(start: usize, parameter: usize, jump: *mut [u64; 8]) -> u32 {
    core::arch::naked_asm!(
        "mov [rdx], rbx",
        "mov [rdx + 8], rbp",
        "mov [rdx + 16], r12",
        "mov [rdx + 24], r13",
        "mov [rdx + 32], r14",
        "mov [rdx + 40], r15",
        // rsp once we have returned.
        "lea rax, [rsp + 8]",
        "mov [rdx + 48], rax",
        // Where we return to.
        "mov rax, [rsp]",
        "mov [rdx + 56], rax",
        // Win64: the parameter in rcx.
        "mov rcx, rsi",
        // Shadow space; rsp was 8 mod 16, is now 0 mod 16.
        "sub rsp, 40",
        "call rdi",
        "add rsp, 40",
        "ret",
    )
}

/// Returns from the [`run_start`] that filled `jump`, with `code` as its
/// result. Never returns here.
///
/// # Safety
/// `jump` was filled by a `run_start` that has not returned yet, on this
/// thread; the frames between it and here hold nothing that needs dropping.
#[unsafe(naked)]
unsafe extern "C" fn exit_to(jump: *const [u64; 8], code: u32) -> ! {
    core::arch::naked_asm!(
        "mov rbx, [rdi]",
        "mov rbp, [rdi + 8]",
        "mov r12, [rdi + 16]",
        "mov r13, [rdi + 24]",
        "mov r14, [rdi + 32]",
        "mov r15, [rdi + 40]",
        "mov rsp, [rdi + 48]",
        "mov eax, esi",
        "jmp qword ptr [rdi + 56]",
    )
}

/// Runs `start(parameter)` (`extern "win64" fn(*mut c_void) -> u32`) on the
/// calling thread as a title thread's start routine: `ExitThread` from
/// inside it returns here with the code. For the title's main thread (the
/// runner) and for tests.
pub fn run_on_this_thread(start: usize, parameter: usize) -> u32 {
    let mut jump = [0u64; 8];
    let previous = EXIT_JUMP.replace(&raw mut jump);
    // SAFETY: the caller's start routine, called as the Win64 convention
    // has it; `jump` lives on this frame until `run_start` returns, and
    // `ExitThread` only returns through it from frames deeper than this.
    let code = unsafe { run_start(start, parameter, &raw mut jump) };
    EXIT_JUMP.set(previous);
    code
}

/// What a thread we create is given.
struct Launch {
    object: Arc<ThreadObject>,
    start: usize,
    parameter: usize,
}

/// The start of a thread we create; `arg` is a leaked `Box<Launch>`.
extern "C" fn thread_main(arg: *mut c_void) -> *mut c_void {
    // SAFETY: `CreateThread` leaked this box for this thread alone.
    let launch = unsafe { Box::from_raw(arg.cast::<Launch>()) };
    let Launch {
        object,
        start,
        parameter,
    } = *launch;
    attach_current_thread();
    let _ = CURRENT.try_with(|slot| *slot.borrow_mut() = Some(Registered(object.clone())));
    {
        let mut state = lock(&object.state);
        state.id = Host::thread_id();
        lock(&THREADS).insert(state.id, object.clone());
        object.cv.notify_all();
    }
    notify(DLL_THREAD_ATTACH);
    {
        let mut state = lock(&object.state);
        while state.suspend_count > 0 {
            state = object
                .cv
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.running = true;
    }
    let code = run_on_this_thread(start, parameter);
    finish(code);
    std::ptr::null_mut()
}

/// A thread's end: its FLS callbacks, the images told, and its object
/// signaled with `code`.
fn finish(code: u32) {
    run_fls_callbacks();
    notify(DLL_THREAD_DETACH);
    let object = current();
    let id = lock(&object.state).id;
    {
        let mut threads = lock(&THREADS);
        if threads.get(&id).is_some_and(|t| Arc::ptr_eq(t, &object)) {
            threads.remove(&id);
        }
    }
    super::sync::signal(|| {
        let mut state = lock(&object.state);
        state.ended = true;
        state.exit_code = code;
        state.exited_at = Some(SystemTime::now());
    });
}

/// The thread object a handle names: the pseudo-handle is the caller's.
fn thread(handle: HANDLE) -> Option<Arc<ThreadObject>> {
    if handle as usize == CURRENT_THREAD {
        return Some(current());
    }
    let object: Arc<dyn Any + Send + Sync> = handles::get(handle as usize)?;
    object.downcast::<ThreadObject>().ok()
}

/// The thread object a handle names; else `ERROR_INVALID_HANDLE` is set.
fn thread_or_error(handle: HANDLE) -> Option<Arc<ThreadObject>> {
    let object = thread(handle);
    if object.is_none() {
        set_last_error(ERROR_INVALID_HANDLE);
    }
    object
}

/// A new thread running `start(parameter)`; with `CREATE_SUSPENDED` it waits
/// for `ResumeThread`. `STACK_SIZE_PARAM_IS_A_RESERVATION` is ignored: the
/// size is always the reservation.
pub extern "win64" fn CreateThread(
    _attributes: *mut c_void,
    stack_size: SIZE_T,
    start: *mut c_void,
    parameter: *mut c_void,
    flags: DWORD,
    thread_id: *mut DWORD,
) -> HANDLE {
    if start.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let reserve = match stack_size {
        0 => DEFAULT_STACK_SIZE.load(Ordering::Relaxed),
        n => n,
    };
    // The title's size, in whole granules, and one more for our own frames.
    let Some(stack) = reserve
        .checked_next_multiple_of(STACK_GRANULE)
        .and_then(|s| s.checked_add(STACK_GRANULE))
    else {
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    };
    let suspend = u32::from(flags & CREATE_SUSPENDED != 0);
    let object = Arc::new(ThreadObject::new(suspend));
    let launch = Box::into_raw(Box::new(Launch {
        object: object.clone(),
        start: start as usize,
        parameter: parameter as usize,
    }));
    if !Host::spawn_thread(stack, thread_main, launch.cast()) {
        // SAFETY: no thread took the box; it is ours again.
        drop(unsafe { Box::from_raw(launch) });
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    let id = {
        let mut state = lock(&object.state);
        while state.id == 0 {
            state = object
                .cv
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.id
    };
    if !thread_id.is_null() {
        // SAFETY: checked non-null; the title passes a place for the id.
        unsafe { thread_id.write_unaligned(id) };
    }
    handles::insert(object) as HANDLE
}

/// [`CreateThread`] in this process, the only one there is: `process` must
/// be `GetCurrentProcess()`.
pub extern "win64" fn CreateRemoteThread(
    process: HANDLE,
    attributes: *mut c_void,
    stack_size: SIZE_T,
    start: *mut c_void,
    parameter: *mut c_void,
    flags: DWORD,
    thread_id: *mut DWORD,
) -> HANDLE {
    if process as usize != INVALID_HANDLE_VALUE {
        set_last_error(ERROR_INVALID_HANDLE);
        return std::ptr::null_mut();
    }
    CreateThread(attributes, stack_size, start, parameter, flags, thread_id)
}

/// Ends the calling thread with `code`: from inside a start routine run by
/// [`run_on_this_thread`], that routine's call returns `code` (nothing of
/// the title's frames is unwound); on another thread, its end is recorded
/// and the host thread exits.
pub extern "win64" fn ExitThread(code: DWORD) -> ! {
    let jump = EXIT_JUMP
        .try_with(Cell::get)
        .unwrap_or(std::ptr::null_mut());
    if !jump.is_null() {
        // SAFETY: set by the innermost `run_on_this_thread` of this thread,
        // whose `run_start` has not returned; the frames above are the
        // title's and this one's, which hold nothing to drop.
        unsafe { exit_to(jump, code) }
    }
    finish(code);
    Host::exit_thread()
}

/// The thread's exit code; `STILL_ACTIVE` while it runs.
pub extern "win64" fn GetExitCodeThread(handle: HANDLE, out: *mut DWORD) -> BOOL {
    let Some(object) = thread_or_error(handle) else {
        return 0;
    };
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let state = lock(&object.state);
    let code = if state.ended {
        state.exit_code
    } else {
        STILL_ACTIVE
    };
    // SAFETY: checked non-null; the title passes a place for the code.
    unsafe { out.write_unaligned(code) };
    1
}

/// The thread's id; 0 for a bad handle.
pub extern "win64" fn GetThreadId(handle: HANDLE) -> DWORD {
    thread_or_error(handle).map_or(0, |object| lock(&object.state).id)
}

/// The id of the thread's process: this one.
pub extern "win64" fn GetProcessIdOfThread(handle: HANDLE) -> DWORD {
    thread_or_error(handle).map_or(0, |_| Host::process_id())
}

/// A new handle to the live thread with id `id`.
pub extern "win64" fn OpenThread(_access: DWORD, _inherit: BOOL, id: DWORD) -> HANDLE {
    if id == Host::thread_id() {
        // The calling thread may not be known yet.
        current();
    }
    let object = lock(&THREADS).get(&id).cloned();
    match object {
        Some(object) => handles::insert(object) as HANDLE,
        None => {
            set_last_error(ERROR_INVALID_PARAMETER);
            std::ptr::null_mut()
        }
    }
}

/// Adds one to the suspend count of a thread that has not begun running;
/// the previous count. A running thread cannot be suspended.
pub extern "win64" fn SuspendThread(handle: HANDLE) -> DWORD {
    let Some(object) = thread_or_error(handle) else {
        return u32::MAX;
    };
    let mut state = lock(&object.state);
    if state.running {
        static WARNED: Once = Once::new();
        warn_once(
            &WARNED,
            "SuspendThread of a running thread is not supported",
        );
        set_last_error(ERROR_NOT_SUPPORTED);
        return u32::MAX;
    }
    let previous = state.suspend_count;
    state.suspend_count = previous.saturating_add(1);
    previous
}

/// Takes one from the suspend count; the previous count. At 0 a thread
/// created suspended starts.
pub extern "win64" fn ResumeThread(handle: HANDLE) -> DWORD {
    let Some(object) = thread_or_error(handle) else {
        return u32::MAX;
    };
    let mut state = lock(&object.state);
    let previous = state.suspend_count;
    if previous > 0 {
        state.suspend_count -= 1;
        if state.suspend_count == 0 {
            object.cv.notify_all();
        }
    }
    previous
}

/// Refused: a thread cannot be stopped from outside here.
pub extern "win64" fn TerminateThread(handle: HANDLE, _code: DWORD) -> BOOL {
    if thread_or_error(handle).is_none() {
        return 0;
    }
    static WARNED: Once = Once::new();
    warn_once(&WARNED, "TerminateThread is not supported");
    set_last_error(ERROR_ACCESS_DENIED);
    0
}

/// Not supported: another thread's registers are not reachable here.
pub extern "win64" fn GetThreadContext(handle: HANDLE, _context: *mut c_void) -> BOOL {
    if thread_or_error(handle).is_none() {
        return 0;
    }
    static WARNED: Once = Once::new();
    warn_once(&WARNED, "GetThreadContext is not supported");
    set_last_error(ERROR_NOT_SUPPORTED);
    0
}

/// See [`GetThreadContext`].
pub extern "win64" fn SetThreadContext(handle: HANDLE, _context: *const c_void) -> BOOL {
    if thread_or_error(handle).is_none() {
        return 0;
    }
    static WARNED: Once = Once::new();
    warn_once(&WARNED, "SetThreadContext is not supported");
    set_last_error(ERROR_NOT_SUPPORTED);
    0
}

/// Not supported: there are no APCs.
pub extern "win64" fn QueueUserAPC(_routine: *mut c_void, handle: HANDLE, _data: usize) -> DWORD {
    if thread_or_error(handle).is_none() {
        return 0;
    }
    static WARNED: Once = Once::new();
    warn_once(&WARNED, "QueueUserAPC is not supported");
    set_last_error(ERROR_NOT_SUPPORTED);
    0
}

/// The thread's priority as kept here (`THREAD_PRIORITY_NORMAL`, 0, unless
/// set).
pub extern "win64" fn GetThreadPriority(handle: HANDLE) -> i32 {
    thread_or_error(handle).map_or(THREAD_PRIORITY_ERROR_RETURN, |object| {
        object.priority.load(Ordering::Relaxed)
    })
}

/// Accepted and ignored: the host decides boosts.
pub extern "win64" fn SetThreadPriorityBoost(handle: HANDLE, _disable: BOOL) -> BOOL {
    BOOL::from(thread_or_error(handle).is_some())
}

/// Boosts are never disabled: writes 0.
pub extern "win64" fn GetThreadPriorityBoost(handle: HANDLE, out: *mut BOOL) -> BOOL {
    if thread_or_error(handle).is_none() {
        return 0;
    }
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: checked non-null; the title passes a place for a BOOL.
    unsafe { out.write_unaligned(0) };
    1
}

/// Keeps the thread's affinity mask; the previous one (every processor's
/// bit until set). The host's scheduler is not told.
pub extern "win64" fn SetThreadAffinityMask(handle: HANDLE, mask: usize) -> usize {
    let Some(object) = thread_or_error(handle) else {
        return 0;
    };
    if mask == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    object.affinity.swap(mask, Ordering::Relaxed)
}

/// Keeps the thread's ideal processor; the previous one (0 until set).
pub extern "win64" fn SetThreadIdealProcessor(handle: HANDLE, number: DWORD) -> DWORD {
    let Some(object) = thread_or_error(handle) else {
        return u32::MAX;
    };
    object.ideal.swap(number, Ordering::Relaxed)
}

/// [`SetThreadIdealProcessor`] with group and number (group 0, the only
/// one); the previous one is written if `previous` is not null.
pub extern "win64" fn SetThreadIdealProcessorEx(
    handle: HANDLE,
    ideal: *const PROCESSOR_NUMBER,
    previous: *mut PROCESSOR_NUMBER,
) -> BOOL {
    let Some(object) = thread_or_error(handle) else {
        return 0;
    };
    if ideal.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: checked non-null; the title passes one processor number.
    let ideal = unsafe { ideal.read_unaligned() };
    let old = object
        .ideal
        .swap(u32::from(ideal.number), Ordering::Relaxed);
    if !previous.is_null() {
        let old = PROCESSOR_NUMBER {
            number: old as u8,
            ..PROCESSOR_NUMBER::default()
        };
        // SAFETY: checked non-null; the title passes a place for one.
        unsafe { previous.write_unaligned(old) };
    }
    1
}

/// The thread's ideal processor, in group 0.
pub extern "win64" fn GetThreadIdealProcessorEx(
    handle: HANDLE,
    out: *mut PROCESSOR_NUMBER,
) -> BOOL {
    let Some(object) = thread_or_error(handle) else {
        return 0;
    };
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let number = PROCESSOR_NUMBER {
        number: object.ideal.load(Ordering::Relaxed) as u8,
        ..PROCESSOR_NUMBER::default()
    };
    // SAFETY: checked non-null; the title passes a place for one.
    unsafe { out.write_unaligned(number) };
    1
}

/// [`SetThreadAffinityMask`] within group 0, the only one; the previous
/// affinity is written if `previous` is not null.
pub extern "win64" fn SetThreadGroupAffinity(
    handle: HANDLE,
    affinity: *const GROUP_AFFINITY,
    previous: *mut GROUP_AFFINITY,
) -> BOOL {
    let Some(object) = thread_or_error(handle) else {
        return 0;
    };
    if affinity.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: checked non-null; the title passes one group affinity.
    let affinity = unsafe { affinity.read_unaligned() };
    if affinity.mask == 0 || affinity.group != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let old = object.affinity.swap(affinity.mask, Ordering::Relaxed);
    if !previous.is_null() {
        let old = GROUP_AFFINITY {
            mask: old,
            ..GROUP_AFFINITY::default()
        };
        // SAFETY: checked non-null; the title passes a place for one.
        unsafe { previous.write_unaligned(old) };
    }
    1
}

/// The thread's affinity, in group 0.
pub extern "win64" fn GetThreadGroupAffinity(handle: HANDLE, out: *mut GROUP_AFFINITY) -> BOOL {
    let Some(object) = thread_or_error(handle) else {
        return 0;
    };
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let affinity = GROUP_AFFINITY {
        mask: object.affinity.load(Ordering::Relaxed),
        ..GROUP_AFFINITY::default()
    };
    // SAFETY: checked non-null; the title passes a place for one.
    unsafe { out.write_unaligned(affinity) };
    1
}

/// The length of a NUL-terminated UTF-16 string.
///
/// # Safety
/// `text` is a NUL-terminated string.
unsafe fn wide_len(text: *const u16) -> usize {
    // SAFETY: the caller's string, read up to its NUL.
    (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count()
}

/// Keeps the thread's description; the calling thread's also names the host
/// thread.
pub extern "win64" fn SetThreadDescription(handle: HANDLE, description: *const u16) -> HRESULT {
    let Some(object) = thread(handle) else {
        return E_INVALIDARG;
    };
    if description.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes a NUL-terminated string.
    let text = unsafe {
        let len = wide_len(description);
        std::slice::from_raw_parts(description, len).to_vec()
    };
    if Arc::ptr_eq(&object, &current()) {
        Host::name_current_thread(&String::from_utf16_lossy(&text));
    }
    *lock(&object.description) = Some(text);
    0
}

/// A `LocalAlloc`'d, NUL-terminated copy of the thread's description (empty
/// when it has none), which the title frees with `LocalFree`.
pub extern "win64" fn GetThreadDescription(handle: HANDLE, out: *mut *mut u16) -> HRESULT {
    let Some(object) = thread(handle) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let text = lock(&object.description).clone().unwrap_or_default();
    let copy = super::memory::LocalAlloc(0, (text.len() + 1) * 2).cast::<u16>();
    if copy.is_null() {
        return E_OUTOFMEMORY;
    }
    // SAFETY: a fresh block of `text.len() + 1` UTF-16 units; `out` is
    // checked non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), copy, text.len());
        copy.add(text.len()).write(0);
        out.write_unaligned(copy);
    }
    0
}

/// A time as a file time: 100-nanosecond intervals since 1601-01-01.
fn file_time(time: SystemTime) -> FILETIME {
    // A host clock before 1970 reads as 1970.
    let since_1970 = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let value = EPOCH_DIFFERENCE + (since_1970.as_nanos() / 100) as u64;
    FILETIME {
        low: value as u32,
        high: (value >> 32) as u32,
    }
}

/// The thread's creation and exit times (exit 0 while it runs); its kernel
/// and user times are not measured: 0. A null pointer is skipped.
pub extern "win64" fn GetThreadTimes(
    handle: HANDLE,
    creation: *mut FILETIME,
    exit: *mut FILETIME,
    kernel: *mut FILETIME,
    user: *mut FILETIME,
) -> BOOL {
    let Some(object) = thread_or_error(handle) else {
        return 0;
    };
    let exited_at = lock(&object.state).exited_at;
    let times = [
        (creation, file_time(object.created)),
        (exit, exited_at.map(file_time).unwrap_or_default()),
        (kernel, FILETIME::default()),
        (user, FILETIME::default()),
    ];
    for (out, time) in times {
        if !out.is_null() {
            // SAFETY: checked non-null; the title passes a place for one.
            unsafe { out.write_unaligned(time) };
        }
    }
    1
}

/// Stack overflows are not handled per thread here: writes 0, the previous
/// guarantee.
pub extern "win64" fn SetThreadStackGuarantee(size: *mut DWORD) -> BOOL {
    if size.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: checked non-null; the title passes its size in and out.
    unsafe { size.write_unaligned(0) };
    1
}

/// Accepted and ignored (memory and power hints).
pub extern "win64" fn SetThreadInformation(
    handle: HANDLE,
    _class: i32,
    _info: *mut c_void,
    _size: DWORD,
) -> BOOL {
    BOOL::from(thread_or_error(handle).is_some())
}

/// No class is answered.
pub extern "win64" fn GetThreadInformation(
    handle: HANDLE,
    _class: i32,
    _info: *mut c_void,
    _size: DWORD,
) -> BOOL {
    if thread_or_error(handle).is_some() {
        set_last_error(ERROR_INVALID_PARAMETER);
    }
    0
}

/// There are no fibers.
pub extern "win64" fn IsThreadAFiber() -> BOOL {
    0
}

/// Which TLS indices are taken.
static TLS_USED: Mutex<[bool; TLS_MINIMUM_AVAILABLE + TLS_EXPANSION_SLOTS]> =
    Mutex::new([false; TLS_MINIMUM_AVAILABLE + TLS_EXPANSION_SLOTS]);

/// The calling thread's TEB, which it is given now if it has none.
fn teb() -> *mut Teb {
    current_teb().unwrap_or_else(attach_current_thread) as *mut Teb
}

/// Whether `index` is a TLS index `TlsAlloc` gave and `TlsFree` did not take
/// back; else `ERROR_INVALID_PARAMETER` is set.
fn tls_allocated(index: DWORD) -> bool {
    let taken = lock(&TLS_USED).get(index as usize).copied() == Some(true);
    if !taken {
        set_last_error(ERROR_INVALID_PARAMETER);
    }
    taken
}

/// The calling thread's TLS slot `index` (a valid one): in the TEB below 64,
/// in its expansion array above, which is made now if `make` (else null).
fn tls_slot(index: usize, make: bool) -> *mut u64 {
    let teb = teb();
    if index < TLS_MINIMUM_AVAILABLE {
        // SAFETY: this thread's own TEB, which lives as long as the thread.
        return unsafe { &raw mut (*teb).tls_slots[index] };
    }
    // SAFETY: as above.
    let mut array = unsafe { (*teb).tls_expansion_slots } as *mut u64;
    if array.is_null() {
        if !make {
            return std::ptr::null_mut();
        }
        // Kept for the process's life: 8 KiB a thread that uses one.
        let made: &mut [u64; TLS_EXPANSION_SLOTS] = Box::leak(Box::new([0; TLS_EXPANSION_SLOTS]));
        array = made.as_mut_ptr();
        // SAFETY: as above.
        unsafe { (*teb).tls_expansion_slots = array as u64 };
    }
    // SAFETY: the array holds `TLS_EXPANSION_SLOTS` values, and `index` is
    // below 64 + that.
    unsafe { array.add(index - TLS_MINIMUM_AVAILABLE) }
}

/// A free TLS index, its slot zeroed in the calling thread.
pub extern "win64" fn TlsAlloc() -> DWORD {
    let index = {
        let mut used = lock(&TLS_USED);
        let Some(index) = used.iter().position(|&u| !u) else {
            drop(used);
            set_last_error(ERROR_NO_MORE_ITEMS);
            return TLS_OUT_OF_INDEXES;
        };
        used[index] = true;
        index
    };
    let slot = tls_slot(index, false);
    if !slot.is_null() {
        // SAFETY: this thread's own slot.
        unsafe { slot.write(0) };
    }
    index as DWORD
}

/// Takes the index back; the threads' values in it stay.
pub extern "win64" fn TlsFree(index: DWORD) -> BOOL {
    if !tls_allocated(index) {
        return 0;
    }
    lock(&TLS_USED)[index as usize] = false;
    1
}

/// The calling thread's value in slot `index`; the last error is
/// `ERROR_SUCCESS` then, as on Windows (code tells a 0 value by it).
pub extern "win64" fn TlsGetValue(index: DWORD) -> LPVOID {
    if !tls_allocated(index) {
        return std::ptr::null_mut();
    }
    let slot = tls_slot(index as usize, false);
    // SAFETY: this thread's own slot, if its array exists.
    let value = if slot.is_null() {
        0
    } else {
        unsafe { slot.read() }
    };
    set_last_error(ERROR_SUCCESS);
    value as LPVOID
}

/// Sets the calling thread's value in slot `index`.
pub extern "win64" fn TlsSetValue(index: DWORD, value: LPVOID) -> BOOL {
    if !tls_allocated(index) {
        return 0;
    }
    let slot = tls_slot(index as usize, true);
    // SAFETY: this thread's own slot.
    unsafe { slot.write(value as u64) };
    1
}

/// What an FLS slot's value is given at the thread's end or `FlsFree`.
type Callback = extern "win64" fn(*mut c_void);

/// FLS slots: `None` is free; a taken one has its callback, if any.
static FLS: Mutex<Vec<Option<Option<Callback>>>> = Mutex::new(Vec::new());

/// The callback of FLS slot `index`, if it is taken and has one.
fn fls_callback(index: usize) -> Option<Callback> {
    lock(&FLS).get(index).copied().flatten().flatten()
}

/// Whether `index` is a taken FLS slot; else `ERROR_INVALID_PARAMETER` is
/// set.
fn fls_allocated(index: DWORD) -> bool {
    let taken = matches!(lock(&FLS).get(index as usize), Some(Some(_)));
    if !taken {
        set_last_error(ERROR_INVALID_PARAMETER);
    }
    taken
}

/// The calling thread's value in FLS slot `index`, or 0.
fn fls_value(index: usize) -> usize {
    FLS_VALUES
        .try_with(|values| values.borrow().get(index).copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Sets the calling thread's value in FLS slot `index`.
fn set_fls_value(index: usize, value: usize) {
    let _ = FLS_VALUES.try_with(|values| {
        let mut values = values.borrow_mut();
        if values.len() <= index {
            values.resize(index + 1, 0);
        }
        values[index] = value;
    });
}

/// The calling thread's FLS callbacks, for its non-null values in slot
/// order; then its values are cleared.
fn run_fls_callbacks() {
    let values = FLS_VALUES
        .try_with(|values| values.borrow().clone())
        .unwrap_or_default();
    for (index, &value) in values.iter().enumerate() {
        if value == 0 {
            continue;
        }
        if let Some(callback) = fls_callback(index) {
            callback(value as *mut c_void);
        }
    }
    let _ = FLS_VALUES.try_with(|values| values.borrow_mut().clear());
}

/// A free FLS index, whose values are given to `callback` at each thread's
/// end; its value in the calling thread is zeroed.
pub extern "win64" fn FlsAlloc(callback: Option<Callback>) -> DWORD {
    let index = {
        let mut slots = lock(&FLS);
        match slots.iter().position(Option::is_none) {
            Some(index) => {
                slots[index] = Some(callback);
                index
            }
            None if slots.len() < FLS_MAXIMUM_AVAILABLE => {
                slots.push(Some(callback));
                slots.len() - 1
            }
            None => {
                drop(slots);
                set_last_error(ERROR_NO_MORE_ITEMS);
                return FLS_OUT_OF_INDEXES;
            }
        }
    };
    set_fls_value(index, 0);
    index as DWORD
}

/// Takes the index back, first giving the calling thread's value to the
/// slot's callback.
pub extern "win64" fn FlsFree(index: DWORD) -> BOOL {
    if !fls_allocated(index) {
        return 0;
    }
    let value = fls_value(index as usize);
    if value != 0 {
        if let Some(callback) = fls_callback(index as usize) {
            callback(value as *mut c_void);
        }
        set_fls_value(index as usize, 0);
    }
    if let Some(slot) = lock(&FLS).get_mut(index as usize) {
        *slot = None;
    }
    1
}

/// The calling thread's value in FLS slot `index`.
pub extern "win64" fn FlsGetValue(index: DWORD) -> LPVOID {
    if !fls_allocated(index) {
        return std::ptr::null_mut();
    }
    set_last_error(ERROR_SUCCESS);
    fls_value(index as usize) as LPVOID
}

/// Sets the calling thread's value in FLS slot `index`.
pub extern "win64" fn FlsSetValue(index: DWORD, value: LPVOID) -> BOOL {
    if !fls_allocated(index) {
        return 0;
    }
    set_fls_value(index as usize, value as usize);
    1
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(CreateThread),
        hle_export!(CreateRemoteThread),
        hle_export!(ExitThread),
        hle_export!(GetExitCodeThread),
        hle_export!(GetThreadId),
        hle_export!(GetProcessIdOfThread),
        hle_export!(OpenThread),
        hle_export!(SuspendThread),
        hle_export!(ResumeThread),
        hle_export!(TerminateThread),
        hle_export!(GetThreadContext),
        hle_export!(SetThreadContext),
        hle_export!(QueueUserAPC),
        hle_export!(GetThreadPriority),
        hle_export!(SetThreadPriorityBoost),
        hle_export!(GetThreadPriorityBoost),
        hle_export!(SetThreadAffinityMask),
        hle_export!(SetThreadIdealProcessor),
        hle_export!(SetThreadIdealProcessorEx),
        hle_export!(GetThreadIdealProcessorEx),
        hle_export!(SetThreadGroupAffinity),
        hle_export!(GetThreadGroupAffinity),
        hle_export!(SetThreadDescription),
        hle_export!(GetThreadDescription),
        hle_export!(GetThreadTimes),
        hle_export!(SetThreadStackGuarantee),
        hle_export!(SetThreadInformation),
        hle_export!(GetThreadInformation),
        hle_export!(IsThreadAFiber),
        hle_export!(TlsAlloc),
        hle_export!(TlsFree),
        hle_export!(TlsGetValue),
        hle_export!(TlsSetValue),
        hle_export!(FlsAlloc),
        hle_export!(FlsFree),
        hle_export!(FlsGetValue),
        hle_export!(FlsSetValue),
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::modules::win32::handles::{
        CloseHandle, DuplicateHandle, GetCurrentProcess, GetCurrentThread, GetCurrentThreadId,
        GetLastError, SetLastError,
    };
    use crate::modules::win32::memory::LocalFree;
    use crate::modules::win32::sync::{WAIT_OBJECT_0, WaitForSingleObject};

    /// A start routine as `CreateThread` takes it.
    type Start = extern "win64" fn(*mut c_void) -> u32;

    fn create(start: Start, parameter: usize, flags: u32, id: &mut u32) -> HANDLE {
        CreateThread(
            std::ptr::null_mut(),
            0,
            start as *mut c_void,
            parameter as *mut c_void,
            flags,
            id,
        )
    }

    fn exit_code(handle: HANDLE) -> u32 {
        let mut code = 0;
        assert_eq!(GetExitCodeThread(handle, &mut code), 1);
        code
    }

    fn wait_and_close(handle: HANDLE) {
        assert_eq!(WaitForSingleObject(handle, 5000), WAIT_OBJECT_0);
        assert_eq!(CloseHandle(handle), 1);
    }

    static HAD_TEB: AtomicBool = AtomicBool::new(false);
    static IDS_AGREE: AtomicBool = AtomicBool::new(false);

    extern "win64" fn add_one(parameter: *mut c_void) -> u32 {
        HAD_TEB.store(current_teb().is_some(), Ordering::SeqCst);
        IDS_AGREE.store(
            GetThreadId(GetCurrentThread()) == GetCurrentThreadId(),
            Ordering::SeqCst,
        );
        parameter as u32 + 1
    }

    #[test]
    fn a_thread_runs_returns_and_is_waited_for() {
        attach_current_thread();
        let mut id = 0;
        let handle = create(add_one, 6, 0, &mut id);
        assert!(!handle.is_null());
        assert_ne!(id, 0);
        assert_eq!(WaitForSingleObject(handle, 5000), WAIT_OBJECT_0);
        assert_eq!(exit_code(handle), 7);
        assert_eq!(GetThreadId(handle), id);
        assert!(HAD_TEB.load(Ordering::SeqCst));
        assert!(IDS_AGREE.load(Ordering::SeqCst));
        assert_eq!(CloseHandle(handle), 1);

        assert!(create_null_start().is_null());
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
    }

    fn create_null_start() -> HANDLE {
        let mut id = 0;
        CreateThread(
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut id,
        )
    }

    static SUSPENDED_RAN: AtomicU32 = AtomicU32::new(0);

    extern "win64" fn mark(_parameter: *mut c_void) -> u32 {
        SUSPENDED_RAN.store(1, Ordering::SeqCst);
        0
    }

    #[test]
    fn a_suspended_thread_waits_for_resume() {
        attach_current_thread();
        let mut id = 0;
        let handle = create(mark, 0, CREATE_SUSPENDED, &mut id);
        assert!(!handle.is_null());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(SUSPENDED_RAN.load(Ordering::SeqCst), 0);
        assert_eq!(exit_code(handle), STILL_ACTIVE);
        assert_eq!(SuspendThread(handle), 1);
        assert_eq!(ResumeThread(handle), 2);
        assert_eq!(ResumeThread(handle), 1);
        assert_eq!(WaitForSingleObject(handle, 5000), WAIT_OBJECT_0);
        assert_eq!(SUSPENDED_RAN.load(Ordering::SeqCst), 1);
        assert_eq!(ResumeThread(handle), 0);
        assert_eq!(CloseHandle(handle), 1);
    }

    extern "win64" fn exit_nine(_parameter: *mut c_void) -> u32 {
        ExitThread(9)
    }

    static EXIT_FLS_SEEN: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    extern "win64" fn record_exit(value: *mut c_void) {
        lock(&EXIT_FLS_SEEN).push(value as usize);
    }

    extern "win64" fn exit_eleven(index: *mut c_void) -> u32 {
        FlsSetValue(index as u32, 0x44 as *mut c_void);
        ExitThread(11)
    }

    #[test]
    fn exit_thread_returns_from_the_start_routine() {
        attach_current_thread();
        assert_eq!(run_on_this_thread(exit_nine as *const () as usize, 0), 9);
        assert_eq!(run_on_this_thread(add_one as *const () as usize, 1), 2);

        let index = FlsAlloc(Some(record_exit));
        assert_ne!(index, FLS_OUT_OF_INDEXES);
        let mut id = 0;
        let handle = create(exit_eleven, index as usize, 0, &mut id);
        assert_eq!(WaitForSingleObject(handle, 5000), WAIT_OBJECT_0);
        assert_eq!(exit_code(handle), 11);
        assert_eq!(*lock(&EXIT_FLS_SEEN), [0x44]);
        assert_eq!(CloseHandle(handle), 1);
        assert_eq!(FlsFree(index), 1);
    }

    static THREAD_READ: AtomicUsize = AtomicUsize::new(0);

    extern "win64" fn tls_worker(index: *mut c_void) -> u32 {
        let index = index as u32;
        TlsSetValue(index, 0x22 as *mut c_void);
        THREAD_READ.store(TlsGetValue(index) as usize, Ordering::SeqCst);
        0
    }

    #[test]
    fn tls_slots_are_each_threads_own() {
        attach_current_thread();
        let index = TlsAlloc();
        assert_ne!(index, TLS_OUT_OF_INDEXES);
        assert_eq!(TlsSetValue(index, 0x11 as *mut c_void), 1);
        let mut id = 0;
        wait_and_close(create(tls_worker, index as usize, 0, &mut id));
        assert_eq!(THREAD_READ.load(Ordering::SeqCst), 0x22);
        assert_eq!(TlsGetValue(index) as usize, 0x11);
        SetLastError(87);
        TlsGetValue(index);
        assert_eq!(GetLastError(), ERROR_SUCCESS);

        let mut taken = vec![index];
        for _ in 0..70 {
            let more = TlsAlloc();
            assert_ne!(more, TLS_OUT_OF_INDEXES);
            taken.push(more);
        }
        let &high = taken.iter().find(|&&i| i >= 64).expect("an expansion slot");
        assert_eq!(TlsGetValue(high) as usize, 0);
        assert_eq!(TlsSetValue(high, 0x55 as *mut c_void), 1);
        assert_eq!(TlsGetValue(high) as usize, 0x55);
        wait_and_close(create(tls_worker, high as usize, 0, &mut id));
        assert_eq!(THREAD_READ.load(Ordering::SeqCst), 0x22);
        assert_eq!(TlsGetValue(high) as usize, 0x55);

        assert!(TlsGetValue(2000).is_null());
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        for &i in &taken {
            assert_eq!(TlsFree(i), 1);
        }
        assert_eq!(TlsFree(index), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
    }

    static FLS_SEEN: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    extern "win64" fn record(value: *mut c_void) {
        lock(&FLS_SEEN).push(value as usize);
    }

    extern "win64" fn fls_worker(index: *mut c_void) -> u32 {
        FlsSetValue(index as u32, 0x33 as *mut c_void);
        0
    }

    #[test]
    fn fls_callbacks_run_when_the_thread_ends() {
        attach_current_thread();
        let index = FlsAlloc(Some(record));
        assert_ne!(index, FLS_OUT_OF_INDEXES);
        let mut id = 0;
        wait_and_close(create(fls_worker, index as usize, 0, &mut id));
        assert_eq!(*lock(&FLS_SEEN), [0x33]);
        assert_eq!(FlsSetValue(index, 0x66 as *mut c_void), 1);
        assert_eq!(FlsGetValue(index) as usize, 0x66);
        assert_eq!(FlsFree(index), 1);
        assert_eq!(*lock(&FLS_SEEN), [0x33, 0x66]);
        assert_eq!(FlsFree(index), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
    }

    static ATTACHED: AtomicUsize = AtomicUsize::new(0);
    static DETACHED: AtomicUsize = AtomicUsize::new(0);

    fn count(reason: u32) {
        match reason {
            DLL_THREAD_ATTACH => ATTACHED.fetch_add(1, Ordering::SeqCst),
            DLL_THREAD_DETACH => DETACHED.fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
    }

    #[test]
    fn the_notifier_sees_attach_and_detach() {
        attach_current_thread();
        set_thread_notifier(count);
        let (attached, detached) = (
            ATTACHED.load(Ordering::SeqCst),
            DETACHED.load(Ordering::SeqCst),
        );
        let mut id = 0;
        wait_and_close(create(add_one, 0, 0, &mut id));
        assert!(ATTACHED.load(Ordering::SeqCst) > attached);
        assert!(DETACHED.load(Ordering::SeqCst) > detached);
    }

    /// 64 KiB of stack a level: the array, which `black_box` keeps.
    fn deep(levels: u32) -> u32 {
        let mut frame = [0u8; 65536];
        core::hint::black_box(&mut frame);
        if levels == 0 {
            return u32::from(frame[0]);
        }
        deep(levels - 1) + u32::from(frame[1])
    }

    static STACK_SEEN: AtomicUsize = AtomicUsize::new(0);

    extern "win64" fn recurse(_parameter: *mut c_void) -> u32 {
        let (low, high) = Host::stack_bounds();
        STACK_SEEN.store(high - low, Ordering::SeqCst);
        deep(100) + 1
    }

    #[test]
    fn a_big_stack_is_honoured() {
        attach_current_thread();
        let mut id = 0;
        let handle = CreateThread(
            std::ptr::null_mut(),
            16 << 20,
            recurse as *mut c_void,
            std::ptr::null_mut(),
            0,
            &mut id,
        );
        assert!(!handle.is_null());
        assert_eq!(WaitForSingleObject(handle, 5000), WAIT_OBJECT_0);
        assert_eq!(exit_code(handle), 1);
        assert!(STACK_SEEN.load(Ordering::SeqCst) >= 16 << 20);
        assert_eq!(CloseHandle(handle), 1);
    }

    #[test]
    fn descriptions_affinity_and_times_are_kept() {
        attach_current_thread();
        let me = GetCurrentThread();
        let name: Vec<u16> = "worker\0".encode_utf16().collect();
        assert_eq!(SetThreadDescription(me, name.as_ptr()), 0);
        let mut out: *mut u16 = std::ptr::null_mut();
        assert_eq!(GetThreadDescription(me, &mut out), 0);
        assert!(!out.is_null());
        // SAFETY: the NUL-terminated copy just made.
        let got = unsafe { std::slice::from_raw_parts(out, wide_len(out)) };
        assert_eq!(String::from_utf16_lossy(got), "worker");
        assert!(LocalFree(out.cast()).is_null());
        assert_eq!(
            SetThreadDescription(0x7FFF_FFF0 as HANDLE, name.as_ptr()),
            E_INVALIDARG
        );

        let previous = SetThreadAffinityMask(me, 1);
        assert_eq!(
            previous.count_ones() as usize,
            Host::logical_processors().min(64)
        );
        assert_eq!(SetThreadAffinityMask(me, previous), 1);
        assert_eq!(GetThreadPriority(me), 0);
        assert_eq!(
            GetThreadPriority(0x7FFF_FFF0 as HANDLE),
            THREAD_PRIORITY_ERROR_RETURN
        );

        let (mut creation, mut exit) = (FILETIME::default(), FILETIME::default());
        let (mut kernel, mut user) = (FILETIME::default(), FILETIME::default());
        assert_eq!(
            GetThreadTimes(me, &mut creation, &mut exit, &mut kernel, &mut user),
            1
        );
        let value = |t: FILETIME| u64::from(t.high) << 32 | u64::from(t.low);
        let now = file_time(SystemTime::now());
        assert!(value(creation) != 0 && value(creation) <= value(now));
        assert_eq!(value(exit), 0);

        let opened = OpenThread(0, 0, GetCurrentThreadId());
        assert!(!opened.is_null());
        assert_eq!(GetThreadId(opened), GetCurrentThreadId());
        assert_eq!(CloseHandle(opened), 1);
        assert!(OpenThread(0, 0, 0).is_null());
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

        let process = GetCurrentProcess();
        let mut duplicate: HANDLE = std::ptr::null_mut();
        assert_eq!(
            DuplicateHandle(process, me, process, &mut duplicate, 0, 0, 0),
            1
        );
        assert_eq!(GetThreadId(duplicate), GetCurrentThreadId());
        assert_eq!(CloseHandle(duplicate), 1);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 37);
    }
}
