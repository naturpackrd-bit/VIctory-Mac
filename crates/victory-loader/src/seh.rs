//! Windows structured exception handling where the host has none
//! (docs/macos.md, M2a): the work of ntdll's `KiUserExceptionDispatcher`,
//! `RtlDispatchException`, `RtlUnwindEx`, `RtlRaiseException`,
//! `RtlCaptureContext`, `RtlRestoreContext` and `__C_specific_handler`.
//!
//! An exception is a record and a context ([`ExceptionRecord`],
//! [`Context`], laid out as Windows'). A fault in title code becomes one in
//! `faults.rs`, which has the thread resume in [`dispatch_entry`] on its own
//! stack; `RaiseException` and `RtlRaiseException` make one from their
//! caller's context. [`dispatch_exception`] offers it to the vectored
//! handlers, then walks the title's frames from the context with their
//! unwind info (`unwind.rs`) and calls each frame's exception handler; a
//! handler that takes it unwinds to its target ([`unwind_ex`]), which calls
//! the termination handlers of the frames on the way and resumes the
//! target frame. Nothing found: the unhandled-exception filter, then a crash
//! report.
//!
//! Our own Rust frames have no unwind data and are never walked: the
//! functions title code calls (`RaiseException`, `RtlUnwindEx`, ...) start
//! from their caller's context, captured by a shim in assembly.
#![allow(non_snake_case)]

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use std::arch::naked_asm;
use std::ffi::c_int;
use std::io::Write as _;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Mutex, MutexGuard};

use victory_platform::faults::{Fault, FaultKind, Registers};

use crate::images::{self, Image};
use crate::teb::{self, Teb};
use crate::unwind::{self, RuntimeFunction};

/// `EXCEPTION_RECORD`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ExceptionRecord {
    pub code: u32,
    pub flags: u32,
    /// The record of the exception this one happened during, or null.
    pub record: *mut ExceptionRecord,
    /// Where it happened.
    pub address: *mut c_void,
    pub number_parameters: u32,
    _pad: u32,
    pub information: [u64; 15],
}

impl ExceptionRecord {
    /// A record of `code` with `flags`, at `address`, with no parameters.
    pub const fn new(code: u32, flags: u32, address: u64) -> ExceptionRecord {
        ExceptionRecord {
            code,
            flags,
            record: ptr::null_mut(),
            address: address as *mut c_void,
            number_parameters: 0,
            _pad: 0,
            information: [0; 15],
        }
    }
}

/// `EXCEPTION_POINTERS`: what a filter or vectored handler gets.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ExceptionPointers {
    pub record: *mut ExceptionRecord,
    pub context: *mut Context,
}

/// x64 `CONTEXT`: a thread's registers as Windows hands them to handlers.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Context {
    pub p1_home: u64,
    pub p2_home: u64,
    pub p3_home: u64,
    pub p4_home: u64,
    pub p5_home: u64,
    pub p6_home: u64,
    pub context_flags: u32,
    pub mx_csr: u32,
    pub seg_cs: u16,
    pub seg_ds: u16,
    pub seg_es: u16,
    pub seg_fs: u16,
    pub seg_gs: u16,
    pub seg_ss: u16,
    pub eflags: u32,
    pub dr0: u64,
    pub dr1: u64,
    pub dr2: u64,
    pub dr3: u64,
    pub dr6: u64,
    pub dr7: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    /// The `fxsave` area (`XMM_SAVE_AREA32`): its MxCsr at 0x18, xmm0..15
    /// at 0xA0.
    pub flt_save: [u8; 512],
    pub vector_register: [[u64; 2]; 26],
    pub vector_control: u64,
    pub debug_control: u64,
    pub last_branch_to_rip: u64,
    pub last_branch_from_rip: u64,
    pub last_exception_to_rip: u64,
    pub last_exception_from_rip: u64,
}

/// `DISPATCHER_CONTEXT`: what a language handler is told of the frame.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DispatcherContext {
    pub control_pc: u64,
    pub image_base: u64,
    pub function_entry: *const RuntimeFunctionRaw,
    pub establisher_frame: u64,
    pub target_ip: u64,
    pub context_record: *mut Context,
    pub language_handler: Option<LanguageHandler>,
    pub handler_data: *mut c_void,
    pub history_table: *mut c_void,
    pub scope_index: u32,
    pub fill0: u32,
}

/// `RUNTIME_FUNCTION`: the 12 bytes of a `.pdata` entry, as RVAs.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFunctionRaw {
    pub begin: u32,
    pub end: u32,
    pub unwind_info: u32,
}

/// A frame's language handler (`PEXCEPTION_ROUTINE`): an
/// `EXCEPTION_DISPOSITION`.
pub type LanguageHandler = unsafe extern "win64" fn(
    *mut ExceptionRecord,
    u64,
    *mut Context,
    *mut DispatcherContext,
) -> u32;
/// `PVECTORED_EXCEPTION_HANDLER`.
pub type VectoredHandler = unsafe extern "win64" fn(*mut ExceptionPointers) -> i32;
/// The unhandled-exception filter, given the `ExceptionPointers` as an
/// opaque pointer (kernel32's `SetUnhandledExceptionFilter` hands us one).
pub type UnhandledFilter = extern "system" fn(*const c_void) -> i32;

const _: () = {
    assert!(size_of::<ExceptionRecord>() == 152);
    assert!(offset_of!(ExceptionRecord, address) == 0x10);
    assert!(offset_of!(ExceptionRecord, number_parameters) == 0x18);
    assert!(offset_of!(ExceptionRecord, information) == 0x20);
    assert!(size_of::<ExceptionPointers>() == 16);
    assert!(size_of::<Context>() == 0x4D0);
    assert!(offset_of!(Context, context_flags) == 0x30);
    assert!(offset_of!(Context, mx_csr) == 0x34);
    assert!(offset_of!(Context, seg_cs) == 0x38);
    assert!(offset_of!(Context, seg_ss) == 0x42);
    assert!(offset_of!(Context, eflags) == 0x44);
    assert!(offset_of!(Context, dr0) == 0x48);
    assert!(offset_of!(Context, dr7) == 0x70);
    assert!(offset_of!(Context, rax) == 0x78);
    assert!(offset_of!(Context, rcx) == 0x80);
    assert!(offset_of!(Context, rdx) == 0x88);
    assert!(offset_of!(Context, rbx) == 0x90);
    assert!(offset_of!(Context, rsp) == 0x98);
    assert!(offset_of!(Context, rbp) == 0xA0);
    assert!(offset_of!(Context, rsi) == 0xA8);
    assert!(offset_of!(Context, rdi) == 0xB0);
    assert!(offset_of!(Context, r8) == 0xB8);
    assert!(offset_of!(Context, r15) == 0xF0);
    assert!(offset_of!(Context, rip) == 0xF8);
    assert!(offset_of!(Context, flt_save) == 0x100);
    assert!(offset_of!(Context, vector_register) == 0x300);
    assert!(offset_of!(Context, vector_control) == 0x4A0);
    assert!(offset_of!(Context, debug_control) == 0x4A8);
    assert!(offset_of!(Context, last_exception_from_rip) == 0x4C8);
    assert!(size_of::<DispatcherContext>() == 0x50);
    assert!(offset_of!(DispatcherContext, context_record) == 0x28);
    assert!(offset_of!(DispatcherContext, handler_data) == 0x38);
    assert!(offset_of!(DispatcherContext, scope_index) == 0x48);
    assert!(size_of::<RuntimeFunctionRaw>() == 12);
};

/// `EXCEPTION_DISPOSITION`s a language handler returns.
pub const EXCEPTION_CONTINUE_EXECUTION_DISPOSITION: u32 = 0;
pub const EXCEPTION_CONTINUE_SEARCH_DISPOSITION: u32 = 1;
pub const EXCEPTION_NESTED_EXCEPTION: u32 = 2;
pub const EXCEPTION_COLLIDED_UNWIND: u32 = 3;

/// What a filter or vectored handler returns.
pub const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
pub const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
pub const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;

/// `ExceptionRecord::flags`.
pub const EXCEPTION_NONCONTINUABLE: u32 = 1;
pub const EXCEPTION_UNWINDING: u32 = 2;
pub const EXCEPTION_EXIT_UNWIND: u32 = 4;
pub const EXCEPTION_STACK_INVALID: u32 = 8;
pub const EXCEPTION_NESTED_CALL: u32 = 0x10;
pub const EXCEPTION_TARGET_UNWIND: u32 = 0x20;
/// The flag (as against [`EXCEPTION_COLLIDED_UNWIND`], the disposition).
pub const EXCEPTION_COLLIDED_UNWIND_FLAG: u32 = 0x40;
pub const EXCEPTION_UNWIND: u32 = EXCEPTION_UNWINDING
    | EXCEPTION_EXIT_UNWIND
    | EXCEPTION_TARGET_UNWIND
    | EXCEPTION_COLLIDED_UNWIND_FLAG;

/// Exception codes.
pub const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const STATUS_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const STATUS_BREAKPOINT: u32 = 0x8000_0003;
pub const STATUS_SINGLE_STEP: u32 = 0x8000_0004;
pub const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub const STATUS_INTEGER_OVERFLOW: u32 = 0xC000_0095;
pub const STATUS_PRIVILEGED_INSTRUCTION: u32 = 0xC000_0096;
/// `__fastfail` (`int 0x29`), the fail-fast code in its first parameter.
pub const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409;
pub const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;
pub const STATUS_NONCONTINUABLE_EXCEPTION: u32 = 0xC000_0025;
pub const STATUS_INVALID_DISPOSITION: u32 = 0xC000_0026;
pub const STATUS_UNWIND: u32 = 0xC000_0027;
pub const STATUS_BAD_STACK: u32 = 0xC000_0028;
pub const STATUS_INVALID_UNWIND_TARGET: u32 = 0xC000_0029;
pub const STATUS_GUARD_PAGE_VIOLATION: u32 = 0x8000_0001;
pub const STATUS_IN_PAGE_ERROR: u32 = 0xC000_0006;
pub const STATUS_FLOAT_DIVIDE_BY_ZERO: u32 = 0xC000_008E;
pub const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
/// `RaiseFailFastException`'s code when it is given no record.
pub const STATUS_FAIL_FAST_EXCEPTION: u32 = 0xC000_0602;
pub const DBG_PRINTEXCEPTION_C: u32 = 0x4001_0006;

/// `Context::context_flags`.
pub const CONTEXT_AMD64: u32 = 0x10_0000;
/// The parts' bits alone (Windows' headers add `CONTEXT_AMD64` to each).
pub const CONTEXT_CONTROL: u32 = 1;
pub const CONTEXT_INTEGER: u32 = 2;
pub const CONTEXT_SEGMENTS: u32 = 4;
pub const CONTEXT_FLOATING_POINT: u32 = 8;
pub const CONTEXT_DEBUG_REGISTERS: u32 = 0x10;
pub const CONTEXT_FULL: u32 = CONTEXT_AMD64 | 1 | 2 | 8;
pub const CONTEXT_ALL: u32 = CONTEXT_FULL | 4 | 0x10;

/// Unwind info flags: the function has an exception handler, a
/// termination handler.
pub const UNW_FLAG_EHANDLER: u8 = 1;
pub const UNW_FLAG_UHANDLER: u8 = 2;

/// The name of an exception code, for crash reports.
pub(crate) fn status_name(code: u32) -> &'static str {
    match code {
        STATUS_ACCESS_VIOLATION => "STATUS_ACCESS_VIOLATION",
        STATUS_ILLEGAL_INSTRUCTION => "STATUS_ILLEGAL_INSTRUCTION",
        STATUS_BREAKPOINT => "STATUS_BREAKPOINT",
        STATUS_SINGLE_STEP => "STATUS_SINGLE_STEP",
        STATUS_INTEGER_DIVIDE_BY_ZERO => "STATUS_INTEGER_DIVIDE_BY_ZERO",
        STATUS_INTEGER_OVERFLOW => "STATUS_INTEGER_OVERFLOW",
        STATUS_PRIVILEGED_INSTRUCTION => "STATUS_PRIVILEGED_INSTRUCTION",
        STATUS_STACK_BUFFER_OVERRUN => "STATUS_STACK_BUFFER_OVERRUN",
        STATUS_STACK_OVERFLOW => "STATUS_STACK_OVERFLOW",
        STATUS_NONCONTINUABLE_EXCEPTION => "STATUS_NONCONTINUABLE_EXCEPTION",
        STATUS_INVALID_DISPOSITION => "STATUS_INVALID_DISPOSITION",
        STATUS_UNWIND => "STATUS_UNWIND",
        STATUS_BAD_STACK => "STATUS_BAD_STACK",
        STATUS_INVALID_UNWIND_TARGET => "STATUS_INVALID_UNWIND_TARGET",
        STATUS_GUARD_PAGE_VIOLATION => "STATUS_GUARD_PAGE_VIOLATION",
        STATUS_IN_PAGE_ERROR => "STATUS_IN_PAGE_ERROR",
        STATUS_FLOAT_DIVIDE_BY_ZERO => "STATUS_FLOAT_DIVIDE_BY_ZERO",
        STATUS_INVALID_HANDLE => "STATUS_INVALID_HANDLE",
        STATUS_FAIL_FAST_EXCEPTION => "STATUS_FAIL_FAST_EXCEPTION",
        DBG_PRINTEXCEPTION_C => "DBG_PRINTEXCEPTION_C",
        _ => "unknown",
    }
}

/// Where xmm0 lies in `Context::flt_save`; xmm1..15 follow, 16 bytes each.
const XMM0: usize = 0xA0;
/// Where the MxCsr lies in `Context::flt_save`.
const FLT_MXCSR: usize = 0x18;
/// MXCSR as a thread starts: every exception masked, round to nearest.
const MXCSR_DEFAULT: u32 = 0x1F80;

impl Context {
    /// A context of zeros.
    pub const fn zeroed() -> Context {
        // SAFETY: integers and arrays of them, for which zero is a value.
        unsafe { core::mem::zeroed() }
    }

    /// The register `xmm{i}` (`i < 16`), low then high qword.
    pub fn xmm(&self, i: usize) -> [u64; 2] {
        let at = XMM0 + 16 * i;
        let qword = |at: usize| {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(&self.flt_save[at..at + 8]);
            u64::from_le_bytes(bytes)
        };
        [qword(at), qword(at + 8)]
    }

    /// Sets the register `xmm{i}` (`i < 16`).
    pub fn set_xmm(&mut self, i: usize, value: [u64; 2]) {
        let at = XMM0 + 16 * i;
        self.flt_save[at..at + 8].copy_from_slice(&value[0].to_le_bytes());
        self.flt_save[at + 8..at + 16].copy_from_slice(&value[1].to_le_bytes());
    }

    /// The context of a thread whose registers are `r`, as Windows x64
    /// shows a user-mode thread: the selectors it uses, the MXCSR a thread
    /// starts with (the signal side has none to give), and `CONTEXT_FULL |
    /// CONTEXT_SEGMENTS`.
    pub fn from_registers(r: &Registers) -> Context {
        let mut c = Context::zeroed();
        c.context_flags = CONTEXT_FULL | CONTEXT_SEGMENTS;
        c.mx_csr = MXCSR_DEFAULT;
        c.flt_save[FLT_MXCSR..FLT_MXCSR + 4].copy_from_slice(&MXCSR_DEFAULT.to_le_bytes());
        c.seg_cs = 0x33;
        (c.seg_ds, c.seg_es, c.seg_gs, c.seg_ss) = (0x2B, 0x2B, 0x2B, 0x2B);
        c.seg_fs = 0x53;
        c.eflags = r.rflags as u32;
        (c.rax, c.rcx, c.rdx, c.rbx) = (r.rax, r.rcx, r.rdx, r.rbx);
        (c.rsp, c.rbp, c.rsi, c.rdi) = (r.rsp, r.rbp, r.rsi, r.rdi);
        (c.r8, c.r9, c.r10, c.r11) = (r.r8, r.r9, r.r10, r.r11);
        (c.r12, c.r13, c.r14, c.r15) = (r.r12, r.r13, r.r14, r.r15);
        c.rip = r.rip;
        for (i, xmm) in r.xmm.iter().enumerate() {
            c.set_xmm(i, *xmm);
        }
        c
    }

    /// The registers of this context.
    pub fn to_registers(&self) -> Registers {
        Registers {
            rax: self.rax,
            rbx: self.rbx,
            rcx: self.rcx,
            rdx: self.rdx,
            rsi: self.rsi,
            rdi: self.rdi,
            rbp: self.rbp,
            rsp: self.rsp,
            r8: self.r8,
            r9: self.r9,
            r10: self.r10,
            r11: self.r11,
            r12: self.r12,
            r13: self.r13,
            r14: self.r14,
            r15: self.r15,
            rip: self.rip,
            rflags: u64::from(self.eflags),
            xmm: core::array::from_fn(|i| self.xmm(i)),
            ymm_upper: None,
        }
    }

    /// This context as the unwinder takes it.
    pub fn to_unwind(&self) -> unwind::Context {
        let r = self.to_registers();
        unwind::Context {
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rbp: r.rbp,
            rsp: r.rsp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rip: r.rip,
            rflags: r.rflags,
            xmm: r.xmm,
        }
    }

    /// Takes the registers of `u` (an unwound frame's): every general
    /// register, rip, the flags and the xmm registers.
    pub fn apply_unwind(&mut self, u: &unwind::Context) {
        (self.rax, self.rcx, self.rdx, self.rbx) = (u.rax, u.rcx, u.rdx, u.rbx);
        (self.rsp, self.rbp, self.rsi, self.rdi) = (u.rsp, u.rbp, u.rsi, u.rdi);
        (self.r8, self.r9, self.r10, self.r11) = (u.r8, u.r9, u.r10, u.r11);
        (self.r12, self.r13, self.r14, self.r15) = (u.r12, u.r13, u.r14, u.r15);
        self.rip = u.rip;
        self.eflags = u.rflags as u32;
        for (i, xmm) in u.xmm.iter().enumerate() {
            self.set_xmm(i, *xmm);
        }
    }
}

// libc's, which this crate does not depend on (`<unistd.h>`).
unsafe extern "C" {
    fn _exit(status: c_int) -> !;
}

/// `RtlCaptureContext`: the caller's registers into `ctx` (`rcx`), as they
/// are when this returns: rip its return address, rsp above it. The
/// context need not be aligned (`movups`).
///
/// # Safety
/// `ctx` points at a writable [`Context`].
#[unsafe(naked)]
pub unsafe extern "win64" fn RtlCaptureContext(ctx: *mut Context) {
    naked_asm!(
        "pushfq",
        "mov [rcx + 0x78], rax",
        "mov [rcx + 0x80], rcx",
        "mov [rcx + 0x88], rdx",
        "mov [rcx + 0x90], rbx",
        "mov [rcx + 0xA0], rbp",
        "mov [rcx + 0xA8], rsi",
        "mov [rcx + 0xB0], rdi",
        "mov [rcx + 0xB8], r8",
        "mov [rcx + 0xC0], r9",
        "mov [rcx + 0xC8], r10",
        "mov [rcx + 0xD0], r11",
        "mov [rcx + 0xD8], r12",
        "mov [rcx + 0xE0], r13",
        "mov [rcx + 0xE8], r14",
        "mov [rcx + 0xF0], r15",
        // The flags pushed, then the return address: the caller's rip.
        "mov rax, [rsp]",
        "mov [rcx + 0x44], eax",
        "mov rax, [rsp + 8]",
        "mov [rcx + 0xF8], rax",
        // The caller's rsp once this returns.
        "lea rax, [rsp + 16]",
        "mov [rcx + 0x98], rax",
        "mov word ptr [rcx + 0x38], cs",
        "mov word ptr [rcx + 0x3A], ds",
        "mov word ptr [rcx + 0x3C], es",
        "mov word ptr [rcx + 0x3E], fs",
        "mov word ptr [rcx + 0x40], gs",
        "mov word ptr [rcx + 0x42], ss",
        "stmxcsr dword ptr [rcx + 0x34]",
        "stmxcsr dword ptr [rcx + 0x118]",
        "movups [rcx + 0x1A0], xmm0",
        "movups [rcx + 0x1B0], xmm1",
        "movups [rcx + 0x1C0], xmm2",
        "movups [rcx + 0x1D0], xmm3",
        "movups [rcx + 0x1E0], xmm4",
        "movups [rcx + 0x1F0], xmm5",
        "movups [rcx + 0x200], xmm6",
        "movups [rcx + 0x210], xmm7",
        "movups [rcx + 0x220], xmm8",
        "movups [rcx + 0x230], xmm9",
        "movups [rcx + 0x240], xmm10",
        "movups [rcx + 0x250], xmm11",
        "movups [rcx + 0x260], xmm12",
        "movups [rcx + 0x270], xmm13",
        "movups [rcx + 0x280], xmm14",
        "movups [rcx + 0x290], xmm15",
        // CONTEXT_FULL | CONTEXT_SEGMENTS
        "mov dword ptr [rcx + 0x30], 0x10000F",
        "popfq",
        "ret",
    )
}

/// `RtlRestoreContext`: the thread goes on in `ctx` (`rcx`): its MXCSR,
/// xmm registers, flags, general registers, rsp and rip. The new rip is
/// pushed below the new rsp and returned to, as Wine does: Win64 code
/// keeps nothing there. `record` (`rdx`) is not used.
///
/// # Safety
/// `ctx` is a context a thread can go on in.
#[unsafe(naked)]
pub unsafe extern "win64" fn RtlRestoreContext(
    ctx: *const Context,
    record: *const ExceptionRecord,
) -> ! {
    naked_asm!(
        "ldmxcsr dword ptr [rcx + 0x34]",
        "movups xmm0, [rcx + 0x1A0]",
        "movups xmm1, [rcx + 0x1B0]",
        "movups xmm2, [rcx + 0x1C0]",
        "movups xmm3, [rcx + 0x1D0]",
        "movups xmm4, [rcx + 0x1E0]",
        "movups xmm5, [rcx + 0x1F0]",
        "movups xmm6, [rcx + 0x200]",
        "movups xmm7, [rcx + 0x210]",
        "movups xmm8, [rcx + 0x220]",
        "movups xmm9, [rcx + 0x230]",
        "movups xmm10, [rcx + 0x240]",
        "movups xmm11, [rcx + 0x250]",
        "movups xmm12, [rcx + 0x260]",
        "movups xmm13, [rcx + 0x270]",
        "movups xmm14, [rcx + 0x280]",
        "movups xmm15, [rcx + 0x290]",
        "mov eax, [rcx + 0x44]",
        "push rax",
        "popfq",
        "mov rbx, [rcx + 0x90]",
        "mov rbp, [rcx + 0xA0]",
        "mov rsi, [rcx + 0xA8]",
        "mov rdi, [rcx + 0xB0]",
        "mov r8, [rcx + 0xB8]",
        "mov r9, [rcx + 0xC0]",
        "mov r10, [rcx + 0xC8]",
        "mov r11, [rcx + 0xD0]",
        "mov r12, [rcx + 0xD8]",
        "mov r13, [rcx + 0xE0]",
        "mov r14, [rcx + 0xE8]",
        "mov r15, [rcx + 0xF0]",
        "mov rdx, [rcx + 0x88]",
        "mov rax, [rcx + 0x78]",
        "mov rsp, [rcx + 0x98]",
        "push qword ptr [rcx + 0xF8]",
        "mov rcx, [rcx + 0x80]",
        "ret",
    )
}

/// The frame of a caller-context shim: below its saved rbp, the four
/// register arguments (32 bytes); below them a [`Context`] at `rsp + 32`,
/// above the 32 bytes of shadow space its calls need. A multiple of 16, so
/// that rsp is 16-aligned at those calls (it is after `push rbp`).
const SHIM_FRAME: usize = 0x510;
const _: () =
    assert!(32 + size_of::<Context>() + 32 <= SHIM_FRAME && SHIM_FRAME.is_multiple_of(16));

/// `RaiseException`: an exception of `code`, with `count` parameters
/// (at most 15 are kept) from `args`, raised from the caller's context.
///
/// # Safety
/// `args` holds `count` values, or `count` is 0. Called from title code.
#[unsafe(naked)]
pub unsafe extern "win64" fn RaiseException(code: u32, flags: u32, count: u32, args: *const u64) {
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
        "mov rdx, [rbp - 24]",
        "mov rcx, [rbp - 32]",
        "lea r8, [rsp + 32]",
        "call {raise}",
        "ud2",
        frame = const SHIM_FRAME,
        capture = sym RtlCaptureContext,
        raise = sym raise_exception,
    )
}

/// `RtlRaiseException`: `record` raised from the caller's context, its
/// address the caller's.
///
/// # Safety
/// `record` points at an exception record. Called from title code.
#[unsafe(naked)]
pub unsafe extern "win64" fn RtlRaiseException(record: *mut ExceptionRecord) {
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
        "mov rax, [rbp + 8]",
        "mov [rsp + 32 + 0xF8], rax",
        "lea rax, [rbp + 16]",
        "mov [rsp + 32 + 0x98], rax",
        "mov rax, [rbp]",
        "mov [rsp + 32 + 0xA0], rax",
        "mov rdi, [rbp - 8]",
        "lea rsi, [rsp + 32]",
        "call {raise}",
        "ud2",
        frame = const SHIM_FRAME,
        capture = sym RtlCaptureContext,
        raise = sym rtl_raise_exception,
    )
}

/// `RtlUnwindEx`: unwinds from the caller's frame to `target_frame` (0:
/// the whole stack, an exit unwind), calling the termination handlers on
/// the way, and resumes the target frame at `target_ip` with rax
/// `return_value`. `ctx`, when not null, is where the walk's context is
/// kept (Windows' scratch); `history` is not used.
///
/// # Safety
/// The arguments are as Windows takes them. Called from title code.
#[unsafe(naked)]
pub unsafe extern "win64" fn RtlUnwindEx(
    target_frame: u64,
    target_ip: u64,
    record: *mut ExceptionRecord,
    return_value: u64,
    ctx: *mut Context,
    history: *mut c_void,
) {
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
        "mov rax, [rbp + 8]",
        "mov [rsp + 32 + 0xF8], rax",
        "lea rax, [rbp + 16]",
        "mov [rsp + 32 + 0x98], rax",
        "mov rax, [rbp]",
        "mov [rsp + 32 + 0xA0], rax",
        "mov rdi, [rbp - 8]",
        "mov rsi, [rbp - 16]",
        "mov rdx, [rbp - 24]",
        "mov rcx, [rbp - 32]",
        "lea r8, [rsp + 32]",
        // The fifth argument: above the return address and shadow space.
        "mov r9, [rbp + 48]",
        "call {unwind}",
        "ud2",
        frame = const SHIM_FRAME,
        capture = sym RtlCaptureContext,
        unwind = sym unwind_from_caller,
    )
}

/// `RtlUnwind`: [`RtlUnwindEx`] with a context of its own.
///
/// # Safety
/// As [`RtlUnwindEx`].
#[unsafe(naked)]
pub unsafe extern "win64" fn RtlUnwind(
    target_frame: u64,
    target_ip: u64,
    record: *mut ExceptionRecord,
    return_value: u64,
) {
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
        "mov rax, [rbp + 8]",
        "mov [rsp + 32 + 0xF8], rax",
        "lea rax, [rbp + 16]",
        "mov [rsp + 32 + 0x98], rax",
        "mov rax, [rbp]",
        "mov [rsp + 32 + 0xA0], rax",
        "mov rdi, [rbp - 8]",
        "mov rsi, [rbp - 16]",
        "mov rdx, [rbp - 24]",
        "mov rcx, [rbp - 32]",
        "lea r8, [rsp + 32]",
        "xor r9d, r9d",
        "call {unwind}",
        "ud2",
        frame = const SHIM_FRAME,
        capture = sym RtlCaptureContext,
        unwind = sym unwind_from_caller,
    )
}

/// `RaiseException`'s work, with the caller's context.
///
/// # Safety
/// As [`RaiseException`]; `ctx` is the shim's captured context.
unsafe extern "C" fn raise_exception(
    code: u32,
    flags: u32,
    count: u32,
    args: *const u64,
    ctx: *mut Context,
) -> ! {
    // SAFETY: the shim's context, alive in its frame, which never returns.
    let ctx = unsafe { &mut *ctx };
    let mut record = ExceptionRecord::new(code, flags & EXCEPTION_NONCONTINUABLE, ctx.rip);
    let count = if args.is_null() { 0 } else { count.min(15) };
    record.number_parameters = count;
    for i in 0..count as usize {
        // SAFETY: the caller's `count` values at `args`.
        record.information[i] = unsafe { args.add(i).read_unaligned() };
    }
    raise(&mut record, ctx)
}

/// `RtlRaiseException`'s work, with the caller's context: a copy of the
/// record, at the caller's address. A null record is an access violation,
/// as reading it would be on Windows.
///
/// # Safety
/// As [`RtlRaiseException`]; `ctx` is the shim's captured context.
unsafe extern "C" fn rtl_raise_exception(record: *mut ExceptionRecord, ctx: *mut Context) -> ! {
    // SAFETY: the shim's context, alive in its frame, which never returns.
    let ctx = unsafe { &mut *ctx };
    if record.is_null() {
        fail(STATUS_ACCESS_VIOLATION, ctx);
    }
    // SAFETY: the caller's record.
    let mut copy = unsafe { record.read_unaligned() };
    copy.address = ctx.rip as *mut c_void;
    raise(&mut copy, ctx)
}

/// `RtlUnwindEx`'s work from the caller's context `captured`: copied into
/// `scratch` when there is one, and the walk done there.
///
/// # Safety
/// As [`RtlUnwindEx`]; `captured` is the shim's captured context.
unsafe extern "C" fn unwind_from_caller(
    target_frame: u64,
    target_ip: u64,
    record: *mut ExceptionRecord,
    return_value: u64,
    captured: *mut Context,
    scratch: *mut Context,
) -> ! {
    let ctx = if scratch.is_null() {
        captured
    } else {
        // SAFETY: the caller's context buffer, and the shim's context.
        unsafe { scratch.write_unaligned(captured.read()) };
        scratch
    };
    // SAFETY: the caller's arguments; a context of the caller's state.
    unsafe { unwind_ex(target_frame, target_ip, record, return_value, ctx) }
}

/// The unhandled-exception filter, as an address; 0 when none.
static UNHANDLED: AtomicUsize = AtomicUsize::new(0);

/// Sets the unhandled-exception filter: what an exception no frame takes
/// goes to before the crash report.
pub fn set_unhandled_filter(filter: Option<UnhandledFilter>) {
    UNHANDLED.store(filter.map_or(0, |f| f as usize), SeqCst);
}

fn unhandled_filter() -> Option<UnhandledFilter> {
    match UNHANDLED.load(SeqCst) {
        0 => None,
        // SAFETY: the address of an `UnhandledFilter` stored above.
        f => Some(unsafe { std::mem::transmute::<usize, UnhandledFilter>(f) }),
    }
}

type Handlers = Mutex<Vec<(usize, VectoredHandler)>>;

/// The vectored exception handlers, in the order they are called, each
/// with its id.
static VECTORED: Handlers = Mutex::new(Vec::new());
/// The vectored continue handlers, likewise.
static CONTINUE: Handlers = Mutex::new(Vec::new());
/// The next handler's id (never 0: a handle of 0 is a failure).
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

fn lock(list: &Handlers) -> MutexGuard<'_, Vec<(usize, VectoredHandler)>> {
    list.lock().unwrap_or_else(|e| e.into_inner())
}

fn add(list: &Handlers, first: bool, handler: VectoredHandler) -> *mut c_void {
    let id = NEXT_ID.fetch_add(1, SeqCst);
    let mut list = lock(list);
    if first {
        list.insert(0, (id, handler));
    } else {
        list.push((id, handler));
    }
    id as *mut c_void
}

fn remove(list: &Handlers, handle: *mut c_void) -> bool {
    let mut list = lock(list);
    let before = list.len();
    list.retain(|&(id, _)| id != handle as usize);
    list.len() != before
}

/// Calls the handlers of `list` with `pointers` (the list copied, its lock
/// released) until one returns `EXCEPTION_CONTINUE_EXECUTION`: whether one
/// did.
fn call_handlers(list: &Handlers, pointers: &mut ExceptionPointers) -> bool {
    let handlers: Vec<VectoredHandler> = lock(list).iter().map(|&(_, h)| h).collect();
    handlers.into_iter().any(|handler| {
        // SAFETY: a handler the title registered, given the exception.
        unsafe { handler(pointers) == EXCEPTION_CONTINUE_EXECUTION }
    })
}

/// `AddVectoredExceptionHandler`: `handler` is called for every exception
/// before any frame's handler, first of all if `first`; its handle.
pub fn add_vectored_handler(first: bool, handler: VectoredHandler) -> *mut c_void {
    add(&VECTORED, first, handler)
}

/// `RemoveVectoredExceptionHandler`: whether `handle` named one.
pub fn remove_vectored_handler(handle: *mut c_void) -> bool {
    remove(&VECTORED, handle)
}

/// `AddVectoredContinueHandler`: `handler` is called when an exception was
/// handled and the thread goes on.
pub fn add_continue_handler(first: bool, handler: VectoredHandler) -> *mut c_void {
    add(&CONTINUE, first, handler)
}

/// `RemoveVectoredContinueHandler`: whether `handle` named one.
pub fn remove_continue_handler(handle: *mut c_void) -> bool {
    remove(&CONTINUE, handle)
}

/// An image's exception table: its address and its bytes (none when it has
/// no table).
fn table_of(image: &Image) -> (usize, &[u8]) {
    match image.exception_table {
        // SAFETY: the mapped image's bytes, alive while it is mapped (title
        // code that runs keeps its image mapped).
        Some((at, len)) => (at, unsafe {
            std::slice::from_raw_parts(at as *const u8, len)
        }),
        None => (0, &[]),
    }
}

/// The address of `function`'s entry in the table at `at` (`pdata`), or
/// null.
fn entry_address(at: usize, pdata: &[u8], function: &RuntimeFunction) -> *const RuntimeFunctionRaw {
    match unwind::functions(pdata).iter().position(|f| f == function) {
        Some(i) => (at + i * 12) as *const RuntimeFunctionRaw,
        None => ptr::null(),
    }
}

/// Whether `rsp` lies outside this thread's stack (as its TEB has it; any
/// rsp is inside without a TEB).
fn off_stack(rsp: u64) -> bool {
    teb::current_teb().is_some_and(|teb| {
        // SAFETY: this thread's TEB, ours, alive for the thread's life.
        let teb = unsafe { &*(teb as *const Teb) };
        rsp < teb.stack_limit || rsp >= teb.stack_base
    })
}

/// `RtlDispatchException`'s search: the vectored handlers, then each
/// title frame's exception handler from `ctx` outward; true when one has
/// the thread go on in `ctx` (`EXCEPTION_CONTINUE_EXECUTION`), false when
/// the walk ends (an address in no image, a frame that cannot be undone,
/// the end of the stack). A handler that takes the exception unwinds and
/// never returns here.
pub fn dispatch_exception(record: &mut ExceptionRecord, ctx: &mut Context) -> bool {
    let mut pointers = ExceptionPointers {
        record,
        context: ctx,
    };
    if call_handlers(&VECTORED, &mut pointers) {
        return true;
    }
    let mut walk = ctx.to_unwind();
    loop {
        let Some(image) = images::find(walk.rip as usize) else {
            return false; // code in no image
        };
        let (at, pdata) = table_of(&image);
        let before = walk;
        let unwound = match unwind::virtual_unwind(image.base as u64, pdata, &mut walk) {
            Err(_) => return false,
            Ok(None) if walk.rsp > before.rsp => continue, // a leaf
            Ok(None) => return false,
            Ok(Some(unwound)) => unwound,
        };
        if let Some(handler) = unwound.handler.filter(|h| h.flags & UNW_FLAG_EHANDLER != 0) {
            // SAFETY: the address of the frame's language handler, from its
            // unwind info.
            let language = unsafe { std::mem::transmute::<u64, LanguageHandler>(handler.address) };
            let mut dc = DispatcherContext {
                control_pc: before.rip,
                image_base: image.base as u64,
                function_entry: entry_address(at, pdata, &unwound.function),
                establisher_frame: unwound.establisher_frame,
                target_ip: 0,
                context_record: ctx,
                language_handler: Some(language),
                handler_data: handler.data as *mut c_void,
                history_table: ptr::null_mut(),
                scope_index: 0,
                // `ControlPcIsUnwound`: the pc is a return address.
                fill0: u32::from(before.rsp != ctx.rsp),
            };
            // SAFETY: the frame's handler, given what Windows gives it.
            let disposition = unsafe { language(record, unwound.establisher_frame, ctx, &mut dc) };
            match disposition {
                EXCEPTION_CONTINUE_EXECUTION_DISPOSITION => {
                    if record.flags & EXCEPTION_NONCONTINUABLE != 0 {
                        fail(STATUS_NONCONTINUABLE_EXCEPTION, ctx);
                    }
                    return true;
                }
                EXCEPTION_CONTINUE_SEARCH_DISPOSITION | EXCEPTION_COLLIDED_UNWIND => {}
                EXCEPTION_NESTED_EXCEPTION => record.flags |= EXCEPTION_NESTED_CALL,
                _ => fail(STATUS_INVALID_DISPOSITION, ctx),
            }
        }
        if walk.rsp <= before.rsp || off_stack(walk.rsp) {
            return false;
        }
    }
}

/// Raises the exception `record` in `ctx`: dispatched; when a handler has
/// the thread go on, the vectored continue handlers, then `ctx` restored;
/// when none takes it, the unhandled-exception filter, then the crash
/// report and the end of the process.
pub fn raise(record: &mut ExceptionRecord, ctx: &mut Context) -> ! {
    raise_resuming(record, ctx, None)
}

/// [`raise`], the thread going on with the upper halves of ymm0..15
/// `upper` (a fault's: the context has the xmm halves only, and the
/// handlers' own code clears the upper ones, `vzeroupper`).
fn raise_resuming(
    record: &mut ExceptionRecord,
    ctx: &mut Context,
    upper: Option<&[[u64; 2]; 16]>,
) -> ! {
    if dispatch_exception(record, ctx) {
        let mut pointers = ExceptionPointers {
            record,
            context: ctx,
        };
        call_handlers(&CONTINUE, &mut pointers);
        // SAFETY: the context a handler has the thread go on in.
        unsafe { restore(ctx, upper) }
    }
    if let Some(filter) = unhandled_filter() {
        let pointers = ExceptionPointers {
            record,
            context: ctx,
        };
        match filter((&raw const pointers).cast()) {
            // SAFETY: as above.
            EXCEPTION_CONTINUE_EXECUTION => unsafe { restore(ctx, upper) },
            // SAFETY: the end of the process, with the exception's code.
            EXCEPTION_EXECUTE_HANDLER => unsafe { _exit(record.code as c_int) },
            _ => {}
        }
    }
    fatal(record, ctx)
}

/// The thread goes on in `ctx`, as [`RtlRestoreContext`] has it, with the
/// upper halves of ymm0..15 `upper` where there are some and the processor
/// has AVX: put in first (`vinsertf128`), and kept by the SSE moves of
/// `RtlRestoreContext`, which leave them alone.
///
/// # Safety
/// As [`RtlRestoreContext`].
unsafe fn restore(ctx: &Context, upper: Option<&[[u64; 2]; 16]>) -> ! {
    match upper {
        // SAFETY: the caller's context; the processor has AVX.
        Some(upper) if std::arch::is_x86_feature_detected!("avx") => unsafe {
            restore_with_upper(ctx, upper)
        },
        // SAFETY: the caller's context.
        _ => unsafe { RtlRestoreContext(ctx, ptr::null()) },
    }
}

/// The upper halves of ymm0..15 from `upper` (`rdx`), then
/// [`RtlRestoreContext`] of `ctx` (`rcx`).
///
/// # Safety
/// As [`RtlRestoreContext`]; the processor has AVX.
#[unsafe(naked)]
unsafe extern "win64" fn restore_with_upper(
    ctx: *const Context,
    upper: *const [[u64; 2]; 16],
) -> ! {
    naked_asm!(
        "vinsertf128 ymm0, ymm0, xmmword ptr [rdx], 1",
        "vinsertf128 ymm1, ymm1, xmmword ptr [rdx + 0x10], 1",
        "vinsertf128 ymm2, ymm2, xmmword ptr [rdx + 0x20], 1",
        "vinsertf128 ymm3, ymm3, xmmword ptr [rdx + 0x30], 1",
        "vinsertf128 ymm4, ymm4, xmmword ptr [rdx + 0x40], 1",
        "vinsertf128 ymm5, ymm5, xmmword ptr [rdx + 0x50], 1",
        "vinsertf128 ymm6, ymm6, xmmword ptr [rdx + 0x60], 1",
        "vinsertf128 ymm7, ymm7, xmmword ptr [rdx + 0x70], 1",
        "vinsertf128 ymm8, ymm8, xmmword ptr [rdx + 0x80], 1",
        "vinsertf128 ymm9, ymm9, xmmword ptr [rdx + 0x90], 1",
        "vinsertf128 ymm10, ymm10, xmmword ptr [rdx + 0xA0], 1",
        "vinsertf128 ymm11, ymm11, xmmword ptr [rdx + 0xB0], 1",
        "vinsertf128 ymm12, ymm12, xmmword ptr [rdx + 0xC0], 1",
        "vinsertf128 ymm13, ymm13, xmmword ptr [rdx + 0xD0], 1",
        "vinsertf128 ymm14, ymm14, xmmword ptr [rdx + 0xE0], 1",
        "vinsertf128 ymm15, ymm15, xmmword ptr [rdx + 0xF0], 1",
        "xor edx, edx",
        "jmp {restore}",
        restore = sym RtlRestoreContext,
    )
}

/// The end of the process for an exception of `code` that cannot go on
/// (an invalid disposition, a stack that cannot be walked).
fn fail(code: u32, ctx: &Context) -> ! {
    fatal(
        &ExceptionRecord::new(code, EXCEPTION_NONCONTINUABLE, ctx.rip),
        ctx,
    )
}

/// The crash report of an exception no handler took (to stderr), and the
/// end of the process with the status of the signal it would have been.
fn fatal(record: &ExceptionRecord, ctx: &Context) -> ! {
    let mut registers = ctx.to_registers();
    let kind = match record.code {
        // A fail-fast comes as the #GP of `int 0x29`: SIGSEGV.
        STATUS_ACCESS_VIOLATION | STATUS_STACK_BUFFER_OVERRUN => FaultKind::Access,
        STATUS_ILLEGAL_INSTRUCTION | STATUS_PRIVILEGED_INSTRUCTION => FaultKind::IllegalInstruction,
        STATUS_BREAKPOINT | STATUS_SINGLE_STEP => FaultKind::Breakpoint,
        _ => FaultKind::Arithmetic,
    };
    let fault = Fault {
        kind,
        address: if record.code == STATUS_ACCESS_VIOLATION {
            record.information[1] as usize
        } else {
            0
        },
        write: record.information[0] == 1,
        trap: None,
        error: 0,
        registers: &mut registers,
    };
    let mut text = String::new();
    crate::faults::report_exception(record.code, record, &fault, &mut text);
    let _ = std::io::stderr().write_all(text.as_bytes());
    // SAFETY: the end of the process.
    unsafe { _exit(128 + crate::faults::signal_number(kind)) }
}

/// `RaiseFailFastException`'s work: `record` (or one of
/// `STATUS_FAIL_FAST_EXCEPTION`) in `ctx` (or a context of zeros) reported
/// as unhandled, with no handler asked, and the end of the process.
///
/// # Safety
/// Each of `record` and `ctx` is null or points at its kind.
pub unsafe fn fail_fast(record: *const ExceptionRecord, ctx: *const Context) -> ! {
    // SAFETY: the caller's pointers.
    let ctx = unsafe { ctx.as_ref() }
        .copied()
        .unwrap_or(Context::zeroed());
    // SAFETY: as above.
    let record = unsafe { record.as_ref() }
        .copied()
        .unwrap_or(ExceptionRecord::new(
            STATUS_FAIL_FAST_EXCEPTION,
            EXCEPTION_NONCONTINUABLE,
            ctx.rip,
        ));
    fatal(&record, &ctx)
}

/// Where a fault in title code goes on (`faults.rs` resumes the thread
/// here, on its own stack, with the exception it built below it, and the
/// upper halves of its ymm registers when the host gave them, or null).
///
/// # Safety
/// `record` and `ctx` point at the exception, which nothing else uses;
/// `upper` is null or points at 16 upper halves.
pub unsafe extern "C" fn dispatch_entry(
    record: *mut ExceptionRecord,
    ctx: *mut Context,
    upper: *const [[u64; 2]; 16],
) -> ! {
    // SAFETY: the caller's exception, and halves or null.
    unsafe { raise_resuming(&mut *record, &mut *ctx, upper.as_ref()) }
}

/// Where a fail-fast in title code (`int 0x29`) goes on, as a fault goes
/// on in [`dispatch_entry`]: reported with no handler asked, as Windows
/// does, and the end of the process ([`fail_fast`]).
///
/// # Safety
/// `record` and `ctx` point at the exception.
pub unsafe extern "C" fn fail_fast_entry(record: *const ExceptionRecord, ctx: *const Context) -> ! {
    // SAFETY: the caller's exception.
    unsafe { fail_fast(record, ctx) }
}

/// `RtlUnwindEx`'s work: from `ctx` (where the walk starts, and the context
/// each frame's termination handler is given), each title frame is undone
/// up to the one whose establisher frame is `target_frame` (0: all of them,
/// an exit unwind), whose state at its control pc is then resumed at
/// `target_ip` with rax `return_value`. `record` null: one of
/// `STATUS_UNWIND` is made. Never returns: a stack that cannot be undone,
/// or that passes the target, ends the process.
///
/// # Safety
/// `ctx` points at a context of a title frame on this thread's stack;
/// `record` is null or points at an exception record.
pub unsafe fn unwind_ex(
    target_frame: u64,
    target_ip: u64,
    record: *mut ExceptionRecord,
    return_value: u64,
    ctx: *mut Context,
) -> ! {
    // SAFETY: the caller's context.
    let ctx = unsafe { &mut *ctx };
    let mut own = ExceptionRecord::new(STATUS_UNWIND, 0, ctx.rip);
    let record = if record.is_null() {
        &mut own
    } else {
        // SAFETY: the caller's record.
        unsafe { &mut *record }
    };
    record.flags |= EXCEPTION_UNWINDING;
    if target_frame == 0 {
        record.flags |= EXCEPTION_EXIT_UNWIND;
    }
    let start = ctx.rsp;
    loop {
        let mut walk = ctx.to_unwind();
        let Some(image) = images::find(walk.rip as usize) else {
            fail(STATUS_BAD_STACK, ctx);
        };
        let (at, pdata) = table_of(&image);
        let before = walk;
        let unwound = match unwind::virtual_unwind(image.base as u64, pdata, &mut walk) {
            Err(_) => fail(STATUS_BAD_STACK, ctx),
            Ok(None) if walk.rsp > before.rsp => {
                ctx.apply_unwind(&walk); // a leaf
                continue;
            }
            Ok(None) => fail(STATUS_INVALID_UNWIND_TARGET, ctx),
            Ok(Some(unwound)) => unwound,
        };
        let is_target = unwound.establisher_frame == target_frame;
        if is_target {
            record.flags |= EXCEPTION_TARGET_UNWIND;
        }
        if let Some(handler) = unwound.handler.filter(|h| h.flags & UNW_FLAG_UHANDLER != 0) {
            // SAFETY: as in `dispatch_exception`.
            let language = unsafe { std::mem::transmute::<u64, LanguageHandler>(handler.address) };
            let mut dc = DispatcherContext {
                control_pc: before.rip,
                image_base: image.base as u64,
                function_entry: entry_address(at, pdata, &unwound.function),
                establisher_frame: unwound.establisher_frame,
                target_ip,
                context_record: ctx,
                language_handler: Some(language),
                handler_data: handler.data as *mut c_void,
                history_table: ptr::null_mut(),
                scope_index: 0,
                fill0: u32::from(before.rsp != start),
            };
            // SAFETY: as in `dispatch_exception`.
            let disposition = unsafe { language(record, unwound.establisher_frame, ctx, &mut dc) };
            match disposition {
                EXCEPTION_CONTINUE_SEARCH_DISPOSITION | EXCEPTION_COLLIDED_UNWIND => {}
                _ => fail(STATUS_INVALID_DISPOSITION, ctx),
            }
        }
        record.flags &= !(EXCEPTION_TARGET_UNWIND | EXCEPTION_COLLIDED_UNWIND_FLAG);
        if is_target {
            // The target frame itself is not undone: it goes on at the
            // target with the state it has here.
            ctx.rip = target_ip;
            ctx.rax = return_value;
            // SAFETY: the target frame's own state, on this thread's stack.
            unsafe { RtlRestoreContext(ctx, record) }
        }
        if walk.rsp <= before.rsp
            || (target_frame != 0 && walk.rsp > target_frame)
            || off_stack(walk.rsp)
        {
            fail(STATUS_INVALID_UNWIND_TARGET, ctx);
        }
        ctx.apply_unwind(&walk);
    }
}

/// `RtlVirtualUnwind`: undoes the frame of the function at `control_pc` in
/// the image at `image_base` in `ctx`, writing its establisher frame and
/// its handler's data; the handler's address when its kind is one of
/// `handler_type`'s, else null. The function is the one the image's table
/// has for `control_pc` (`function_entry` is trusted to be it).
/// `context_pointers` is not filled in. A frame that cannot be undone
/// leaves `ctx` as it is.
///
/// # Safety
/// `ctx` points at a context; `handler_data` and `establisher_frame` at
/// their kinds.
pub unsafe extern "win64" fn RtlVirtualUnwind(
    handler_type: u32,
    image_base: u64,
    control_pc: u64,
    _function_entry: *const RuntimeFunctionRaw,
    ctx: *mut Context,
    handler_data: *mut *mut c_void,
    establisher_frame: *mut u64,
    _context_pointers: *mut c_void,
) -> *mut c_void {
    // SAFETY: the caller's context.
    let ctx = unsafe { &mut *ctx };
    let image = images::find(image_base as usize);
    let (_, pdata) = image.as_ref().map_or((0, &[][..]), table_of);
    let mut walk = ctx.to_unwind();
    walk.rip = control_pc;
    let rsp = walk.rsp;
    let Ok(unwound) = unwind::virtual_unwind(image_base, pdata, &mut walk) else {
        return ptr::null_mut();
    };
    ctx.apply_unwind(&walk);
    let (frame, handler) = match unwound {
        Some(u) => (u.establisher_frame, u.handler),
        None => (rsp, None),
    };
    // SAFETY: the caller's out-pointers.
    unsafe {
        if !establisher_frame.is_null() {
            *establisher_frame = frame;
        }
        if !handler_data.is_null() {
            *handler_data = handler.map_or(ptr::null_mut(), |h| h.data as *mut c_void);
        }
    }
    match handler {
        Some(h) if u32::from(h.flags) & handler_type != 0 => h.address as *mut c_void,
        _ => ptr::null_mut(),
    }
}

/// `RtlLookupFunctionEntry`: the address of the table entry of the function
/// at `control_pc`, the image's base written to `image_base`; null when the
/// address is in no image's table. `history` is not used.
///
/// # Safety
/// `image_base` points at a u64 or is null.
pub unsafe extern "win64" fn RtlLookupFunctionEntry(
    control_pc: u64,
    image_base: *mut u64,
    _history: *mut c_void,
) -> *const RuntimeFunctionRaw {
    let Some(image) = images::find(control_pc as usize) else {
        return ptr::null();
    };
    let (at, pdata) = table_of(&image);
    let rva = (control_pc - image.base as u64) as u32;
    let Some(function) = unwind::lookup(pdata, rva) else {
        return ptr::null();
    };
    if !image_base.is_null() {
        // SAFETY: the caller's out-pointer.
        unsafe { *image_base = image.base as u64 };
    }
    entry_address(at, pdata, &function)
}

/// `RtlPcToFileHeader`: the base of the image `pc` lies in (also written
/// to `base`), or null.
///
/// # Safety
/// `base` points at a pointer or is null.
pub unsafe extern "win64" fn RtlPcToFileHeader(
    pc: *mut c_void,
    base: *mut *mut c_void,
) -> *mut c_void {
    let found = images::find(pc as usize).map_or(ptr::null_mut(), |i| i.base as *mut c_void);
    if !base.is_null() {
        // SAFETY: the caller's out-pointer.
        unsafe { *base = found };
    }
    found
}

/// Warns, once, that a dynamic function table was asked for.
fn dynamic_tables_unkept() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, SeqCst) {
        tracing::warn!(
            "the title registers dynamic function tables; they are not kept, so exceptions in \
             code it generates find no handler"
        );
    }
}

/// `RtlAddFunctionTable`: accepted, not kept (dynamic tables are not
/// walked).
///
/// # Safety
/// None: nothing is read.
pub unsafe extern "win64" fn RtlAddFunctionTable(
    _table: *const RuntimeFunctionRaw,
    _count: u32,
    _base: u64,
) -> u8 {
    dynamic_tables_unkept();
    1
}

/// `RtlDeleteFunctionTable`: as [`RtlAddFunctionTable`].
///
/// # Safety
/// None: nothing is read.
pub unsafe extern "win64" fn RtlDeleteFunctionTable(_table: *const RuntimeFunctionRaw) -> u8 {
    1
}

/// `RtlInstallFunctionTableCallback`: accepted, not kept.
///
/// # Safety
/// None: nothing is read or called.
pub unsafe extern "win64" fn RtlInstallFunctionTableCallback(
    _table_id: u64,
    _base: u64,
    _length: u32,
    _callback: *const c_void,
    _context: *mut c_void,
    _out_of_process_dll: *const u16,
) -> u8 {
    dynamic_tables_unkept();
    1
}

/// A scope of `__C_specific_handler`'s table (`SCOPE_TABLE_AMD64`), RVAs:
/// `handler` is the filter (1: `EXCEPTION_EXECUTE_HANDLER`), or the
/// `__finally` block when `target` is 0; `target` is the `__except` block.
#[repr(C)]
#[derive(Clone, Copy)]
struct Scope {
    begin: u32,
    end: u32,
    handler: u32,
    target: u32,
}

/// A filter: (the exception, the establisher frame).
type Filter = unsafe extern "win64" fn(*mut ExceptionPointers, u64) -> i32;
/// A `__finally` block: (abnormal termination, the establisher frame).
type Termination = unsafe extern "win64" fn(u8, u64);

/// `__C_specific_handler`, MSVC's language handler for C's `__try`: its
/// scope table at `dc.handler_data` (a count, then the scopes). In the
/// search, the filter of each scope around the control pc is asked: to
/// go on (`EXCEPTION_CONTINUE_EXECUTION`), or to run its `__except` block,
/// unwinding to it; in the unwind, the `__finally` blocks around the
/// control pc are run, up to the scope the target lies in.
///
/// # Safety
/// The dispatcher's arguments, for a frame whose handler data is a scope
/// table.
pub unsafe extern "win64" fn __C_specific_handler(
    record: *mut ExceptionRecord,
    frame: u64,
    _ctx: *mut Context,
    dc: *mut DispatcherContext,
) -> u32 {
    // SAFETY: the dispatcher's.
    let (dc, flags) = unsafe { (&mut *dc, (*record).flags) };
    let table = dc.handler_data as *const u32;
    // SAFETY: the scope table in the image's unwind info.
    let count = unsafe { table.read_unaligned() };
    // SAFETY: as above: `count` scopes follow it.
    let scope = |i: u32| unsafe {
        table
            .add(1)
            .cast::<Scope>()
            .add(i as usize)
            .read_unaligned()
    };
    let base = dc.image_base;
    let pc = dc.control_pc;
    let holds =
        |s: &Scope, at: u64| base + u64::from(s.begin) <= at && at < base + u64::from(s.end);
    if flags & EXCEPTION_UNWIND == 0 {
        for i in dc.scope_index..count {
            let s = scope(i);
            if !holds(&s, pc) || s.target == 0 {
                continue; // not around the pc, or a `__finally`
            }
            let result = if s.handler == 1 {
                EXCEPTION_EXECUTE_HANDLER
            } else {
                let mut pointers = ExceptionPointers {
                    record,
                    context: dc.context_record,
                };
                // SAFETY: the scope's filter, in the image.
                let filter =
                    unsafe { std::mem::transmute::<u64, Filter>(base + u64::from(s.handler)) };
                // SAFETY: the filter, given what MSVC's gives it.
                unsafe { filter(&mut pointers, frame) }
            };
            if result < 0 {
                return EXCEPTION_CONTINUE_EXECUTION_DISPOSITION;
            }
            if result > 0 {
                dc.scope_index = i + 1;
                // SAFETY: the frame's own context; the record the dispatch
                // was given.
                unsafe {
                    unwind_ex(
                        frame,
                        base + u64::from(s.target),
                        record,
                        u64::from((*record).code),
                        dc.context_record,
                    )
                }
            }
        }
    } else {
        for i in dc.scope_index..count {
            let s = scope(i);
            if !holds(&s, pc) {
                continue;
            }
            // The target lies in this scope: nothing more to run. Only in
            // the target frame: another frame of the same function (a
            // recursion) has scopes holding that address too.
            if flags & EXCEPTION_TARGET_UNWIND != 0 && holds(&s, dc.target_ip) {
                break;
            }
            if s.target != 0 {
                continue; // an `__except`
            }
            dc.scope_index = i + 1;
            // SAFETY: the scope's `__finally` block, in the image.
            let finally =
                unsafe { std::mem::transmute::<u64, Termination>(base + u64::from(s.handler)) };
            // SAFETY: the block, given what MSVC's gives it.
            unsafe { finally(1, frame) };
        }
    }
    EXCEPTION_CONTINUE_SEARCH_DISPOSITION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structures_have_windows_sizes() {
        assert_eq!(size_of::<ExceptionRecord>(), 152);
        assert_eq!(size_of::<Context>(), 0x4D0);
        assert_eq!(align_of::<Context>(), 16);
        assert_eq!(size_of::<DispatcherContext>(), 0x50);
        assert_eq!(size_of::<RuntimeFunctionRaw>(), 12);
    }

    #[test]
    fn registers_go_through_a_context_and_back() {
        let mut r = Registers {
            rax: 1,
            rcx: 2,
            r15: 3,
            rip: 4,
            rsp: 5,
            rflags: 0x246,
            ..Registers::default()
        };
        r.xmm[15] = [6, 7];
        let c = Context::from_registers(&r);
        assert_eq!(c.flt_save[0x1A0 - 0x100 + 15 * 16], 6);
        assert_eq!(c.to_registers(), r);
        let mut d = Context::zeroed();
        d.apply_unwind(&c.to_unwind());
        assert_eq!(d.to_registers(), r);
    }

    static mut CAPTURED: Context = Context::zeroed();
    static mut PASSES: u64 = 0;
    static mut RAX_AFTER: u64 = 0;

    /// Captures a context, keeping rax right after; on the first pass,
    /// restores it with rax 0x42, which comes back to the same place.
    #[unsafe(naked)]
    extern "win64" fn round_trip() {
        naked_asm!(
            "push rbx",
            "sub rsp, 32",
            "lea rcx, [rip + {ctx}]",
            "call {capture}",
            "mov [rip + {rax}], rax",
            "inc qword ptr [rip + {passes}]",
            "cmp qword ptr [rip + {passes}], 1",
            "jne 2f",
            "lea rcx, [rip + {ctx}]",
            "mov qword ptr [rcx + 0x78], 0x42",
            "xor edx, edx",
            "call {restore}",
            "2:",
            "add rsp, 32",
            "pop rbx",
            "ret",
            ctx = sym CAPTURED,
            rax = sym RAX_AFTER,
            passes = sym PASSES,
            capture = sym RtlCaptureContext,
            restore = sym RtlRestoreContext,
        )
    }

    static mut UPPER_CAPTURED: Context = Context::zeroed();
    static mut UPPER_PASSES: u64 = 0;
    static mut UPPER: [[u64; 2]; 16] = [[0; 2]; 16];
    static mut YMM15_HIGH: [u64; 2] = [0; 2];

    /// Captures a context; on the first pass, clears the ymm upper halves
    /// and restores it with those of [`UPPER`]; then keeps ymm15's.
    #[unsafe(naked)]
    extern "win64" fn upper_round_trip() {
        naked_asm!(
            "push rbx",
            "sub rsp, 32",
            "lea rcx, [rip + {ctx}]",
            "call {capture}",
            "inc qword ptr [rip + {passes}]",
            "cmp qword ptr [rip + {passes}], 1",
            "jne 2f",
            "vzeroupper",
            "lea rcx, [rip + {ctx}]",
            "lea rdx, [rip + {upper}]",
            "call {restore}",
            "2:",
            "vextractf128 xmmword ptr [rip + {high}], ymm15, 1",
            "add rsp, 32",
            "pop rbx",
            "ret",
            ctx = sym UPPER_CAPTURED,
            passes = sym UPPER_PASSES,
            upper = sym UPPER,
            high = sym YMM15_HIGH,
            capture = sym RtlCaptureContext,
            restore = sym restore_with_upper,
        )
    }

    #[test]
    fn a_restore_puts_the_ymm_upper_halves_back() {
        if !std::arch::is_x86_feature_detected!("avx") {
            return;
        }
        // SAFETY: ours, read by `upper_round_trip` on this thread only.
        unsafe { (&raw mut UPPER).cast::<[u64; 2]>().add(15).write([1, 2]) };
        upper_round_trip();
        // SAFETY: written by `upper_round_trip` on this thread, which is done.
        let passes = unsafe { (&raw const UPPER_PASSES).read() };
        // SAFETY: as above.
        let high = unsafe { (&raw const YMM15_HIGH).read() };
        assert_eq!(passes, 2);
        assert_eq!(high, [1, 2]);
    }

    #[test]
    fn capture_and_restore_round_trip() {
        round_trip();
        // SAFETY: written by `round_trip` on this thread, which is done.
        let (passes, rax) = unsafe { ((&raw const PASSES).read(), (&raw const RAX_AFTER).read()) };
        assert_eq!(passes, 2);
        assert_eq!(rax, 0x42);
        // SAFETY: as above.
        let ctx = unsafe { (&raw const CAPTURED).read() };
        assert_eq!(ctx.context_flags, CONTEXT_FULL | CONTEXT_SEGMENTS);
        assert_eq!(ctx.seg_cs & 3, 3, "user mode");
    }
}
