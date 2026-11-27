//! The C++ standard library's DLL (`msvcp140.dll` and its satellites): the
//! part every MSVC-built program uses whose names the STL's headers fix.
//! On Windows the host's MSVCP140 serves the title; here it is ours.
//!
//! - The C-level threads, mutexes, condition variables and time behind
//!   `std::thread`, `std::mutex`, `std::condition_variable` and
//!   `std::chrono` (`xthreads.h`, `xtimec.h`), the shared mutex (an SRW
//!   lock), and the `_Lockit` locks.
//! - The functions that throw the standard exceptions (`_Xlength_error` and
//!   friends, `xthrow.cpp`), with objects laid out as MSVC lays them out,
//!   their vtables, and RTTI in `cxx`'s absolute form; the error categories
//!   and maps (`syserror.cpp`).
//! - `exception_ptr` (`excptptr.cpp`).
//! - The C helpers the locale facets call (`xlocinfo.h`), for the "C"
//!   locale.
//!
//! A `_Mtx_t` (`_Cnd_t`) is the title's storage of `_Mtx_internal_imp_size`
//! (`_Cnd_internal_imp_size`) bytes; its first 8 bytes hold a pointer to our
//! state, made by `_Mtx_init_in_situ` (`_Cnd_init_in_situ`). Storage whose
//! first word is not one of ours (below 0x10000: the constexpr constructors
//! of newer STLs write the mutex type there, or zeros) gets its state on
//! first use, with that type.
//!
//! Limits (what waits for the census of the title's 113 imports):
//! - `locale`, `ios_base`, `basic_streambuf`, `basic_ostream` and the
//!   facets, whose object layouts the title's inline code depends on, and
//!   `locale::_Getgloballocale`.
//! - Our exception objects' vtables have no complete object locator (the
//!   slot before them is 0): `typeid` and `dynamic_cast` of them do not
//!   work.
//! - `_Gettnames` and `_W_Gettnames` return null (see there).
//! - `_Winerror_message` has no message table: "Windows error <code>".
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::arch::naked_asm;
use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::{MaybeUninit, offset_of, size_of};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use victory_loader::seh::{Context, EXCEPTION_NONCONTINUABLE, ExceptionRecord, RtlCaptureContext};
use victory_platform::Host;
use victory_platform::system::System;

use super::ucrt::cxx::{
    self, CT_IS_STD_BAD_ALLOC, CatchableTypeAbs, CatchableTypeArrayAbs, EH_EXCEPTION_NUMBER,
    EH_MAGIC_NUMBER1, Pmd, TI_VICTORY_ABSOLUTE, ThrowInfoAbs, Thrown, TypeDescriptor,
};
use super::ucrt::runtime::{
    self, __std_exception_copy, __std_exception_destroy, EILSEQ, ERANGE, ExceptionData, set_errno,
};
use super::ucrt::{heap, string};
use super::win32::{handles, sync, threads, time as wtime};
use crate::{HleExport, HleModule};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// 1. Threads, mutexes, condition variables, time.

/// `_Thrd_start_t`.
pub type Thrd = unsafe extern "win64" fn(*mut c_void) -> u32;

/// `_Thrd_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ThrdT {
    pub handle: *mut c_void,
    pub id: u32,
}

/// `xtime`: seconds and nanoseconds since the Unix epoch (UTC).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Xtime {
    pub sec: i64,
    pub nsec: i32,
}

const _: () = {
    assert!(size_of::<ThrdT>() == 16);
    assert!(size_of::<Xtime>() == 16);
};

/// `_Thrd_result`.
pub const THRD_SUCCESS: i32 = 0;
pub const THRD_NOMEM: i32 = 1;
pub const THRD_TIMEDOUT: i32 = 2;
pub const THRD_BUSY: i32 = 3;
pub const THRD_ERROR: i32 = 4;

/// Mutex types (bits).
pub const MTX_PLAIN: i32 = 1;
pub const MTX_TRY: i32 = 2;
pub const MTX_TIMED: i32 = 4;
pub const MTX_RECURSIVE: i32 = 0x100;

/// `_Mtx_internal_imp_size`, `_Cnd_internal_imp_size`.
pub const MTX_INTERNAL_IMP_SIZE: usize = 80;
pub const CND_INTERNAL_IMP_SIZE: usize = 72;

/// `TIME_UTC`.
const TIME_UTC: i32 = 1;
const NSEC_PER_SEC: i128 = 1_000_000_000;
/// 1601-01-01 to 1970-01-01, in 100 ns units.
const FILETIME_EPOCH: i64 = 0x019D_B1DE_D53E_8000;

/// The first word of a title's `_Mtx_t` or `_Cnd_t` storage.
///
/// # Safety
/// `storage` is the title's storage, 8-aligned, alive while used.
unsafe fn first_word<'a>(storage: *mut c_void) -> &'a AtomicU64 {
    // SAFETY: the caller's.
    unsafe { &*storage.cast::<AtomicU64>() }
}

/// Below this, a storage's first word is not a pointer of ours.
const OURS: u64 = 0x1_0000;

/// The state behind the storage `storage`: the one its first word names, or
/// one made now from what is there (`make` given the word).
///
/// # Safety
/// As [`first_word`].
unsafe fn state_of<'a, T>(storage: *mut c_void, make: impl Fn(u64) -> T) -> &'a T {
    // SAFETY: the caller's.
    let word = unsafe { first_word(storage) };
    loop {
        let value = word.load(Ordering::Acquire);
        if value >= OURS {
            // SAFETY: a state of ours, which lives until the storage is
            // destroyed.
            return unsafe { &*(value as *const T) };
        }
        let fresh = Box::into_raw(Box::new(make(value)));
        match word.compare_exchange(value, fresh as u64, Ordering::AcqRel, Ordering::Acquire) {
            // SAFETY: just made, now the storage's.
            Ok(_) => return unsafe { &*fresh },
            // SAFETY: ours, never shared.
            Err(_) => drop(unsafe { Box::from_raw(fresh) }),
        }
    }
}

/// Takes the state out of the storage `storage` and drops it.
///
/// # Safety
/// As [`first_word`]; nothing uses the state any more.
unsafe fn drop_state<T>(storage: *mut c_void) {
    // SAFETY: the caller's.
    let value = unsafe { first_word(storage) }.swap(0, Ordering::AcqRel);
    if value >= OURS {
        // SAFETY: a state of ours, which nothing uses any more.
        drop(unsafe { Box::from_raw(value as *mut T) });
    }
}

/// When a wait ends.
#[derive(Debug, Clone, Copy)]
enum Target {
    Forever,
    /// Not at all: a try.
    Now,
    Until(Instant),
}

/// Nanoseconds since the Unix epoch of now.
fn now_nanos() -> i128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn nanos(xt: &Xtime) -> i128 {
    i128::from(xt.sec) * NSEC_PER_SEC + i128::from(xt.nsec)
}

/// The title's `xtime`.
fn read_xtime(xt: *const Xtime) -> Xtime {
    if xt.is_null() {
        return Xtime::default();
    }
    // SAFETY: the title's time.
    unsafe { xt.read_unaligned() }
}

/// How long from now until the absolute time `xt`; zero once it has passed.
fn until(xt: *const Xtime) -> Duration {
    let diff = nanos(&read_xtime(xt)) - now_nanos();
    if diff <= 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(u64::try_from(diff).unwrap_or(u64::MAX))
    }
}

/// The wait for the absolute time `xt` (one too far to name is forever).
fn target_at(xt: *const Xtime) -> Target {
    match Instant::now().checked_add(until(xt)) {
        Some(at) => Target::Until(at),
        None => Target::Forever,
    }
}

/// The calling thread's id.
fn me() -> u32 {
    handles::GetCurrentThreadId()
}

/// A mutex's state: its type, and its owner and count (0: free).
struct MtxInner {
    kind: i32,
    state: Mutex<(u32, u32)>,
    free: Condvar,
}

impl MtxInner {
    fn new(kind: i32) -> MtxInner {
        MtxInner {
            kind,
            state: Mutex::new((0, 0)),
            free: Condvar::new(),
        }
    }

    /// `mutex.cpp`'s `mtx_do_lock`: a plain mutex taken by its owner again
    /// counts up; a try or timed one does too when it is recursive, else it
    /// is busy (timed out, for a timed lock); otherwise it is taken once
    /// free, waiting as `target` says.
    fn lock(&self, target: Target) -> i32 {
        let me = me();
        let mut st = lock(&self.state);
        if st.0 == me && st.1 > 0 {
            let plain = self.kind & !MTX_RECURSIVE == MTX_PLAIN;
            if plain || self.kind & MTX_RECURSIVE != 0 {
                st.1 += 1;
                return THRD_SUCCESS;
            }
            return match target {
                Target::Until(_) => THRD_TIMEDOUT,
                _ => THRD_BUSY,
            };
        }
        loop {
            if st.1 == 0 {
                *st = (me, 1);
                return THRD_SUCCESS;
            }
            st = match target {
                Target::Forever => self.free.wait(st).unwrap_or_else(PoisonError::into_inner),
                Target::Now => return THRD_BUSY,
                Target::Until(at) => {
                    let now = Instant::now();
                    if now >= at {
                        return THRD_TIMEDOUT;
                    }
                    self.free
                        .wait_timeout(st, at - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    /// Counts down; free at 0.
    fn unlock(&self) -> i32 {
        let mut st = lock(&self.state);
        if st.1 == 0 {
            return THRD_ERROR;
        }
        st.1 -= 1;
        if st.1 == 0 {
            st.0 = 0;
            self.free.notify_one();
        }
        THRD_SUCCESS
    }

    /// Lets go of it entirely: its count, which [`MtxInner::retake`] gives
    /// back.
    fn release_fully(&self) -> u32 {
        let mut st = lock(&self.state);
        let count = st.1;
        *st = (0, 0);
        self.free.notify_one();
        count
    }

    /// Takes it again, `count` more deep (held at least once), once no
    /// other thread holds it.
    fn retake(&self, count: u32) {
        let me = me();
        let mut st = lock(&self.state);
        while st.1 > 0 && st.0 != 0 && st.0 != me {
            st = self.free.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        *st = (me, (st.1 + count).max(1));
    }
}

/// Where a mutex's storage holds our state: its `_Critical_section` slot.
/// `_Mtx_internal_imp_t` starts with `int _Type`, which the constexpr
/// `std::mutex` constructor of newer STLs writes in place (it never calls
/// `_Mtx_init_in_situ`, and it zeroes the critical section but not the
/// padding after the type), so the type is read from offset 0 and our state
/// lives at offset 8, where a zero means "none yet".
const MTX_STATE: usize = 8;

/// The state of the mutex `mtx`: the one made by [`_Mtx_init_in_situ`], or
/// one made now of the type the title wrote at offset 0.
fn mtx(mtx: *mut c_void) -> Option<&'static MtxInner> {
    if mtx.is_null() {
        return None;
    }
    // SAFETY: the title's mutex storage, `_Mtx_internal_imp_size` bytes.
    Some(unsafe {
        state_of(mtx.byte_add(MTX_STATE), |_| {
            MtxInner::new(mtx.cast::<i32>().read_unaligned())
        })
    })
}

/// Makes the mutex in the title's storage, of type `kind`.
pub extern "win64" fn _Mtx_init_in_situ(mtx: *mut c_void, kind: i32) {
    if mtx.is_null() {
        return;
    }
    let fresh = Box::into_raw(Box::new(MtxInner::new(kind)));
    // SAFETY: the title's storage.
    unsafe {
        mtx.cast::<i32>().write_unaligned(kind);
        first_word(mtx.byte_add(MTX_STATE)).store(fresh as u64, Ordering::Release);
    }
}

/// Ends the mutex in the title's storage.
pub extern "win64" fn _Mtx_destroy_in_situ(mtx: *mut c_void) {
    if !mtx.is_null() {
        // SAFETY: the title's storage, which no one uses any more.
        unsafe { drop_state::<MtxInner>(mtx.byte_add(MTX_STATE)) };
    }
}

/// A mutex of type `kind` in a block of ours (`malloc`), into `*out`.
pub extern "win64" fn _Mtx_init(out: *mut *mut c_void, kind: i32) -> i32 {
    if out.is_null() {
        return THRD_ERROR;
    }
    let block = heap::calloc(1, MTX_INTERNAL_IMP_SIZE);
    // SAFETY: the title's place for the mutex.
    unsafe { out.write_unaligned(block) };
    if block.is_null() {
        return THRD_NOMEM;
    }
    _Mtx_init_in_situ(block, kind);
    THRD_SUCCESS
}

/// Ends a mutex from [`_Mtx_init`] and frees its block.
pub extern "win64" fn _Mtx_destroy(mtx: *mut c_void) {
    if !mtx.is_null() {
        _Mtx_destroy_in_situ(mtx);
        heap::free(mtx);
    }
}

/// Takes the mutex, waiting for it.
pub extern "win64" fn _Mtx_lock(m: *mut c_void) -> i32 {
    mtx(m).map_or(THRD_ERROR, |m| m.lock(Target::Forever))
}

/// Takes the mutex if it is free; `THRD_BUSY` if not.
pub extern "win64" fn _Mtx_trylock(m: *mut c_void) -> i32 {
    mtx(m).map_or(THRD_ERROR, |m| m.lock(Target::Now))
}

/// Takes the mutex, waiting until the absolute time `xt` at most
/// (`THRD_TIMEDOUT`).
pub extern "win64" fn _Mtx_timedlock(m: *mut c_void, xt: *const Xtime) -> i32 {
    mtx(m).map_or(THRD_ERROR, |m| m.lock(target_at(xt)))
}

/// Counts the mutex down, free at 0.
pub extern "win64" fn _Mtx_unlock(m: *mut c_void) -> i32 {
    mtx(m).map_or(THRD_ERROR, MtxInner::unlock)
}

/// Whether the calling thread holds the mutex.
pub extern "win64" fn _Mtx_current_owns(m: *mut c_void) -> i32 {
    mtx(m).map_or(0, |m| {
        let st = lock(&m.state);
        i32::from(st.1 > 0 && st.0 == me())
    })
}

/// The address of the mutex's critical section (`native_handle`).
pub extern "win64" fn _Mtx_getconcrtcs(m: *mut c_void) -> *mut c_void {
    if m.is_null() {
        return m;
    }
    // SAFETY: within the title's storage.
    unsafe { m.byte_add(MTX_STATE) }
}

/// `mutex.cpp`'s: the owner set to nobody while the mutex stays held (the
/// STL's SRW lock is let go of by the condition variable's wait, not here),
/// so no other thread takes it between this and [`_Mtx_reset_owner`].
pub extern "win64" fn _Mtx_clear_owner(m: *mut c_void) {
    if let Some(m) = mtx(m) {
        lock(&m.state).0 = 0;
    }
}

/// `mutex.cpp`'s: the calling thread the owner again, its count kept
/// (waiting, here, while another thread holds it; a free mutex is taken).
pub extern "win64" fn _Mtx_reset_owner(m: *mut c_void) {
    if let Some(m) = mtx(m) {
        m.retake(0);
    }
}

/// A condition variable's state: a count of signals, and what waits for
/// it to change.
struct CndInner {
    seq: Mutex<u64>,
    cv: Condvar,
}

impl CndInner {
    fn new() -> CndInner {
        CndInner {
            seq: Mutex::new(0),
            cv: Condvar::new(),
        }
    }
}

/// The state of the condition variable `cnd`.
fn cnd(cnd: *mut c_void) -> Option<&'static CndInner> {
    // SAFETY: the title's condition variable storage.
    (!cnd.is_null()).then(|| unsafe { state_of(cnd, |_| CndInner::new()) })
}

/// Makes the condition variable in the title's storage.
pub extern "win64" fn _Cnd_init_in_situ(cnd: *mut c_void) {
    if cnd.is_null() {
        return;
    }
    let fresh = Box::into_raw(Box::new(CndInner::new()));
    // SAFETY: the title's storage.
    unsafe { first_word(cnd) }.store(fresh as u64, Ordering::Release);
}

/// Ends the condition variable in the title's storage.
pub extern "win64" fn _Cnd_destroy_in_situ(cnd: *mut c_void) {
    if !cnd.is_null() {
        // SAFETY: the title's storage, which no one uses any more.
        unsafe { drop_state::<CndInner>(cnd) };
    }
}

/// A condition variable in a block of ours (`malloc`), into `*out`.
pub extern "win64" fn _Cnd_init(out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return THRD_ERROR;
    }
    let block = heap::calloc(1, CND_INTERNAL_IMP_SIZE);
    // SAFETY: the title's place for the condition variable.
    unsafe { out.write_unaligned(block) };
    if block.is_null() {
        return THRD_NOMEM;
    }
    _Cnd_init_in_situ(block);
    THRD_SUCCESS
}

/// Ends a condition variable from [`_Cnd_init`] and frees its block.
pub extern "win64" fn _Cnd_destroy(cnd: *mut c_void) {
    if !cnd.is_null() {
        _Cnd_destroy_in_situ(cnd);
        heap::free(cnd);
    }
}

/// `_Cnd_wait`'s work: the mutex let go of entirely while the signal count
/// is held, a wait for it to change (until `target`), the mutex taken
/// again with its count.
fn cnd_wait(c: *mut c_void, m: *mut c_void, target: Target) -> i32 {
    let (Some(c), Some(m)) = (cnd(c), mtx(m)) else {
        return THRD_ERROR;
    };
    let mut seq = lock(&c.seq);
    let start = *seq;
    let count = m.release_fully();
    let mut result = THRD_SUCCESS;
    while *seq == start {
        seq = match target {
            Target::Forever => c.cv.wait(seq).unwrap_or_else(PoisonError::into_inner),
            Target::Now => {
                result = THRD_TIMEDOUT;
                break;
            }
            Target::Until(at) => {
                let now = Instant::now();
                if now >= at {
                    result = THRD_TIMEDOUT;
                    break;
                }
                c.cv.wait_timeout(seq, at - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
    }
    drop(seq);
    m.retake(count);
    result
}

/// Waits for a signal, the mutex let go of meanwhile.
pub extern "win64" fn _Cnd_wait(cnd: *mut c_void, mtx: *mut c_void) -> i32 {
    cnd_wait(cnd, mtx, Target::Forever)
}

/// [`_Cnd_wait`] until the absolute time `xt` at most (`THRD_TIMEDOUT`).
pub extern "win64" fn _Cnd_timedwait(cnd: *mut c_void, mtx: *mut c_void, xt: *const Xtime) -> i32 {
    cnd_wait(cnd, mtx, target_at(xt))
}

/// Wakes one waiter.
pub extern "win64" fn _Cnd_signal(c: *mut c_void) -> i32 {
    let Some(c) = cnd(c) else {
        return THRD_ERROR;
    };
    // Under the lock the waiters read the count with: a waiter between its
    // read and its wait cannot miss this, and a waiter that has seen it
    // cannot have destroyed the state before the notify.
    let mut seq = lock(&c.seq);
    *seq += 1;
    c.cv.notify_one();
    drop(seq);
    THRD_SUCCESS
}

/// Wakes every waiter.
pub extern "win64" fn _Cnd_broadcast(c: *mut c_void) -> i32 {
    let Some(c) = cnd(c) else {
        return THRD_ERROR;
    };
    // As `_Cnd_signal`: under the lock.
    let mut seq = lock(&c.seq);
    *seq += 1;
    c.cv.notify_all();
    drop(seq);
    THRD_SUCCESS
}

/// A `notify_all_at_thread_exit`: the thread, the condition variable, the
/// mutex and the flag to set.
struct AtExit {
    thread: u32,
    cnd: usize,
    mtx: usize,
    flag: usize,
}

static AT_EXIT: Mutex<Vec<AtExit>> = Mutex::new(Vec::new());

/// Has the calling thread's end unlock `mtx`, set `*flag` and broadcast
/// `cnd` ([`_Cnd_do_broadcast_at_thread_exit`]).
pub extern "win64" fn _Cnd_register_at_thread_exit(
    cnd: *mut c_void,
    mtx: *mut c_void,
    flag: *mut i32,
) {
    lock(&AT_EXIT).push(AtExit {
        thread: me(),
        cnd: cnd as usize,
        mtx: mtx as usize,
        flag: flag as usize,
    });
}

/// Forgets the registrations of `mtx`.
pub extern "win64" fn _Cnd_unregister_at_thread_exit(mtx: *mut c_void) {
    lock(&AT_EXIT).retain(|e| e.mtx != mtx as usize);
}

/// Runs the calling thread's registrations, as `cond.cpp`: the flag set,
/// the mutex unlocked, the condition variable broadcast; then forgotten.
pub extern "win64" fn _Cnd_do_broadcast_at_thread_exit() {
    let me = me();
    let mine: Vec<AtExit> = {
        let mut all = lock(&AT_EXIT);
        let (mine, rest) = all.drain(..).partition(|e| e.thread == me);
        *all = rest;
        mine
    };
    for e in mine {
        let flag = e.flag as *mut i32;
        if !flag.is_null() {
            // SAFETY: the title's flag, registered for this.
            unsafe { flag.write_unaligned(1) };
        }
        _Mtx_unlock(e.mtx as *mut c_void);
        _Cnd_broadcast(e.cnd as *mut c_void);
    }
}

/// A new thread running `start(arg)` (`CreateThread`); `THRD_NOMEM` on any
/// failure, as `cthread.cpp`.
pub extern "win64" fn _Thrd_create(out: *mut ThrdT, start: Option<Thrd>, arg: *mut c_void) -> i32 {
    let Some(start) = start else {
        return THRD_NOMEM;
    };
    let mut id = 0u32;
    let handle = threads::CreateThread(ptr::null_mut(), 0, start as *mut c_void, arg, 0, &mut id);
    if handle.is_null() {
        return THRD_NOMEM;
    }
    if !out.is_null() {
        // SAFETY: the title's place for the thread.
        unsafe { out.write_unaligned(ThrdT { handle, id }) };
    }
    THRD_SUCCESS
}

/// The title's `_Thrd_t`, which Win64 passes (16 bytes) by pointer.
fn read_thrd(thr: *const ThrdT) -> Option<ThrdT> {
    // SAFETY: the title's thread.
    (!thr.is_null()).then(|| unsafe { thr.read_unaligned() })
}

/// Waits for the thread to end, its exit code into `*code` (when not null);
/// its handle closed.
pub extern "win64" fn _Thrd_join(thr: *const ThrdT, code: *mut i32) -> i32 {
    let Some(thr) = read_thrd(thr) else {
        return THRD_ERROR;
    };
    if sync::WaitForSingleObject(thr.handle, sync::INFINITE) == sync::WAIT_FAILED {
        return THRD_ERROR;
    }
    let mut exit = 0u32;
    if threads::GetExitCodeThread(thr.handle, &mut exit) == 0 {
        return THRD_ERROR;
    }
    if !code.is_null() {
        // SAFETY: the title's place for the code.
        unsafe { code.write_unaligned(exit as i32) };
    }
    if handles::CloseHandle(thr.handle) == 0 {
        return THRD_ERROR;
    }
    THRD_SUCCESS
}

/// Lets the thread run on its own: its handle closed.
pub extern "win64" fn _Thrd_detach(thr: *const ThrdT) -> i32 {
    match read_thrd(thr) {
        Some(thr) if handles::CloseHandle(thr.handle) != 0 => THRD_SUCCESS,
        _ => THRD_ERROR,
    }
}

/// Sleeps until the absolute time `xt` (not at all once it has passed).
pub extern "win64" fn _Thrd_sleep(xt: *const Xtime) {
    loop {
        let left = until(xt);
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left);
    }
}

/// Gives up the rest of the time slice.
pub extern "win64" fn _Thrd_yield() {
    sync::SwitchToThread();
}

/// The calling thread's id.
pub extern "win64" fn _Thrd_id() -> u32 {
    me()
}

/// The calling thread: no handle, its id. A 16-byte `_Thrd_t` is returned
/// as Win64 returns it: into the hidden first argument, which comes back in
/// rax.
pub extern "win64" fn _Thrd_current(out: *mut ThrdT) -> *mut ThrdT {
    if !out.is_null() {
        // SAFETY: the caller's place for the result.
        unsafe {
            out.write_unaligned(ThrdT {
                handle: ptr::null_mut(),
                id: me(),
            });
        }
    }
    out
}

/// Whether two threads are the same (16-byte arguments, which Win64 passes
/// by pointer).
pub extern "win64" fn _Thrd_equal(a: *const ThrdT, b: *const ThrdT) -> i32 {
    match (read_thrd(a), read_thrd(b)) {
        (Some(a), Some(b)) => i32::from(a.id == b.id),
        _ => 0,
    }
}

/// Whether `a`'s id is below `b`'s.
pub extern "win64" fn _Thrd_lt(a: *const ThrdT, b: *const ThrdT) -> i32 {
    match (read_thrd(a), read_thrd(b)) {
        (Some(a), Some(b)) => i32::from(a.id < b.id),
        _ => 0,
    }
}

/// The logical processors the process may run on.
pub extern "win64" fn _Thrd_hardware_concurrency() -> u32 {
    u32::try_from(Host::logical_processors()).unwrap_or(u32::MAX)
}

/// Now into `*out` for `TIME_UTC` (and `TIME_UTC` returned); 0 for another
/// base.
pub extern "win64" fn xtime_get(out: *mut Xtime, base: i32) -> i32 {
    if base != TIME_UTC || out.is_null() {
        return 0;
    }
    let now = now_nanos();
    let xt = Xtime {
        sec: now.div_euclid(NSEC_PER_SEC) as i64,
        nsec: now.rem_euclid(NSEC_PER_SEC) as i32,
    };
    // SAFETY: the title's place for the time.
    unsafe { out.write_unaligned(xt) };
    base
}

/// Now, in 100 ns units since 1970 (`xtime.cpp`: the precise file time less
/// the 1601 epoch; `system_clock`'s count).
pub extern "win64" fn _Xtime_get_ticks() -> i64 {
    let mut ft = wtime::FILETIME { low: 0, high: 0 };
    wtime::GetSystemTimePreciseAsFileTime(&mut ft);
    ((i64::from(ft.high) << 32) | i64::from(ft.low)) - FILETIME_EPOCH
}

/// `a - b` in milliseconds, rounded up; 0 when not positive.
pub extern "win64" fn _Xtime_diff_to_millis2(a: *const Xtime, b: *const Xtime) -> i32 {
    let diff = nanos(&read_xtime(a)) - nanos(&read_xtime(b));
    if diff <= 0 {
        return 0;
    }
    i32::try_from((diff + 999_999) / 1_000_000).unwrap_or(i32::MAX)
}

/// Milliseconds from now to `xt`, rounded up; 0 once it has passed.
pub extern "win64" fn _Xtime_diff_to_millis(xt: *const Xtime) -> i32 {
    let mut now = Xtime::default();
    xtime_get(&mut now, TIME_UTC);
    _Xtime_diff_to_millis2(xt, &now)
}

/// `QueryPerformanceCounter`.
pub extern "win64" fn _Query_perf_counter() -> i64 {
    let mut v = 0;
    wtime::QueryPerformanceCounter(&mut v);
    v
}

/// `QueryPerformanceFrequency`.
pub extern "win64" fn _Query_perf_frequency() -> i64 {
    let mut v = 0;
    wtime::QueryPerformanceFrequency(&mut v);
    v
}

/// `shared_mutex` (an SRW lock): exclusively.
pub extern "win64" fn _Smtx_lock_exclusive(lock: *mut c_void) {
    sync::AcquireSRWLockExclusive(lock);
}

/// Shared.
pub extern "win64" fn _Smtx_lock_shared(lock: *mut c_void) {
    sync::AcquireSRWLockShared(lock);
}

/// Exclusively if no one holds it.
pub extern "win64" fn _Smtx_try_lock_exclusive(lock: *mut c_void) -> i32 {
    i32::from(sync::TryAcquireSRWLockExclusive(lock))
}

/// Shared if it is not held exclusively.
pub extern "win64" fn _Smtx_try_lock_shared(lock: *mut c_void) -> i32 {
    i32::from(sync::TryAcquireSRWLockShared(lock))
}

pub extern "win64" fn _Smtx_unlock_exclusive(lock: *mut c_void) {
    sync::ReleaseSRWLockExclusive(lock);
}

pub extern "win64" fn _Smtx_unlock_shared(lock: *mut c_void) {
    sync::ReleaseSRWLockShared(lock);
}

/// The spin lock of `shared_ptr`'s atomic functions.
static SHARED_PTR_SPIN: AtomicBool = AtomicBool::new(false);

pub extern "win64" fn _Lock_shared_ptr_spin_lock() {
    while SHARED_PTR_SPIN
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        std::hint::spin_loop();
    }
}

pub extern "win64" fn _Unlock_shared_ptr_spin_lock() {
    SHARED_PTR_SPIN.store(false, Ordering::Release);
}

/// A recursive lock: owner and depth.
struct RecursiveLock {
    state: Mutex<(u32, u32)>,
    free: Condvar,
}

impl RecursiveLock {
    const fn new() -> RecursiveLock {
        RecursiveLock {
            state: Mutex::new((0, 0)),
            free: Condvar::new(),
        }
    }

    fn lock(&self) {
        let me = me();
        let mut st = lock(&self.state);
        while st.1 > 0 && st.0 != me {
            st = self.free.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        *st = (me, st.1 + 1);
    }

    fn unlock(&self) {
        let mut st = lock(&self.state);
        if st.1 == 0 {
            return;
        }
        st.1 -= 1;
        if st.1 == 0 {
            st.0 = 0;
            self.free.notify_one();
        }
    }
}

/// `_MAX_LOCK`: the `_Lockit` kinds (`_LOCK_LOCALE` 0, `_LOCK_MALLOC` 1,
/// `_LOCK_STREAM` 2, `_LOCK_DEBUG` 3, `_LOCK_AT_THREAD_EXIT` 4, ...).
const MAX_LOCK: usize = 8;
static LOCKITS: [RecursiveLock; MAX_LOCK] = [const { RecursiveLock::new() }; MAX_LOCK];

/// The `_Lockit` lock of `kind`; none out of range (as `xlock.cpp`).
fn lockit(kind: i32) -> Option<&'static RecursiveLock> {
    usize::try_from(kind).ok().and_then(|k| LOCKITS.get(k))
}

/// `_Lockit::_Lockit(int)`: the kind stored in the object, its lock taken.
extern "win64" fn lockit_new_kind(this: *mut i32, kind: i32) -> *mut i32 {
    lockit_ctor_this_kind(this, kind);
    this
}

/// `_Lockit::_Lockit()`: kind 0.
extern "win64" fn lockit_new(this: *mut i32) -> *mut i32 {
    lockit_ctor_this_kind(this, 0);
    this
}

/// `_Lockit::~_Lockit()`: the stored kind's lock released.
extern "win64" fn lockit_drop(this: *mut i32) {
    if !this.is_null() {
        // SAFETY: the title's `_Lockit`.
        lockit_dtor_kind(unsafe { this.read_unaligned() });
    }
}

/// `_Lockit::_Lockit_ctor(_Lockit*)`: kind 0.
extern "win64" fn lockit_ctor_this(this: *mut i32) {
    lockit_ctor_this_kind(this, 0);
}

/// `_Lockit::_Lockit_ctor(_Lockit*, int)`.
extern "win64" fn lockit_ctor_this_kind(this: *mut i32, kind: i32) {
    if !this.is_null() {
        // SAFETY: the title's `_Lockit`.
        unsafe { this.write_unaligned(kind) };
    }
    lockit_ctor_kind(kind);
}

/// `_Lockit::_Lockit_dtor(_Lockit*)`.
extern "win64" fn lockit_dtor_this(this: *mut i32) {
    lockit_drop(this);
}

/// `_Lockit::_Lockit_ctor(int)`: the lock of `kind` taken.
extern "win64" fn lockit_ctor_kind(kind: i32) {
    if let Some(l) = lockit(kind) {
        l.lock();
    }
}

/// `_Lockit::_Lockit_dtor(int)`: released.
extern "win64" fn lockit_dtor_kind(kind: i32) {
    if let Some(l) = lockit(kind) {
        l.unlock();
    }
}

// ---------------------------------------------------------------------------
// 2. The exceptions the STL throws.

/// `std::exception` as MSVC lays it out (x64).
#[repr(C)]
pub(crate) struct StdException {
    pub(crate) vftable: *const c_void,
    pub(crate) data: ExceptionData,
}

/// `std::error_code` (and `error_condition`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErrorCode {
    pub value: i32,
    pub pad: u32,
    pub category: *const c_void,
}

/// `_System_error` and `system_error`: `runtime_error` and the code.
#[repr(C)]
pub(crate) struct StdSystemError {
    pub(crate) base: StdException,
    pub(crate) code: ErrorCode,
}

/// `future_error`: `logic_error` and the code.
#[repr(C)]
pub(crate) struct StdFutureError {
    pub(crate) base: StdException,
    pub(crate) code: ErrorCode,
}

/// `regex_error`: `runtime_error` and its `error_type`.
#[repr(C)]
pub(crate) struct StdRegexError {
    pub(crate) base: StdException,
    pub(crate) code: i32,
}

const _: () = {
    assert!(size_of::<StdException>() == 24);
    assert!(size_of::<StdSystemError>() == 40);
    assert!(size_of::<StdFutureError>() == 40);
    assert!(size_of::<StdRegexError>() == 32);
    assert!(size_of::<ErrorCode>() == 16);
};

/// The classes, in [`CLASSES`]' order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
// `bad_cast`, `bad_typeid` and `bad_weak_ptr` have RTTI but no export of
// this slice throws them.
#[allow(dead_code)]
pub(crate) enum Kind {
    Exception,
    LogicError,
    RuntimeError,
    LengthError,
    OutOfRange,
    InvalidArgument,
    OverflowError,
    BadAlloc,
    BadArrayNewLength,
    BadFunctionCall,
    /// `_System_error`.
    SystemErrorBase,
    SystemError,
    FutureError,
    RegexError,
    BadCast,
    BadTypeid,
    BadWeakPtr,
    BadException,
}

struct Class {
    name: &'static str,
    parent: Option<Kind>,
    size: i32,
    /// Its catchable type's properties.
    properties: u32,
}

const fn class(name: &'static str, parent: Option<Kind>, size: i32) -> Class {
    Class {
        name,
        parent,
        size,
        properties: 0,
    }
}

const CLASS_COUNT: usize = 18;

/// Each class: its decorated name, its base and its size.
const CLASSES: [Class; CLASS_COUNT] = [
    class(".?AVexception@std@@", None, 24),
    class(".?AVlogic_error@std@@", Some(Kind::Exception), 24),
    class(".?AVruntime_error@std@@", Some(Kind::Exception), 24),
    class(".?AVlength_error@std@@", Some(Kind::LogicError), 24),
    class(".?AVout_of_range@std@@", Some(Kind::LogicError), 24),
    class(".?AVinvalid_argument@std@@", Some(Kind::LogicError), 24),
    class(".?AVoverflow_error@std@@", Some(Kind::RuntimeError), 24),
    // The compiler marks `std::bad_alloc`'s catchable type for the
    // handlers it makes `HT_IsBadAllocCompat`.
    Class {
        properties: CT_IS_STD_BAD_ALLOC,
        ..class(".?AVbad_alloc@std@@", Some(Kind::Exception), 24)
    },
    class(".?AVbad_array_new_length@std@@", Some(Kind::BadAlloc), 24),
    class(".?AVbad_function_call@std@@", Some(Kind::Exception), 24),
    class(".?AV_System_error@std@@", Some(Kind::RuntimeError), 40),
    class(".?AVsystem_error@std@@", Some(Kind::SystemErrorBase), 40),
    class(".?AVfuture_error@std@@", Some(Kind::LogicError), 40),
    class(".?AVregex_error@std@@", Some(Kind::RuntimeError), 32),
    class(".?AVbad_cast@std@@", Some(Kind::Exception), 24),
    class(".?AVbad_typeid@std@@", Some(Kind::Exception), 24),
    class(".?AVbad_weak_ptr@std@@", Some(Kind::Exception), 24),
    class(".?AVbad_exception@std@@", Some(Kind::Exception), 24),
];

/// The scalar deleting destructor of a vtable: (`this`, flags).
type DeletingDtor = unsafe extern "win64" fn(*mut c_void, u32) -> *mut c_void;
/// `what`.
type What = unsafe extern "win64" fn(*const c_void) -> *const c_char;
/// A copy constructor: (`this`, the source).
type CopyCtor = unsafe extern "win64" fn(*mut c_void, *const c_void) -> *mut c_void;

/// MSVC's vtable of `std::exception` and its classes, after the slot of
/// the complete object locator (0: none).
#[repr(C)]
struct ExceptionVtable {
    locator: usize,
    dtor: DeletingDtor,
    what: What,
}

/// One vtable per class (they share the two functions).
static VTABLES: [ExceptionVtable; CLASS_COUNT] = [const {
    ExceptionVtable {
        locator: 0,
        dtor: deleting_dtor,
        what: exception_what,
    }
}; CLASS_COUNT];

/// The vtable pointer of `kind`'s objects.
fn vtable(kind: usize) -> *const c_void {
    (&raw const VTABLES[kind].dtor).cast()
}

/// Slot 0: destroys the data; frees `this` (ucrt's `free`) when `flags & 1`.
unsafe extern "win64" fn deleting_dtor(this: *mut c_void, flags: u32) -> *mut c_void {
    // SAFETY: one of our objects.
    unsafe { destroy_object(this) };
    if flags & 1 != 0 {
        heap::free(this);
    }
    this
}

/// Slot 1: the message, or "Unknown exception".
unsafe extern "win64" fn exception_what(this: *const c_void) -> *const c_char {
    // SAFETY: one of our objects.
    let what = unsafe { (*this.cast::<StdException>()).data.what };
    if what.is_null() {
        c"Unknown exception".as_ptr()
    } else {
        what
    }
}

/// The destructor the throw infos name: the data destroyed.
unsafe extern "win64" fn destroy_object(this: *mut c_void) {
    if !this.is_null() {
        // SAFETY: one of our objects.
        unsafe { __std_exception_destroy(&raw mut (*this.cast::<StdException>()).data) };
    }
}

/// The copy constructor of class `K`: its vtable, the data copied
/// (`__std_exception_copy`), the fields after it as they are.
unsafe extern "win64" fn copy_object<const K: usize>(
    this: *mut c_void,
    from: *const c_void,
) -> *mut c_void {
    let size = CLASSES[K].size as usize;
    let to = this.cast::<StdException>();
    // SAFETY: the room for a class `K` object, and one of ours to copy.
    unsafe {
        (&raw mut (*to).vftable).write(vtable(K));
        __std_exception_copy(
            &raw const (*from.cast::<StdException>()).data,
            &raw mut (*to).data,
        );
        let header = size_of::<StdException>();
        ptr::copy_nonoverlapping(
            from.cast::<u8>().add(header),
            this.cast::<u8>().add(header),
            size - header,
        );
    }
    this
}

/// The copy constructors, by class.
const COPIES: [CopyCtor; CLASS_COUNT] = [
    copy_object::<0>,
    copy_object::<1>,
    copy_object::<2>,
    copy_object::<3>,
    copy_object::<4>,
    copy_object::<5>,
    copy_object::<6>,
    copy_object::<7>,
    copy_object::<8>,
    copy_object::<9>,
    copy_object::<10>,
    copy_object::<11>,
    copy_object::<12>,
    copy_object::<13>,
    copy_object::<14>,
    copy_object::<15>,
    copy_object::<16>,
    copy_object::<17>,
];

/// A `TypeDescriptor` with its name (the longest is 31 characters).
#[repr(C)]
struct Descriptor {
    vftable: *const c_void,
    spare: *const c_void,
    name: [u8; 32],
}

/// The RTTI of the classes, made once and kept: each class's
/// `TypeDescriptor` and absolute-form `ThrowInfoAbs`.
struct Rtti {
    // The tests look the descriptors up.
    #[cfg_attr(not(test), allow(dead_code))]
    descriptors: [usize; CLASS_COUNT],
    infos: [usize; CLASS_COUNT],
}

static RTTI: LazyLock<Rtti> = LazyLock::new(make_rtti);

fn make_rtti() -> Rtti {
    let descriptors: [usize; CLASS_COUNT] = std::array::from_fn(|k| {
        let mut name = [0u8; 32];
        name[..CLASSES[k].name.len()].copy_from_slice(CLASSES[k].name.as_bytes());
        let d = Box::leak(Box::new(Descriptor {
            vftable: ptr::null(),
            spare: ptr::null(),
            name,
        }));
        ptr::from_ref(d) as usize
    });
    let catchables: [usize; CLASS_COUNT] = std::array::from_fn(|k| {
        let ct = Box::leak(Box::new(CatchableTypeAbs {
            properties: CLASSES[k].properties,
            pad: 0,
            type_descriptor: descriptors[k] as *const TypeDescriptor,
            this_displacement: Pmd {
                mdisp: 0,
                pdisp: -1,
                vdisp: 0,
            },
            size_or_offset: CLASSES[k].size,
            copy_function: COPIES[k] as *const () as usize,
        }));
        ptr::from_ref(ct) as usize
    });
    let infos = std::array::from_fn(|k| {
        let mut types = [ptr::null(); 4];
        let mut count = 0;
        let mut at = Some(k);
        while let Some(c) = at {
            types[count] = catchables[c] as *const CatchableTypeAbs;
            count += 1;
            at = CLASSES[c].parent.map(|p| p as usize);
        }
        let array = Box::leak(Box::new(CatchableTypeArrayAbs { count, types }));
        let info = Box::leak(Box::new(ThrowInfoAbs {
            attributes: TI_VICTORY_ABSOLUTE,
            pad: 0,
            destructor: destroy_object as *const () as usize,
            forward_compat: 0,
            catchable_types: array,
        }));
        ptr::from_ref(info) as usize
    });
    Rtti { descriptors, infos }
}

/// The throw info of `kind`'s objects.
pub(crate) fn throw_info(kind: Kind) -> *const ThrowInfoAbs {
    RTTI.infos[kind as usize] as *const ThrowInfoAbs
}

/// The `TypeDescriptor` of `kind`.
#[cfg(test)]
fn descriptor(kind: Kind) -> *const TypeDescriptor {
    RTTI.descriptors[kind as usize] as *const TypeDescriptor
}

/// Room for any of the objects (the largest is 40 bytes).
#[repr(C, align(16))]
pub(crate) struct Object(pub(crate) [u64; 5]);

impl Object {
    pub(crate) fn as_ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }

    /// The fields after the `std::exception` part.
    fn code(&mut self) -> *mut ErrorCode {
        // SAFETY: within the object.
        unsafe {
            self.as_ptr()
                .cast::<u8>()
                .add(size_of::<StdException>())
                .cast()
        }
    }
}

/// An object of `kind` with `message` (copied, owned; null: none), and its
/// throw info.
pub(crate) fn make_exception(kind: Kind, message: *const c_char) -> (Object, *const ThrowInfoAbs) {
    let mut object = Object([0; 5]);
    let e = object.as_ptr().cast::<StdException>();
    let from = ExceptionData {
        what: message,
        do_free: 1,
    };
    // SAFETY: room for the object; the title's (or our) message.
    unsafe {
        (&raw mut (*e).vftable).write(vtable(kind as usize));
        __std_exception_copy(&from, &raw mut (*e).data);
    }
    (object, throw_info(kind))
}

/// An object of `kind` whose message is the literal `message`, not owned
/// (`exception(const char*, int)`, as `bad_alloc` and its kin are made).
fn make_literal(kind: Kind, message: &'static CStr) -> (Object, *const ThrowInfoAbs) {
    let (mut object, info) = make_exception(kind, ptr::null());
    let e = object.as_ptr().cast::<StdException>();
    // SAFETY: the object just made, its data empty.
    unsafe {
        (&raw mut (*e).data).write(ExceptionData {
            what: message.as_ptr(),
            do_free: 0,
        });
    }
    (object, info)
}

/// An object of `kind` with the message `text` (copied, owned).
fn make_text(kind: Kind, text: &str) -> (Object, *const ThrowInfoAbs) {
    let text = CString::new(text.replace('\0', "")).unwrap_or_default();
    make_exception(kind, text.as_ptr())
}

/// Throws `object` from the shim's context `ctx`. It lives in this frame,
/// which the throw never returns to, as a throw's object lives in its
/// thrower's frame.
///
/// # Safety
/// `ctx` is a throwing shim's context; `info` describes `object`.
unsafe fn throw_object(mut object: Object, info: *const ThrowInfoAbs, ctx: *mut Context) -> ! {
    // SAFETY: the caller's.
    unsafe { cxx::throw_absolute(object.as_ptr(), info, &mut *ctx) }
}

/// Defines a throwing export: a naked function with the prologue of
/// `cxx`'s `_CxxThrowException` (the caller's context captured, as it is
/// when the export returns), which calls `work(first argument, second
/// argument, context)`, an `unsafe extern "sysv64" fn(usize, usize, *mut
/// Context) -> !`.
macro_rules! throwing_shim {
    ($(#[$meta:meta])* $name:ident($($arg:ident: $ty:ty),*) => $work:path) => {
        $(#[$meta])*
        ///
        /// # Safety
        /// Called from title code, with the arguments it names.
        #[unsafe(naked)]
        pub unsafe extern "win64" fn $name($($arg: $ty),*) {
            naked_asm!(
                "push rbp",
                "mov rbp, rsp",
                "sub rsp, {frame}",
                "mov [rbp - 8], rcx",
                "mov [rbp - 16], rdx",
                "mov [rbp - 24], r8",
                "mov [rbp - 32], r9",
                "lea rcx, [rsp + 32]",
                "call {capture}",
                // The caller's rip, rsp and rbp, in place of this frame's.
                "mov rax, [rbp + 8]",
                "mov [rsp + 32 + 0xF8], rax",
                "lea rax, [rbp + 16]",
                "mov [rsp + 32 + 0x98], rax",
                "mov rax, [rbp]",
                "mov [rsp + 32 + 0xA0], rax",
                "mov rdi, [rbp - 8]",
                "mov rsi, [rbp - 16]",
                "lea rdx, [rsp + 32]",
                "call {work}",
                "ud2",
                frame = const cxx::SHIM_FRAME,
                capture = sym RtlCaptureContext,
                work = sym $work,
            )
        }
    };
}

/// Throws `kind` with the title's message (null: none).
///
/// # Safety
/// As [`throw_object`]; `message` is null or NUL-terminated.
unsafe fn throw_message(kind: Kind, message: usize, ctx: *mut Context) -> ! {
    let (object, info) = make_exception(kind, message as *const c_char);
    // SAFETY: the caller's.
    unsafe { throw_object(object, info, ctx) }
}

unsafe extern "sysv64" fn x_bad_alloc(_: usize, _: usize, ctx: *mut Context) -> ! {
    let (object, info) = make_literal(Kind::BadAlloc, c"bad allocation");
    // SAFETY: the shim's context.
    unsafe { throw_object(object, info, ctx) }
}

unsafe extern "sysv64" fn x_bad_function_call(_: usize, _: usize, ctx: *mut Context) -> ! {
    let (object, info) = make_literal(Kind::BadFunctionCall, c"bad function call");
    // SAFETY: the shim's context.
    unsafe { throw_object(object, info, ctx) }
}

unsafe extern "sysv64" fn x_invalid_argument(message: usize, _: usize, ctx: *mut Context) -> ! {
    // SAFETY: the shim's context and the title's message.
    unsafe { throw_message(Kind::InvalidArgument, message, ctx) }
}

unsafe extern "sysv64" fn x_length_error(message: usize, _: usize, ctx: *mut Context) -> ! {
    // SAFETY: as above.
    unsafe { throw_message(Kind::LengthError, message, ctx) }
}

unsafe extern "sysv64" fn x_out_of_range(message: usize, _: usize, ctx: *mut Context) -> ! {
    // SAFETY: as above.
    unsafe { throw_message(Kind::OutOfRange, message, ctx) }
}

unsafe extern "sysv64" fn x_overflow_error(message: usize, _: usize, ctx: *mut Context) -> ! {
    // SAFETY: as above.
    unsafe { throw_message(Kind::OverflowError, message, ctx) }
}

unsafe extern "sysv64" fn x_runtime_error(message: usize, _: usize, ctx: *mut Context) -> ! {
    // SAFETY: as above.
    unsafe { throw_message(Kind::RuntimeError, message, ctx) }
}

/// `regex_error`'s messages, by `error_type` (`<regex>`'s `_Stringify`).
const REGEX_MESSAGES: [&str; 15] = [
    "regex_error(error_collate): The expression contained an invalid collating element name.",
    "regex_error(error_ctype): The expression contained an invalid character class name.",
    "regex_error(error_escape): The expression contained an invalid escaped character, or a trailing escape.",
    "regex_error(error_backref): The expression contained an invalid back reference.",
    "regex_error(error_brack): The expression contained mismatched [ and ].",
    "regex_error(error_paren): The expression contained mismatched ( and ).",
    "regex_error(error_brace): The expression contained mismatched { and }.",
    "regex_error(error_badbrace): The expression contained an invalid range in a { expression }.",
    "regex_error(error_range): The expression contained an invalid character range, such as [b-a] in most encodings.",
    "regex_error(error_space): There was insufficient memory to convert the expression into a finite state machine.",
    "regex_error(error_badrepeat): One of *?+{ was not preceded by a valid regular expression.",
    "regex_error(error_complexity): The complexity of an attempted match against a regular expression exceeded a pre-set level.",
    "regex_error(error_stack): There was insufficient memory to determine whether the regular expression could match the specified character sequence.",
    "regex_error(error_parse)",
    "regex_error(error_syntax)",
];

/// A `regex_error` of `code` and its message.
pub(crate) fn make_regex_error(code: i32) -> (Object, *const ThrowInfoAbs) {
    let text = usize::try_from(code)
        .ok()
        .and_then(|i| REGEX_MESSAGES.get(i))
        .copied()
        .unwrap_or("regex_error");
    let (mut object, info) = make_text(Kind::RegexError, text);
    // SAFETY: a `regex_error`'s room.
    unsafe { object.code().cast::<i32>().write(code) };
    (object, info)
}

unsafe extern "sysv64" fn x_regex_error(code: usize, _: usize, ctx: *mut Context) -> ! {
    let (object, info) = make_regex_error(code as i32);
    // SAFETY: the shim's context.
    unsafe { throw_object(object, info, ctx) }
}

/// `errc` values.
const ERRC_RESOURCE_UNAVAILABLE_TRY_AGAIN: i32 = 11;
const ERRC_DEVICE_OR_RESOURCE_BUSY: i32 = 16;
const ERRC_INVALID_ARGUMENT: i32 = 22;

/// A `system_error` of the generic category's `errc`, whose message is the
/// category's text (as `system_error(code, "")` makes it).
pub(crate) fn make_system_error(errc: i32) -> (Object, *const ThrowInfoAbs) {
    let (mut object, info) = make_text(Kind::SystemError, &category_message(GENERIC_ADDR, errc));
    // SAFETY: a `system_error`'s room.
    unsafe {
        object.code().write(ErrorCode {
            value: errc,
            pad: 0,
            category: generic(),
        });
    }
    (object, info)
}

/// `_Thrd_result` to `system_error`, as `thread0.cpp` maps them: no memory
/// and a timeout are `resource_unavailable_try_again`, busy is
/// `device_or_resource_busy`, an error is `invalid_argument`; `_Thrd_success`
/// (which the STL aborts on) ends the title, as a `[[noreturn]]` function
/// returning would, less clearly.
unsafe extern "sysv64" fn throw_c_error(code: usize, _: usize, ctx: *mut Context) -> ! {
    let errc = match code as i32 {
        THRD_NOMEM | THRD_TIMEDOUT => ERRC_RESOURCE_UNAVAILABLE_TRY_AGAIN,
        THRD_BUSY => ERRC_DEVICE_OR_RESOURCE_BUSY,
        THRD_ERROR => ERRC_INVALID_ARGUMENT,
        other => {
            tracing::error!("_Throw_C_error({other}): not a _Thrd_result to throw for");
            cxx::__std_terminate()
        }
    };
    let (object, info) = make_system_error(errc);
    // SAFETY: the shim's context.
    unsafe { throw_object(object, info, ctx) }
}

/// `_Cpp_error` (`_DEVICE_OR_RESOURCE_BUSY` 0 ... `_RESOURCE_UNAVAILABLE_TRY_AGAIN`
/// 6) to `errc`.
const CPP_ERRORS: [i32; 7] = [16, 22, 3, 12, 1, 36, 11];

unsafe extern "sysv64" fn throw_cpp_error(code: usize, _: usize, ctx: *mut Context) -> ! {
    let Some(&errc) = CPP_ERRORS.get(code as u32 as usize) else {
        tracing::error!("_Throw_Cpp_error({}): not a _Cpp_error", code as i32);
        cxx::__std_terminate()
    };
    let (object, info) = make_system_error(errc);
    // SAFETY: the shim's context.
    unsafe { throw_object(object, info, ctx) }
}

/// `future_errc`'s texts (1 to 4).
const FUTURE_MESSAGES: [&CStr; 4] = [
    c"broken promise",
    c"future already retrieved",
    c"promise already satisfied",
    c"no state",
];

/// `_Future_error_map`: the text of a `future_errc`; None for another value.
fn future_text(value: i32) -> Option<&'static CStr> {
    usize::try_from(value)
        .ok()
        .and_then(|v| v.checked_sub(1))
        .and_then(|i| FUTURE_MESSAGES.get(i))
        .copied()
}

/// A `future_error` of `code`, its message the future category's.
pub(crate) fn make_future_error(code: ErrorCode) -> (Object, *const ThrowInfoAbs) {
    let message = future_text(code.value).map_or(ptr::null(), CStr::as_ptr);
    let (mut object, info) = make_exception(Kind::FutureError, message);
    // SAFETY: a `future_error`'s room.
    unsafe { object.code().write(code) };
    (object, info)
}

unsafe extern "sysv64" fn throw_future_error(code: usize, _: usize, ctx: *mut Context) -> ! {
    let code = code as *const ErrorCode;
    let code = if code.is_null() {
        ErrorCode {
            value: 0,
            pad: 0,
            category: future(),
        }
    } else {
        // SAFETY: the title's `error_code`.
        unsafe { code.read_unaligned() }
    };
    let (object, info) = make_future_error(code);
    // SAFETY: the shim's context.
    unsafe { throw_object(object, info, ctx) }
}

unsafe extern "sysv64" fn rethrow_future_exception(ptr: usize, _: usize, ctx: *mut Context) -> ! {
    // SAFETY: the title's `exception_ptr`, passed by value (by pointer to
    // its copy, which this callee owns); the shim's context.
    unsafe { rethrow(ptr as *mut ExceptionPtr, true, ctx) }
}

throwing_shim! {
    /// `std::_Xbad_alloc`: throws `bad_alloc`.
    x_bad_alloc_shim() => x_bad_alloc
}
throwing_shim! {
    /// `std::_Xbad_function_call`: throws `bad_function_call`.
    x_bad_function_call_shim() => x_bad_function_call
}
throwing_shim! {
    /// `std::_Xinvalid_argument`: throws `invalid_argument` with `message`.
    x_invalid_argument_shim(message: *const c_char) => x_invalid_argument
}
throwing_shim! {
    /// `std::_Xlength_error`: throws `length_error` with `message`.
    x_length_error_shim(message: *const c_char) => x_length_error
}
throwing_shim! {
    /// `std::_Xout_of_range`: throws `out_of_range` with `message`.
    x_out_of_range_shim(message: *const c_char) => x_out_of_range
}
throwing_shim! {
    /// `std::_Xoverflow_error`: throws `overflow_error` with `message`.
    x_overflow_error_shim(message: *const c_char) => x_overflow_error
}
throwing_shim! {
    /// `std::_Xruntime_error`: throws `runtime_error` with `message`.
    x_runtime_error_shim(message: *const c_char) => x_runtime_error
}
throwing_shim! {
    /// `std::_Xregex_error`: throws `regex_error` of `code`.
    x_regex_error_shim(code: i32) => x_regex_error
}
throwing_shim! {
    /// `std::_Throw_C_error`: a `_Thrd_result` as a `system_error`.
    throw_c_error_shim(code: i32) => throw_c_error
}
throwing_shim! {
    /// `std::_Throw_Cpp_error`: a `_Cpp_error` as a `system_error`.
    throw_cpp_error_shim(code: i32) => throw_cpp_error
}
throwing_shim! {
    /// `std::_Throw_future_error`: throws `future_error` of `code`.
    throw_future_error_shim(code: *const ErrorCode) => throw_future_error
}
throwing_shim! {
    /// `std::_Rethrow_future_exception`: rethrows the `exception_ptr`.
    rethrow_future_exception_shim(ptr: *mut ExceptionPtr) => rethrow_future_exception
}
throwing_shim! {
    /// `std::rethrow_exception`: rethrows the `exception_ptr`.
    __ExceptionPtrRethrow(ptr: *const ExceptionPtr) => exception_ptr_rethrow
}

/// MSVC's `std::string` (`basic_string<char>`, x64): the text in place when
/// it is under 16 bytes, else a pointer to it; its size and capacity.
#[repr(C)]
pub(crate) struct MsvcString {
    pub(crate) bx: [u8; 16],
    pub(crate) size: usize,
    pub(crate) res: usize,
}

/// The longest text we put in a heap `std::string`: below MSVC's 4096-byte
/// "big allocation", whose block has a header ours lacks.
const STRING_LIMIT: usize = 4000;

/// Writes `text` into `out` as `xstring` makes a string of it: in place
/// under 16 bytes (capacity 15), else a block of `capacity + 1` bytes
/// (`operator new`, ucrt's `malloc` here) with the capacity `len | 15`.
///
/// # Safety
/// `out` is writable for a string.
unsafe fn write_string(out: *mut MsvcString, text: &[u8]) {
    let n = text.len().min(STRING_LIMIT);
    let mut s = MsvcString {
        bx: [0; 16],
        size: n,
        res: 15,
    };
    if n < 16 {
        s.bx[..n].copy_from_slice(&text[..n]);
    } else {
        let res = n | 15;
        let block = heap::malloc(res + 1).cast::<u8>();
        if block.is_null() {
            s.size = 0;
        } else {
            // SAFETY: a fresh block of `res + 1 > n` bytes.
            unsafe {
                ptr::copy_nonoverlapping(text.as_ptr(), block, n);
                block.add(n).write(0);
            }
            s.bx[..8].copy_from_slice(&(block as usize).to_ne_bytes());
            s.res = res;
        }
    }
    // SAFETY: the caller's.
    unsafe { out.write_unaligned(s) };
}

/// The text of a string [`write_string`] wrote.
#[cfg(test)]
fn read_string(s: &MsvcString) -> Vec<u8> {
    if s.res < 16 {
        s.bx[..s.size].to_vec()
    } else {
        let at = usize::from_ne_bytes(s.bx[..8].try_into().unwrap()) as *const u8;
        // SAFETY: the block the string names.
        unsafe { std::slice::from_raw_parts(at, s.size) }.to_vec()
    }
}

/// `error_category`'s `_Addr` values of the STL's categories.
const FUTURE_ADDR: usize = 1;
const GENERIC_ADDR: usize = 3;
const SYSTEM_ADDR: usize = 7;

/// MSVC's `error_category` vtable: the destructor, `name`, `message`,
/// `default_error_condition` and the two `equivalent`s, after the slot of
/// the complete object locator (0). MSVC lays overloads out together in the
/// reverse order of their declaration, so `equivalent(const error_code&,
/// int)`, declared second, comes first.
#[repr(C)]
struct CategoryVtable {
    locator: usize,
    dtor: DeletingDtor,
    name: unsafe extern "win64" fn(*const c_void) -> *const c_char,
    /// (`this`, the hidden return pointer, the value): a member function
    /// returns a class through the pointer after `this`.
    message: unsafe extern "win64" fn(*const c_void, *mut MsvcString, i32) -> *mut MsvcString,
    default_error_condition:
        unsafe extern "win64" fn(*const c_void, *mut ErrorCode, i32) -> *mut ErrorCode,
    equivalent_code: unsafe extern "win64" fn(*const c_void, *const ErrorCode, i32) -> u8,
    equivalent_condition: unsafe extern "win64" fn(*const c_void, i32, *const ErrorCode) -> u8,
}

/// An `error_category`: its vtable and `_Addr`.
#[repr(C)]
pub(crate) struct Category {
    vftable: *const c_void,
    addr: usize,
}

// SAFETY: read-only.
unsafe impl Sync for Category {}

const fn category_vtable(
    name: unsafe extern "win64" fn(*const c_void) -> *const c_char,
) -> CategoryVtable {
    CategoryVtable {
        locator: 0,
        dtor: category_dtor,
        name,
        message: category_message_export,
        default_error_condition: category_default_condition,
        equivalent_code: category_equivalent_code,
        equivalent_condition: category_equivalent_condition,
    }
}

static GENERIC_VTABLE: CategoryVtable = category_vtable(generic_name);
static SYSTEM_VTABLE: CategoryVtable = category_vtable(system_name);
static FUTURE_VTABLE: CategoryVtable = category_vtable(future_name);

static GENERIC_CATEGORY: Category = Category {
    vftable: (&raw const GENERIC_VTABLE.dtor).cast(),
    addr: GENERIC_ADDR,
};
static SYSTEM_CATEGORY: Category = Category {
    vftable: (&raw const SYSTEM_VTABLE.dtor).cast(),
    addr: SYSTEM_ADDR,
};
static FUTURE_CATEGORY: Category = Category {
    vftable: (&raw const FUTURE_VTABLE.dtor).cast(),
    addr: FUTURE_ADDR,
};

fn generic() -> *const c_void {
    (&raw const GENERIC_CATEGORY).cast()
}

fn future() -> *const c_void {
    (&raw const FUTURE_CATEGORY).cast()
}

/// A category's `_Addr` (ours or the title's: the same layout).
///
/// # Safety
/// `category` is an `error_category`.
unsafe fn addr_of_category(category: *const c_void) -> usize {
    if category.is_null() {
        return 0;
    }
    // SAFETY: the caller's.
    unsafe { (*category.cast::<Category>()).addr }
}

/// Our categories are static: nothing to destroy, nothing to free.
unsafe extern "win64" fn category_dtor(this: *mut c_void, _flags: u32) -> *mut c_void {
    this
}

unsafe extern "win64" fn generic_name(_this: *const c_void) -> *const c_char {
    c"generic".as_ptr()
}

unsafe extern "win64" fn system_name(_this: *const c_void) -> *const c_char {
    c"system".as_ptr()
}

unsafe extern "win64" fn future_name(_this: *const c_void) -> *const c_char {
    c"future".as_ptr()
}

/// The message of `value` in the category of `_Addr` `addr`: the generic
/// one's `_Syserror_map`, the system one's `_Winerror_message`, the future
/// one's `future_errc` text (else `_Syserror_map`).
fn category_message(addr: usize, value: i32) -> String {
    match addr {
        SYSTEM_ADDR => winerror_text(value as u32),
        FUTURE_ADDR => future_text(value).map_or_else(
            || syserror_text(value).to_owned(),
            |t| t.to_string_lossy().into_owned(),
        ),
        _ => syserror_text(value).to_owned(),
    }
}

unsafe extern "win64" fn category_message_export(
    this: *const c_void,
    out: *mut MsvcString,
    value: i32,
) -> *mut MsvcString {
    // SAFETY: one of our categories; the caller's place for the string.
    unsafe {
        let text = category_message(addr_of_category(this), value);
        write_string(out, text.as_bytes());
    }
    out
}

/// The condition of `value`: `value` with this category; for the system
/// category, the generic `errc` `_Winerror_map` gives it, when it gives one
/// (`_System_error_category::default_error_condition`).
unsafe extern "win64" fn category_default_condition(
    this: *const c_void,
    out: *mut ErrorCode,
    value: i32,
) -> *mut ErrorCode {
    // SAFETY: one of our categories, and the caller's place for the condition.
    unsafe {
        let condition = match (addr_of_category(this), _Winerror_map(value)) {
            (SYSTEM_ADDR, errc) if errc != 0 => ErrorCode {
                value: errc,
                pad: 0,
                category: generic(),
            },
            _ => ErrorCode {
                value,
                pad: 0,
                category: this,
            },
        };
        out.write_unaligned(condition);
    }
    out
}

/// Whether `condition` is this category's with `value`.
unsafe extern "win64" fn category_equivalent_condition(
    this: *const c_void,
    value: i32,
    condition: *const ErrorCode,
) -> u8 {
    if condition.is_null() {
        return 0;
    }
    // SAFETY: the title's condition and the categories.
    unsafe {
        let c = condition.read_unaligned();
        u8::from(addr_of_category(c.category) == addr_of_category(this) && c.value == value)
    }
}

/// Whether `code` is this category's with `value`.
unsafe extern "win64" fn category_equivalent_code(
    this: *const c_void,
    code: *const ErrorCode,
    value: i32,
) -> u8 {
    // SAFETY: as above.
    unsafe { category_equivalent_condition(this, value, code) }
}

/// `std::generic_category()`.
pub extern "win64" fn generic_category() -> *const c_void {
    generic()
}

/// `std::system_category()`.
pub extern "win64" fn system_category() -> *const c_void {
    (&raw const SYSTEM_CATEGORY).cast()
}

/// `std::future_category()`.
pub extern "win64" fn future_category() -> *const c_void {
    future()
}

/// The `strerror` text of an `errno` value; "unknown error" past the table.
fn syserror_text(code: i32) -> &'static str {
    if (0..=42).contains(&code) {
        runtime::message(code)
    } else {
        "unknown error"
    }
}

/// [`syserror_text`] of 0 to 42 and "unknown error", NUL-terminated.
static SYSERROR: LazyLock<Vec<CString>> = LazyLock::new(|| {
    (0..=43)
        .map(|c| CString::new(syserror_text(c)).unwrap_or_default())
        .collect()
});

/// `std::_Syserror_map`: the text of an `errno` value.
pub extern "win64" fn _Syserror_map(code: i32) -> *const c_char {
    let i = usize::try_from(code)
        .ok()
        .filter(|&c| c <= 42)
        .unwrap_or(43);
    SYSERROR[i].as_ptr()
}

/// `syserror.cpp`'s map of Win32 errors to `errc`.
const WINERROR_MAP: [(u32, i32); 85] = [
    (1, 40),      // ERROR_INVALID_FUNCTION: function_not_supported
    (2, 2),       // ERROR_FILE_NOT_FOUND
    (3, 2),       // ERROR_PATH_NOT_FOUND
    (4, 24),      // ERROR_TOO_MANY_OPEN_FILES
    (5, 13),      // ERROR_ACCESS_DENIED
    (6, 22),      // ERROR_INVALID_HANDLE
    (8, 12),      // ERROR_NOT_ENOUGH_MEMORY
    (12, 13),     // ERROR_INVALID_ACCESS
    (14, 12),     // ERROR_OUTOFMEMORY
    (15, 19),     // ERROR_INVALID_DRIVE: no_such_device
    (16, 13),     // ERROR_CURRENT_DIRECTORY
    (17, 18),     // ERROR_NOT_SAME_DEVICE: cross_device_link
    (19, 13),     // ERROR_WRITE_PROTECT
    (20, 19),     // ERROR_BAD_UNIT: no_such_device
    (21, 11),     // ERROR_NOT_READY
    (25, 5),      // ERROR_SEEK: io_error
    (29, 5),      // ERROR_WRITE_FAULT
    (30, 5),      // ERROR_READ_FAULT
    (32, 13),     // ERROR_SHARING_VIOLATION
    (33, 39),     // ERROR_LOCK_VIOLATION: no_lock_available
    (39, 28),     // ERROR_HANDLE_DISK_FULL
    (50, 40),     // ERROR_NOT_SUPPORTED
    (53, 2),      // ERROR_BAD_NETPATH
    (55, 19),     // ERROR_DEV_NOT_EXIST
    (67, 2),      // ERROR_BAD_NET_NAME
    (80, 17),     // ERROR_FILE_EXISTS
    (82, 13),     // ERROR_CANNOT_MAKE
    (87, 22),     // ERROR_INVALID_PARAMETER
    (109, 32),    // ERROR_BROKEN_PIPE
    (110, 5),     // ERROR_OPEN_FAILED
    (111, 38),    // ERROR_BUFFER_OVERFLOW: filename_too_long
    (112, 28),    // ERROR_DISK_FULL
    (121, 138),   // ERROR_SEM_TIMEOUT
    (123, 2),     // ERROR_INVALID_NAME
    (131, 22),    // ERROR_NEGATIVE_SEEK
    (132, 29),    // ERROR_SEEK_ON_DEVICE
    (142, 16),    // ERROR_BUSY_DRIVE
    (145, 41),    // ERROR_DIR_NOT_EMPTY
    (170, 16),    // ERROR_BUSY
    (183, 17),    // ERROR_ALREADY_EXISTS
    (206, 38),    // ERROR_FILENAME_EXCED_RANGE
    (212, 39),    // ERROR_LOCKED
    (258, 138),   // WAIT_TIMEOUT
    (267, 22),    // ERROR_DIRECTORY
    (995, 105),   // ERROR_OPERATION_ABORTED: operation_canceled
    (998, 13),    // ERROR_NOACCESS
    (1011, 5),    // ERROR_CANTOPEN
    (1012, 5),    // ERROR_CANTREAD
    (1013, 5),    // ERROR_CANTWRITE
    (1113, 42),   // ERROR_NO_UNICODE_TRANSLATION: illegal_byte_sequence
    (1223, 105),  // ERROR_CANCELLED
    (1237, 11),   // ERROR_RETRY
    (2401, 16),   // ERROR_OPEN_FILES
    (2404, 16),   // ERROR_DEVICE_IN_USE
    (10004, 4),   // WSAEINTR: interrupted
    (10009, 9),   // WSAEBADF: bad_file_descriptor
    (10013, 13),  // WSAEACCES
    (10014, 14),  // WSAEFAULT: bad_address
    (10022, 22),  // WSAEINVAL
    (10024, 24),  // WSAEMFILE
    (10035, 140), // WSAEWOULDBLOCK: operation_would_block
    (10036, 112), // WSAEINPROGRESS: operation_in_progress
    (10037, 103), // WSAEALREADY: connection_already_in_progress
    (10038, 128), // WSAENOTSOCK: not_a_socket
    (10039, 109), // WSAEDESTADDRREQ: destination_address_required
    (10040, 115), // WSAEMSGSIZE: message_size
    (10041, 136), // WSAEPROTOTYPE: wrong_protocol_type
    (10042, 123), // WSAENOPROTOOPT: no_protocol_option
    (10043, 135), // WSAEPROTONOSUPPORT: protocol_not_supported
    (10045, 130), // WSAEOPNOTSUPP: operation_not_supported
    (10047, 102), // WSAEAFNOSUPPORT: address_family_not_supported
    (10048, 100), // WSAEADDRINUSE: address_in_use
    (10049, 101), // WSAEADDRNOTAVAIL: address_not_available
    (10050, 116), // WSAENETDOWN: network_down
    (10051, 118), // WSAENETUNREACH: network_unreachable
    (10052, 117), // WSAENETRESET: network_reset
    (10053, 106), // WSAECONNABORTED: connection_aborted
    (10054, 108), // WSAECONNRESET: connection_reset
    (10055, 119), // WSAENOBUFS: no_buffer_space
    (10056, 113), // WSAEISCONN: already_connected
    (10057, 126), // WSAENOTCONN: not_connected
    (10060, 138), // WSAETIMEDOUT: timed_out
    (10061, 107), // WSAECONNREFUSED: connection_refused
    (10063, 38),  // WSAENAMETOOLONG: filename_too_long
    (10065, 110), // WSAEHOSTUNREACH: host_unreachable
];

/// `std::_Winerror_map`: a Win32 error's `errc` value; 0 when it has none.
pub extern "win64" fn _Winerror_map(code: i32) -> i32 {
    WINERROR_MAP
        .iter()
        .find(|&&(win, _)| win == code as u32)
        .map_or(0, |&(_, errc)| errc)
}

/// The text we give a Win32 error: there is no message table off Windows.
fn winerror_text(code: u32) -> String {
    format!("Windows error {code}")
}

/// `std::_Winerror_message`: the message of a Win32 error into the
/// buffer of `size` bytes (truncated, NUL-terminated); its length. Off
/// Windows there is no message table: the message is "Windows error
/// <code>".
pub extern "win64" fn _Winerror_message(code: u32, buffer: *mut c_char, size: u32) -> u32 {
    if buffer.is_null() || size == 0 {
        return 0;
    }
    let text = winerror_text(code);
    let n = text.len().min(size as usize - 1);
    // SAFETY: the title's buffer of `size > n` bytes.
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr(), buffer.cast::<u8>(), n);
        buffer.add(n).write(0);
    }
    n as u32
}

/// `std::uncaught_exception`.
pub extern "win64" fn uncaught_exception() -> u8 {
    cxx::__uncaught_exception()
}

/// `std::uncaught_exceptions`.
pub extern "win64" fn uncaught_exceptions() -> i32 {
    cxx::__uncaught_exceptions()
}

/// `std::_Random_device`: four bytes of the host's entropy (the time, if it
/// has none).
pub extern "win64" fn _Random_device() -> u32 {
    let mut bytes = [0u8; 4];
    // SAFETY: four writable bytes.
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } == 0 {
        return u32::from_ne_bytes(bytes);
    }
    let now = now_nanos();
    (now as u32) ^ ((now >> 32) as u32)
}

// ---------------------------------------------------------------------------
// 3. exception_ptr.

/// `std::exception_ptr`: our [`Shared`] and its object (null: empty).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ExceptionPtr {
    pub data1: *mut c_void,
    pub data2: *mut c_void,
}

/// An exception an `exception_ptr` holds: a copy of the thrown object (in
/// `thrown`, ours), its throw info and record.
struct Shared {
    refs: AtomicUsize,
    /// The exception's record, its object parameter the copy's (kept as
    /// `excptptr.cpp` keeps it; the tests read it).
    #[cfg_attr(not(test), allow(dead_code))]
    record: ExceptionRecord,
    thrown: Thrown,
}

/// A copy of `t`'s object in a block of ours (`malloc`): through its most
/// derived type's copy constructor, else its bytes; null when there is no
/// room.
///
/// # Safety
/// `t`'s object is alive, its throw info mapped.
unsafe fn copy_object_of(t: &Thrown) -> *mut c_void {
    // SAFETY: the caller's.
    let size = unsafe { t.size() };
    let block = heap::malloc(size.max(1));
    if !block.is_null() {
        // SAFETY: as above; a block of `size` bytes.
        unsafe { t.copy_to(block as u64) };
    }
    block
}

/// An `exception_ptr` of a copy of `t` thrown with `record`; empty when
/// there is no room.
///
/// # Safety
/// As [`copy_object_of`].
unsafe fn share(t: Thrown, mut record: ExceptionRecord) -> ExceptionPtr {
    // SAFETY: the caller's.
    let object = unsafe { copy_object_of(&t) };
    if object.is_null() {
        return EMPTY;
    }
    record.information[1] = object as u64;
    let shared = Box::new(Shared {
        refs: AtomicUsize::new(1),
        record,
        thrown: Thrown {
            object: object as u64,
            ..t
        },
    });
    ExceptionPtr {
        data1: Box::into_raw(shared).cast(),
        data2: object,
    }
}

const EMPTY: ExceptionPtr = ExceptionPtr {
    data1: ptr::null_mut(),
    data2: ptr::null_mut(),
};

/// One more holder of `p`'s exception.
fn retain(p: &ExceptionPtr) {
    if let Some(shared) = shared_of(p) {
        shared.refs.fetch_add(1, Ordering::Relaxed);
    }
}

/// One holder fewer: the last destroys the object and frees it.
fn release(p: &ExceptionPtr) {
    let Some(shared) = shared_of(p) else {
        return;
    };
    if shared.refs.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    // SAFETY: the last holder: the box and its object are ours alone.
    unsafe {
        let shared = Box::from_raw(p.data1.cast::<Shared>());
        shared.thrown.destroy();
        heap::free(shared.thrown.object as *mut c_void);
    }
}

fn shared_of(p: &ExceptionPtr) -> Option<&Shared> {
    // SAFETY: null or a box of ours, alive while `p` holds it.
    unsafe { p.data1.cast::<Shared>().as_ref() }
}

/// The title's `exception_ptr`.
fn read_ptr(p: *const ExceptionPtr) -> ExceptionPtr {
    if p.is_null() {
        return EMPTY;
    }
    // SAFETY: the title's `exception_ptr`.
    unsafe { p.read_unaligned() }
}

fn write_ptr(p: *mut ExceptionPtr, value: ExceptionPtr) {
    if !p.is_null() {
        // SAFETY: the title's `exception_ptr`.
        unsafe { p.write_unaligned(value) };
    }
}

/// An empty `exception_ptr`.
pub extern "win64" fn __ExceptionPtrCreate(p: *mut ExceptionPtr) {
    write_ptr(p, EMPTY);
}

/// Lets go of the exception.
pub extern "win64" fn __ExceptionPtrDestroy(p: *mut ExceptionPtr) {
    release(&read_ptr(p));
    write_ptr(p, EMPTY);
}

/// `dst` (not yet made) holds `src`'s exception too.
pub extern "win64" fn __ExceptionPtrCopy(dst: *mut ExceptionPtr, src: *const ExceptionPtr) {
    let src = read_ptr(src);
    retain(&src);
    write_ptr(dst, src);
}

/// `dst` lets go of its exception and holds `src`'s.
pub extern "win64" fn __ExceptionPtrAssign(dst: *mut ExceptionPtr, src: *const ExceptionPtr) {
    let src = read_ptr(src);
    retain(&src);
    let old = read_ptr(dst);
    write_ptr(dst, src);
    release(&old);
}

/// Whether the two hold the same exception.
pub extern "win64" fn __ExceptionPtrCompare(a: *const ExceptionPtr, b: *const ExceptionPtr) -> u8 {
    u8::from(read_ptr(a).data1 == read_ptr(b).data1)
}

/// Whether it holds an exception.
pub extern "win64" fn __ExceptionPtrToBool(p: *const ExceptionPtr) -> u8 {
    u8::from(!read_ptr(p).data1.is_null())
}

pub extern "win64" fn __ExceptionPtrSwap(a: *mut ExceptionPtr, b: *mut ExceptionPtr) {
    let (x, y) = (read_ptr(a), read_ptr(b));
    write_ptr(a, y);
    write_ptr(b, x);
}

/// `std::current_exception`: a copy of the C++ exception being handled on
/// this thread; empty when there is none or it is not a C++ exception.
pub extern "win64" fn __ExceptionPtrCurrentException(p: *mut ExceptionPtr) {
    let value = match cxx::current_cxx() {
        // SAFETY: the exception being handled, its object alive.
        Some((record, t)) => unsafe { share(t, record) },
        None => EMPTY,
    };
    write_ptr(p, value);
}

/// `std::make_exception_ptr`: a copy of `object`, made through `info`'s most
/// derived type.
pub extern "win64" fn __ExceptionPtrCopyException(
    p: *mut ExceptionPtr,
    object: *const c_void,
    info: *const c_void,
) {
    if object.is_null() || info.is_null() {
        write_ptr(p, EMPTY);
        return;
    }
    let t = cxx::thrown_from(object.cast_mut(), info);
    let mut record = ExceptionRecord::new(EH_EXCEPTION_NUMBER, EXCEPTION_NONCONTINUABLE, 0);
    record.number_parameters = 4;
    record.information[..4].copy_from_slice(&[
        u64::from(EH_MAGIC_NUMBER1),
        t.object,
        t.info,
        t.base,
    ]);
    // SAFETY: the title's object and its throw info.
    write_ptr(p, unsafe { share(t, record) });
}

/// The room a rethrow copies an object into, in the rethrowing frame.
const RETHROW_ROOM: usize = 1024;

/// Rethrows `p`'s exception from `ctx`: a copy of its object (so that the
/// catch's destructor does not take the shared one), in this frame (or, a
/// larger one, a block that is never freed); `owned`: `p` is let go of.
/// An empty one throws `bad_exception`.
///
/// # Safety
/// As [`throw_object`].
unsafe fn rethrow(p: *mut ExceptionPtr, owned: bool, ctx: *mut Context) -> ! {
    let value = read_ptr(p);
    let Some(shared) = shared_of(&value) else {
        let (object, info) = make_literal(Kind::BadException, c"bad exception");
        // SAFETY: the caller's.
        unsafe { throw_object(object, info, ctx) }
    };
    let t = shared.thrown;
    let mut room = [MaybeUninit::<u128>::uninit(); RETHROW_ROOM / 16];
    // SAFETY: the shared object, alive while `value` holds it.
    let size = unsafe { t.size() };
    let at = if size <= RETHROW_ROOM {
        room.as_mut_ptr().cast::<c_void>()
    } else {
        heap::malloc(size)
    };
    // SAFETY: room for the object.
    unsafe { t.copy_to(at as u64) };
    if owned {
        release(&value);
    }
    let copy = Thrown {
        object: at as u64,
        ..t
    };
    // SAFETY: the caller's; the copy lives in this frame, which the throw
    // never returns to.
    unsafe { cxx::throw_with(copy, &mut *ctx) }
}

unsafe extern "sysv64" fn exception_ptr_rethrow(p: usize, _: usize, ctx: *mut Context) -> ! {
    // SAFETY: the title's `exception_ptr`; the shim's context.
    unsafe { rethrow(p as *mut ExceptionPtr, false, ctx) }
}

// ---------------------------------------------------------------------------
// 4. The locale facets' C helpers, for the "C" locale.

/// `_Cvtvec`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Cvtvec {
    pub page: u32,
    pub mbcurmax: u32,
    pub isclocale: i32,
    pub isleadbyte: [u8; 32],
}

/// `_Ctypevec`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Ctypevec {
    pub page: u32,
    pub table: *const i16,
    pub delfl: i32,
    pub locale_name: *mut u16,
}

/// `_Collvec`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Collvec {
    pub page: u32,
    pub locale_name: *mut u16,
}

const _: () = {
    assert!(size_of::<Cvtvec>() == 44);
    assert!(size_of::<Ctypevec>() == 32);
    assert!(size_of::<Collvec>() == 16);
    assert!(offset_of!(Ctypevec, delfl) == 16);
};

/// The C locale's code page.
const CODE_PAGE: u32 = 1252;

/// The `_ctype` classification bits.
pub const UPPER: i16 = 1;
pub const LOWER: i16 = 2;
pub const DIGIT: i16 = 4;
pub const SPACE: i16 = 8;
pub const PUNCT: i16 = 0x10;
pub const CONTROL: i16 = 0x20;
pub const BLANK: i16 = 0x40;
pub const HEX: i16 = 0x80;
pub const ALPHA: i16 = 0x100;

/// The class of the byte `c`: ASCII as the C locale has it, and the Latin-1
/// letters (`×` and `÷` are not) and no-break space.
const fn ctype_bits(c: u8) -> i16 {
    match c {
        b'A'..=b'F' => UPPER | HEX | ALPHA,
        b'G'..=b'Z' => UPPER | ALPHA,
        b'a'..=b'f' => LOWER | HEX | ALPHA,
        b'g'..=b'z' => LOWER | ALPHA,
        b'0'..=b'9' => DIGIT | HEX,
        b' ' => SPACE | BLANK,
        b'\t' => SPACE | CONTROL | BLANK,
        0x0A..=0x0D => SPACE | CONTROL,
        0..=0x1F | 0x7F => CONTROL,
        0x21..=0x7E => PUNCT,
        0xA0 => SPACE,
        0xD7 | 0xF7 => 0,
        0xC0..=0xDE => UPPER | ALPHA,
        0xDF..=0xFF => LOWER | ALPHA,
        _ => 0,
    }
}

static CTYPE_TABLE: [i16; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = ctype_bits(i as u8);
        i += 1;
    }
    table
};

/// The C locale's conversion data. A 44-byte struct is returned as Win64
/// returns it: into the hidden first argument, which comes back in rax.
pub extern "win64" fn _Getcvt(out: *mut Cvtvec) -> *mut Cvtvec {
    if !out.is_null() {
        // SAFETY: the caller's place for the result.
        unsafe {
            out.write_unaligned(Cvtvec {
                page: CODE_PAGE,
                mbcurmax: 1,
                isclocale: 1,
                isleadbyte: [0; 32],
            });
        }
    }
    out
}

/// The C locale's classification table (ours, not to be freed). Returned
/// through the hidden first argument (see [`_Getcvt`]).
pub extern "win64" fn _Getctype(out: *mut Ctypevec) -> *mut Ctypevec {
    if !out.is_null() {
        // SAFETY: the caller's place for the result.
        unsafe {
            out.write_unaligned(Ctypevec {
                page: CODE_PAGE,
                table: CTYPE_TABLE.as_ptr(),
                delfl: 0,
                locale_name: ptr::null_mut(),
            });
        }
    }
    out
}

/// The C locale's collation data. Returned through the hidden first
/// argument (see [`_Getcvt`]).
pub extern "win64" fn _Getcoll(out: *mut Collvec) -> *mut Collvec {
    if !out.is_null() {
        // SAFETY: the caller's place for the result.
        unsafe {
            out.write_unaligned(Collvec {
                page: CODE_PAGE,
                locale_name: ptr::null_mut(),
            });
        }
    }
    out
}

/// One byte to one wide character (Latin-1): 1; -2 for no bytes; 0 for
/// the NUL.
pub extern "win64" fn _Mbrtowc(
    out: *mut u16,
    s: *const c_char,
    n: usize,
    _state: *mut c_void,
    _cvt: *const Cvtvec,
) -> i32 {
    if s.is_null() {
        return 0;
    }
    if n == 0 {
        return -2;
    }
    // SAFETY: the title's byte.
    let byte = unsafe { s.cast::<u8>().read() };
    if !out.is_null() {
        // SAFETY: the title's place for the character.
        unsafe { out.write_unaligned(u16::from(byte)) };
    }
    i32::from(byte != 0)
}

/// A wide character below 256 to its byte: 1; else `EILSEQ` and -1.
pub extern "win64" fn _Wcrtomb(
    out: *mut c_char,
    wc: u16,
    _state: *mut c_void,
    _cvt: *const Cvtvec,
) -> i32 {
    let Ok(byte) = u8::try_from(wc) else {
        set_errno(EILSEQ);
        return -1;
    };
    if !out.is_null() {
        // SAFETY: the title's place for the byte.
        unsafe { out.cast::<u8>().write(byte) };
    }
    1
}

/// The units of `[first, last)`.
///
/// # Safety
/// The range is the title's, `first <= last`, or either is null.
unsafe fn range<'a, T>(first: *const T, last: *const T) -> &'a [T] {
    if first.is_null() || last.is_null() || last < first {
        return &[];
    }
    // SAFETY: the caller's range.
    unsafe { std::slice::from_raw_parts(first, last.offset_from(first) as usize) }
}

/// -1, 0 or 1 as `a` sorts before, with or after `b`.
fn compare<T: Ord>(a: &[T], b: &[T]) -> i32 {
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `[first, last)` copied into `[out1, out2)` when it fits (as the STL's C
/// locale has it); the room it needs.
///
/// # Safety
/// The ranges are the title's.
unsafe fn transform<T: Copy>(out1: *mut T, out2: *mut T, first: *const T, last: *const T) -> usize {
    // SAFETY: the caller's.
    let text = unsafe { range(first, last) };
    // SAFETY: as above.
    let room = unsafe { range(out1.cast_const(), out2.cast_const()) }.len();
    if text.len() <= room {
        // SAFETY: `room` units at `out1`.
        unsafe { ptr::copy_nonoverlapping(text.as_ptr(), out1, text.len()) };
    }
    text.len()
}

/// Compares two byte ranges.
pub extern "win64" fn _Strcoll(
    f1: *const c_char,
    l1: *const c_char,
    f2: *const c_char,
    l2: *const c_char,
    _coll: *const Collvec,
) -> i32 {
    // SAFETY: the title's ranges.
    unsafe {
        compare(
            range(f1.cast::<u8>(), l1.cast()),
            range(f2.cast::<u8>(), l2.cast()),
        )
    }
}

/// The collation key of a byte range: itself.
pub extern "win64" fn _Strxfrm(
    out1: *mut c_char,
    out2: *mut c_char,
    first: *const c_char,
    last: *const c_char,
    _coll: *const Collvec,
) -> usize {
    // SAFETY: the title's ranges.
    unsafe { transform(out1, out2, first, last) }
}

/// Compares two UTF-16 ranges, unit by unit.
pub extern "win64" fn _Wcscoll(
    f1: *const u16,
    l1: *const u16,
    f2: *const u16,
    l2: *const u16,
    _coll: *const Collvec,
) -> i32 {
    // SAFETY: the title's ranges.
    unsafe { compare(range(f1, l1), range(f2, l2)) }
}

/// The collation key of a UTF-16 range: itself.
pub extern "win64" fn _Wcsxfrm(
    out1: *mut u16,
    out2: *mut u16,
    first: *const u16,
    last: *const u16,
    _coll: *const Collvec,
) -> usize {
    // SAFETY: the title's ranges.
    unsafe { transform(out1, out2, first, last) }
}

/// The class bits of the byte `c`; 0 outside a byte.
fn byte_class(c: i32) -> i16 {
    u8::try_from(c).map_or(0, |b| CTYPE_TABLE[usize::from(b)])
}

/// The lower case of a byte (ASCII and Latin-1).
pub extern "win64" fn _Tolower(c: i32, _ct: *const Ctypevec) -> i32 {
    if byte_class(c) & UPPER != 0 {
        c + 0x20
    } else {
        c
    }
}

/// The upper case of a byte (ASCII and Latin-1; `ß` and `ÿ` have none in a
/// byte).
pub extern "win64" fn _Toupper(c: i32, _ct: *const Ctypevec) -> i32 {
    if byte_class(c) & LOWER != 0 && c != 0xDF && c != 0xFF {
        c - 0x20
    } else {
        c
    }
}

/// A one-to-one case mapping of `c` within UTF-16 units; `c` otherwise.
fn map_case<I: Iterator<Item = char>>(c: u16, map: impl Fn(char) -> I) -> u16 {
    let Some(ch) = char::from_u32(u32::from(c)) else {
        return c;
    };
    let mut mapped = map(ch);
    match (mapped.next(), mapped.next()) {
        (Some(m), None) => u16::try_from(u32::from(m)).unwrap_or(c),
        _ => c,
    }
}

/// The lower case of a wide character (Unicode's simple mapping).
pub extern "win64" fn _Towlower(c: u16, _ct: *const Ctypevec) -> u16 {
    map_case(c, char::to_lowercase)
}

/// The upper case of a wide character (Unicode's simple mapping).
pub extern "win64" fn _Towupper(c: u16, _ct: *const Ctypevec) -> u16 {
    map_case(c, char::to_uppercase)
}

/// The class bits of a wide character, by its Unicode properties.
pub extern "win64" fn _Getwctype(c: u16, _ct: *const Ctypevec) -> i16 {
    let Some(ch) = char::from_u32(u32::from(c)) else {
        return 0;
    };
    let mut bits = 0;
    if ch.is_alphabetic() {
        bits |= ALPHA;
        if ch.is_uppercase() {
            bits |= UPPER;
        }
        if ch.is_lowercase() {
            bits |= LOWER;
        }
    }
    if ch.is_ascii_digit() {
        bits |= DIGIT;
    }
    if ch.is_ascii_hexdigit() {
        bits |= HEX;
    }
    if ch.is_whitespace() {
        bits |= SPACE;
    }
    if ch == ' ' || ch == '\t' {
        bits |= BLANK;
    }
    if ch.is_control() {
        bits |= CONTROL;
    }
    if !ch.is_alphanumeric() && !ch.is_whitespace() && !ch.is_control() {
        bits |= PUNCT;
    }
    bits
}

/// [`_Getwctype`] of each of `[first, last)` into `dest`; `last`.
pub extern "win64" fn _Getwctypes(
    first: *const u16,
    last: *const u16,
    dest: *mut i16,
    ct: *const Ctypevec,
) -> *const u16 {
    // SAFETY: the title's range.
    let units = unsafe { range(first, last) };
    if !dest.is_null() {
        for (i, &c) in units.iter().enumerate() {
            // SAFETY: the title's room for one class per unit.
            unsafe { dest.add(i).write_unaligned(_Getwctype(c, ct)) };
        }
    }
    last
}

const DAYS: &str =
    ":Sun:Sunday:Mon:Monday:Tue:Tuesday:Wed:Wednesday:Thu:Thursday:Fri:Friday:Sat:Saturday";
const MONTHS: &str = ":Jan:January:Feb:February:Mar:March:Apr:April:May:May:Jun:June:Jul:July:Aug:August:Sep:September:Oct:October:Nov:November:Dec:December";

/// `text` NUL-terminated in a block of ours (`malloc`), which the caller
/// frees.
fn narrow_copy(text: &str) -> *mut c_char {
    let block = heap::malloc(text.len() + 1).cast::<u8>();
    if !block.is_null() {
        // SAFETY: a fresh block of `len + 1` bytes.
        unsafe {
            ptr::copy_nonoverlapping(text.as_ptr(), block, text.len());
            block.add(text.len()).write(0);
        }
    }
    block.cast()
}

/// [`narrow_copy`] in UTF-16.
fn wide_copy(text: &str) -> *mut u16 {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let block = heap::malloc(units.len() * 2).cast::<u16>();
    if !block.is_null() {
        // SAFETY: a fresh block of `units.len()` units.
        unsafe { ptr::copy_nonoverlapping(units.as_ptr(), block, units.len()) };
    }
    block
}

/// The C locale's day names, `:Sun:Sunday:...`, in a block the caller
/// frees.
pub extern "win64" fn _Getdays() -> *mut c_char {
    narrow_copy(DAYS)
}

/// The C locale's month names, `:Jan:January:...`, in a block the caller
/// frees.
pub extern "win64" fn _Getmonths() -> *mut c_char {
    narrow_copy(MONTHS)
}

/// The UCRT's `__lc_time_data` copy for the time facets: null. Nothing of
/// ours reads that block (we have no `_Strftime`), and its layout is the
/// UCRT's private one; the facets' `_Timevec` frees a null pointer fine.
pub extern "win64" fn _Gettnames() -> *mut c_void {
    ptr::null_mut()
}

/// [`_Getdays`] in UTF-16.
pub extern "win64" fn _W_Getdays() -> *mut u16 {
    wide_copy(DAYS)
}

/// [`_Getmonths`] in UTF-16.
pub extern "win64" fn _W_Getmonths() -> *mut u16 {
    wide_copy(MONTHS)
}

/// See [`_Gettnames`].
pub extern "win64" fn _W_Gettnames() -> *mut c_void {
    ptr::null_mut()
}

/// `v` times 10 to the `pten`.
fn scale(v: f64, pten: i32) -> f64 {
    if pten == 0 { v } else { v * 10f64.powi(pten) }
}

/// Runs a parse with `errno` cleared; `*perr` (when not null) `ERANGE` if
/// it overflowed, else 0.
fn reporting<T>(perr: *mut i32, parse: impl FnOnce() -> T) -> T {
    set_errno(0);
    let value = parse();
    if !perr.is_null() {
        let err = if runtime::errno() == ERANGE {
            ERANGE
        } else {
            0
        };
        // SAFETY: the title's place for the error.
        unsafe { perr.write_unaligned(err) };
    }
    value
}

/// `strtod` of `s`, times 10 to the `pten`.
pub extern "win64" fn _Stod(s: *const c_char, end: *mut *mut c_char, pten: i32) -> f64 {
    scale(string::strtod(s, end), pten)
}

/// [`_Stod`] as a float.
pub extern "win64" fn _Stof(s: *const c_char, end: *mut *mut c_char, pten: i32) -> f32 {
    _Stod(s, end, pten) as f32
}

/// [`_Stod`] (a long double is a double).
pub extern "win64" fn _Stold(s: *const c_char, end: *mut *mut c_char, pten: i32) -> f64 {
    _Stod(s, end, pten)
}

/// `strtoll`.
pub extern "win64" fn _Stoll(s: *const c_char, end: *mut *mut c_char, base: i32) -> i64 {
    string::strtoll(s, end, base)
}

/// `strtoull`.
pub extern "win64" fn _Stoull(s: *const c_char, end: *mut *mut c_char, base: i32) -> u64 {
    string::strtoull(s, end, base)
}

/// `strtol`, `*perr` `ERANGE` on overflow.
pub extern "win64" fn _Stolx(
    s: *const c_char,
    end: *mut *mut c_char,
    base: i32,
    perr: *mut i32,
) -> i32 {
    reporting(perr, || string::strtol(s, end, base))
}

/// `strtoul`, `*perr` `ERANGE` on overflow.
pub extern "win64" fn _Stoulx(
    s: *const c_char,
    end: *mut *mut c_char,
    base: i32,
    perr: *mut i32,
) -> u32 {
    reporting(perr, || string::strtoul(s, end, base))
}

/// `strtoll`, `*perr` `ERANGE` on overflow.
pub extern "win64" fn _Stollx(
    s: *const c_char,
    end: *mut *mut c_char,
    base: i32,
    perr: *mut i32,
) -> i64 {
    reporting(perr, || string::strtoll(s, end, base))
}

/// `strtoull`, `*perr` `ERANGE` on overflow.
pub extern "win64" fn _Stoullx(
    s: *const c_char,
    end: *mut *mut c_char,
    base: i32,
    perr: *mut i32,
) -> u64 {
    reporting(perr, || string::strtoull(s, end, base))
}

/// [`_Stod`], `*perr` `ERANGE` on overflow or underflow.
pub extern "win64" fn _Stodx(
    s: *const c_char,
    end: *mut *mut c_char,
    pten: i32,
    perr: *mut i32,
) -> f64 {
    reporting(perr, || _Stod(s, end, pten))
}

/// [`_Stodx`] as a float.
pub extern "win64" fn _Stofx(
    s: *const c_char,
    end: *mut *mut c_char,
    pten: i32,
    perr: *mut i32,
) -> f32 {
    _Stodx(s, end, pten, perr) as f32
}

/// [`_Stodx`] (a long double is a double).
pub extern "win64" fn _Stoldx(
    s: *const c_char,
    end: *mut *mut c_char,
    pten: i32,
    perr: *mut i32,
) -> f64 {
    _Stodx(s, end, pten, perr)
}

// ---------------------------------------------------------------------------
// The module.

/// An export under its name as the DLL has it (mangled, for C++ names).
macro_rules! named {
    ($name:expr, $f:path) => {
        HleExport {
            name: $name,
            address: $f as *const () as usize,
        }
    };
}

/// An export under the function's own name.
macro_rules! plain {
    ($($f:ident),* $(,)?) => {
        [$(named!(stringify!($f), $f)),*]
    };
}

fn exports() -> Vec<HleExport> {
    let mut exports = Vec::new();
    exports.extend(plain![
        _Mtx_init_in_situ,
        _Mtx_destroy_in_situ,
        _Mtx_init,
        _Mtx_destroy,
        _Mtx_lock,
        _Mtx_trylock,
        _Mtx_timedlock,
        _Mtx_unlock,
        _Mtx_current_owns,
        _Mtx_getconcrtcs,
        _Mtx_clear_owner,
        _Mtx_reset_owner,
        _Cnd_init_in_situ,
        _Cnd_destroy_in_situ,
        _Cnd_init,
        _Cnd_destroy,
        _Cnd_wait,
        _Cnd_timedwait,
        _Cnd_signal,
        _Cnd_broadcast,
        _Cnd_register_at_thread_exit,
        _Cnd_unregister_at_thread_exit,
        _Cnd_do_broadcast_at_thread_exit,
        _Thrd_create,
        _Thrd_join,
        _Thrd_detach,
        _Thrd_sleep,
        _Thrd_yield,
        _Thrd_id,
        _Thrd_current,
        _Thrd_equal,
        _Thrd_lt,
        _Thrd_hardware_concurrency,
        xtime_get,
        _Xtime_get_ticks,
        _Xtime_diff_to_millis,
        _Xtime_diff_to_millis2,
        _Query_perf_counter,
        _Query_perf_frequency,
        _Smtx_lock_exclusive,
        _Smtx_lock_shared,
        _Smtx_try_lock_exclusive,
        _Smtx_try_lock_shared,
        _Smtx_unlock_exclusive,
        _Smtx_unlock_shared,
        _Lock_shared_ptr_spin_lock,
        _Unlock_shared_ptr_spin_lock,
    ]);
    exports.extend([
        named!("??0_Lockit@std@@QEAA@H@Z", lockit_new_kind),
        named!("??0_Lockit@std@@QEAA@XZ", lockit_new),
        named!("??1_Lockit@std@@QEAA@XZ", lockit_drop),
        named!("?_Lockit_ctor@_Lockit@std@@CAXPEAV12@@Z", lockit_ctor_this),
        named!(
            "?_Lockit_ctor@_Lockit@std@@CAXPEAV12@H@Z",
            lockit_ctor_this_kind
        ),
        named!("?_Lockit_dtor@_Lockit@std@@CAXPEAV12@@Z", lockit_dtor_this),
        named!("?_Lockit_ctor@_Lockit@std@@SAXH@Z", lockit_ctor_kind),
        named!("?_Lockit_dtor@_Lockit@std@@SAXH@Z", lockit_dtor_kind),
        named!("?_Xbad_alloc@std@@YAXXZ", x_bad_alloc_shim),
        named!("?_Xbad_function_call@std@@YAXXZ", x_bad_function_call_shim),
        named!(
            "?_Xinvalid_argument@std@@YAXPEBD@Z",
            x_invalid_argument_shim
        ),
        named!("?_Xlength_error@std@@YAXPEBD@Z", x_length_error_shim),
        named!("?_Xout_of_range@std@@YAXPEBD@Z", x_out_of_range_shim),
        named!("?_Xoverflow_error@std@@YAXPEBD@Z", x_overflow_error_shim),
        named!("?_Xruntime_error@std@@YAXPEBD@Z", x_runtime_error_shim),
        named!(
            "?_Xregex_error@std@@YAXW4error_type@regex_constants@1@@Z",
            x_regex_error_shim
        ),
        named!("?_Throw_C_error@std@@YAXH@Z", throw_c_error_shim),
        named!("?_Throw_Cpp_error@std@@YAXH@Z", throw_cpp_error_shim),
        named!(
            "?_Throw_future_error@std@@YAXAEBVerror_code@1@@Z",
            throw_future_error_shim
        ),
        named!(
            "?_Rethrow_future_exception@std@@YAXVexception_ptr@1@@Z",
            rethrow_future_exception_shim
        ),
        named!(
            "?generic_category@std@@YAAEBVerror_category@1@XZ",
            generic_category
        ),
        named!(
            "?system_category@std@@YAAEBVerror_category@1@XZ",
            system_category
        ),
        named!(
            "?future_category@std@@YAAEBVerror_category@1@XZ",
            future_category
        ),
        named!("?_Syserror_map@std@@YAPEBDH@Z", _Syserror_map),
        named!("?_Winerror_map@std@@YAHH@Z", _Winerror_map),
        named!("?_Winerror_message@std@@YAKKPEADK@Z", _Winerror_message),
        named!("?uncaught_exception@std@@YA_NXZ", uncaught_exception),
        named!("?uncaught_exceptions@std@@YAHXZ", uncaught_exceptions),
        named!("?_Random_device@std@@YAIXZ", _Random_device),
    ]);
    exports.extend(plain![
        __ExceptionPtrCreate,
        __ExceptionPtrDestroy,
        __ExceptionPtrCopy,
        __ExceptionPtrAssign,
        __ExceptionPtrCompare,
        __ExceptionPtrToBool,
        __ExceptionPtrSwap,
        __ExceptionPtrCurrentException,
        __ExceptionPtrRethrow,
        __ExceptionPtrCopyException,
        _Getcvt,
        _Getctype,
        _Getcoll,
        _Mbrtowc,
        _Wcrtomb,
        _Strcoll,
        _Strxfrm,
        _Wcscoll,
        _Wcsxfrm,
        _Tolower,
        _Toupper,
        _Towlower,
        _Towupper,
        _Getwctype,
        _Getwctypes,
        _Getdays,
        _Getmonths,
        _Gettnames,
        _W_Getdays,
        _W_Getmonths,
        _W_Gettnames,
        _Stod,
        _Stof,
        _Stold,
        _Stoll,
        _Stoull,
        _Stolx,
        _Stoulx,
        _Stollx,
        _Stoullx,
        _Stodx,
        _Stofx,
        _Stoldx,
    ]);
    exports
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "msvcp",
        dlls: &[
            "msvcp140.dll",
            "msvcp140_1.dll",
            "msvcp140_2.dll",
            "msvcp140_atomic_wait.dll",
            "msvcp140_codecvt_ids.dll",
        ],
        exports: exports(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::ucrt::cxx::{HT_IS_CONST, HT_IS_REFERENCE};

    /// Storage as `std::mutex` keeps it.
    #[repr(C, align(8))]
    struct Storage([u8; MTX_INTERNAL_IMP_SIZE]);

    impl Storage {
        fn new() -> Storage {
            Storage([0; MTX_INTERNAL_IMP_SIZE])
        }

        fn at(&mut self) -> *mut c_void {
            self.0.as_mut_ptr().cast()
        }
    }

    /// The absolute time `d` from now.
    fn after(d: Duration) -> Xtime {
        let at = now_nanos() + d.as_nanos() as i128;
        Xtime {
            sec: (at / NSEC_PER_SEC) as i64,
            nsec: (at % NSEC_PER_SEC) as i32,
        }
    }

    /// A pointer another thread may use.
    #[derive(Clone, Copy)]
    struct Sendable(usize);

    impl Sendable {
        fn of(p: *mut c_void) -> Sendable {
            Sendable(p as usize)
        }

        fn get(self) -> *mut c_void {
            self.0 as *mut c_void
        }
    }

    #[test]
    fn mutexes_lock_try_and_time_out() {
        let mut storage = Storage::new();
        let m = storage.at();
        _Mtx_init_in_situ(m, MTX_PLAIN);
        assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
        assert_eq!(_Mtx_current_owns(m), 1);
        let shared = Sendable::of(m);
        std::thread::spawn(move || {
            let m = shared.get();
            assert_eq!(_Mtx_current_owns(m), 0);
            assert_eq!(_Mtx_trylock(m), THRD_BUSY);
            let start = Instant::now();
            assert_eq!(
                _Mtx_timedlock(m, &after(Duration::from_millis(50))),
                THRD_TIMEDOUT
            );
            assert!(start.elapsed() >= Duration::from_millis(45));
        })
        .join()
        .unwrap();
        assert_eq!(_Mtx_unlock(m), THRD_SUCCESS);
        assert_eq!(_Mtx_current_owns(m), 0);
        std::thread::spawn(move || {
            let m = shared.get();
            assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
            assert_eq!(_Mtx_unlock(m), THRD_SUCCESS);
        })
        .join()
        .unwrap();
        _Mtx_destroy_in_situ(m);

        _Mtx_init_in_situ(m, MTX_PLAIN | MTX_TRY | MTX_RECURSIVE);
        assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
        assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
        assert_eq!(_Mtx_current_owns(m), 1);
        assert_eq!(_Mtx_unlock(m), THRD_SUCCESS);
        assert_eq!(_Mtx_current_owns(m), 1);
        assert_eq!(_Mtx_unlock(m), THRD_SUCCESS);
        assert_eq!(_Mtx_current_owns(m), 0);
        _Mtx_destroy_in_situ(m);

        // A try mutex (`std::mutex`) taken again by its owner is busy.
        _Mtx_init_in_situ(m, MTX_PLAIN | MTX_TRY);
        assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
        assert_eq!(_Mtx_lock(m), THRD_BUSY);
        assert_eq!(_Mtx_trylock(m), THRD_BUSY);
        assert_eq!(_Mtx_unlock(m), THRD_SUCCESS);
        _Mtx_destroy_in_situ(m);

        // Storage a constexpr constructor wrote (the type, no state) works.
        let mut fresh = Storage::new();
        fresh.0[0] = (MTX_PLAIN | MTX_TRY) as u8;
        assert_eq!(_Mtx_lock(fresh.at()), THRD_SUCCESS);
        assert_eq!(_Mtx_lock(fresh.at()), THRD_BUSY);
        assert_eq!(_Mtx_unlock(fresh.at()), THRD_SUCCESS);
        _Mtx_destroy_in_situ(fresh.at());

        let mut heap_mtx = ptr::null_mut();
        assert_eq!(_Mtx_init(&mut heap_mtx, MTX_PLAIN), THRD_SUCCESS);
        // SAFETY: within the block.
        assert_eq!(_Mtx_getconcrtcs(heap_mtx), unsafe {
            heap_mtx.byte_add(MTX_STATE)
        });
        // SAFETY: the block, 80 bytes: the type at 0.
        assert_eq!(unsafe { heap_mtx.cast::<i32>().read() }, MTX_PLAIN);
        assert_eq!(_Mtx_lock(heap_mtx), THRD_SUCCESS);
        _Mtx_clear_owner(heap_mtx);
        assert_eq!(_Mtx_current_owns(heap_mtx), 0);
        _Mtx_reset_owner(heap_mtx);
        assert_eq!(_Mtx_current_owns(heap_mtx), 1);
        assert_eq!(_Mtx_unlock(heap_mtx), THRD_SUCCESS);
        _Mtx_destroy(heap_mtx);
    }

    #[test]
    fn condition_variables_wait_signal_and_time_out() {
        let (mut ms, mut cs) = (Storage::new(), Storage::new());
        let (m, c) = (ms.at(), cs.at());
        _Mtx_init_in_situ(m, MTX_PLAIN | MTX_TRY);
        _Cnd_init_in_situ(c);
        let (sm, sc) = (Sendable::of(m), Sendable::of(c));

        // Signalled by another thread; the mutex held again after.
        assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
        let signaller = std::thread::spawn(move || {
            assert_eq!(_Mtx_lock(sm.get()), THRD_SUCCESS);
            assert_eq!(_Cnd_signal(sc.get()), THRD_SUCCESS);
            assert_eq!(_Mtx_unlock(sm.get()), THRD_SUCCESS);
        });
        assert_eq!(_Cnd_wait(c, m), THRD_SUCCESS);
        assert_eq!(_Mtx_current_owns(m), 1);
        signaller.join().unwrap();

        // Timed out at about the deadline.
        let start = Instant::now();
        assert_eq!(
            _Cnd_timedwait(c, m, &after(Duration::from_millis(100))),
            THRD_TIMEDOUT
        );
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(95), "{took:?}");
        assert!(took < Duration::from_millis(300), "{took:?}");
        assert_eq!(_Mtx_current_owns(m), 1);
        assert_eq!(_Mtx_unlock(m), THRD_SUCCESS);

        // A broadcast wakes two waiters.
        static WAITING: AtomicUsize = AtomicUsize::new(0);
        static WOKEN: AtomicUsize = AtomicUsize::new(0);
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(move || {
                    assert_eq!(_Mtx_lock(sm.get()), THRD_SUCCESS);
                    WAITING.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(_Cnd_wait(sc.get(), sm.get()), THRD_SUCCESS);
                    WOKEN.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(_Mtx_unlock(sm.get()), THRD_SUCCESS);
                })
            })
            .collect();
        loop {
            assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
            let n = WAITING.load(Ordering::SeqCst);
            assert_eq!(_Mtx_unlock(m), THRD_SUCCESS);
            if n == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(_Cnd_broadcast(c), THRD_SUCCESS);
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(WOKEN.load(Ordering::SeqCst), 2);
        _Cnd_destroy_in_situ(c);
        _Mtx_destroy_in_situ(m);

        // At thread exit: the flag set, the mutex unlocked.
        let mut heap_cnd = ptr::null_mut();
        assert_eq!(_Cnd_init(&mut heap_cnd), THRD_SUCCESS);
        _Mtx_init_in_situ(m, MTX_PLAIN | MTX_TRY);
        assert_eq!(_Mtx_lock(m), THRD_SUCCESS);
        let mut flag = 0i32;
        _Cnd_register_at_thread_exit(heap_cnd, m, &mut flag);
        _Cnd_do_broadcast_at_thread_exit();
        assert_eq!(flag, 1);
        assert_eq!(_Mtx_current_owns(m), 0);
        _Cnd_destroy(heap_cnd);
        _Mtx_destroy_in_situ(m);
    }

    extern "win64" fn returns_seven(arg: *mut c_void) -> u32 {
        // SAFETY: the test's place for the id.
        unsafe { arg.cast::<u32>().write(_Thrd_id()) };
        7
    }

    #[test]
    fn threads_run_join_and_sleep() {
        let mut thr = ThrdT {
            handle: ptr::null_mut(),
            id: 0,
        };
        let mut id = 0u32;
        let start: Thrd = returns_seven;
        assert_eq!(
            _Thrd_create(&mut thr, Some(start), (&raw mut id).cast()),
            THRD_SUCCESS
        );
        let mut code = 0;
        assert_eq!(_Thrd_join(&thr, &mut code), THRD_SUCCESS);
        assert_eq!(code, 7);
        assert_eq!(id, thr.id);
        assert_ne!(id, _Thrd_id());
        assert!(_Thrd_hardware_concurrency() >= 1);
        let mut current = ThrdT {
            handle: ptr::dangling_mut(),
            id: 0,
        };
        assert_eq!(_Thrd_current(&mut current), &raw mut current);
        assert_eq!(current.id, _Thrd_id());
        assert_eq!(_Thrd_equal(&current, &current), 1);
        assert_eq!(_Thrd_equal(&current, &thr), 0);
        assert_eq!(
            _Thrd_lt(&current, &thr) + _Thrd_lt(&thr, &current),
            1,
            "one is below the other"
        );
        let start = Instant::now();
        _Thrd_sleep(&after(Duration::from_millis(20)));
        assert!(start.elapsed() >= Duration::from_millis(20));
        _Thrd_yield();
    }

    #[test]
    fn times_are_read_and_compared() {
        let mut now = Xtime::default();
        assert_eq!(xtime_get(&mut now, TIME_UTC), TIME_UTC);
        let host = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert!((now.sec - host.as_secs() as i64).abs() <= 1);
        assert!((0..1_000_000_000).contains(&now.nsec));
        assert_eq!(xtime_get(&mut now, 0), 0);
        let a = Xtime { sec: 5, nsec: 1 };
        let b = Xtime { sec: 3, nsec: 0 };
        assert_eq!(_Xtime_diff_to_millis2(&a, &b), 2001, "rounded up");
        assert_eq!(_Xtime_diff_to_millis2(&b, &a), 0);
        assert_eq!(_Xtime_diff_to_millis2(&a, &a), 0);
        assert_eq!(_Xtime_diff_to_millis(&b), 0, "long past");
        let ticks = _Xtime_get_ticks();
        let host = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert!((ticks / 10_000_000 - host.as_secs() as i64).abs() <= 1);
        assert!(_Query_perf_frequency() > 0);
        assert!(_Query_perf_counter() <= _Query_perf_counter());
    }

    #[test]
    fn lockits_are_recursive_and_exclusive() {
        let mut first = [0u64; 1];
        let mut second = [0u64; 1];
        let (a, b) = (first.as_mut_ptr().cast::<i32>(), second.as_mut_ptr().cast());
        assert_eq!(lockit_new_kind(a, 0), a);
        assert_eq!(lockit_new(b), b);
        static TAKEN: AtomicBool = AtomicBool::new(false);
        let other = std::thread::spawn(|| {
            let mut mine = [0u64; 1];
            let l = mine.as_mut_ptr().cast::<i32>();
            lockit_new_kind(l, 0);
            TAKEN.store(true, Ordering::SeqCst);
            lockit_drop(l);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!TAKEN.load(Ordering::SeqCst), "held by this thread");
        lockit_drop(b);
        std::thread::sleep(Duration::from_millis(20));
        assert!(!TAKEN.load(Ordering::SeqCst), "still held once");
        lockit_drop(a);
        other.join().unwrap();
        assert!(TAKEN.load(Ordering::SeqCst));
        // The static forms, and a kind out of range (nothing).
        lockit_ctor_kind(2);
        lockit_ctor_kind(2);
        lockit_dtor_kind(2);
        lockit_dtor_kind(2);
        lockit_ctor_kind(99);
        lockit_dtor_kind(99);
    }

    /// The record of a throw of `object` as `info` describes it.
    fn record_of(object: *mut c_void, info: *const ThrowInfoAbs) -> ExceptionRecord {
        let mut r = ExceptionRecord::new(EH_EXCEPTION_NUMBER, EXCEPTION_NONCONTINUABLE, 0);
        r.number_parameters = 4;
        r.information[..4].copy_from_slice(&[
            u64::from(EH_MAGIC_NUMBER1),
            object as u64,
            info as u64,
            0,
        ]);
        r
    }

    /// `what` through the object's vtable.
    fn what_of(object: *const c_void) -> String {
        // SAFETY: one of our objects, its vtable's slot 1 `what`.
        unsafe {
            let vftable = *object.cast::<*const What>();
            let what = *vftable.add(1);
            CStr::from_ptr(what(object)).to_string_lossy().into_owned()
        }
    }

    fn data_of(object: *const c_void) -> &'static ExceptionData {
        // SAFETY: one of our objects, alive while the test looks.
        unsafe { &(*object.cast::<StdException>()).data }
    }

    const REF: u32 = HT_IS_REFERENCE | HT_IS_CONST;

    #[test]
    fn thrown_objects_match_their_bases() {
        let (mut object, info) = make_exception(Kind::LengthError, c"too long".as_ptr());
        let at = object.as_ptr();
        let t = cxx::thrown_in(&record_of(at, info)).unwrap();
        // SAFETY: the object and our tables, alive here.
        unsafe {
            for name in [".?AVlength_error@std@@", ".?AVlogic_error@std@@"] {
                assert!(
                    t.catchable_as(REF, Some(name.as_bytes())).is_some(),
                    "{name}"
                );
            }
            let base = t
                .catchable_as(REF, Some(b".?AVexception@std@@"))
                .unwrap()
                .unwrap();
            assert_eq!(base.type_descriptor, descriptor(Kind::Exception) as u64);
            assert!(
                t.catchable_as(REF, Some(b".?AVruntime_error@std@@"))
                    .is_none()
            );
            assert_eq!(t.size(), 24);
            assert_eq!(what_of(at), "too long");
            assert_eq!(data_of(at).do_free, 1);

            // By value, as `catch (std::exception e)`: through the copy
            // constructor; then both destroyed.
            let mut copy = Object([0; 5]);
            cxx::build_catch_object_at(copy.as_ptr() as u64, 0, &t, &base);
            assert_eq!(what_of(copy.as_ptr()), "too long");
            assert_eq!(data_of(copy.as_ptr()).do_free, 1);
            assert_ne!(data_of(copy.as_ptr()).what, data_of(at).what);
            assert_eq!(
                copy.0[0] as usize,
                vtable(Kind::Exception as usize) as usize
            );
            t.destroy();
            assert_eq!(data_of(at).do_free, 0);
            assert!(data_of(at).what.is_null());
            Thrown {
                object: copy.as_ptr() as u64,
                ..t
            }
            .destroy();
            assert_eq!(data_of(copy.as_ptr()).do_free, 0);
            assert_eq!(what_of(copy.as_ptr()), "Unknown exception");
        }

        // A null message: not owned, and `what` says so.
        let (mut object, _) = make_exception(Kind::RuntimeError, ptr::null());
        assert_eq!(data_of(object.as_ptr()).do_free, 0);
        assert_eq!(what_of(object.as_ptr()), "Unknown exception");

        // `bad_alloc`: its literal, `bad_alloc` compatible.
        let (mut object, info) = make_literal(Kind::BadArrayNewLength, c"bad array new length");
        let t = cxx::thrown_in(&record_of(object.as_ptr(), info)).unwrap();
        // SAFETY: as above.
        unsafe {
            let ct = t
                .catchable_as(0, Some(b".?AVbad_alloc@std@@"))
                .unwrap()
                .unwrap();
            assert_eq!(ct.properties, CT_IS_STD_BAD_ALLOC);
            assert!(t.catchable_as(REF, Some(b".?AVexception@std@@")).is_some());
            assert_eq!(what_of(object.as_ptr()), "bad array new length");
            assert_eq!(data_of(object.as_ptr()).do_free, 0, "a literal");
            t.destroy();
        }

        // `system_error`: its chain, its code and category message.
        let (mut object, info) = make_system_error(ERRC_RESOURCE_UNAVAILABLE_TRY_AGAIN);
        let t = cxx::thrown_in(&record_of(object.as_ptr(), info)).unwrap();
        // SAFETY: as above.
        unsafe {
            for name in [
                ".?AVsystem_error@std@@",
                ".?AV_System_error@std@@",
                ".?AVruntime_error@std@@",
                ".?AVexception@std@@",
            ] {
                assert!(
                    t.catchable_as(REF, Some(name.as_bytes())).is_some(),
                    "{name}"
                );
            }
            assert_eq!(t.size(), 40);
            let code = object.code().read();
            assert_eq!(code.value, 11);
            assert_eq!(code.category, generic_category());
            assert_eq!(what_of(object.as_ptr()), "Resource temporarily unavailable");
            t.destroy();
        }

        // `regex_error`.
        let (mut object, info) = make_regex_error(4);
        // SAFETY: as above.
        unsafe {
            assert_eq!(object.code().cast::<i32>().read(), 4);
            assert_eq!(
                cxx::thrown_in(&record_of(object.as_ptr(), info))
                    .unwrap()
                    .size(),
                32
            );
        }
        assert!(what_of(object.as_ptr()).starts_with("regex_error(error_brack)"));

        // `future_error`.
        let code = ErrorCode {
            value: 4,
            pad: 0,
            category: future_category(),
        };
        let (mut object, _) = make_future_error(code);
        assert_eq!(what_of(object.as_ptr()), "no state");
    }

    #[test]
    fn categories_name_and_explain() {
        let call = |category: *const c_void| {
            // SAFETY: our category's vtable.
            unsafe { &*(*category.cast::<*const CategoryVtable>()).byte_sub(8) }
        };
        for (category, name) in [
            (generic_category(), "generic"),
            (system_category(), "system"),
            (future_category(), "future"),
        ] {
            let vt = call(category);
            // SAFETY: our category's functions.
            unsafe {
                assert_eq!(CStr::from_ptr((vt.name)(category)).to_str(), Ok(name));
                let mut s = MaybeUninit::<MsvcString>::uninit();
                (vt.message)(category, s.as_mut_ptr(), 2);
                let s = s.assume_init();
                let text = read_string(&s);
                match name {
                    "generic" => assert_eq!(text, b"No such file or directory"),
                    "system" => assert_eq!(text, b"Windows error 2"),
                    _ => assert_eq!(text, b"future already retrieved"),
                }
                if s.res > 15 {
                    heap::free(usize::from_ne_bytes(s.bx[..8].try_into().unwrap()) as *mut c_void);
                }
                let mut cond = MaybeUninit::<ErrorCode>::uninit();
                // ERROR_ACCESS_DENIED: the system category's condition is the
                // generic `permission_denied`; the others keep the value.
                (vt.default_error_condition)(category, cond.as_mut_ptr(), 5);
                let cond = cond.assume_init();
                if name == "system" {
                    assert_eq!((cond.value, cond.category), (13, generic_category()));
                } else {
                    assert_eq!((cond.value, cond.category), (5, category));
                    assert_eq!((vt.equivalent_condition)(category, 5, &cond), 1);
                    assert_eq!((vt.equivalent_condition)(category, 6, &cond), 0);
                    assert_eq!((vt.equivalent_code)(category, &cond, 5), 1);
                }
                let mut cond = MaybeUninit::<ErrorCode>::uninit();
                // 99999 maps to nothing: the system category keeps it.
                (vt.default_error_condition)(category, cond.as_mut_ptr(), 99999);
                let cond = cond.assume_init();
                assert_eq!((cond.value, cond.category), (99999, category));
            }
        }
        // A short string in place, a long one in a block.
        let mut s = MaybeUninit::<MsvcString>::uninit();
        // SAFETY: room for a string.
        let s = unsafe {
            write_string(s.as_mut_ptr(), b"0123456789abcdefXYZ");
            s.assume_init()
        };
        assert_eq!((s.size, s.res), (19, 31));
        assert_eq!(read_string(&s), b"0123456789abcdefXYZ");
        heap::free(usize::from_ne_bytes(s.bx[..8].try_into().unwrap()) as *mut c_void);
    }

    #[test]
    fn error_maps() {
        assert_eq!(_Winerror_map(2), 2);
        assert_eq!(_Winerror_map(10060), 138);
        assert_eq!(_Winerror_map(99999), 0);
        assert_eq!(_Winerror_map(33), 39);
        // SAFETY: our static texts.
        unsafe {
            assert_eq!(
                CStr::from_ptr(_Syserror_map(2)).to_str(),
                Ok("No such file or directory")
            );
            assert_eq!(
                CStr::from_ptr(_Syserror_map(500)).to_str(),
                Ok("unknown error")
            );
            assert_eq!(
                CStr::from_ptr(_Syserror_map(-1)).to_str(),
                Ok("unknown error")
            );
        }
        let mut buffer = [0 as c_char; 8];
        assert_eq!(_Winerror_message(5, buffer.as_mut_ptr(), 8), 7);
        // SAFETY: NUL-terminated by the call.
        assert_eq!(
            unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str(),
            Ok("Windows")
        );
        let _ = _Random_device();
        assert_eq!(uncaught_exceptions(), 0);
        assert_eq!(uncaught_exception(), 0);
    }

    #[test]
    fn exception_ptrs_share_and_copy() {
        let mut a = ExceptionPtr {
            data1: ptr::dangling_mut(),
            data2: ptr::dangling_mut(),
        };
        __ExceptionPtrCreate(&mut a);
        assert_eq!(__ExceptionPtrToBool(&a), 0);
        __ExceptionPtrCurrentException(&mut a);
        assert_eq!(__ExceptionPtrToBool(&a), 0, "nothing being handled");

        let (mut object, info) = make_exception(Kind::OutOfRange, c"index".as_ptr());
        let mut b = EMPTY;
        __ExceptionPtrCopyException(&mut b, object.as_ptr(), info.cast());
        assert_eq!(__ExceptionPtrToBool(&b), 1);
        assert_ne!(b.data2, object.as_ptr());
        assert_eq!(what_of(b.data2), "index");
        assert_ne!(data_of(b.data2).what, data_of(object.as_ptr()).what);
        assert_eq!(__ExceptionPtrCompare(&a, &b), 0);

        let mut c = EMPTY;
        __ExceptionPtrCopy(&mut c, &b);
        assert_eq!(__ExceptionPtrCompare(&b, &c), 1);
        assert_eq!(shared_of(&b).unwrap().refs.load(Ordering::Relaxed), 2);
        __ExceptionPtrAssign(&mut a, &c);
        assert_eq!(shared_of(&b).unwrap().refs.load(Ordering::Relaxed), 3);
        let itself = &raw mut a;
        __ExceptionPtrAssign(itself, itself);
        assert_eq!(shared_of(&b).unwrap().refs.load(Ordering::Relaxed), 3);
        let mut d = EMPTY;
        __ExceptionPtrSwap(&mut a, &mut d);
        assert_eq!(__ExceptionPtrToBool(&a), 0);
        assert_eq!(__ExceptionPtrCompare(&d, &b), 1);
        let held = cxx::thrown_in(&shared_of(&b).unwrap().record).unwrap();
        assert_eq!(held.object, b.data2 as u64);
        __ExceptionPtrDestroy(&mut d);
        __ExceptionPtrDestroy(&mut c);
        assert_eq!(shared_of(&b).unwrap().refs.load(Ordering::Relaxed), 1);
        __ExceptionPtrDestroy(&mut b);
        assert_eq!(__ExceptionPtrToBool(&b), 0);
        __ExceptionPtrDestroy(&mut a);
        // SAFETY: the original, alive here.
        unsafe {
            cxx::thrown_in(&record_of(object.as_ptr(), info))
                .unwrap()
                .destroy()
        };
    }

    #[test]
    fn locale_helpers_follow_the_c_locale() {
        let mut cvt = MaybeUninit::<Cvtvec>::uninit();
        assert_eq!(_Getcvt(cvt.as_mut_ptr()), cvt.as_mut_ptr());
        // SAFETY: written.
        let cvt = unsafe { cvt.assume_init() };
        assert_eq!((cvt.page, cvt.mbcurmax, cvt.isclocale), (1252, 1, 1));
        let mut ct = MaybeUninit::<Ctypevec>::uninit();
        _Getctype(ct.as_mut_ptr());
        // SAFETY: written; the table has 256 entries.
        let table = unsafe { std::slice::from_raw_parts(ct.assume_init().table, 256) };
        assert_eq!(table[usize::from(b'A')], UPPER | ALPHA | HEX);
        assert_eq!(table[usize::from(b'z')], LOWER | ALPHA);
        assert_eq!(table[usize::from(b'\t')], SPACE | CONTROL | BLANK);
        assert_eq!(table[0xE9], LOWER | ALPHA);
        assert_eq!(table[0xD7], 0);
        let mut coll = MaybeUninit::<Collvec>::uninit();
        _Getcoll(coll.as_mut_ptr());

        assert_eq!(_Tolower(i32::from(b'Q'), ptr::null()), i32::from(b'q'));
        assert_eq!(_Toupper(0xE9, ptr::null()), 0xC9);
        assert_eq!(_Toupper(0xDF, ptr::null()), 0xDF);
        assert_eq!(_Tolower(-1, ptr::null()), -1);
        assert_eq!(_Towupper(u16::from(b'a'), ptr::null()), u16::from(b'A'));
        assert_eq!(
            _Towupper(0xDF, ptr::null()),
            0xDF,
            "ß has no one-to-one upper case"
        );
        assert_eq!(_Towlower(0x0391, ptr::null()), 0x03B1);
        assert_eq!(_Getwctype(u16::from(b'7'), ptr::null()), DIGIT | HEX);
        assert_eq!(_Getwctype(u16::from(b' '), ptr::null()), SPACE | BLANK);
        assert_eq!(_Getwctype(u16::from(b'!'), ptr::null()), PUNCT);
        assert_eq!(_Getwctype(0x00C9, ptr::null()), ALPHA | UPPER);
        let text = [u16::from(b'a'), u16::from(b'1')];
        let mut classes = [0i16; 2];
        let end = text.as_ptr_range().end;
        assert_eq!(
            _Getwctypes(text.as_ptr(), end, classes.as_mut_ptr(), ptr::null()),
            end
        );
        assert_eq!(classes, [ALPHA | LOWER | HEX, DIGIT | HEX]);

        let mut wc = 0u16;
        assert_eq!(
            _Mbrtowc(&mut wc, c"\xE9".as_ptr(), 1, ptr::null_mut(), ptr::null()),
            1
        );
        assert_eq!(wc, 0xE9);
        assert_eq!(
            _Mbrtowc(&mut wc, c"x".as_ptr(), 0, ptr::null_mut(), ptr::null()),
            -2
        );
        assert_eq!(
            _Mbrtowc(&mut wc, c"".as_ptr(), 1, ptr::null_mut(), ptr::null()),
            0
        );
        assert_eq!(wc, 0);
        let mut byte: c_char = 0;
        assert_eq!(_Wcrtomb(&mut byte, 0x41, ptr::null_mut(), ptr::null()), 1);
        assert_eq!(byte as u8, b'A');
        assert_eq!(
            _Wcrtomb(&mut byte, 0x263A, ptr::null_mut(), ptr::null()),
            -1
        );
        assert_eq!(runtime::errno(), EILSEQ);

        let (a, b) = (b"abc".as_slice(), b"abd".as_slice());
        let r = |s: &[u8]| {
            let p = s.as_ptr_range();
            (p.start.cast::<c_char>(), p.end.cast::<c_char>())
        };
        let ((a1, a2), (b1, b2)) = (r(a), r(b));
        assert_eq!(_Strcoll(a1, a2, b1, b2, ptr::null()), -1);
        assert_eq!(_Strcoll(b1, b2, a1, a2, ptr::null()), 1);
        assert_eq!(_Strcoll(a1, a2, a1, a2, ptr::null()), 0);
        let mut out = [0 as c_char; 4];
        let o = out.as_mut_ptr_range();
        assert_eq!(_Strxfrm(o.start, o.end, a1, a2, ptr::null()), 3);
        assert_eq!(
            out[..3].iter().map(|&c| c as u8).collect::<Vec<_>>(),
            b"abc"
        );
        let wide: Vec<u16> = "ab".encode_utf16().collect();
        let w = wide.as_ptr_range();
        assert_eq!(_Wcscoll(w.start, w.end, w.start, w.end, ptr::null()), 0);
        let mut small = [0u16; 1];
        let s = small.as_mut_ptr_range();
        assert_eq!(_Wcsxfrm(s.start, s.end, w.start, w.end, ptr::null()), 2);
        assert_eq!(small, [0], "no room: nothing copied");

        let days = _Getdays();
        // SAFETY: our NUL-terminated block.
        assert!(
            unsafe { CStr::from_ptr(days) }
                .to_bytes()
                .starts_with(b":Sun:Sunday:Mon:")
        );
        heap::free(days.cast());
        let months = _W_Getmonths();
        // SAFETY: our block of UTF-16.
        assert_eq!(unsafe { *months.add(1) }, u16::from(b'J'));
        heap::free(months.cast());
        assert!(_Gettnames().is_null());

        let mut end = ptr::null_mut();
        let s = c"1.5e2x";
        assert_eq!(_Stod(s.as_ptr(), &mut end, 1), 1500.0);
        // SAFETY: within `s`.
        assert_eq!(unsafe { *end } as u8, b'x');
        let mut err = -1;
        assert_eq!(_Stolx(c"42".as_ptr(), ptr::null_mut(), 10, &mut err), 42);
        assert_eq!(err, 0);
        _Stolx(c"99999999999".as_ptr(), ptr::null_mut(), 10, &mut err);
        assert_eq!(err, ERANGE);
        assert_eq!(_Stoull(c"ff".as_ptr(), ptr::null_mut(), 16), 255);
        assert_eq!(_Stollx(c"-7".as_ptr(), ptr::null_mut(), 10, &mut err), -7);
        assert_eq!(_Stofx(c"2.5".as_ptr(), ptr::null_mut(), 0, &mut err), 2.5);
        assert_eq!(err, 0);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 119);
    }
}
