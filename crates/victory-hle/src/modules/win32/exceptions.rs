//! Structured exception handling and the debugger's API: the dispatch,
//! unwinding and context functions are `victory_loader::seh`'s (it does the
//! work ntdll does on Windows); here the vectored handlers' API, the error
//! modes, and the answers to "is a debugger there" (none is).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

use victory_loader::seh::{
    self, __C_specific_handler, Context, EXCEPTION_EXECUTE_HANDLER, ExceptionPointers,
    ExceptionRecord, RaiseException, RtlAddFunctionTable, RtlCaptureContext,
    RtlDeleteFunctionTable, RtlInstallFunctionTableCallback, RtlLookupFunctionEntry,
    RtlPcToFileHeader, RtlRaiseException, RtlRestoreContext, RtlUnwind, RtlUnwindEx,
    RtlVirtualUnwind, VectoredHandler,
};

use super::handles::{ERROR_INVALID_PARAMETER, set_last_error};
use crate::{HleExport, hle_export};

/// `AddVectoredExceptionHandler`: `handler` is called for every exception
/// before the frames' handlers (first of all when `first` is not 0); its
/// handle, or null for a null handler.
pub extern "win64" fn AddVectoredExceptionHandler(
    first: u32,
    handler: Option<VectoredHandler>,
) -> *mut c_void {
    match handler {
        Some(handler) => seh::add_vectored_handler(first != 0, handler),
        None => std::ptr::null_mut(),
    }
}

/// `RemoveVectoredExceptionHandler`: nonzero when `handle` named one.
pub extern "win64" fn RemoveVectoredExceptionHandler(handle: *mut c_void) -> u32 {
    u32::from(seh::remove_vectored_handler(handle))
}

/// `AddVectoredContinueHandler`: `handler` is called when a handled
/// exception lets the thread go on.
pub extern "win64" fn AddVectoredContinueHandler(
    first: u32,
    handler: Option<VectoredHandler>,
) -> *mut c_void {
    match handler {
        Some(handler) => seh::add_continue_handler(first != 0, handler),
        None => std::ptr::null_mut(),
    }
}

/// `RemoveVectoredContinueHandler`: nonzero when `handle` named one.
pub extern "win64" fn RemoveVectoredContinueHandler(handle: *mut c_void) -> u32 {
    u32::from(seh::remove_continue_handler(handle))
}

/// `UnhandledExceptionFilter`: with no debugger to hand the exception to,
/// the process is to end (`EXCEPTION_EXECUTE_HANDLER`).
pub extern "win64" fn UnhandledExceptionFilter(_pointers: *mut ExceptionPointers) -> i32 {
    EXCEPTION_EXECUTE_HANDLER
}

/// `RaiseFailFastException`: `record` in `ctx` reported as an exception no
/// handler took, and the end of the process; `flags` are not used.
pub extern "win64" fn RaiseFailFastException(
    record: *const ExceptionRecord,
    ctx: *const Context,
    _flags: u32,
) -> ! {
    // SAFETY: the title's pointers, each null or at its kind.
    unsafe { seh::fail_fast(record, ctx) }
}

/// `IsDebuggerPresent`: no debugger is there.
pub extern "win64" fn IsDebuggerPresent() -> i32 {
    0
}

/// `CheckRemoteDebuggerPresent`: none, for any process.
pub extern "win64" fn CheckRemoteDebuggerPresent(_process: *mut c_void, present: *mut i32) -> i32 {
    if present.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: the title's out-pointer.
    unsafe { *present = 0 };
    1
}

/// `DebugBreak`: an `int3`.
pub extern "win64" fn DebugBreak() {
    // SAFETY: a breakpoint trap, which the process's fault handling takes.
    unsafe { core::arch::asm!("int3") };
}

/// `RtlCaptureStackBackTrace`: no frames. Its callers are our own frames,
/// which have no unwind data and cannot be walked.
pub extern "win64" fn RtlCaptureStackBackTrace(
    _skip: u32,
    _count: u32,
    _frames: *mut *mut c_void,
    _hash: *mut u32,
) -> u16 {
    0
}

/// The error modes `SetErrorMode` and `SetThreadErrorMode` take:
/// `SEM_FAILCRITICALERRORS`, `SEM_NOGPFAULTERRORBOX`,
/// `SEM_NOALIGNMENTFAULTEXCEPT`, `SEM_NOOPENFILEERRORBOX`.
const ERROR_MODES: u32 = 0x0001 | 0x0002 | 0x0004 | 0x8000;

/// The process's error mode: kept and given back, nothing else (there are
/// no error boxes to show).
static ERROR_MODE: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// This thread's error mode, likewise.
    static THREAD_ERROR_MODE: Cell<u32> = const { Cell::new(0) };
}

/// `SetErrorMode`: the mode before.
pub extern "win64" fn SetErrorMode(mode: u32) -> u32 {
    ERROR_MODE.swap(mode, SeqCst)
}

/// `GetErrorMode`.
pub extern "win64" fn GetErrorMode() -> u32 {
    ERROR_MODE.load(SeqCst)
}

/// `SetThreadErrorMode`: the mode before into `old` (when not null); a mode
/// with other bits than the error modes' is refused.
pub extern "win64" fn SetThreadErrorMode(mode: u32, old: *mut u32) -> i32 {
    if mode & !ERROR_MODES != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let before = THREAD_ERROR_MODE.with(|m| m.replace(mode));
    if !old.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { *old = before };
    }
    1
}

/// `GetThreadErrorMode`.
pub extern "win64" fn GetThreadErrorMode() -> u32 {
    THREAD_ERROR_MODE.with(Cell::get)
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(RaiseException),
        hle_export!(RtlRaiseException),
        hle_export!(RtlUnwind),
        hle_export!(RtlUnwindEx),
        hle_export!(RtlCaptureContext),
        hle_export!(RtlRestoreContext),
        hle_export!(RtlVirtualUnwind),
        hle_export!(RtlLookupFunctionEntry),
        hle_export!(RtlPcToFileHeader),
        hle_export!(RtlAddFunctionTable),
        hle_export!(RtlDeleteFunctionTable),
        hle_export!(RtlInstallFunctionTableCallback),
        hle_export!(__C_specific_handler),
        hle_export!(AddVectoredExceptionHandler),
        hle_export!(RemoveVectoredExceptionHandler),
        hle_export!(AddVectoredContinueHandler),
        hle_export!(RemoveVectoredContinueHandler),
        hle_export!(UnhandledExceptionFilter),
        hle_export!(RaiseFailFastException),
        hle_export!(IsDebuggerPresent),
        hle_export!(CheckRemoteDebuggerPresent),
        hle_export!(DebugBreak),
        hle_export!(RtlCaptureStackBackTrace),
        hle_export!(SetErrorMode),
        hle_export!(GetErrorMode),
        hle_export!(SetThreadErrorMode),
        hle_export!(GetThreadErrorMode),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_modes_are_kept() {
        let mut old = 7;
        assert_eq!(SetThreadErrorMode(0x8001, &mut old), 1);
        assert_eq!(old, 0);
        assert_eq!(GetThreadErrorMode(), 0x8001);
        assert_eq!(SetThreadErrorMode(0x10, &mut old), 0, "not an error mode");
        assert_eq!(GetThreadErrorMode(), 0x8001);
        let before = SetErrorMode(2);
        assert_eq!(GetErrorMode(), 2);
        SetErrorMode(before);
    }

    #[test]
    fn no_debugger_is_there() {
        assert_eq!(IsDebuggerPresent(), 0);
        let mut present = 1;
        assert_eq!(
            CheckRemoteDebuggerPresent(std::ptr::null_mut(), &mut present),
            1
        );
        assert_eq!(present, 0);
    }

    #[test]
    fn vectored_handlers_are_added_and_removed() {
        unsafe extern "win64" fn handler(_: *mut ExceptionPointers) -> i32 {
            0
        }
        let handle = AddVectoredExceptionHandler(1, Some(handler));
        assert!(!handle.is_null());
        assert_eq!(RemoveVectoredExceptionHandler(handle), 1);
        assert_eq!(RemoveVectoredExceptionHandler(handle), 0, "already removed");
        assert!(AddVectoredContinueHandler(0, None).is_null());
    }
}
