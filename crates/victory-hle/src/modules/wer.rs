//! Windows Error Reporting off Windows (`wer.dll`, the
//! `api-ms-win-core-windowserrorreporting-*` sets): there is none here (our
//! crash report is `seh`'s), so every registration is accepted and dropped;
//! only the flags are kept, for `WerGetFlags`.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::{HleExport, HleModule, hle_export};

type HRESULT = i32;

const S_OK: HRESULT = 0;
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;

/// The flags of `WerSetFlags`.
static FLAGS: AtomicU32 = AtomicU32::new(0);

/// Logs, once per function, that its registration is dropped.
fn noted(function: &'static str) -> HRESULT {
    static SEEN: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    if !seen.contains(&function) {
        tracing::info!(
            function,
            "no Windows Error Reporting here: accepted, not kept"
        );
        seen.push(function);
    }
    S_OK
}

pub extern "win64" fn WerRegisterMemoryBlock(_address: *mut c_void, _size: u32) -> HRESULT {
    noted("WerRegisterMemoryBlock")
}

pub extern "win64" fn WerUnregisterMemoryBlock(_address: *mut c_void) -> HRESULT {
    noted("WerUnregisterMemoryBlock")
}

pub extern "win64" fn WerRegisterFile(_file: *const u16, _kind: i32, _flags: u32) -> HRESULT {
    noted("WerRegisterFile")
}

pub extern "win64" fn WerUnregisterFile(_file: *const u16) -> HRESULT {
    noted("WerUnregisterFile")
}

/// Kept for [`WerGetFlags`].
pub extern "win64" fn WerSetFlags(flags: u32) -> HRESULT {
    FLAGS.store(flags, Ordering::Relaxed);
    noted("WerSetFlags")
}

/// The flags [`WerSetFlags`] kept; the process must be this one.
pub extern "win64" fn WerGetFlags(_process: *mut c_void, flags: *mut u32) -> HRESULT {
    if flags.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title's output.
    unsafe { flags.write(FLAGS.load(Ordering::Relaxed)) };
    noted("WerGetFlags")
}

pub extern "win64" fn WerRegisterRuntimeExceptionModule(
    _dll: *const u16,
    _context: *mut c_void,
) -> HRESULT {
    noted("WerRegisterRuntimeExceptionModule")
}

pub extern "win64" fn WerUnregisterRuntimeExceptionModule(
    _dll: *const u16,
    _context: *mut c_void,
) -> HRESULT {
    noted("WerUnregisterRuntimeExceptionModule")
}

pub extern "win64" fn WerRegisterCustomMetadata(_key: *const u16, _value: *const u16) -> HRESULT {
    noted("WerRegisterCustomMetadata")
}

pub extern "win64" fn WerUnregisterCustomMetadata(_key: *const u16) -> HRESULT {
    noted("WerUnregisterCustomMetadata")
}

pub extern "win64" fn WerRegisterAdditionalProcess(_process: u32, _thread: u32) -> HRESULT {
    noted("WerRegisterAdditionalProcess")
}

pub extern "win64" fn WerUnregisterAdditionalProcess(_process: u32) -> HRESULT {
    noted("WerUnregisterAdditionalProcess")
}

pub extern "win64" fn WerRegisterAppLocalDump(_folder: *const u16) -> HRESULT {
    noted("WerRegisterAppLocalDump")
}

pub extern "win64" fn WerUnregisterAppLocalDump() -> HRESULT {
    noted("WerUnregisterAppLocalDump")
}

fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(WerRegisterMemoryBlock),
        hle_export!(WerUnregisterMemoryBlock),
        hle_export!(WerRegisterFile),
        hle_export!(WerUnregisterFile),
        hle_export!(WerSetFlags),
        hle_export!(WerGetFlags),
        hle_export!(WerRegisterRuntimeExceptionModule),
        hle_export!(WerUnregisterRuntimeExceptionModule),
        hle_export!(WerRegisterCustomMetadata),
        hle_export!(WerUnregisterCustomMetadata),
        hle_export!(WerRegisterAdditionalProcess),
        hle_export!(WerUnregisterAdditionalProcess),
        hle_export!(WerRegisterAppLocalDump),
        hle_export!(WerUnregisterAppLocalDump),
    ]
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "wer",
        dlls: &["api-ms-win-core-windowserrorreporting-*", "wer.dll"],
        exports: exports(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_are_accepted_and_the_flags_kept() {
        let null = std::ptr::null_mut();
        assert_eq!(WerRegisterMemoryBlock(null, 16), S_OK);
        assert_eq!(WerUnregisterMemoryBlock(null), S_OK);
        assert_eq!(WerSetFlags(4), S_OK);
        let mut flags = 0;
        assert_eq!(WerGetFlags(usize::MAX as *mut c_void, &mut flags), S_OK);
        assert_eq!(flags, 4);
        assert_eq!(WerGetFlags(null, std::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 14);
    }
}
