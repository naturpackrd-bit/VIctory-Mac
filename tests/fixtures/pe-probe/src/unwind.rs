//! Title code that the loader's unwinder walks through: plain functions,
//! not naked, so that lld-link files real unwind info for them in `.pdata`.

use core::ptr::{read_volatile, write_volatile};

/// Calls `f(x)` through [`probe_call_middle`], and adds one: a frame
/// between the caller and the middle one.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_call_through(f: extern "win64" fn(u64) -> u64, x: u64) -> u64 {
    probe_call_middle(f, x).wrapping_add(1)
}

/// Calls `f(x)` and adds `x` to its result, read back from a local array
/// kept alive across the call, so that the frame has a stack allocation.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_call_middle(f: extern "win64" fn(u64) -> u64, x: u64) -> u64 {
    let mut slots = [0u64; 8];
    for slot in &mut slots {
        // SAFETY: a slot of our own local array.
        unsafe { write_volatile(slot, x) };
    }
    let result = f(x);
    // SAFETY: as above.
    result.wrapping_add(unsafe { read_volatile(&slots[3]) })
}
