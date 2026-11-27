//! Windows handles: small integers naming objects of ours in a process-wide
//! table, as the kernel's handle table; pseudo-handles for the current
//! process and thread; and the thread's last error, which lives in its TEB.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::any::Any;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use victory_loader::teb::{Teb, current_teb};
use victory_platform::{Host, Thread};

use crate::{HleExport, hle_export};

/// What a handle names. Waitable objects (events, threads, ...) also
/// implement `sync::Waitable`.
///
/// `Any` (which implies `'static`) lets a function that takes a handle of one
/// kind get at the object's own type.
pub(crate) trait Object: Any + Send + Sync {
    /// A short name of the object's kind ("event", "mutex", ...).
    fn kind(&self) -> &'static str;
    /// The object as something a thread can wait for, if it is one.
    fn waitable(&self) -> Option<&dyn crate::modules::win32::sync::Waitable> {
        None
    }
}

/// `INVALID_HANDLE_VALUE`, which is also `GetCurrentProcess()`'s value (-1).
pub(crate) const INVALID_HANDLE_VALUE: usize = usize::MAX;
/// `GetCurrentThread()`'s value (-2).
pub(crate) const CURRENT_THREAD: usize = usize::MAX - 1;

/// The operation completed.
pub(crate) const ERROR_SUCCESS: u32 = 0;
/// No object of that name.
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
/// The handle names nothing, or nothing of the kind the function takes.
pub(crate) const ERROR_INVALID_HANDLE: u32 = 6;
/// Not used yet: our allocations do not fail without aborting.
#[allow(dead_code)]
pub(crate) const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
/// A parameter is out of range or a required pointer is null.
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
/// A `Create*` found an object of that name and opened it.
pub(crate) const ERROR_ALREADY_EXISTS: u32 = 183;
/// A wait's time ran out.
pub(crate) const ERROR_TIMEOUT: u32 = 1460;

/// `DuplicateHandle`'s option to close the source handle.
const DUPLICATE_CLOSE_SOURCE: u32 = 1;

/// The handle table: slot `i` is handle `(i + 1) * 4`; `free` lists empty
/// slots to use again.
struct Table {
    slots: Vec<Option<Arc<dyn Object>>>,
    free: Vec<usize>,
}

impl Table {
    fn insert(&mut self, object: Arc<dyn Object>) -> usize {
        let index = match self.free.pop() {
            Some(index) => {
                self.slots[index] = Some(object);
                index
            }
            None => {
                self.slots.push(Some(object));
                self.slots.len() - 1
            }
        };
        (index + 1) * 4
    }

    fn remove(&mut self, handle: usize) -> Option<Arc<dyn Object>> {
        let index = index(handle)?;
        let removed = self.slots.get_mut(index).and_then(Option::take)?;
        self.free.push(index);
        Some(removed)
    }
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    slots: Vec::new(),
    free: Vec::new(),
});

/// The table, even if a thread panicked holding it (its state is consistent
/// between our operations, none of which panics halfway).
fn table() -> MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The slot index a handle names, if it has the form of a table handle.
fn index(handle: usize) -> Option<usize> {
    if handle == 0 || !handle.is_multiple_of(4) {
        return None;
    }
    Some(handle / 4 - 1)
}

/// Puts `object` in the table; the handle, a multiple of 4 from 4 up (0 is
/// never one). Freed slots are used again.
pub(crate) fn insert(object: Arc<dyn Object>) -> usize {
    table().insert(object)
}

/// The object a handle names, if it is a live table handle (pseudo-handles
/// are not: callers check them first).
pub(crate) fn get(handle: usize) -> Option<Arc<dyn Object>> {
    let index = index(handle)?;
    table().slots.get(index)?.clone()
}

/// Removes the handle (the object lives while anything else holds it);
/// false for an unknown handle.
pub(crate) fn remove(handle: usize) -> bool {
    // The table's guard is a temporary of this statement, so the object
    // (which may run code when dropped) is dropped after it is released.
    let removed = table().remove(handle);
    removed.is_some()
}

/// Sets the current thread's last error (its TEB's `last_error`); nothing if
/// the thread has no TEB.
pub(crate) fn set_last_error(code: u32) {
    if let Some(teb) = current_teb() {
        // SAFETY: this thread's own TEB, which only this thread writes; it
        // lives as long as the thread.
        unsafe { (*(teb as *mut Teb)).last_error = code };
    }
}

/// The current thread's last error; 0 if the thread has no TEB.
pub(crate) fn last_error() -> u32 {
    match current_teb() {
        // SAFETY: this thread's own TEB, which only this thread writes.
        Some(teb) => unsafe { (*(teb as *const Teb)).last_error },
        None => 0,
    }
}

/// Whether a handle is one of the pseudo-handles for this process or thread.
fn is_pseudo(handle: usize) -> bool {
    handle == INVALID_HANDLE_VALUE || handle == CURRENT_THREAD
}

/// Closes a table handle; a pseudo-handle needs no closing.
pub extern "win64" fn CloseHandle(handle: *mut c_void) -> i32 {
    let handle = handle as usize;
    if is_pseudo(handle) || remove(handle) {
        return 1;
    }
    set_last_error(ERROR_INVALID_HANDLE);
    0
}

/// A new handle to the object `source` names, within this process (the only
/// process there is: both process handles must be `GetCurrentProcess()`).
/// The thread pseudo-handle gives a real handle to the calling thread's
/// object (`threads.rs`). The process pseudo-handle gives itself (Windows
/// gives a real handle; the pseudo-handle works wherever ours take a
/// process handle).
pub extern "win64" fn DuplicateHandle(
    source_process: *mut c_void,
    source: *mut c_void,
    target_process: *mut c_void,
    target: *mut *mut c_void,
    _access: u32,
    _inherit: i32,
    options: u32,
) -> i32 {
    if source_process as usize != INVALID_HANDLE_VALUE
        || target_process as usize != INVALID_HANDLE_VALUE
    {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if target.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // `INVALID_HANDLE_VALUE` is `GetCurrentProcess()`'s pseudo-handle.
    if source as usize == INVALID_HANDLE_VALUE {
        // SAFETY: checked non-null; the title passes a place for one handle.
        unsafe { *target = source };
        return 1;
    }
    let object = if source as usize == CURRENT_THREAD {
        Some(super::threads::current() as Arc<dyn Object>)
    } else {
        get(source as usize)
    };
    let Some(object) = object else {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let duplicate = insert(object);
    if options & DUPLICATE_CLOSE_SOURCE != 0 {
        remove(source as usize);
    }
    // SAFETY: checked non-null; the title passes a place for one handle.
    unsafe { *target = duplicate as *mut c_void };
    1
}

/// The pseudo-handle for this process (-1).
pub extern "win64" fn GetCurrentProcess() -> *mut c_void {
    INVALID_HANDLE_VALUE as *mut c_void
}

/// The pseudo-handle for the calling thread (-2).
pub extern "win64" fn GetCurrentThread() -> *mut c_void {
    CURRENT_THREAD as *mut c_void
}

/// The calling thread's last error.
pub extern "win64" fn GetLastError() -> u32 {
    last_error()
}

/// Sets the calling thread's last error.
pub extern "win64" fn SetLastError(code: u32) {
    set_last_error(code);
}

/// The host's id for this process.
pub extern "win64" fn GetCurrentProcessId() -> u32 {
    Host::process_id()
}

/// The host's id for the calling thread.
pub extern "win64" fn GetCurrentThreadId() -> u32 {
    Host::thread_id()
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(CloseHandle),
        hle_export!(DuplicateHandle),
        hle_export!(GetCurrentProcess),
        hle_export!(GetCurrentThread),
        hle_export!(GetLastError),
        hle_export!(SetLastError),
        hle_export!(GetCurrentProcessId),
        hle_export!(GetCurrentThreadId),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;

    impl Object for Dummy {
        fn kind(&self) -> &'static str {
            "dummy"
        }
    }

    #[test]
    fn handles_are_small_multiples_of_four_and_reused() {
        victory_loader::teb::attach_current_thread();
        // Reuse, on a table of the test's own: other tests use the global
        // one at the same time and may take a freed slot first.
        let mut own = Table {
            slots: Vec::new(),
            free: Vec::new(),
        };
        let a = own.insert(Arc::new(Dummy));
        let b = own.insert(Arc::new(Dummy));
        assert_eq!((a, b), (4, 8));
        assert!(own.remove(a).is_some());
        assert!(own.remove(a).is_none());
        assert_eq!(own.insert(Arc::new(Dummy)), a);

        let first = insert(Arc::new(Dummy));
        let second = insert(Arc::new(Dummy));
        assert!(first != 0 && first.is_multiple_of(4));
        assert!(second != 0 && second.is_multiple_of(4));
        assert_ne!(first, second);
        assert!(remove(first));
        assert!(get(0).is_none() && get(INVALID_HANDLE_VALUE).is_none());
        assert!(get(CURRENT_THREAD).is_none());

        set_last_error(ERROR_SUCCESS);
        assert_eq!(CloseHandle(0x7FFF_FFF0 as *mut c_void), 0);
        assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
        assert_eq!(CloseHandle(GetCurrentProcess()), 1);
        assert_eq!(CloseHandle(GetCurrentThread()), 1);

        let mut duplicate: *mut c_void = std::ptr::null_mut();
        let process = GetCurrentProcess();
        let source = second as *mut c_void;
        assert_eq!(
            DuplicateHandle(process, source, process, &mut duplicate, 0, 0, 0),
            1
        );
        assert_ne!(duplicate as usize, second);
        let (Some(a), Some(b)) = (get(second), get(duplicate as usize)) else {
            panic!("both handles name the object");
        };
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(CloseHandle(duplicate), 1);
        assert_eq!(CloseHandle(source), 1);

        // The process pseudo-handle duplicates to itself.
        let mut copy: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            DuplicateHandle(process, process, process, &mut copy, 0, 0, 0),
            1
        );
        assert_eq!(copy, process);
        assert!(get(second).is_none() || !Arc::ptr_eq(&get(second).unwrap(), &a));
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 8);
    }
}
