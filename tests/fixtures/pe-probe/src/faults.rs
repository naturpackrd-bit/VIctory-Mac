//! Title code that faults, for the loader's fault handling: a load from
//! address 0, and the Zen 2 instructions an Intel processor lacks. Naked,
//! so that the faulting bytes, and their lengths, are exactly these.

use core::arch::naked_asm;

/// Reads address 0 and faults: `xor eax, eax`, then `mov eax, dword ptr
/// [rax]` (2 bytes), then `ret`. The tests' handler resumes the thread at
/// [`probe_fault_resume`].
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_fault() -> u32 {
    naked_asm!("xor eax, eax", ".byte 0x8b, 0x00", "ret")
}

/// Where the tests' handler resumes [`probe_fault`]: returns 0x5E to its
/// caller.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_fault_resume() -> u32 {
    naked_asm!("mov eax, 0x5e", "ret")
}

/// `clzero` on the 64-byte line `line` lies in (the pointer arrives in
/// rcx; `clzero` takes rax).
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_clzero(line: *mut u8) {
    naked_asm!("mov rax, rcx", ".byte 0x0f, 0x01, 0xfc", "ret")
}

/// `monitorx`, then returns 1.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_monitorx() -> u32 {
    naked_asm!(".byte 0x0f, 0x01, 0xfa", "mov eax, 1", "ret")
}

/// `mwaitx`, then returns 2.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_mwaitx() -> u32 {
    naked_asm!(".byte 0x0f, 0x01, 0xfb", "mov eax, 2", "ret")
}
