//! `bcrypt.dll` off Windows: the random number generator (the host's
//! `getentropy`); hashes and ciphers are not here (each fails with
//! `STATUS_NOT_SUPPORTED` and a warning, once).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// Windows' own type names, so that signatures read as its documentation.
#![allow(clippy::upper_case_acronyms)]
// Windows' signatures, as they are.
#![allow(clippy::too_many_arguments)]

use std::any::Any;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, PoisonError};

use super::win32::handles::{self, Object};
use crate::{HleExport, HleModule, hle_export};

type NTSTATUS = i32;

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_INVALID_HANDLE: NTSTATUS = 0xC000_0008_u32 as i32;
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000D_u32 as i32;
const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC000_00BB_u32 as i32;
const STATUS_NOT_FOUND: NTSTATUS = 0xC000_0225_u32 as i32;

/// `BCryptGenRandom` without a handle: the system's generator.
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;

/// The most `getentropy` gives in one call.
const ENTROPY_PIECE: usize = 256;

/// An algorithm provider's object (kind "bcrypt algorithm").
struct Algorithm {
    #[allow(dead_code)]
    name: String,
}

impl Object for Algorithm {
    fn kind(&self) -> &'static str {
        "bcrypt algorithm"
    }
}

/// Logs `what` at warn level the first time it is seen.
fn warn_once(what: &str, message: &str) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    if !seen.iter().any(|s| s == what) {
        tracing::warn!(what, "{message}");
        seen.push(what.to_owned());
    }
}

/// A NUL-terminated UTF-16 string from the title; empty for null.
fn wide(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

/// Whether `handle` names an algorithm provider.
const BCRYPT_RNG_ALG_HANDLE: usize = 0x81;

fn is_algorithm(handle: *mut c_void) -> bool {
    handles::get(handle as usize).is_some_and(|object| {
        let any: &dyn Any = &*object;
        any.is::<Algorithm>()
    })
}

/// Fills `out` from the host's generator.
fn fill_random(out: &mut [u8]) -> bool {
    out.chunks_mut(ENTROPY_PIECE).all(|piece| {
        // SAFETY: `piece` is ours to write, at most 256 bytes.
        unsafe { libc::getentropy(piece.as_mut_ptr().cast(), piece.len()) == 0 }
    })
}

/// The random number generator (`"RNG"`); any other algorithm is not here.
pub extern "win64" fn BCryptOpenAlgorithmProvider(
    out: *mut *mut c_void,
    algorithm: *const u16,
    _implementation: *const u16,
    _flags: u32,
) -> NTSTATUS {
    if out.is_null() || algorithm.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let name = wide(algorithm);
    if name != "RNG" {
        warn_once(&name, "a bcrypt algorithm that is not here (only RNG)");
        return STATUS_NOT_FOUND;
    }
    let handle = handles::insert(Arc::new(Algorithm { name }));
    // SAFETY: checked non-null; the title passes room for the handle.
    unsafe { out.write(handle as *mut c_void) };
    STATUS_SUCCESS
}

/// Closes a provider from [`BCryptOpenAlgorithmProvider`].
pub extern "win64" fn BCryptCloseAlgorithmProvider(handle: *mut c_void, _flags: u32) -> NTSTATUS {
    if !is_algorithm(handle) {
        return STATUS_INVALID_HANDLE;
    }
    handles::remove(handle as usize);
    STATUS_SUCCESS
}

/// `len` random bytes from the host's generator, through an RNG provider
/// or, with `BCRYPT_USE_SYSTEM_PREFERRED_RNG`, none.
pub extern "win64" fn BCryptGenRandom(
    handle: *mut c_void,
    buffer: *mut u8,
    len: u32,
    flags: u32,
) -> NTSTATUS {
    let system = flags & BCRYPT_USE_SYSTEM_PREFERRED_RNG != 0;
    // `BCRYPT_RNG_ALG_HANDLE`: Windows 10's pseudo-handle for the system
    // generator, usable without opening a provider.
    let pseudo = handle as usize == BCRYPT_RNG_ALG_HANDLE;
    if !(system && handle.is_null()) && !pseudo && !is_algorithm(handle) {
        return STATUS_INVALID_HANDLE;
    }
    if len == 0 {
        return STATUS_SUCCESS;
    }
    if buffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: checked non-null; the title's buffer holds `len` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(buffer, len as usize) };
    if fill_random(out) {
        STATUS_SUCCESS
    } else {
        STATUS_INVALID_PARAMETER
    }
}

/// The calls of hashes, ciphers and properties: none is here.
fn not_supported(name: &str) -> NTSTATUS {
    warn_once(name, "a bcrypt function that is not here");
    STATUS_NOT_SUPPORTED
}

pub extern "win64" fn BCryptGetProperty(
    _object: *mut c_void,
    _property: *const u16,
    _out: *mut u8,
    _len: u32,
    _result: *mut u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptGetProperty")
}

pub extern "win64" fn BCryptSetProperty(
    _object: *mut c_void,
    _property: *const u16,
    _input: *const u8,
    _len: u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptSetProperty")
}

pub extern "win64" fn BCryptCreateHash(
    _algorithm: *mut c_void,
    _hash: *mut *mut c_void,
    _object: *mut u8,
    _object_len: u32,
    _secret: *const u8,
    _secret_len: u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptCreateHash")
}

pub extern "win64" fn BCryptHashData(
    _hash: *mut c_void,
    _input: *const u8,
    _len: u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptHashData")
}

pub extern "win64" fn BCryptFinishHash(
    _hash: *mut c_void,
    _out: *mut u8,
    _len: u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptFinishHash")
}

pub extern "win64" fn BCryptDestroyHash(_hash: *mut c_void) -> NTSTATUS {
    not_supported("BCryptDestroyHash")
}

pub extern "win64" fn BCryptGenerateSymmetricKey(
    _algorithm: *mut c_void,
    _key: *mut *mut c_void,
    _object: *mut u8,
    _object_len: u32,
    _secret: *const u8,
    _secret_len: u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptGenerateSymmetricKey")
}

pub extern "win64" fn BCryptEncrypt(
    _key: *mut c_void,
    _input: *const u8,
    _input_len: u32,
    _padding: *const c_void,
    _iv: *mut u8,
    _iv_len: u32,
    _out: *mut u8,
    _out_len: u32,
    _result: *mut u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptEncrypt")
}

pub extern "win64" fn BCryptDecrypt(
    _key: *mut c_void,
    _input: *const u8,
    _input_len: u32,
    _padding: *const c_void,
    _iv: *mut u8,
    _iv_len: u32,
    _out: *mut u8,
    _out_len: u32,
    _result: *mut u32,
    _flags: u32,
) -> NTSTATUS {
    not_supported("BCryptDecrypt")
}

pub extern "win64" fn BCryptDestroyKey(_key: *mut c_void) -> NTSTATUS {
    not_supported("BCryptDestroyKey")
}

fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(BCryptOpenAlgorithmProvider),
        hle_export!(BCryptCloseAlgorithmProvider),
        hle_export!(BCryptGenRandom),
        hle_export!(BCryptGetProperty),
        hle_export!(BCryptSetProperty),
        hle_export!(BCryptCreateHash),
        hle_export!(BCryptHashData),
        hle_export!(BCryptFinishHash),
        hle_export!(BCryptDestroyHash),
        hle_export!(BCryptGenerateSymmetricKey),
        hle_export!(BCryptEncrypt),
        hle_export!(BCryptDecrypt),
        hle_export!(BCryptDestroyKey),
    ]
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "bcrypt",
        dlls: &["bcrypt.dll"],
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
    fn the_generator_gives_random_bytes() {
        let mut rng = std::ptr::null_mut();
        let null = std::ptr::null();
        assert_eq!(
            BCryptOpenAlgorithmProvider(&mut rng, w("RNG").as_ptr(), null, 0),
            STATUS_SUCCESS
        );
        let mut a = vec![0u8; 1000];
        let mut b = vec![0u8; 1000];
        assert_eq!(
            BCryptGenRandom(rng, a.as_mut_ptr(), 1000, 0),
            STATUS_SUCCESS
        );
        assert_eq!(
            BCryptGenRandom(rng, b.as_mut_ptr(), 1000, 0),
            STATUS_SUCCESS
        );
        assert!(a.iter().any(|&x| x != a[0]));
        assert_ne!(a, b);
        assert_eq!(
            BCryptGenRandom(rng, std::ptr::null_mut(), 4, 0),
            STATUS_INVALID_PARAMETER
        );
        assert_eq!(BCryptCloseAlgorithmProvider(rng, 0), STATUS_SUCCESS);
        assert_eq!(BCryptCloseAlgorithmProvider(rng, 0), STATUS_INVALID_HANDLE);
        assert_eq!(
            BCryptGenRandom(rng, a.as_mut_ptr(), 4, 0),
            STATUS_INVALID_HANDLE
        );
    }

    #[test]
    fn an_unknown_algorithm_is_not_found() {
        let mut out = std::ptr::null_mut();
        let null = std::ptr::null();
        assert_eq!(
            BCryptOpenAlgorithmProvider(&mut out, w("SHA256").as_ptr(), null, 0),
            STATUS_NOT_FOUND
        );
        assert!(out.is_null());
        assert_eq!(
            BCryptCreateHash(out, &mut out, std::ptr::null_mut(), 0, null.cast(), 0, 0),
            STATUS_NOT_SUPPORTED
        );
    }

    #[test]
    fn the_system_generator_needs_no_handle() {
        let mut bytes = [0u8; 64];
        let null = std::ptr::null_mut();
        assert_eq!(
            BCryptGenRandom(
                null,
                bytes.as_mut_ptr(),
                64,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG
            ),
            STATUS_SUCCESS
        );
        assert!(bytes.iter().any(|&x| x != 0));
        assert_eq!(
            BCryptGenRandom(null, bytes.as_mut_ptr(), 64, 0),
            STATUS_INVALID_HANDLE
        );
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 13);
    }
}
