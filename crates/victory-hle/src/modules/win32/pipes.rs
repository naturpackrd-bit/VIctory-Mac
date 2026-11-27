//! Pipes (`api-ms-win-core-namedpipe-*`): anonymous pipes are the host's
//! (`CreatePipe`: two file handles of `files.rs`); named pipes are not here
//! (each call fails with `ERROR_NOT_SUPPORTED` and a warning, once).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]
// Windows' signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::sync::{Mutex, PoisonError};

use super::handles::{ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE, set_last_error};
use crate::{HleExport, hle_export};

type HANDLE = *mut c_void;
type BOOL = i32;

const TRUE: BOOL = 1;
const FALSE: BOOL = 0;

/// Named pipes are not here.
const ERROR_NOT_SUPPORTED: u32 = 50;

/// `ERROR_NOT_SUPPORTED`, warned once per function; `value` returned.
fn not_supported<T>(function: &'static str, value: T) -> T {
    static SEEN: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    if !seen.contains(&function) {
        tracing::warn!(function, "no named pipes here");
        seen.push(function);
    }
    set_last_error(ERROR_NOT_SUPPORTED);
    value
}

pub extern "win64" fn CreateNamedPipeW(
    _name: *const u16,
    _open_mode: u32,
    _pipe_mode: u32,
    _max_instances: u32,
    _out_size: u32,
    _in_size: u32,
    _timeout: u32,
    _security: *const c_void,
) -> HANDLE {
    not_supported("CreateNamedPipeW", INVALID_HANDLE_VALUE as HANDLE)
}

pub extern "win64" fn CreateNamedPipeA(
    _name: *const u8,
    _open_mode: u32,
    _pipe_mode: u32,
    _max_instances: u32,
    _out_size: u32,
    _in_size: u32,
    _timeout: u32,
    _security: *const c_void,
) -> HANDLE {
    not_supported("CreateNamedPipeA", INVALID_HANDLE_VALUE as HANDLE)
}

pub extern "win64" fn ConnectNamedPipe(_pipe: HANDLE, _overlapped: *mut c_void) -> BOOL {
    not_supported("ConnectNamedPipe", FALSE)
}

pub extern "win64" fn DisconnectNamedPipe(_pipe: HANDLE) -> BOOL {
    not_supported("DisconnectNamedPipe", FALSE)
}

pub extern "win64" fn PeekNamedPipe(
    _pipe: HANDLE,
    _buffer: *mut c_void,
    _size: u32,
    _read: *mut u32,
    _available: *mut u32,
    _left: *mut u32,
) -> BOOL {
    not_supported("PeekNamedPipe", FALSE)
}

pub extern "win64" fn WaitNamedPipeW(_name: *const u16, _timeout: u32) -> BOOL {
    not_supported("WaitNamedPipeW", FALSE)
}

pub extern "win64" fn TransactNamedPipe(
    _pipe: HANDLE,
    _input: *mut c_void,
    _input_size: u32,
    _output: *mut c_void,
    _output_size: u32,
    _read: *mut u32,
    _overlapped: *mut c_void,
) -> BOOL {
    not_supported("TransactNamedPipe", FALSE)
}

pub extern "win64" fn CallNamedPipeW(
    _name: *const u16,
    _input: *mut c_void,
    _input_size: u32,
    _output: *mut c_void,
    _output_size: u32,
    _read: *mut u32,
    _timeout: u32,
) -> BOOL {
    not_supported("CallNamedPipeW", FALSE)
}

pub extern "win64" fn SetNamedPipeHandleState(
    _pipe: HANDLE,
    _mode: *mut u32,
    _max_collection: *mut u32,
    _timeout: *mut u32,
) -> BOOL {
    not_supported("SetNamedPipeHandleState", FALSE)
}

pub extern "win64" fn GetNamedPipeInfo(
    _pipe: HANDLE,
    _flags: *mut u32,
    _out_size: *mut u32,
    _in_size: *mut u32,
    _max_instances: *mut u32,
) -> BOOL {
    not_supported("GetNamedPipeInfo", FALSE)
}

pub extern "win64" fn GetNamedPipeHandleStateW(
    _pipe: HANDLE,
    _state: *mut u32,
    _instances: *mut u32,
    _max_collection: *mut u32,
    _timeout: *mut u32,
    _user: *mut u16,
    _user_size: u32,
) -> BOOL {
    not_supported("GetNamedPipeHandleStateW", FALSE)
}

pub extern "win64" fn ImpersonateNamedPipeClient(_pipe: HANDLE) -> BOOL {
    not_supported("ImpersonateNamedPipeClient", FALSE)
}

/// An anonymous pipe of the host's: its read and its write handle (the
/// size hint and the inheritance are ignored).
pub extern "win64" fn CreatePipe(
    read: *mut HANDLE,
    write: *mut HANDLE,
    _attributes: *const c_void,
    _size: u32,
) -> BOOL {
    if read.is_null() || write.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    match super::files::create_pipe() {
        Ok((reader, writer)) => {
            // SAFETY: checked non-null; the title's outputs.
            unsafe {
                read.write(reader as HANDLE);
                write.write(writer as HANDLE);
            }
            TRUE
        }
        Err(code) => {
            set_last_error(code);
            FALSE
        }
    }
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(CreateNamedPipeW),
        hle_export!(CreateNamedPipeA),
        hle_export!(ConnectNamedPipe),
        hle_export!(DisconnectNamedPipe),
        hle_export!(PeekNamedPipe),
        hle_export!(WaitNamedPipeW),
        hle_export!(TransactNamedPipe),
        hle_export!(CallNamedPipeW),
        hle_export!(SetNamedPipeHandleState),
        hle_export!(GetNamedPipeInfo),
        hle_export!(GetNamedPipeHandleStateW),
        hle_export!(ImpersonateNamedPipeClient),
        hle_export!(CreatePipe),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::files::{GetFileType, ReadFile, SetFilePointer, WriteFile};
    use super::super::handles::{CloseHandle, last_error};
    use super::*;

    #[test]
    fn an_anonymous_pipe_carries_bytes() {
        victory_loader::teb::attach_current_thread();
        let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(CreatePipe(&mut read, &mut write, std::ptr::null(), 0), TRUE);
        assert_eq!((GetFileType(read), GetFileType(write)), (3, 3));
        let mut moved = 0;
        let data = *b"hello";
        let null = std::ptr::null_mut();
        assert_eq!(
            WriteFile(write, data.as_ptr().cast(), 5, &mut moved, null),
            TRUE
        );
        assert_eq!(moved, 5);
        let mut got = [0u8; 16];
        assert_eq!(
            ReadFile(read, got.as_mut_ptr().cast(), 16, &mut moved, null),
            TRUE
        );
        assert_eq!(&got[..moved as usize], b"hello");
        assert_eq!(SetFilePointer(read, 0, null.cast(), 0), u32::MAX);
        assert_eq!(last_error(), 1);
        assert_eq!(CloseHandle(write), TRUE);
        // No writer left: the pipe has ended.
        assert_eq!(
            ReadFile(read, got.as_mut_ptr().cast(), 16, &mut moved, null),
            FALSE
        );
        assert_eq!(last_error(), 109);
        assert_eq!(CloseHandle(read), TRUE);
    }

    #[test]
    fn named_pipes_are_not_here() {
        victory_loader::teb::attach_current_thread();
        let pipe = CreateNamedPipeW(std::ptr::null(), 0, 0, 1, 0, 0, 0, std::ptr::null());
        assert_eq!(pipe as usize, INVALID_HANDLE_VALUE);
        assert_eq!(last_error(), ERROR_NOT_SUPPORTED);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 13);
    }
}
