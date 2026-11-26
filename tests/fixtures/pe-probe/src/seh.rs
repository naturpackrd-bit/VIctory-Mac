//! Title code with structured exception handling, as MSVC lays it out: the
//! functions carry `__C_specific_handler` and a scope table in their unwind
//! info (`.seh_*` directives), so that the loader's dispatch finds the
//! `__try` blocks around a fault and runs their filters, `__except` blocks
//! and `__finally` blocks (docs/macos.md, M2a). Written in assembly so that
//! the scopes, and what faults inside them, are exactly these.

use core::arch::{global_asm, naked_asm};
use core::ffi::c_void;
use core::ptr::{addr_of_mut, read_volatile, write_volatile};

/// What `probe_seh_filtered` divides by; its filter sets it to 2.
#[unsafe(no_mangle)]
pub static mut PROBE_SEH_DIVISOR: u32 = 0;
/// How many times `probe_seh_finally`'s `__finally` block ran.
#[unsafe(no_mangle)]
pub static mut PROBE_SEH_FINALLY_COUNT: u32 = 0;
/// The code of the last exception `probe_seh_filter` saw.
#[unsafe(no_mangle)]
pub static mut PROBE_SEH_LAST_CODE: u32 = 0;
/// The low half of that exception's second parameter (for an access
/// violation, the address).
#[unsafe(no_mangle)]
pub static mut PROBE_SEH_LAST_INFO1: u32 = 0;

/// `probe_seh_raise`'s one exception parameter.
static PROBE_SEH_ARGS: u64 = 0x77;

/// `EXCEPTION_POINTERS`, as a filter gets it.
#[repr(C)]
pub struct ExceptionPointersRaw {
    record: *const u8,
    context: *const c_void,
}

/// `STATUS_INTEGER_DIVIDE_BY_ZERO`.
const DIVIDE_BY_ZERO: u32 = 0xC000_0094;

/// The filter of `probe_seh_filtered` and `probe_seh_fault_read`: records
/// the exception's code and its second parameter; a division by zero is
/// fixed (the divisor becomes 2) and resumed (`EXCEPTION_CONTINUE_EXECUTION`),
/// anything else goes to the `__except` block (`EXCEPTION_EXECUTE_HANDLER`).
#[unsafe(no_mangle)]
pub extern "win64" fn probe_seh_filter(pointers: *const ExceptionPointersRaw, _frame: u64) -> i32 {
    // SAFETY: the dispatcher's pointers: a record of at least 0x30 bytes
    // (`code` at 0, `information[1]` at 0x28), and our own statics.
    unsafe {
        let record = (*pointers).record;
        let code = read_volatile(record.cast::<u32>());
        let info1 = read_volatile(record.add(0x28).cast::<u64>());
        write_volatile(addr_of_mut!(PROBE_SEH_LAST_CODE), code);
        write_volatile(addr_of_mut!(PROBE_SEH_LAST_INFO1), info1 as u32);
        if code == DIVIDE_BY_ZERO {
            write_volatile(addr_of_mut!(PROBE_SEH_DIVISOR), 2);
            -1
        } else {
            1
        }
    }
}

/// `ud2`, then returns 7: for a vectored handler that steps over it. No
/// handler of its own.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "win64" fn probe_seh_ud2() -> u32 {
    naked_asm!("ud2", "mov eax, 7", "ret")
}

// Each scope record is { begin, end, handler, target } as RVAs: `handler`
// is the filter (1: `EXCEPTION_EXECUTE_HANDLER` without one) or, with a
// `target` of 0, the `__finally` block; `target` is the `__except` block.
global_asm!(
    // Exported by name: the linker takes these as /EXPORT options.
    ".section .drectve,\"yn\"",
    ".ascii \" /EXPORT:probe_seh_divide /EXPORT:probe_seh_filtered\"",
    ".ascii \" /EXPORT:probe_seh_fault_read /EXPORT:probe_seh_finally\"",
    ".ascii \" /EXPORT:probe_seh_raise /EXPORT:probe_seh_nested\"",
    ".text",
    // `__try { eax = dividend / divisor } __except (1) { eax = 0x1234 }`,
    // the 0x1234 kept in rbx across the fault.
    ".globl probe_seh_divide",
    ".p2align 4",
    "probe_seh_divide:",
    ".seh_proc probe_seh_divide",
    ".seh_handler {handler}, @unwind, @except",
    "    push rbx",
    ".seh_pushreg rbx",
    "    sub rsp, 32",
    ".seh_stackalloc 32",
    ".seh_endprologue",
    "    mov ebx, 0x1234",
    "probe_seh_divide_try_begin:",
    "    mov eax, ecx",
    "    xor ecx, ecx",
    "    xchg ecx, edx",
    "    div ecx",
    "probe_seh_divide_try_end:",
    "    jmp probe_seh_divide_done",
    "probe_seh_divide_except:",
    "    mov eax, ebx",
    "probe_seh_divide_done:",
    "    add rsp, 32",
    "    pop rbx",
    "    ret",
    ".seh_handlerdata",
    "    .long 1",
    "    .long probe_seh_divide_try_begin@IMGREL",
    "    .long probe_seh_divide_try_end@IMGREL",
    "    .long 1",
    "    .long probe_seh_divide_except@IMGREL",
    ".text",
    ".seh_endproc",
    // `__try { eax = dividend / PROBE_SEH_DIVISOR } __except
    // (probe_seh_filter(...)) { eax = 0x5678 }`.
    ".globl probe_seh_filtered",
    ".p2align 4",
    "probe_seh_filtered:",
    ".seh_proc probe_seh_filtered",
    ".seh_handler {handler}, @unwind, @except",
    "    sub rsp, 40",
    ".seh_stackalloc 40",
    ".seh_endprologue",
    "probe_seh_filtered_try_begin:",
    "    mov eax, ecx",
    "    xor edx, edx",
    "    div dword ptr [rip + {divisor}]",
    "probe_seh_filtered_try_end:",
    "    jmp probe_seh_filtered_done",
    "probe_seh_filtered_except:",
    "    mov eax, 0x5678",
    "probe_seh_filtered_done:",
    "    add rsp, 40",
    "    ret",
    ".seh_handlerdata",
    "    .long 1",
    "    .long probe_seh_filtered_try_begin@IMGREL",
    "    .long probe_seh_filtered_try_end@IMGREL",
    "    .long {filter}@IMGREL",
    "    .long probe_seh_filtered_except@IMGREL",
    ".text",
    ".seh_endproc",
    // `__try { eax = 0x1111; if (trigger) eax = *(int *)8; } __except
    // (probe_seh_filter(...)) { eax = 0x5678 }`.
    ".globl probe_seh_fault_read",
    ".p2align 4",
    "probe_seh_fault_read:",
    ".seh_proc probe_seh_fault_read",
    ".seh_handler {handler}, @unwind, @except",
    "    sub rsp, 40",
    ".seh_stackalloc 40",
    ".seh_endprologue",
    "probe_seh_fault_read_try_begin:",
    "    mov eax, 0x1111",
    "    test ecx, ecx",
    "    jz probe_seh_fault_read_try_end",
    "    mov eax, dword ptr [8]",
    "probe_seh_fault_read_try_end:",
    "    jmp probe_seh_fault_read_done",
    "probe_seh_fault_read_except:",
    "    mov eax, 0x5678",
    "probe_seh_fault_read_done:",
    "    add rsp, 40",
    "    ret",
    ".seh_handlerdata",
    "    .long 1",
    "    .long probe_seh_fault_read_try_begin@IMGREL",
    "    .long probe_seh_fault_read_try_end@IMGREL",
    "    .long {filter}@IMGREL",
    "    .long probe_seh_fault_read_except@IMGREL",
    ".text",
    ".seh_endproc",
    // `__try { __try { if (trigger) *(int *)8; } __finally { count++ }
    // eax = 0x1111 } __except (1) { eax = 0x9999 }`: the `__finally` block
    // is called on the normal path, as MSVC does, and by the unwind after a
    // fault. The inner scope comes first in the table.
    ".globl probe_seh_finally",
    ".p2align 4",
    "probe_seh_finally:",
    ".seh_proc probe_seh_finally",
    ".seh_handler {handler}, @unwind, @except",
    "    sub rsp, 40",
    ".seh_stackalloc 40",
    ".seh_endprologue",
    "probe_seh_finally_outer_begin:",
    "probe_seh_finally_inner_begin:",
    "    test ecx, ecx",
    "    jz probe_seh_finally_inner_end",
    "    mov eax, dword ptr [8]",
    "probe_seh_finally_inner_end:",
    "    mov ecx, 0",
    "    mov rdx, rsp",
    "    call probe_seh_finally_funclet",
    "    nop",
    "probe_seh_finally_outer_end:",
    "    mov eax, 0x1111",
    "    jmp probe_seh_finally_done",
    "probe_seh_finally_except:",
    "    mov eax, 0x9999",
    "probe_seh_finally_done:",
    "    add rsp, 40",
    "    ret",
    ".seh_handlerdata",
    "    .long 2",
    "    .long probe_seh_finally_inner_begin@IMGREL",
    "    .long probe_seh_finally_inner_end@IMGREL",
    "    .long probe_seh_finally_funclet@IMGREL",
    "    .long 0",
    "    .long probe_seh_finally_outer_begin@IMGREL",
    "    .long probe_seh_finally_outer_end@IMGREL",
    "    .long 1",
    "    .long probe_seh_finally_except@IMGREL",
    ".text",
    ".seh_endproc",
    // The `__finally` block: (abnormal: cl, frame: rdx).
    ".p2align 4",
    "probe_seh_finally_funclet:",
    ".seh_proc probe_seh_finally_funclet",
    ".seh_endprologue",
    "    inc dword ptr [rip + {count}]",
    "    ret",
    ".seh_endproc",
    // `__try { RaiseException(code, 0, 1, &0x77) } __except (1) { eax =
    // 0xABCD }`; the nop keeps the call's return address in the scope.
    ".globl probe_seh_raise",
    ".p2align 4",
    "probe_seh_raise:",
    ".seh_proc probe_seh_raise",
    ".seh_handler {handler}, @unwind, @except",
    "    sub rsp, 40",
    ".seh_stackalloc 40",
    ".seh_endprologue",
    "probe_seh_raise_try_begin:",
    "    lea r9, [rip + {args}]",
    "    mov r8d, 1",
    "    xor edx, edx",
    "    call qword ptr [rip + __imp_RaiseException]",
    "    nop",
    "probe_seh_raise_try_end:",
    "    xor eax, eax",
    "    jmp probe_seh_raise_done",
    "probe_seh_raise_except:",
    "    mov eax, 0xABCD",
    "probe_seh_raise_done:",
    "    add rsp, 40",
    "    ret",
    ".seh_handlerdata",
    "    .long 1",
    "    .long probe_seh_raise_try_begin@IMGREL",
    "    .long probe_seh_raise_try_end@IMGREL",
    "    .long 1",
    "    .long probe_seh_raise_except@IMGREL",
    ".text",
    ".seh_endproc",
    // `__try { eax = probe_seh_inner(trigger) } __except (1) { eax =
    // 0x4321 }`: the fault is a frame further in, with no handler there.
    ".globl probe_seh_nested",
    ".p2align 4",
    "probe_seh_nested:",
    ".seh_proc probe_seh_nested",
    ".seh_handler {handler}, @unwind, @except",
    "    sub rsp, 40",
    ".seh_stackalloc 40",
    ".seh_endprologue",
    "probe_seh_nested_try_begin:",
    "    call probe_seh_inner",
    "    nop",
    "probe_seh_nested_try_end:",
    "    jmp probe_seh_nested_done",
    "probe_seh_nested_except:",
    "    mov eax, 0x4321",
    "probe_seh_nested_done:",
    "    add rsp, 40",
    "    ret",
    ".seh_handlerdata",
    "    .long 1",
    "    .long probe_seh_nested_try_begin@IMGREL",
    "    .long probe_seh_nested_try_end@IMGREL",
    "    .long 1",
    "    .long probe_seh_nested_except@IMGREL",
    ".text",
    ".seh_endproc",
    // Reads address 8 when `trigger` is not 0, else returns 0x2222.
    ".p2align 4",
    "probe_seh_inner:",
    ".seh_proc probe_seh_inner",
    "    sub rsp, 40",
    ".seh_stackalloc 40",
    ".seh_endprologue",
    "    test ecx, ecx",
    "    jz probe_seh_inner_ok",
    "    mov eax, dword ptr [8]",
    "probe_seh_inner_ok:",
    "    mov eax, 0x2222",
    "    add rsp, 40",
    "    ret",
    ".seh_endproc",
    handler = sym crate::__C_specific_handler,
    divisor = sym PROBE_SEH_DIVISOR,
    filter = sym probe_seh_filter,
    count = sym PROBE_SEH_FINALLY_COUNT,
    args = sym PROBE_SEH_ARGS,
);
