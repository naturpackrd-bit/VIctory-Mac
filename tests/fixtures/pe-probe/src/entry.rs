//! The title's entry point and TLS callbacks as the loader runs them: each
//! notes the reasons it was called with, as bits, for the tests to read.

use core::sync::atomic::{AtomicU64, Ordering};

static DLLMAIN_REASONS: AtomicU64 = AtomicU64::new(0);
static TLS_REASONS: AtomicU64 = AtomicU64::new(0);
static DLLMAIN_BASE: AtomicU64 = AtomicU64::new(0);

/// The entry point of the DLL: notes the reason (bit `1 << reason`) and the
/// base it was given, and agrees to attach.
#[unsafe(no_mangle)]
pub extern "win64" fn _DllMainCRTStartup(base: *mut u8, reason: u32, _: *mut u8) -> i32 {
    DLLMAIN_REASONS.fetch_or(1 << reason, Ordering::Relaxed);
    DLLMAIN_BASE.store(base as u64, Ordering::Relaxed);
    1
}

/// The image's one TLS callback: notes the reason.
pub extern "win64" fn tls_callback(_: *mut u8, reason: u32, _: *mut u8) {
    TLS_REASONS.fetch_or(1 << reason, Ordering::Relaxed);
}

/// The callback array `_tls_used` points at, ended by a null entry.
pub(crate) static TLS_CALLBACKS: [Option<extern "win64" fn(*mut u8, u32, *mut u8)>; 2] =
    [Some(tls_callback), None];

/// The reasons DllMain was called with, one bit each.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_dllmain_reasons() -> u64 {
    DLLMAIN_REASONS.load(Ordering::Relaxed)
}

/// The reasons the TLS callback was called with, one bit each.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_tls_reasons() -> u64 {
    TLS_REASONS.load(Ordering::Relaxed)
}

/// The base DllMain was last given.
#[unsafe(no_mangle)]
pub extern "win64" fn probe_dllmain_base() -> u64 {
    DLLMAIN_BASE.load(Ordering::Relaxed)
}
