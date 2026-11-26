//! Events, semaphores, mutexes and the waits on them, with one dispatcher
//! lock and condition variable for all of them (every signal wakes every
//! waiter, who checks again: simple and right, to be made finer when it
//! costs); critical sections, SRW locks, condition variables,
//! `WaitOnAddress`, `InitOnce` and `SList` on the title's own words, with
//! waiters parked by address; `Sleep`.
//!
//! Not yet: APCs (an alertable wait is an ordinary one), abandoned mutexes
//! (no thread's end is seen yet, so `WAIT_ABANDONED_0` is never returned).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::any::Any;
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use victory_platform::{Host, Thread};

use super::handles::{
    self, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE,
    ERROR_INVALID_PARAMETER, ERROR_SUCCESS, ERROR_TIMEOUT, Object, set_last_error,
};
use crate::{HleExport, hle_export};

/// A wait without a time limit.
pub(crate) const INFINITE: u32 = 0xFFFF_FFFF;
/// A wait succeeded on the object at index 0 (`+ n` for index n).
pub(crate) const WAIT_OBJECT_0: u32 = 0;
/// A wait got a mutex whose owner ended holding it; never yet (see above).
#[allow(dead_code)]
pub(crate) const WAIT_ABANDONED_0: u32 = 0x80;
/// A wait's time ran out.
pub(crate) const WAIT_TIMEOUT: u32 = 0x102;
/// A wait could not be made (the last error says why).
pub(crate) const WAIT_FAILED: u32 = 0xFFFF_FFFF;
/// The most objects one wait takes.
pub(crate) const MAXIMUM_WAIT_OBJECTS: usize = 64;

/// `ReleaseSemaphore` would pass the maximum.
const ERROR_TOO_MANY_POSTS: u32 = 298;
/// `ReleaseMutex` by a thread that does not own the mutex.
const ERROR_NOT_OWNER: u32 = 288;
/// `InitOnceBeginInitialize` with `INIT_ONCE_CHECK_ONLY` before the
/// initialization completed, or `InitOnceComplete` with none in progress.
const ERROR_GEN_FAILURE: u32 = 31;

const CREATE_EVENT_MANUAL_RESET: u32 = 1;
const CREATE_EVENT_INITIAL_SET: u32 = 2;
const CREATE_MUTEX_INITIAL_OWNER: u32 = 1;
const CONDITION_VARIABLE_LOCKMODE_SHARED: u32 = 1;
const INIT_ONCE_CHECK_ONLY: u32 = 1;
const INIT_ONCE_ASYNC: u32 = 2;
const INIT_ONCE_INIT_FAILED: u32 = 4;

/// A dispatcher object a thread can wait for. Both methods run under the
/// dispatcher lock.
pub(crate) trait Waitable: Send + Sync {
    /// Whether a wait by `thread` would succeed now (a mutex it owns counts).
    fn is_signaled(&self, thread: u32) -> bool;
    /// Takes the signal for `thread`: an auto-reset event is cleared, a
    /// semaphore decremented, a mutex acquired (recursively if owned); a
    /// manual-reset event is left as it is.
    fn take(&self, thread: u32);
}

/// The one lock and condition variable every signal and wait go through.
static DISPATCHER: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// A lock, even if a thread panicked holding it: what our locks guard is
/// consistent between our operations.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Called by every function that signals an object, with the lock held
/// while the object changes: then `notify_all`.
pub(crate) fn signal<R>(change: impl FnOnce() -> R) -> R {
    let guard = lock(&DISPATCHER.0);
    let result = change();
    drop(guard);
    DISPATCHER.1.notify_all();
    result
}

/// The calling thread's id, as waits and mutexes record it.
fn me() -> u32 {
    Host::thread_id()
}

/// When a wait of `ms` milliseconds from now ends; none for `INFINITE`.
fn deadline(ms: u32) -> Option<Instant> {
    (ms != INFINITE).then(|| Instant::now() + Duration::from_millis(u64::from(ms)))
}

/// An event: `SetEvent` signals it; a wait clears an auto-reset one.
struct Event {
    manual_reset: bool,
    signaled: AtomicBool,
}

impl Object for Event {
    fn kind(&self) -> &'static str {
        "event"
    }
    fn waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}

impl Waitable for Event {
    fn is_signaled(&self, _thread: u32) -> bool {
        self.signaled.load(Ordering::Relaxed)
    }
    fn take(&self, _thread: u32) {
        if !self.manual_reset {
            self.signaled.store(false, Ordering::Relaxed);
        }
    }
}

/// A semaphore: signaled while its count is above zero.
struct Semaphore {
    count: AtomicI32,
    maximum: i32,
}

impl Object for Semaphore {
    fn kind(&self) -> &'static str {
        "semaphore"
    }
    fn waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}

impl Waitable for Semaphore {
    fn is_signaled(&self, _thread: u32) -> bool {
        self.count.load(Ordering::Relaxed) > 0
    }
    fn take(&self, _thread: u32) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A Windows mutex: owned by one thread at a time, recursively.
struct MutexObject {
    /// The owner's thread id; 0 for none.
    owner: AtomicU32,
    recursion: AtomicU32,
}

impl Object for MutexObject {
    fn kind(&self) -> &'static str {
        "mutex"
    }
    fn waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}

impl Waitable for MutexObject {
    fn is_signaled(&self, thread: u32) -> bool {
        let owner = self.owner.load(Ordering::Relaxed);
        owner == 0 || owner == thread
    }
    fn take(&self, thread: u32) {
        self.owner.store(thread, Ordering::Relaxed);
        self.recursion.fetch_add(1, Ordering::Relaxed);
    }
}

/// The object a handle names, as its own type `T`; none for an unknown
/// handle or one of another kind.
fn object<T: Object>(handle: *mut c_void) -> Option<Arc<dyn Object>> {
    let object = handles::get(handle as usize)?;
    let any: &dyn Any = &*object;
    any.is::<T>().then_some(object)
}

/// `object` as its own type; the caller checked it is one.
fn downcast<T: Object>(object: &Arc<dyn Object>) -> Option<&T> {
    let any: &dyn Any = &**object;
    any.downcast_ref::<T>()
}

/// Runs `body` on the object `handle` names if it is a `T`; else FALSE and
/// `ERROR_INVALID_HANDLE`.
fn with<T: Object>(handle: *mut c_void, body: impl FnOnce(&T) -> i32) -> i32 {
    let Some(object) = object::<T>(handle) else {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    match downcast::<T>(&object) {
        Some(typed) => body(typed),
        None => {
            set_last_error(ERROR_INVALID_HANDLE);
            0
        }
    }
}

/// Named objects, by name; kept for the process's life (Windows frees one
/// with its last handle).
static NAMED: LazyLock<Mutex<HashMap<String, Arc<dyn Object>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A NUL-terminated UTF-16 name from the title; none for a null pointer.
fn wide_name(name: *const u16) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(
            name, len,
        )))
    }
}

/// See [`wide_name`].
fn narrow_name(name: *const u8) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated string.
    let name = unsafe { CStr::from_ptr(name.cast()) };
    Some(name.to_string_lossy().into_owned())
}

/// A handle to a new object, or with a name that exists, to that object
/// (`ERROR_ALREADY_EXISTS`; one of another kind is `ERROR_INVALID_HANDLE`
/// and null, as on Windows).
fn create(name: Option<String>, kind: &str, make: impl FnOnce() -> Arc<dyn Object>) -> *mut c_void {
    let Some(name) = name else {
        return handles::insert(make()) as *mut c_void;
    };
    let mut named = lock(&NAMED);
    if let Some(existing) = named.get(&name) {
        if existing.kind() != kind {
            set_last_error(ERROR_INVALID_HANDLE);
            return std::ptr::null_mut();
        }
        let handle = handles::insert(existing.clone());
        set_last_error(ERROR_ALREADY_EXISTS);
        return handle as *mut c_void;
    }
    let object = make();
    named.insert(name, object.clone());
    set_last_error(ERROR_SUCCESS);
    handles::insert(object) as *mut c_void
}

/// A new handle to the named object of `kind`; null and
/// `ERROR_FILE_NOT_FOUND` if there is none (`ERROR_INVALID_HANDLE` if the
/// name is another kind's, as on Windows).
fn open(name: Option<String>, kind: &str) -> *mut c_void {
    let Some(name) = name else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    let named = lock(&NAMED);
    match named.get(&name) {
        Some(object) if object.kind() == kind => handles::insert(object.clone()) as *mut c_void,
        Some(_) => {
            set_last_error(ERROR_INVALID_HANDLE);
            std::ptr::null_mut()
        }
        None => {
            set_last_error(ERROR_FILE_NOT_FOUND);
            std::ptr::null_mut()
        }
    }
}

fn new_event(name: Option<String>, manual_reset: bool, initial: bool) -> *mut c_void {
    create(name, "event", || {
        Arc::new(Event {
            manual_reset,
            signaled: AtomicBool::new(initial),
        })
    })
}

/// A new event, or the existing one of that name.
pub extern "win64" fn CreateEventW(
    _attributes: *const c_void,
    manual_reset: i32,
    initial: i32,
    name: *const u16,
) -> *mut c_void {
    new_event(wide_name(name), manual_reset != 0, initial != 0)
}

/// See [`CreateEventW`].
pub extern "win64" fn CreateEventA(
    _attributes: *const c_void,
    manual_reset: i32,
    initial: i32,
    name: *const u8,
) -> *mut c_void {
    new_event(narrow_name(name), manual_reset != 0, initial != 0)
}

/// See [`CreateEventW`]; the kind and initial state are `flags`.
pub extern "win64" fn CreateEventExW(
    _attributes: *const c_void,
    name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    new_event(
        wide_name(name),
        flags & CREATE_EVENT_MANUAL_RESET != 0,
        flags & CREATE_EVENT_INITIAL_SET != 0,
    )
}

/// A new handle to the event of that name.
pub extern "win64" fn OpenEventW(_access: u32, _inherit: i32, name: *const u16) -> *mut c_void {
    open(wide_name(name), "event")
}

/// See [`OpenEventW`].
pub extern "win64" fn OpenEventA(_access: u32, _inherit: i32, name: *const u8) -> *mut c_void {
    open(narrow_name(name), "event")
}

/// Signals the event.
pub extern "win64" fn SetEvent(handle: *mut c_void) -> i32 {
    with::<Event>(handle, |event| {
        signal(|| event.signaled.store(true, Ordering::Relaxed));
        1
    })
}

/// Clears the event.
pub extern "win64" fn ResetEvent(handle: *mut c_void) -> i32 {
    with::<Event>(handle, |event| {
        let _guard = lock(&DISPATCHER.0);
        event.signaled.store(false, Ordering::Relaxed);
        1
    })
}

/// Sets the event and clears it again under the dispatcher lock. Waiters
/// check the event again when they wake, by which time it is clear, so in
/// this design a pulse releases nobody.
pub extern "win64" fn PulseEvent(handle: *mut c_void) -> i32 {
    with::<Event>(handle, |event| {
        signal(|| {
            event.signaled.store(true, Ordering::Relaxed);
            event.signaled.store(false, Ordering::Relaxed);
        });
        1
    })
}

fn new_semaphore(name: Option<String>, initial: i32, maximum: i32) -> *mut c_void {
    if maximum <= 0 || initial < 0 || initial > maximum {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    create(name, "semaphore", || {
        Arc::new(Semaphore {
            count: AtomicI32::new(initial),
            maximum,
        })
    })
}

/// A new semaphore, or the existing one of that name.
pub extern "win64" fn CreateSemaphoreW(
    _attributes: *const c_void,
    initial: i32,
    maximum: i32,
    name: *const u16,
) -> *mut c_void {
    new_semaphore(wide_name(name), initial, maximum)
}

/// See [`CreateSemaphoreW`].
pub extern "win64" fn CreateSemaphoreA(
    _attributes: *const c_void,
    initial: i32,
    maximum: i32,
    name: *const u8,
) -> *mut c_void {
    new_semaphore(narrow_name(name), initial, maximum)
}

/// See [`CreateSemaphoreW`].
pub extern "win64" fn CreateSemaphoreExW(
    _attributes: *const c_void,
    initial: i32,
    maximum: i32,
    name: *const u16,
    _flags: u32,
    _access: u32,
) -> *mut c_void {
    new_semaphore(wide_name(name), initial, maximum)
}

/// A new handle to the semaphore of that name.
pub extern "win64" fn OpenSemaphoreW(_access: u32, _inherit: i32, name: *const u16) -> *mut c_void {
    open(wide_name(name), "semaphore")
}

/// See [`OpenSemaphoreW`].
pub extern "win64" fn OpenSemaphoreA(_access: u32, _inherit: i32, name: *const u8) -> *mut c_void {
    open(narrow_name(name), "semaphore")
}

/// Adds `release_count` to the semaphore's count, unless that passes its
/// maximum; the count before goes to `previous`.
pub extern "win64" fn ReleaseSemaphore(
    handle: *mut c_void,
    release_count: i32,
    previous: *mut i32,
) -> i32 {
    if release_count <= 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    with::<Semaphore>(handle, |semaphore| {
        let before = signal(|| {
            let before = semaphore.count.load(Ordering::Relaxed);
            match before.checked_add(release_count) {
                Some(after) if after <= semaphore.maximum => {
                    semaphore.count.store(after, Ordering::Relaxed);
                    Some(before)
                }
                _ => None,
            }
        });
        let Some(before) = before else {
            set_last_error(ERROR_TOO_MANY_POSTS);
            return 0;
        };
        if !previous.is_null() {
            // SAFETY: checked non-null; the title passes a place for a LONG.
            unsafe { *previous = before };
        }
        1
    })
}

fn new_mutex(name: Option<String>, initial_owner: bool) -> *mut c_void {
    create(name, "mutex", || {
        let (owner, recursion) = if initial_owner { (me(), 1) } else { (0, 0) };
        Arc::new(MutexObject {
            owner: AtomicU32::new(owner),
            recursion: AtomicU32::new(recursion),
        })
    })
}

/// A new mutex, or the existing one of that name (which is then not
/// acquired, as on Windows).
pub extern "win64" fn CreateMutexW(
    _attributes: *const c_void,
    initial_owner: i32,
    name: *const u16,
) -> *mut c_void {
    new_mutex(wide_name(name), initial_owner != 0)
}

/// See [`CreateMutexW`].
pub extern "win64" fn CreateMutexA(
    _attributes: *const c_void,
    initial_owner: i32,
    name: *const u8,
) -> *mut c_void {
    new_mutex(narrow_name(name), initial_owner != 0)
}

/// See [`CreateMutexW`]; the initial owner is in `flags`.
pub extern "win64" fn CreateMutexExW(
    _attributes: *const c_void,
    name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    new_mutex(wide_name(name), flags & CREATE_MUTEX_INITIAL_OWNER != 0)
}

/// A new handle to the mutex of that name.
pub extern "win64" fn OpenMutexW(_access: u32, _inherit: i32, name: *const u16) -> *mut c_void {
    open(wide_name(name), "mutex")
}

/// See [`OpenMutexW`].
pub extern "win64" fn OpenMutexA(_access: u32, _inherit: i32, name: *const u8) -> *mut c_void {
    open(narrow_name(name), "mutex")
}

/// Releases one level of the calling thread's ownership; at the last the
/// mutex is free and its waiters are woken.
pub extern "win64" fn ReleaseMutex(handle: *mut c_void) -> i32 {
    with::<MutexObject>(handle, |mutex| {
        let thread = me();
        let owned = signal(|| {
            if mutex.owner.load(Ordering::Relaxed) != thread {
                return false;
            }
            if mutex.recursion.fetch_sub(1, Ordering::Relaxed) == 1 {
                mutex.owner.store(0, Ordering::Relaxed);
            }
            true
        });
        if !owned {
            set_last_error(ERROR_NOT_OWNER);
            return 0;
        }
        1
    })
}

/// Waits for one of the objects `handles` name (or all of them, with
/// `wait_all`) for up to `ms` milliseconds: `WAIT_OBJECT_0 + n` for the
/// object that ended it (0 for all), `WAIT_TIMEOUT`, or `WAIT_FAILED` with
/// the last error set.
pub(crate) fn wait(handles: &[usize], wait_all: bool, ms: u32) -> u32 {
    if handles.is_empty() || handles.len() > MAXIMUM_WAIT_OBJECTS {
        set_last_error(ERROR_INVALID_PARAMETER);
        return WAIT_FAILED;
    }
    let mut objects = Vec::with_capacity(handles.len());
    for &handle in handles {
        // Pseudo-handles are not in the table, so they fail here too.
        match handles::get(handle) {
            Some(object) if object.waitable().is_some() => objects.push(object),
            _ => {
                set_last_error(ERROR_INVALID_HANDLE);
                return WAIT_FAILED;
            }
        }
    }
    let waitables: Vec<&dyn Waitable> = objects.iter().filter_map(|o| o.waitable()).collect();
    let thread = me();
    let deadline = deadline(ms);
    let mut guard = lock(&DISPATCHER.0);
    loop {
        if wait_all {
            if waitables.iter().all(|w| w.is_signaled(thread)) {
                waitables.iter().for_each(|w| w.take(thread));
                return WAIT_OBJECT_0;
            }
        } else if let Some(index) = waitables.iter().position(|w| w.is_signaled(thread)) {
            waitables[index].take(thread);
            return WAIT_OBJECT_0 + index as u32;
        }
        guard = match deadline {
            None => DISPATCHER
                .1
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return WAIT_TIMEOUT;
                }
                let (guard, _) = DISPATCHER
                    .1
                    .wait_timeout(guard, deadline - now)
                    .unwrap_or_else(PoisonError::into_inner);
                guard
            }
        };
    }
}

/// Waits for the object for up to `ms` milliseconds.
pub extern "win64" fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32 {
    wait(&[handle as usize], false, ms)
}

/// See [`WaitForSingleObject`]; `alertable` is ignored: there are no APCs
/// yet.
pub extern "win64" fn WaitForSingleObjectEx(handle: *mut c_void, ms: u32, _alertable: i32) -> u32 {
    wait(&[handle as usize], false, ms)
}

/// Waits for one of `count` objects (or all, with `wait_all`).
pub extern "win64" fn WaitForMultipleObjects(
    count: u32,
    handles: *const *mut c_void,
    wait_all: i32,
    ms: u32,
) -> u32 {
    if handles.is_null() || count == 0 || count as usize > MAXIMUM_WAIT_OBJECTS {
        set_last_error(ERROR_INVALID_PARAMETER);
        return WAIT_FAILED;
    }
    // SAFETY: checked non-null; the title passes `count` handles (at most
    // 64, checked).
    let handles = unsafe { std::slice::from_raw_parts(handles.cast::<usize>(), count as usize) };
    wait(handles, wait_all != 0, ms)
}

/// See [`WaitForMultipleObjects`]; `alertable` is ignored: there are no APCs
/// yet.
pub extern "win64" fn WaitForMultipleObjectsEx(
    count: u32,
    handles: *const *mut c_void,
    wait_all: i32,
    ms: u32,
    _alertable: i32,
) -> u32 {
    WaitForMultipleObjects(count, handles, wait_all, ms)
}

/// Signals one object as `SetEvent`, `ReleaseSemaphore` (by 1) or
/// `ReleaseMutex` would, then waits for another. The two are not one atomic
/// step here, as they are on Windows. `alertable` is ignored: no APCs yet.
pub extern "win64" fn SignalObjectAndWait(
    to_signal: *mut c_void,
    to_wait: *mut c_void,
    ms: u32,
    _alertable: i32,
) -> u32 {
    let kind = handles::get(to_signal as usize).map(|o| o.kind());
    let signaled = match kind {
        Some("event") => SetEvent(to_signal),
        Some("semaphore") => ReleaseSemaphore(to_signal, 1, std::ptr::null_mut()),
        Some("mutex") => ReleaseMutex(to_signal),
        _ => {
            set_last_error(ERROR_INVALID_HANDLE);
            0
        }
    };
    if signaled == 0 {
        return WAIT_FAILED;
    }
    wait(&[to_wait as usize], false, ms)
}

/// Suspends the thread for `ms` milliseconds; 0 gives up the rest of its
/// time slice, `INFINITE` sleeps forever.
pub extern "win64" fn Sleep(ms: u32) {
    match ms {
        0 => std::thread::yield_now(),
        INFINITE => loop {
            std::thread::park();
        },
        ms => std::thread::sleep(Duration::from_millis(u64::from(ms))),
    }
}

/// See [`Sleep`]; returns 0 (no APC ended the sleep: there are none yet).
pub extern "win64" fn SleepEx(ms: u32, _alertable: i32) -> u32 {
    Sleep(ms);
    0
}

/// Gives up the rest of the thread's time slice.
pub extern "win64" fn SwitchToThread() -> i32 {
    std::thread::yield_now();
    1
}

/// The title's `RTL_CRITICAL_SECTION`. `debug_info` holds our [`CsState`];
/// the other fields are kept in step for title code that reads them.
#[repr(C)]
pub struct RtlCriticalSection {
    debug_info: *mut c_void,
    /// -1 free, 0 held (Windows' encoding differs and counts waiters;
    /// nothing we know of reads it).
    lock_count: i32,
    recursion_count: i32,
    /// The owner's thread id, as a pointer (what Windows stores).
    owning_thread: *mut c_void,
    lock_semaphore: *mut c_void,
    spin_count: usize,
}

const _: () = assert!(size_of::<RtlCriticalSection>() == 40);

/// A critical section's own lock: its owner and recursion, and its waiters.
struct CsState {
    inner: Mutex<CsInner>,
    cv: Condvar,
}

struct CsInner {
    /// The owner's thread id; 0 for none.
    owner: u32,
    recursion: u32,
}

/// Serializes initializing critical sections that were never initialized.
static LAZY: Mutex<()> = Mutex::new(());

/// The critical section's `debug_info`, read and written atomically: a
/// thread entering a static section reads it while another may set it.
fn debug_info<'a>(cs: *mut RtlCriticalSection) -> &'a AtomicPtr<c_void> {
    // SAFETY: the caller checked `cs` non-null; the title's section is
    // 8-byte aligned and lives while in use; every access to the field from
    // our side is atomic.
    unsafe { &*(&raw mut (*cs).debug_info).cast::<AtomicPtr<c_void>>() }
}

/// Whether `debug_info` names no state of ours: null (a zeroed static
/// section) or -1 (the static initializer `RTL_CRITICAL_SECTION_DEBUG` of
/// -1 some code uses).
fn uninitialized(info: *mut c_void) -> bool {
    info.is_null() || info as usize == usize::MAX
}

/// Makes the section's state, keeping its title-visible fields in step.
fn init_cs(cs: *mut RtlCriticalSection, spin: usize) {
    let state = Box::new(CsState {
        inner: Mutex::new(CsInner {
            owner: 0,
            recursion: 0,
        }),
        cv: Condvar::new(),
    });
    // SAFETY: the caller checked `cs` non-null; the title's section, which
    // no thread uses while it is being initialized.
    unsafe {
        (*cs).lock_count = -1;
        (*cs).recursion_count = 0;
        (*cs).owning_thread = std::ptr::null_mut();
        (*cs).lock_semaphore = std::ptr::null_mut();
        (*cs).spin_count = spin;
    }
    debug_info(cs).store(Box::into_raw(state).cast(), Ordering::Release);
}

/// The section's state, made first if the section was never initialized;
/// none for a null section.
fn cs_state<'a>(cs: *mut RtlCriticalSection) -> Option<&'a CsState> {
    if cs.is_null() {
        return None;
    }
    let mut info = debug_info(cs).load(Ordering::Acquire);
    if uninitialized(info) {
        let _lazy = lock(&LAZY);
        info = debug_info(cs).load(Ordering::Acquire);
        if uninitialized(info) {
            init_cs(cs, 0);
            info = debug_info(cs).load(Ordering::Acquire);
        }
    }
    // SAFETY: made by `init_cs` from a `Box<CsState>`, freed only by
    // `DeleteCriticalSection`, after which the title may not use it.
    Some(unsafe { &*info.cast::<CsState>() })
}

/// Writes the section's title-visible fields from our state.
fn publish(cs: *mut RtlCriticalSection, inner: &CsInner) {
    // SAFETY: the caller checked `cs` non-null; written under the section's
    // own lock, so by one thread at a time.
    unsafe {
        (*cs).lock_count = if inner.owner == 0 { -1 } else { 0 };
        (*cs).recursion_count = inner.recursion as i32;
        (*cs).owning_thread = inner.owner as usize as *mut c_void;
    }
}

/// Initializes the critical section.
pub extern "win64" fn InitializeCriticalSection(cs: *mut RtlCriticalSection) {
    if !cs.is_null() {
        init_cs(cs, 0);
    }
}

/// Initializes the critical section with a spin count (stored, not used:
/// waiters block at once).
pub extern "win64" fn InitializeCriticalSectionAndSpinCount(
    cs: *mut RtlCriticalSection,
    spin: u32,
) -> i32 {
    if cs.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    init_cs(cs, spin as usize);
    1
}

/// See [`InitializeCriticalSectionAndSpinCount`]; `flags` are ignored.
pub extern "win64" fn InitializeCriticalSectionEx(
    cs: *mut RtlCriticalSection,
    spin: u32,
    _flags: u32,
) -> i32 {
    InitializeCriticalSectionAndSpinCount(cs, spin)
}

/// Frees the section's state; the title may not use it again until it is
/// initialized again.
pub extern "win64" fn DeleteCriticalSection(cs: *mut RtlCriticalSection) {
    if cs.is_null() {
        return;
    }
    let info = debug_info(cs).swap(std::ptr::null_mut(), Ordering::AcqRel);
    if !uninitialized(info) {
        // SAFETY: made by `init_cs` from a `Box<CsState>`; taken out of the
        // section above, so freed once.
        drop(unsafe { Box::from_raw(info.cast::<CsState>()) });
    }
}

/// Enters the section, waiting while another thread owns it; recursive for
/// the owner.
pub extern "win64" fn EnterCriticalSection(cs: *mut RtlCriticalSection) {
    let Some(state) = cs_state(cs) else { return };
    let thread = me();
    let mut inner = lock(&state.inner);
    if inner.owner == thread {
        inner.recursion += 1;
    } else {
        while inner.owner != 0 {
            inner = state.cv.wait(inner).unwrap_or_else(PoisonError::into_inner);
        }
        inner.owner = thread;
        inner.recursion = 1;
    }
    publish(cs, &inner);
}

/// Enters the section if it is free or the caller owns it.
pub extern "win64" fn TryEnterCriticalSection(cs: *mut RtlCriticalSection) -> i32 {
    let Some(state) = cs_state(cs) else { return 0 };
    let thread = me();
    let mut inner = lock(&state.inner);
    if inner.owner == thread {
        inner.recursion += 1;
    } else if inner.owner == 0 {
        inner.owner = thread;
        inner.recursion = 1;
    } else {
        return 0;
    }
    publish(cs, &inner);
    1
}

/// Leaves one level of the section; at the last, a waiter may enter. A
/// thread that does not own it changes nothing.
pub extern "win64" fn LeaveCriticalSection(cs: *mut RtlCriticalSection) {
    let Some(state) = cs_state(cs) else { return };
    let mut inner = lock(&state.inner);
    if inner.owner != me() {
        return;
    }
    inner.recursion -= 1;
    if inner.recursion == 0 {
        inner.owner = 0;
        state.cv.notify_one();
    }
    publish(cs, &inner);
}

/// Stores a new spin count; returns the old.
pub extern "win64" fn SetCriticalSectionSpinCount(cs: *mut RtlCriticalSection, spin: u32) -> u32 {
    if cs.is_null() {
        return 0;
    }
    // SAFETY: checked non-null; the title's section.
    unsafe {
        let old = (*cs).spin_count;
        (*cs).spin_count = spin as usize;
        old as u32
    }
}

/// Where threads park on one address: its waiter count, and their
/// condition variable.
struct Slot {
    lock: Mutex<usize>,
    cv: Condvar,
}

/// The slots of the addresses threads are parked on.
static PARKING: LazyLock<Mutex<HashMap<usize, Arc<Slot>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The slot for `address`, made on demand, with this thread counted as one
/// of its waiters (under the parking table's lock, so a waker that changed
/// the word before finds the slot, and one that comes after finds this
/// thread in it).
fn slot(address: usize) -> Arc<Slot> {
    let mut parking = lock(&PARKING);
    let slot = parking
        .entry(address)
        .or_insert_with(|| {
            Arc::new(Slot {
                lock: Mutex::new(0),
                cv: Condvar::new(),
            })
        })
        .clone();
    *lock(&slot.lock) += 1;
    slot
}

/// Uncounts this thread from the slot; a slot without waiters is removed.
fn leave_slot(address: usize, slot: &Arc<Slot>) {
    let mut parking = lock(&PARKING);
    let mut waiters = lock(&slot.lock);
    *waiters -= 1;
    if *waiters == 0 {
        parking.remove(&address);
    }
}

/// Parks the thread on `address` until `ready()` (checked under the slot's
/// lock, which wakers take) or the deadline; whether it became ready.
fn park(address: usize, deadline: Option<Instant>, mut ready: impl FnMut() -> bool) -> bool {
    let slot = slot(address);
    let mut guard = lock(&slot.lock);
    let became_ready = loop {
        if ready() {
            break true;
        }
        guard = match deadline {
            None => slot.cv.wait(guard).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    break false;
                }
                let (guard, _) = slot
                    .cv
                    .wait_timeout(guard, deadline - now)
                    .unwrap_or_else(PoisonError::into_inner);
                guard
            }
        };
    };
    drop(guard);
    leave_slot(address, &slot);
    became_ready
}

/// Wakes one (or every) thread parked on `address`; call after changing
/// what they wait for.
fn wake(address: usize, all: bool) {
    let Some(slot) = lock(&PARKING).get(&address).cloned() else {
        return;
    };
    let _guard = lock(&slot.lock);
    if all {
        slot.cv.notify_all();
    } else {
        slot.cv.notify_one();
    }
}

/// The title's 8-byte word at `address`, as an atomic.
fn word<'a>(address: *mut c_void) -> &'a AtomicUsize {
    // SAFETY: the caller checked `address` non-null; the title's 8-byte
    // aligned word (an SRWLOCK, CONDITION_VARIABLE or INIT_ONCE), which
    // only these functions touch while it is in use.
    unsafe { &*address.cast::<AtomicUsize>() }
}

/// The SRW lock bit for an exclusive holder; shared holders count in 2s.
const SRW_EXCLUSIVE: usize = 1;
const SRW_SHARED: usize = 2;

/// Makes the SRW lock free.
pub extern "win64" fn InitializeSRWLock(lock: *mut c_void) {
    if !lock.is_null() {
        word(lock).store(0, Ordering::Release);
    }
}

fn try_exclusive(lock: &AtomicUsize) -> bool {
    lock.compare_exchange(0, SRW_EXCLUSIVE, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

fn try_shared(lock: &AtomicUsize) -> bool {
    let mut value = lock.load(Ordering::Relaxed);
    while value & SRW_EXCLUSIVE == 0 {
        match lock.compare_exchange_weak(
            value,
            value + SRW_SHARED,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(now) => value = now,
        }
    }
    false
}

/// Takes the SRW lock exclusively, waiting while anyone holds it.
pub extern "win64" fn AcquireSRWLockExclusive(lock: *mut c_void) {
    if lock.is_null() {
        return;
    }
    let state = word(lock);
    while !try_exclusive(state) {
        park(lock as usize, None, || state.load(Ordering::Relaxed) == 0);
    }
}

/// Takes the SRW lock shared, waiting while it is held exclusively.
pub extern "win64" fn AcquireSRWLockShared(lock: *mut c_void) {
    if lock.is_null() {
        return;
    }
    let state = word(lock);
    while !try_shared(state) {
        park(lock as usize, None, || {
            state.load(Ordering::Relaxed) & SRW_EXCLUSIVE == 0
        });
    }
}

/// Releases an exclusive hold and wakes every waiter.
pub extern "win64" fn ReleaseSRWLockExclusive(lock: *mut c_void) {
    if lock.is_null() {
        return;
    }
    word(lock).store(0, Ordering::Release);
    wake(lock as usize, true);
}

/// Releases a shared hold; the last wakes every waiter.
pub extern "win64" fn ReleaseSRWLockShared(lock: *mut c_void) {
    if lock.is_null() {
        return;
    }
    if word(lock).fetch_sub(SRW_SHARED, Ordering::Release) == SRW_SHARED {
        wake(lock as usize, true);
    }
}

/// Takes the SRW lock exclusively if no one holds it.
pub extern "win64" fn TryAcquireSRWLockExclusive(lock: *mut c_void) -> u8 {
    u8::from(!lock.is_null() && try_exclusive(word(lock)))
}

/// Takes the SRW lock shared if it is not held exclusively.
pub extern "win64" fn TryAcquireSRWLockShared(lock: *mut c_void) -> u8 {
    u8::from(!lock.is_null() && try_shared(word(lock)))
}

/// Makes the condition variable's generation 0.
pub extern "win64" fn InitializeConditionVariable(cv: *mut c_void) {
    if !cv.is_null() {
        word(cv).store(0, Ordering::Release);
    }
}

/// Releases a lock with `release`, waits until the condition variable is
/// woken or `ms` passes, and takes the lock again with `retake` (either
/// way): TRUE, or FALSE with `ERROR_TIMEOUT`.
fn sleep_on(cv: *mut c_void, ms: u32, release: impl FnOnce(), retake: impl FnOnce()) -> i32 {
    let generation = word(cv);
    let before = generation.load(Ordering::Acquire);
    release();
    let woken = park(cv as usize, deadline(ms), || {
        generation.load(Ordering::Acquire) != before
    });
    retake();
    if woken {
        1
    } else {
        set_last_error(ERROR_TIMEOUT);
        0
    }
}

/// Leaves the critical section and waits on the condition variable.
pub extern "win64" fn SleepConditionVariableCS(
    cv: *mut c_void,
    cs: *mut RtlCriticalSection,
    ms: u32,
) -> i32 {
    if cv.is_null() || cs.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    sleep_on(
        cv,
        ms,
        || LeaveCriticalSection(cs),
        || EnterCriticalSection(cs),
    )
}

/// Releases the SRW lock (shared with `CONDITION_VARIABLE_LOCKMODE_SHARED`)
/// and waits on the condition variable.
pub extern "win64" fn SleepConditionVariableSRW(
    cv: *mut c_void,
    lock: *mut c_void,
    ms: u32,
    flags: u32,
) -> i32 {
    if cv.is_null() || lock.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if flags & CONDITION_VARIABLE_LOCKMODE_SHARED != 0 {
        sleep_on(
            cv,
            ms,
            || ReleaseSRWLockShared(lock),
            || AcquireSRWLockShared(lock),
        )
    } else {
        sleep_on(
            cv,
            ms,
            || ReleaseSRWLockExclusive(lock),
            || AcquireSRWLockExclusive(lock),
        )
    }
}

/// Wakes one thread sleeping on the condition variable.
pub extern "win64" fn WakeConditionVariable(cv: *mut c_void) {
    if !cv.is_null() {
        word(cv).fetch_add(1, Ordering::Release);
        wake(cv as usize, false);
    }
}

/// Wakes every thread sleeping on the condition variable.
pub extern "win64" fn WakeAllConditionVariable(cv: *mut c_void) {
    if !cv.is_null() {
        word(cv).fetch_add(1, Ordering::Release);
        wake(cv as usize, true);
    }
}

/// The `size` bytes at `address`, read once with a volatile read; none for
/// a size other than 1, 2, 4 or 8.
fn read_sized(address: *const c_void, size: usize) -> Option<u64> {
    // SAFETY: the caller checked `address` non-null; the title passes a
    // value of `size` bytes, aligned to its size.
    unsafe {
        Some(match size {
            1 => u64::from(std::ptr::read_volatile(address.cast::<u8>())),
            2 => u64::from(std::ptr::read_volatile(address.cast::<u16>())),
            4 => u64::from(std::ptr::read_volatile(address.cast::<u32>())),
            8 => std::ptr::read_volatile(address.cast::<u64>()),
            _ => return None,
        })
    }
}

/// Waits while the `size` bytes at `address` equal those at `compare`, for
/// up to `ms` milliseconds: TRUE when they differ, FALSE with
/// `ERROR_TIMEOUT` when the time ran out.
pub extern "win64" fn WaitOnAddress(
    address: *mut c_void,
    compare: *const c_void,
    size: usize,
    ms: u32,
) -> i32 {
    if address.is_null() || compare.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(undesired) = read_sized(compare, size) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let changed = park(address as usize, deadline(ms), || {
        read_sized(address, size) != Some(undesired)
    });
    if changed {
        1
    } else {
        set_last_error(ERROR_TIMEOUT);
        0
    }
}

/// Wakes one thread waiting on the address.
pub extern "win64" fn WakeByAddressSingle(address: *mut c_void) {
    wake(address as usize, false);
}

/// Wakes every thread waiting on the address.
pub extern "win64" fn WakeByAddressAll(address: *mut c_void) {
    wake(address as usize, true);
}

/// `INIT_ONCE` states, in the word's low 2 bits; the rest is the context.
const ONCE_NOT_BEGUN: usize = 0;
const ONCE_IN_PROGRESS: usize = 1;
const ONCE_DONE: usize = 2;
const ONCE_STATE: usize = 3;

/// The callback `InitOnceExecuteOnce` runs: (once, parameter, context out).
type InitOnceCallback = extern "win64" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> i32;

/// Writes a done one-time initialization's context, if there is a place.
fn write_context(context_out: *mut *mut c_void, value: usize) {
    if !context_out.is_null() {
        // SAFETY: checked non-null; the title passes a place for a pointer.
        unsafe { *context_out = (value & !ONCE_STATE) as *mut c_void };
    }
}

/// Makes the one-time initialization not begun.
pub extern "win64" fn InitOnceInitialize(once: *mut c_void) {
    if !once.is_null() {
        word(once).store(ONCE_NOT_BEGUN, Ordering::Release);
    }
}

/// Runs `callback` once for `once`, however many threads call this: the
/// others wait for it; once it succeeded, its context goes to everyone.
pub extern "win64" fn InitOnceExecuteOnce(
    once: *mut c_void,
    callback: Option<InitOnceCallback>,
    parameter: *mut c_void,
    context_out: *mut *mut c_void,
) -> i32 {
    let Some(callback) = callback.filter(|_| !once.is_null()) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let state = word(once);
    loop {
        let value = state.load(Ordering::Acquire);
        match value & ONCE_STATE {
            ONCE_DONE => {
                write_context(context_out, value);
                return 1;
            }
            ONCE_NOT_BEGUN => {
                let begun = state.compare_exchange(
                    value,
                    ONCE_IN_PROGRESS,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                );
                if begun.is_err() {
                    continue;
                }
                let mut context: *mut c_void = std::ptr::null_mut();
                let ok = callback(once, parameter, &mut context);
                if ok != 0 {
                    let done = (context as usize & !ONCE_STATE) | ONCE_DONE;
                    state.store(done, Ordering::Release);
                    wake(once as usize, true);
                    write_context(context_out, done);
                    return 1;
                }
                state.store(ONCE_NOT_BEGUN, Ordering::Release);
                wake(once as usize, true);
                return 0;
            }
            _ => {
                park(once as usize, None, || {
                    state.load(Ordering::Acquire) & ONCE_STATE != ONCE_IN_PROGRESS
                });
            }
        }
    }
}

/// Begins a one-time initialization: `pending` TRUE means the caller runs it
/// and calls `InitOnceComplete`; FALSE means it is done, its context in
/// `context_out`. With `INIT_ONCE_CHECK_ONLY` nothing is begun (FALSE with
/// `ERROR_GEN_FAILURE` if not done). `INIT_ONCE_ASYNC` is not supported
/// (FALSE with `ERROR_INVALID_PARAMETER`).
pub extern "win64" fn InitOnceBeginInitialize(
    once: *mut c_void,
    flags: u32,
    pending: *mut i32,
    context_out: *mut *mut c_void,
) -> i32 {
    if once.is_null() || pending.is_null() || flags & INIT_ONCE_ASYNC != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let state = word(once);
    let set_pending = |value: i32| {
        // SAFETY: checked non-null; the title passes a place for a BOOL.
        unsafe { *pending = value };
    };
    loop {
        let value = state.load(Ordering::Acquire);
        match value & ONCE_STATE {
            ONCE_DONE => {
                set_pending(0);
                write_context(context_out, value);
                return 1;
            }
            _ if flags & INIT_ONCE_CHECK_ONLY != 0 => {
                set_last_error(ERROR_GEN_FAILURE);
                return 0;
            }
            ONCE_NOT_BEGUN => {
                let begun = state.compare_exchange(
                    value,
                    ONCE_IN_PROGRESS,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                );
                if begun.is_ok() {
                    set_pending(1);
                    return 1;
                }
            }
            _ => {
                park(once as usize, None, || {
                    state.load(Ordering::Acquire) & ONCE_STATE != ONCE_IN_PROGRESS
                });
            }
        }
    }
}

/// Ends a one-time initialization begun by `InitOnceBeginInitialize`: done
/// with `context`, or with `INIT_ONCE_INIT_FAILED` back to not begun.
pub extern "win64" fn InitOnceComplete(once: *mut c_void, flags: u32, context: *mut c_void) -> i32 {
    if once.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let state = word(once);
    if state.load(Ordering::Acquire) & ONCE_STATE != ONCE_IN_PROGRESS {
        set_last_error(ERROR_GEN_FAILURE);
        return 0;
    }
    let value = if flags & INIT_ONCE_INIT_FAILED != 0 {
        ONCE_NOT_BEGUN
    } else {
        (context as usize & !ONCE_STATE) | ONCE_DONE
    };
    state.store(value, Ordering::Release);
    wake(once as usize, true);
    1
}

/// Every SList operation, on every list: one at a time.
static SLIST: Mutex<()> = Mutex::new(());

/// An SList's top entry and depth, in the title's 16-byte `SLIST_HEADER`:
/// the top entry's address in the first 8 bytes, the depth in the next 2.
struct SListHead(*mut c_void);

impl SListHead {
    fn top(&self) -> *mut c_void {
        // SAFETY: the caller checked the header non-null; the title's
        // 16-byte aligned header, touched under `SLIST` only.
        unsafe { *self.0.cast::<*mut c_void>() }
    }
    fn set_top(&self, entry: *mut c_void) {
        // SAFETY: as in `top`.
        unsafe { *self.0.cast::<*mut c_void>() = entry };
    }
    fn depth(&self) -> u16 {
        // SAFETY: as in `top`; bytes 8 and 9 of the header.
        unsafe { *self.0.cast::<u8>().add(8).cast::<u16>() }
    }
    fn set_depth(&self, depth: u16) {
        // SAFETY: as in `depth`.
        unsafe { *self.0.cast::<u8>().add(8).cast::<u16>() = depth };
    }
}

/// An entry's `SINGLE_LIST_ENTRY::Next`.
fn next(entry: *mut c_void) -> *mut c_void {
    // SAFETY: the caller checked the entry non-null; an entry on the list,
    // whose first 8 bytes are its link.
    unsafe { *entry.cast::<*mut c_void>() }
}

fn set_next(entry: *mut c_void, next: *mut c_void) {
    // SAFETY: as in `next`; the title passes an entry it owns.
    unsafe { *entry.cast::<*mut c_void>() = next };
}

/// Makes the SList empty.
pub extern "win64" fn InitializeSListHead(head: *mut c_void) {
    if head.is_null() {
        return;
    }
    let _guard = lock(&SLIST);
    // SAFETY: checked non-null; the title's 16-byte header.
    unsafe { std::ptr::write_bytes(head.cast::<u8>(), 0, 16) };
}

/// Pushes `entry`; returns the entry that was on top (null if none).
pub extern "win64" fn InterlockedPushEntrySList(
    head: *mut c_void,
    entry: *mut c_void,
) -> *mut c_void {
    InterlockedPushListSListEx(head, entry, entry, 1)
}

/// Pops the top entry; null when the list is empty.
pub extern "win64" fn InterlockedPopEntrySList(head: *mut c_void) -> *mut c_void {
    if head.is_null() {
        return std::ptr::null_mut();
    }
    let _guard = lock(&SLIST);
    let head = SListHead(head);
    let top = head.top();
    if !top.is_null() {
        head.set_top(next(top));
        head.set_depth(head.depth().wrapping_sub(1));
    }
    top
}

/// Empties the list; returns its old top entry (the rest linked from it).
pub extern "win64" fn InterlockedFlushSList(head: *mut c_void) -> *mut c_void {
    if head.is_null() {
        return std::ptr::null_mut();
    }
    let _guard = lock(&SLIST);
    let head = SListHead(head);
    let top = head.top();
    head.set_top(std::ptr::null_mut());
    head.set_depth(0);
    top
}

/// The number of entries on the list.
pub extern "win64" fn QueryDepthSList(head: *mut c_void) -> u16 {
    if head.is_null() {
        return 0;
    }
    let _guard = lock(&SLIST);
    SListHead(head).depth()
}

/// Pushes `count` entries already linked from `first` to `last`; returns
/// the entry that was on top (null if none).
pub extern "win64" fn InterlockedPushListSListEx(
    head: *mut c_void,
    first: *mut c_void,
    last: *mut c_void,
    count: u32,
) -> *mut c_void {
    if head.is_null() || first.is_null() || last.is_null() {
        return std::ptr::null_mut();
    }
    let _guard = lock(&SLIST);
    let head = SListHead(head);
    let top = head.top();
    set_next(last, top);
    head.set_top(first);
    head.set_depth(head.depth().wrapping_add(count as u16));
    top
}

/// An export under ntdll's name for a function of ours.
fn alias(name: &'static str, address: usize) -> HleExport {
    HleExport { name, address }
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(CreateEventW),
        hle_export!(CreateEventA),
        hle_export!(CreateEventExW),
        hle_export!(OpenEventW),
        hle_export!(OpenEventA),
        hle_export!(SetEvent),
        hle_export!(ResetEvent),
        hle_export!(PulseEvent),
        hle_export!(CreateSemaphoreW),
        hle_export!(CreateSemaphoreA),
        hle_export!(CreateSemaphoreExW),
        hle_export!(OpenSemaphoreW),
        hle_export!(OpenSemaphoreA),
        hle_export!(ReleaseSemaphore),
        hle_export!(CreateMutexW),
        hle_export!(CreateMutexA),
        hle_export!(CreateMutexExW),
        hle_export!(OpenMutexW),
        hle_export!(OpenMutexA),
        hle_export!(ReleaseMutex),
        hle_export!(WaitForSingleObject),
        hle_export!(WaitForSingleObjectEx),
        hle_export!(WaitForMultipleObjects),
        hle_export!(WaitForMultipleObjectsEx),
        hle_export!(SignalObjectAndWait),
        hle_export!(Sleep),
        hle_export!(SleepEx),
        hle_export!(SwitchToThread),
        hle_export!(InitializeCriticalSection),
        hle_export!(InitializeCriticalSectionAndSpinCount),
        hle_export!(InitializeCriticalSectionEx),
        hle_export!(DeleteCriticalSection),
        hle_export!(EnterCriticalSection),
        hle_export!(TryEnterCriticalSection),
        hle_export!(LeaveCriticalSection),
        hle_export!(SetCriticalSectionSpinCount),
        hle_export!(InitializeSRWLock),
        hle_export!(AcquireSRWLockExclusive),
        hle_export!(AcquireSRWLockShared),
        hle_export!(ReleaseSRWLockExclusive),
        hle_export!(ReleaseSRWLockShared),
        hle_export!(TryAcquireSRWLockExclusive),
        hle_export!(TryAcquireSRWLockShared),
        hle_export!(InitializeConditionVariable),
        hle_export!(SleepConditionVariableCS),
        hle_export!(SleepConditionVariableSRW),
        hle_export!(WakeConditionVariable),
        hle_export!(WakeAllConditionVariable),
        hle_export!(WaitOnAddress),
        hle_export!(WakeByAddressSingle),
        hle_export!(WakeByAddressAll),
        hle_export!(InitOnceInitialize),
        hle_export!(InitOnceExecuteOnce),
        hle_export!(InitOnceBeginInitialize),
        hle_export!(InitOnceComplete),
        hle_export!(InitializeSListHead),
        hle_export!(InterlockedPushEntrySList),
        hle_export!(InterlockedPopEntrySList),
        hle_export!(InterlockedFlushSList),
        hle_export!(QueryDepthSList),
        hle_export!(InterlockedPushListSListEx),
        alias(
            "RtlInitializeSListHead",
            InitializeSListHead as *const () as usize,
        ),
        alias(
            "RtlInterlockedPushEntrySList",
            InterlockedPushEntrySList as *const () as usize,
        ),
        alias(
            "RtlInterlockedPopEntrySList",
            InterlockedPopEntrySList as *const () as usize,
        ),
        alias(
            "RtlInterlockedFlushSList",
            InterlockedFlushSList as *const () as usize,
        ),
        alias("RtlQueryDepthSList", QueryDepthSList as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use std::cell::UnsafeCell;
    use std::collections::VecDeque;
    use std::thread;

    use super::*;
    use crate::modules::win32::handles::{CloseHandle, GetLastError, last_error};

    /// Test data shared between threads, guarded by the lock under test.
    struct Shared<T>(UnsafeCell<T>);

    // SAFETY: every access in these tests is made under the lock being
    // tested (or by one thread at a time).
    unsafe impl<T> Sync for Shared<T> {}
    // SAFETY: as above.
    unsafe impl<T> Send for Shared<T> {}

    impl<T> Shared<T> {
        fn new(value: T) -> Arc<Self> {
            Arc::new(Shared(UnsafeCell::new(value)))
        }
        fn get(&self) -> *mut T {
            self.0.get()
        }
    }

    fn new_cs() -> RtlCriticalSection {
        RtlCriticalSection {
            debug_info: std::ptr::null_mut(),
            lock_count: 0,
            recursion_count: 0,
            owning_thread: std::ptr::null_mut(),
            lock_semaphore: std::ptr::null_mut(),
            spin_count: 0,
        }
    }

    fn null() -> *const c_void {
        std::ptr::null()
    }

    fn auto_event() -> usize {
        CreateEventW(null(), 0, 0, std::ptr::null()) as usize
    }

    fn manual_event() -> usize {
        CreateEventW(null(), 1, 0, std::ptr::null()) as usize
    }

    fn h(handle: usize) -> *mut c_void {
        handle as *mut c_void
    }

    #[test]
    fn an_auto_reset_event_wakes_one_waiter() {
        let event = auto_event();
        let waiters: Vec<_> = (0..2)
            .map(|_| thread::spawn(move || WaitForSingleObject(h(event), 200)))
            .collect();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(SetEvent(h(event)), 1);
        let mut results: Vec<u32> = waiters.into_iter().map(|t| t.join().unwrap()).collect();
        results.sort_unstable();
        assert_eq!(results, [WAIT_OBJECT_0, WAIT_TIMEOUT]);
        assert_eq!(CloseHandle(h(event)), 1);
    }

    #[test]
    fn a_manual_reset_event_wakes_everyone() {
        let event = manual_event();
        let waiters: Vec<_> = (0..3)
            .map(|_| thread::spawn(move || WaitForSingleObject(h(event), 5000)))
            .collect();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(SetEvent(h(event)), 1);
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap(), WAIT_OBJECT_0);
        }
        // Still set: a manual-reset event stays so until reset.
        assert_eq!(WaitForSingleObject(h(event), 0), WAIT_OBJECT_0);
        assert_eq!(ResetEvent(h(event)), 1);
        assert_eq!(WaitForSingleObject(h(event), 0), WAIT_TIMEOUT);
        assert_eq!(CloseHandle(h(event)), 1);
    }

    #[test]
    fn a_semaphore_counts() {
        victory_loader::teb::attach_current_thread();
        let semaphore = CreateSemaphoreW(null(), 2, 3, std::ptr::null());
        assert!(!semaphore.is_null());
        assert_eq!(WaitForSingleObject(semaphore, 0), WAIT_OBJECT_0);
        assert_eq!(WaitForSingleObject(semaphore, 0), WAIT_OBJECT_0);
        assert_eq!(WaitForSingleObject(semaphore, 50), WAIT_TIMEOUT);
        let mut previous = -1;
        assert_eq!(ReleaseSemaphore(semaphore, 2, &mut previous), 1);
        assert_eq!(previous, 0);
        assert_eq!(ReleaseSemaphore(semaphore, 1, &mut previous), 1);
        assert_eq!(previous, 2);
        assert_eq!(ReleaseSemaphore(semaphore, 1, &mut previous), 0);
        assert_eq!(GetLastError(), ERROR_TOO_MANY_POSTS);
        assert!(CreateSemaphoreW(null(), 4, 3, std::ptr::null()).is_null());
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(CloseHandle(semaphore), 1);
    }

    #[test]
    fn a_mutex_is_recursive_and_owned() {
        victory_loader::teb::attach_current_thread();
        let mutex = CreateMutexW(null(), 0, std::ptr::null()) as usize;
        assert_eq!(WaitForSingleObject(h(mutex), 0), WAIT_OBJECT_0);
        assert_eq!(WaitForSingleObject(h(mutex), 0), WAIT_OBJECT_0);
        let (blocked, not_owner) = thread::spawn(move || {
            victory_loader::teb::attach_current_thread();
            let blocked = WaitForSingleObject(h(mutex), 100);
            let released = ReleaseMutex(h(mutex));
            (blocked, (released, last_error()))
        })
        .join()
        .unwrap();
        assert_eq!(blocked, WAIT_TIMEOUT);
        assert_eq!(not_owner, (0, ERROR_NOT_OWNER));
        let other = thread::spawn(move || {
            let got = WaitForSingleObject(h(mutex), 5000);
            let released = ReleaseMutex(h(mutex));
            (got, released)
        });
        thread::sleep(Duration::from_millis(50));
        assert_eq!(ReleaseMutex(h(mutex)), 1);
        assert_eq!(ReleaseMutex(h(mutex)), 1);
        assert_eq!(other.join().unwrap(), (WAIT_OBJECT_0, 1));
        assert_eq!(ReleaseMutex(h(mutex)), 0);
        assert_eq!(GetLastError(), ERROR_NOT_OWNER);
        assert_eq!(CloseHandle(h(mutex)), 1);
    }

    #[test]
    fn wait_all_needs_every_object_and_wait_any_reports_the_first() {
        victory_loader::teb::attach_current_thread();
        let list = [manual_event(), manual_event()].map(h);
        assert_eq!(SetEvent(list[1]), 1);
        assert_eq!(
            WaitForMultipleObjects(2, list.as_ptr(), 1, 50),
            WAIT_TIMEOUT
        );
        assert_eq!(
            WaitForMultipleObjects(2, list.as_ptr(), 0, 0),
            WAIT_OBJECT_0 + 1
        );
        assert_eq!(SetEvent(list[0]), 1);
        assert_eq!(
            WaitForMultipleObjects(2, list.as_ptr(), 0, 0),
            WAIT_OBJECT_0
        );
        assert_eq!(
            WaitForMultipleObjects(2, list.as_ptr(), 1, 0),
            WAIT_OBJECT_0
        );

        assert_eq!(WaitForMultipleObjects(0, list.as_ptr(), 0, 0), WAIT_FAILED);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(WaitForMultipleObjects(65, list.as_ptr(), 0, 0), WAIT_FAILED);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        let pseudo = [list[0], usize::MAX as *mut c_void];
        assert_eq!(
            WaitForMultipleObjects(2, pseudo.as_ptr(), 0, 0),
            WAIT_FAILED
        );
        assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
        for handle in list {
            assert_eq!(CloseHandle(handle), 1);
        }
    }

    #[test]
    fn a_timeout_is_kept() {
        let event = auto_event();
        let start = Instant::now();
        assert_eq!(WaitForSingleObject(h(event), 100), WAIT_TIMEOUT);
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(90) && took <= Duration::from_millis(1000));
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            SetEvent(h(event))
        });
        assert_eq!(WaitForSingleObject(h(event), INFINITE), WAIT_OBJECT_0);
        assert_eq!(setter.join().unwrap(), 1);
        assert_eq!(CloseHandle(h(event)), 1);
    }

    #[test]
    fn critical_sections_exclude_and_recurse() {
        let shared = Shared::new((new_cs(), 0u64));
        // SAFETY: the shared value outlives every use here.
        let cs = unsafe { &raw mut (*shared.get()).0 };
        InitializeCriticalSection(cs);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let shared = shared.clone();
                thread::spawn(move || {
                    // SAFETY: the section is initialized and outlives the
                    // threads; the counter is touched under it only.
                    let (cs, counter) =
                        unsafe { (&raw mut (*shared.get()).0, &raw mut (*shared.get()).1) };
                    for _ in 0..10_000 {
                        EnterCriticalSection(cs);
                        // SAFETY: under the section.
                        unsafe { *counter += 1 };
                        LeaveCriticalSection(cs);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        // SAFETY: the workers are done.
        assert_eq!(unsafe { (*shared.get()).1 }, 80_000);

        for _ in 0..3 {
            EnterCriticalSection(cs);
        }
        // SAFETY: the fields are written by the owner, this thread.
        unsafe {
            assert_eq!((*cs).recursion_count, 3);
            assert_eq!((*cs).lock_count, 0);
            assert_eq!((*cs).owning_thread as usize, me() as usize);
        }
        let try_other = || {
            let shared = shared.clone();
            thread::spawn(move || {
                // SAFETY: the section outlives the thread.
                let cs = unsafe { &raw mut (*shared.get()).0 };
                let entered = TryEnterCriticalSection(cs);
                if entered == 1 {
                    LeaveCriticalSection(cs);
                }
                entered
            })
            .join()
            .unwrap()
        };
        assert_eq!(try_other(), 0);
        for _ in 0..3 {
            LeaveCriticalSection(cs);
        }
        // SAFETY: the section is free; nobody writes its fields now.
        assert_eq!(unsafe { (*cs).lock_count }, -1);
        assert_eq!(try_other(), 1);
        DeleteCriticalSection(cs);
    }

    #[test]
    fn an_uninitialized_critical_section_initializes_itself() {
        let mut cs = new_cs();
        let cs: *mut RtlCriticalSection = &mut cs;
        EnterCriticalSection(cs);
        // SAFETY: this thread's own section.
        unsafe {
            assert!(!(*cs).debug_info.is_null());
            assert_eq!((*cs).recursion_count, 1);
        }
        assert_eq!(TryEnterCriticalSection(cs), 1);
        LeaveCriticalSection(cs);
        LeaveCriticalSection(cs);
        assert_eq!(SetCriticalSectionSpinCount(cs, 4000), 0);
        assert_eq!(SetCriticalSectionSpinCount(cs, 0), 4000);
        DeleteCriticalSection(cs);
        // SAFETY: as above.
        assert!(unsafe { (*cs).debug_info.is_null() });
    }

    #[test]
    fn srw_locks_share_reads_and_exclude_writes() {
        let lock = Arc::new(AtomicUsize::new(1234));
        let address = lock.as_ptr() as usize;
        InitializeSRWLock(h(address));
        AcquireSRWLockShared(h(address));
        let reader = thread::spawn(move || {
            let shared = TryAcquireSRWLockShared(h(address));
            let exclusive = TryAcquireSRWLockExclusive(h(address));
            (shared, exclusive)
        })
        .join()
        .unwrap();
        assert_eq!(reader, (1, 0));
        ReleaseSRWLockShared(h(address));
        assert_eq!(TryAcquireSRWLockExclusive(h(address)), 0);
        ReleaseSRWLockShared(h(address));
        assert_eq!(TryAcquireSRWLockExclusive(h(address)), 1);
        assert_eq!(TryAcquireSRWLockShared(h(address)), 0);
        // A writer waiting while we hold it gets it once we release.
        let writer = thread::spawn(move || {
            AcquireSRWLockExclusive(h(address));
            ReleaseSRWLockExclusive(h(address));
        });
        thread::sleep(Duration::from_millis(50));
        ReleaseSRWLockExclusive(h(address));
        writer.join().unwrap();
        assert_eq!(lock.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn condition_variables_hand_over() {
        // Through a critical section.
        let shared = Shared::new((new_cs(), VecDeque::new()));
        // SAFETY: the shared value outlives every use here.
        let (cs, queue) = unsafe { (&raw mut (*shared.get()).0, &raw mut (*shared.get()).1) };
        InitializeCriticalSection(cs);
        let cv = Arc::new(AtomicUsize::new(0));
        let cv_address = cv.as_ptr() as usize;
        InitializeConditionVariable(h(cv_address));
        let producer = {
            let shared = shared.clone();
            thread::spawn(move || {
                // SAFETY: the section outlives the thread; the queue is
                // touched under it only.
                let (cs, queue) =
                    unsafe { (&raw mut (*shared.get()).0, &raw mut (*shared.get()).1) };
                for item in 0..100 {
                    EnterCriticalSection(cs);
                    // SAFETY: under the section.
                    unsafe { (*queue).push_back(item) };
                    LeaveCriticalSection(cs);
                    WakeConditionVariable(h(cv_address));
                }
            })
        };
        let mut received = Vec::new();
        EnterCriticalSection(cs);
        while received.len() < 100 {
            // SAFETY: under the section.
            match unsafe { (*queue).pop_front() } {
                Some(item) => received.push(item),
                None => {
                    SleepConditionVariableCS(h(cv_address), cs, 5000);
                }
            }
        }
        LeaveCriticalSection(cs);
        producer.join().unwrap();
        assert_eq!(received, (0..100).collect::<Vec<_>>());
        DeleteCriticalSection(cs);

        // Through an SRW lock.
        let srw = Arc::new(AtomicUsize::new(0));
        let srw_address = srw.as_ptr() as usize;
        let queue = Shared::new(VecDeque::new());
        let producer = {
            let queue = queue.clone();
            thread::spawn(move || {
                for item in 0..100 {
                    AcquireSRWLockExclusive(h(srw_address));
                    // SAFETY: under the lock.
                    unsafe { (*queue.get()).push_back(item) };
                    ReleaseSRWLockExclusive(h(srw_address));
                    WakeAllConditionVariable(h(cv_address));
                }
            })
        };
        let mut received = Vec::new();
        AcquireSRWLockExclusive(h(srw_address));
        while received.len() < 100 {
            // SAFETY: under the lock.
            match unsafe { (*queue.get()).pop_front() } {
                Some(item) => received.push(item),
                None => {
                    SleepConditionVariableSRW(h(cv_address), h(srw_address), 5000, 0);
                }
            }
        }
        ReleaseSRWLockExclusive(h(srw_address));
        producer.join().unwrap();
        assert_eq!(received, (0..100).collect::<Vec<_>>());

        // A sleep nobody wakes times out, with the lock taken again.
        victory_loader::teb::attach_current_thread();
        AcquireSRWLockShared(h(srw_address));
        assert_eq!(
            SleepConditionVariableSRW(h(cv_address), h(srw_address), 50, 1),
            0
        );
        assert_eq!(GetLastError(), ERROR_TIMEOUT);
        assert_eq!(srw.load(Ordering::Relaxed), SRW_SHARED);
        ReleaseSRWLockShared(h(srw_address));
    }

    #[test]
    fn wait_on_address_returns_when_the_word_changes() {
        victory_loader::teb::attach_current_thread();
        let word = Arc::new(AtomicU32::new(7));
        let address = word.as_ptr() as usize;
        let undesired = 7u32;
        let compare = (&raw const undesired).cast::<c_void>();
        let changer = {
            let word = word.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                word.store(8, Ordering::Relaxed);
                WakeByAddressSingle(h(address));
            })
        };
        assert_eq!(WaitOnAddress(h(address), compare, 4, 5000), 1);
        changer.join().unwrap();
        assert_eq!(word.load(Ordering::Relaxed), 8);

        let unchanged = 8u32;
        let compare = (&raw const unchanged).cast::<c_void>();
        assert_eq!(WaitOnAddress(h(address), compare, 4, 50), 0);
        assert_eq!(GetLastError(), ERROR_TIMEOUT);
        assert_eq!(WaitOnAddress(h(address), compare, 3, 50), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
    }

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    extern "win64" fn initialize(
        _once: *mut c_void,
        parameter: *mut c_void,
        context: *mut *mut c_void,
    ) -> i32 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(20));
        // SAFETY: `InitOnceExecuteOnce` passes a place for the context.
        unsafe { *context = parameter };
        1
    }

    #[test]
    fn init_once_runs_the_callback_once() {
        let once = Arc::new(AtomicUsize::new(0));
        let address = once.as_ptr() as usize;
        InitOnceInitialize(h(address));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                thread::spawn(move || {
                    let mut context = std::ptr::null_mut();
                    let ok =
                        InitOnceExecuteOnce(h(address), Some(initialize), h(0x1000), &mut context);
                    (ok, context as usize)
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), (1, 0x1000));
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        // The two-step form: one thread begins and completes; then it is done.
        let other = AtomicUsize::new(0);
        let other = other.as_ptr().cast::<c_void>();
        let mut pending = 0;
        let mut context = std::ptr::null_mut();
        assert_eq!(
            InitOnceBeginInitialize(other, INIT_ONCE_CHECK_ONLY, &mut pending, &mut context),
            0
        );
        assert_eq!(
            InitOnceBeginInitialize(other, 0, &mut pending, &mut context),
            1
        );
        assert_eq!(pending, 1);
        assert_eq!(InitOnceComplete(other, 0, h(0x2000)), 1);
        assert_eq!(
            InitOnceBeginInitialize(other, 0, &mut pending, &mut context),
            1
        );
        assert_eq!((pending, context as usize), (0, 0x2000));
    }

    #[test]
    fn slist_pushes_and_pops_in_order() {
        #[repr(C, align(16))]
        struct Header([u64; 2]);
        let mut header = Header([u64::MAX; 2]);
        let head: *mut c_void = (&raw mut header).cast();
        let mut entries = [[0usize; 2]; 3];
        let base: *mut [usize; 2] = entries.as_mut_ptr();
        // SAFETY: indices within the array.
        let [e0, e1, e2] = [0, 1, 2].map(|i| unsafe { base.add(i) }.cast::<c_void>());
        InitializeSListHead(head);
        assert_eq!(QueryDepthSList(head), 0);
        assert!(InterlockedPopEntrySList(head).is_null());
        assert!(InterlockedPushEntrySList(head, e0).is_null());
        assert_eq!(InterlockedPushEntrySList(head, e1), e0);
        assert_eq!(InterlockedPushEntrySList(head, e2), e1);
        assert_eq!(QueryDepthSList(head), 3);
        assert_eq!(InterlockedPopEntrySList(head), e2);
        assert_eq!(QueryDepthSList(head), 2);
        let rest = InterlockedFlushSList(head);
        assert_eq!(rest, e1);
        assert_eq!(next(rest), e0);
        assert!(next(e0).is_null());
        assert_eq!(QueryDepthSList(head), 0);
        assert!(InterlockedPopEntrySList(head).is_null());
    }

    #[test]
    fn named_objects_are_shared() {
        victory_loader::teb::attach_current_thread();
        let name: Vec<u16> = "Local\\victory-test-named-event\0".encode_utf16().collect();
        let first = CreateEventW(null(), 1, 0, name.as_ptr());
        assert!(!first.is_null());
        let second = CreateEventW(null(), 1, 0, name.as_ptr());
        assert_eq!(GetLastError(), ERROR_ALREADY_EXISTS);
        assert_ne!(first, second);
        let (Some(a), Some(b)) = (handles::get(first as usize), handles::get(second as usize))
        else {
            panic!("both handles name the event");
        };
        assert!(Arc::ptr_eq(&a, &b));
        let opened = OpenEventW(0, 0, name.as_ptr());
        assert!(!opened.is_null());
        assert_eq!(SetEvent(opened), 1);
        assert_eq!(WaitForSingleObject(first, 0), WAIT_OBJECT_0);
        assert!(CreateMutexW(null(), 0, name.as_ptr()).is_null());
        assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
        let unknown: Vec<u16> = "victory-test-no-such-event\0".encode_utf16().collect();
        assert!(OpenEventW(0, 0, unknown.as_ptr()).is_null());
        assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
        for handle in [first, second, opened] {
            assert_eq!(CloseHandle(handle), 1);
        }
    }

    #[test]
    fn every_export_is_listed() {
        // 61 functions, 5 of them also under their ntdll names.
        assert_eq!(exports().len(), 61 + 5);
    }
}
