//! Faults in title code, where the host has no Windows exception dispatch:
//! our handlers in order, then the Zen 2 instructions the title uses and
//! this processor lacks, then a crash report and the end of the process;
//! faults outside title code pass to the host (Rust's own handlers).
//!
//! Everything here runs in a signal handler (`victory_platform::faults`):
//! nothing allocates or waits.

use core::fmt::Write;
use std::ffi::c_int;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use victory_platform::faults::{
    Fault, FaultKind, Faults, GENERAL_PROTECTION, Handled, Handler, PAGE_FAULT, Registers,
};
use victory_platform::{Host, Memory, Protection};

use crate::seh;

/// How many handlers [`register`] takes.
const HANDLERS_MAX: usize = 8;

/// The registered handlers as addresses, in order; 0 is none.
static HANDLERS: [AtomicUsize; HANDLERS_MAX] = [const { AtomicUsize::new(0) }; HANDLERS_MAX];
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Installs the process's fault handling (once); false if the host refused.
pub fn install() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| Host::install(dispatch))
}

/// Adds a handler tried before the built-in ones, in order of registration;
/// at most 8 (false when full). Call before title code runs.
pub fn register(handler: Handler) -> bool {
    let at = COUNT.fetch_add(1, SeqCst);
    if at >= HANDLERS_MAX {
        COUNT.fetch_sub(1, SeqCst);
        return false;
    }
    HANDLERS[at].store(handler as usize, SeqCst);
    true
}

/// How many Zen 2 instructions were emulated since the start.
pub fn emulated() -> u64 {
    zen2::EMULATED.load(SeqCst)
}

fn dispatch(fault: &mut Fault) -> Handled {
    let count = COUNT.load(SeqCst).min(HANDLERS_MAX);
    for slot in &HANDLERS[..count] {
        let handler = slot.load(SeqCst);
        if handler == 0 {
            continue; // being registered
        }
        // SAFETY: the address of a `Handler` that `register` stored.
        let handler = unsafe { std::mem::transmute::<usize, Handler>(handler) };
        if handler(fault) == Handled::Resume {
            return Handled::Resume;
        }
    }
    if fault.kind == FaultKind::IllegalInstruction && zen2::emulate(fault.registers) {
        return Handled::Resume;
    }
    let rip = fault.registers.rip as usize;
    if !crate::images::is_title(rip) {
        return Handled::Pass; // our code or the host's
    }
    let why = match to_exception(fault) {
        Ok(()) => return Handled::Resume,
        Err(why) => why,
    };
    let mut buffer = [0u8; 2048];
    let mut out = Buf {
        bytes: &mut buffer,
        len: 0,
    };
    report(fault, &mut out);
    let _ = writeln!(out, "  {why}");
    let len = out.len;
    // SAFETY: our buffer's written bytes to stderr, then the end of the
    // process: both async-signal-safe.
    unsafe {
        write(2, buffer.as_ptr(), len);
        _exit(128 + signal_number(fault.kind))
    }
}

/// Bytes below the faulting rsp for the exception: its record, its context
/// at [`CONTEXT_AT`], then the ymm upper halves at [`UPPER_AT`].
const EXCEPTION_ROOM: u64 = 0x680;
/// Where the context lies in [`EXCEPTION_ROOM`]: past the record, 16-aligned.
const CONTEXT_AT: u64 = 0xA0;
/// Where the upper halves of ymm0..15 lie in [`EXCEPTION_ROOM`], past the
/// context: what the thread resumes with besides it (`seh::dispatch_entry`).
const UPPER_AT: u64 = CONTEXT_AT + size_of::<seh::Context>() as u64;
/// The stack the dispatch needs: the exception and our frames.
const STACK_ROOM: u64 = EXCEPTION_ROOM + (64 << 10);
const _: () = assert!(
    size_of::<seh::ExceptionRecord>() as u64 <= CONTEXT_AT
        && UPPER_AT.is_multiple_of(16)
        && UPPER_AT + size_of::<[[u64; 2]; 16]>() as u64 <= EXCEPTION_ROOM
);

/// Why a fault in title code is not dispatched: a thread of no TEB of ours
/// has no stack bounds to check, and no alternate signal stack, so that an
/// exception written below its rsp would lie over the kernel's signal frame.
const NO_TEB: &str =
    "a thread with no TEB (not created through CreateThread, not attached): no dispatch";
/// Why a fault in title code is not dispatched: no room on its stack.
const NO_STACK: &str = "no stack left to dispatch it on: a stack overflow";

/// Turns a fault in title code into a Windows exception: its record and
/// context written on the faulting thread's own stack, below its rsp, and
/// the thread sent to `seh::dispatch_entry` there (`seh::fail_fast_entry`
/// for a `__fastfail`), which it runs once the signal handler returns (with
/// the signal mask restored). Why not, changing nothing, when the thread
/// has no TEB of ours ([`NO_TEB`]) or its stack no room left ([`NO_STACK`]).
fn to_exception(fault: &mut Fault) -> Result<(), &'static str> {
    let Some(teb) = crate::teb::current_teb() else {
        return Err(NO_TEB);
    };
    // SAFETY: this thread's TEB, ours, alive for the thread's life; a read
    // of it, which nothing in a signal handler can disturb.
    let limit = unsafe { (*(teb as *const crate::teb::Teb)).stack_limit };
    let regs = &mut *fault.registers;
    if regs.rsp < limit.saturating_add(STACK_ROOM) {
        return Err(NO_STACK);
    }
    let mut code = [0u8; 16];
    let code = read_code(regs.rip as usize, &mut code);
    let raised = raised(fault.kind, fault.trap, fault.error, fault.address, code);
    if fault.kind == FaultKind::Breakpoint && raised != Raised::FailFast {
        // Windows reports the `int3` itself, not the byte after it.
        let mut byte = [0u8];
        let before = regs.rip.wrapping_sub(1) as usize;
        if Host::read_into(before, &mut byte) && byte[0] == 0xCC {
            regs.rip -= 1;
        }
    }
    let (record, entry) = match raised {
        Raised::FailFast => {
            let mut record = seh::ExceptionRecord::new(
                seh::STATUS_STACK_BUFFER_OVERRUN,
                seh::EXCEPTION_NONCONTINUABLE,
                regs.rip,
            );
            record.number_parameters = 1;
            record.information[0] = regs.rcx;
            (record, seh::fail_fast_entry as *const () as u64)
        }
        Raised::Exception(code, parameters) => {
            let mut record = seh::ExceptionRecord::new(code, 0, regs.rip);
            if let Some(parameters) = parameters {
                record.number_parameters = 2;
                record.information[..2].copy_from_slice(&parameters);
            }
            (record, seh::dispatch_entry as *const () as u64)
        }
    };
    let base = (regs.rsp - EXCEPTION_ROOM) & !15;
    // SAFETY: the faulting thread's own stack below its rsp, which Win64
    // code keeps nothing in, and which has room (checked above).
    unsafe {
        std::ptr::write(base as *mut seh::ExceptionRecord, record);
        std::ptr::write(
            (base + CONTEXT_AT) as *mut seh::Context,
            seh::Context::from_registers(regs),
        );
        if let Some(upper) = regs.ymm_upper {
            std::ptr::write((base + UPPER_AT) as *mut [[u64; 2]; 16], upper);
        }
    }
    regs.rdx = if regs.ymm_upper.is_some() {
        base + UPPER_AT
    } else {
        0
    };
    regs.rip = entry;
    regs.rdi = base;
    regs.rsi = base + CONTEXT_AT;
    // As after a call: rsp + 8 is 16-aligned at a SysV function's entry.
    regs.rsp = base - 8;
    // Our code runs with the direction flag clear, as the SysV ABI has it,
    // and without single steps; the context keeps the title's flags.
    regs.rflags &= !(RFLAGS_DF | RFLAGS_TF);
    Ok(())
}

/// The code at `rip`: 16 bytes, else those left in its page (the next one
/// may not be mapped), else none.
fn read_code(rip: usize, code: &mut [u8; 16]) -> &[u8] {
    let page = Host::page_size();
    let room = (page - rip % page).min(code.len());
    if Host::read_into(rip, code) {
        code
    } else if Host::read_into(rip, &mut code[..room]) {
        &code[..room]
    } else {
        &[]
    }
}

/// What a fault in title code is raised as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Raised {
    /// `int 0x29` (`__fastfail`): `STATUS_STACK_BUFFER_OVERRUN` with the
    /// code in rcx, which ends the process with no handler asked.
    FailFast,
    /// An exception: its code, and its two parameters if it has them.
    Exception(u32, Option<[u64; 2]>),
}

/// What a fault is raised as, from its kind, its trap number and error
/// code, the address it reached for, and the code at its rip (what could
/// be read of it). `CD 29` is a fail-fast whatever the signal; a page fault
/// an access violation that reads (0), writes (1) or executes (8); a #GP a
/// privileged instruction, else an access violation of no address (a
/// non-canonical one); any other trap goes by the signal.
fn raised(kind: FaultKind, trap: Option<u16>, error: u64, address: usize, code: &[u8]) -> Raised {
    if code.starts_with(&[0xCD, 0x29]) {
        return Raised::FailFast;
    }
    let violation = |what: u64, address: u64| {
        Raised::Exception(seh::STATUS_ACCESS_VIOLATION, Some([what, address]))
    };
    match trap {
        Some(PAGE_FAULT) => {
            let what = match error {
                e if e & 0x10 != 0 => 8, // an instruction fetch
                e if e & 2 != 0 => 1,
                _ => 0,
            };
            return violation(what, address as u64);
        }
        Some(GENERAL_PROTECTION) if privileged(code) => {
            return Raised::Exception(seh::STATUS_PRIVILEGED_INSTRUCTION, None);
        }
        Some(GENERAL_PROTECTION) => return violation(0, 0),
        _ => {}
    }
    match kind {
        FaultKind::Access => violation(0, address as u64),
        FaultKind::IllegalInstruction => Raised::Exception(seh::STATUS_ILLEGAL_INSTRUCTION, None),
        FaultKind::Breakpoint => Raised::Exception(seh::STATUS_BREAKPOINT, None),
        FaultKind::Arithmetic => Raised::Exception(seh::STATUS_INTEGER_DIVIDE_BY_ZERO, None),
    }
}

/// Whether `code` starts with an instruction user mode may not run (a
/// #GP): `hlt`, `cli`, `sti`, `in`, `out`, `ins`, `outs`, `rdmsr`,
/// `wrmsr`, a move to or from a control or debug register, `clts`, `invd`,
/// `wbinvd`; after any legacy prefixes and a REX.
fn privileged(code: &[u8]) -> bool {
    let prefixes = code
        .iter()
        .take_while(|&&b| {
            matches!(
                b,
                0x26 | 0x2E | 0x36 | 0x3E | 0x64..=0x67 | 0xF0 | 0xF2 | 0xF3
            )
        })
        .count();
    let mut code = &code[prefixes..];
    if let [0x40..=0x4F, rest @ ..] = code {
        code = rest;
    }
    matches!(
        code,
        [0xF4 | 0xFA | 0xFB | 0x6C..=0x6F | 0xE4..=0xE7 | 0xEC..=0xEF, ..]
            | [0x0F, 0x06 | 0x08 | 0x09 | 0x20..=0x23 | 0x30 | 0x32, ..]
    )
}

/// The protection the loader gave the code page at `page` (`map.rs`, from
/// the sections in it), as its image's section table has it (mapped
/// read-only at the image's base): read-write-execute where a section in
/// the page is writable, else read-execute (also for a page in no image of
/// ours, or headers that cannot be read). Nothing allocates or locks.
fn code_protection(page: usize, page_size: usize) -> Protection {
    let writable = crate::images::title_base(page)
        .and_then(|base| page_writable(base, page - base, page_size))
        .unwrap_or(false);
    if writable {
        Protection::ReadWriteExecute
    } else {
        Protection::ReadExecute
    }
}

/// Whether a section with write lies in the `page_size` bytes at `rva` of
/// the image at `base`, from its headers in memory; `None` when they cannot
/// be read.
fn page_writable(base: usize, rva: usize, page_size: usize) -> Option<bool> {
    let read = |at: usize, bytes: &mut [u8]| Host::read_into(at, bytes).then_some(());
    let mut word = [0u8; 4];
    read(base + 0x3C, &mut word)?; // e_lfanew
    let nt = base + u32::from_le_bytes(word) as usize;
    let mut file_header = [0u8; 24]; // the signature, then IMAGE_FILE_HEADER
    read(nt, &mut file_header)?;
    let sections = usize::from(u16::from_le_bytes([file_header[6], file_header[7]]));
    let optional = usize::from(u16::from_le_bytes([file_header[20], file_header[21]]));
    let mut writable = false;
    for i in 0..sections.min(96) {
        let mut header = [0u8; 40];
        read(nt + 24 + optional + 40 * i, &mut header)?;
        let field = |at: usize| {
            u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        // VirtualSize (0: the raw size, as `map.rs` takes it), VirtualAddress.
        let size = match field(8) {
            0 => field(16),
            n => n,
        } as usize;
        let start = field(12) as usize;
        if start < rva + page_size && rva < start + size {
            writable |= field(36) & crate::map::SCN_MEM_WRITE != 0;
        }
    }
    Some(writable)
}

/// The flags' trap (single step) and direction bits.
const RFLAGS_TF: u64 = 1 << 8;
const RFLAGS_DF: u64 = 1 << 10;

// libc's, which this crate does not depend on (`<unistd.h>`).
unsafe extern "C" {
    fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
    fn _exit(status: c_int) -> !;
}

/// The signal a kind of fault is reported as, for the exit status.
pub(crate) fn signal_number(kind: FaultKind) -> c_int {
    match kind {
        FaultKind::Access => 11,
        FaultKind::IllegalInstruction => 4,
        FaultKind::Breakpoint => 5,
        FaultKind::Arithmetic => 8,
    }
}

/// A buffer that text is written into, cut off where it is full.
struct Buf<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// `name+rva` for an address in a mapped image, else the address; written
/// without allocating.
fn locate(address: u64, out: &mut impl Write) {
    let named = crate::images::with_image(address as usize, |image| match image {
        Some(image) => {
            let _ = write!(out, "{}+{:#x}", image.name, address as usize - image.base);
            true
        }
        None => false,
    });
    if named != Some(true) {
        let _ = write!(out, "{address:#x}");
    }
}

/// The crash report of a fault in title code: the fault, the registers, and
/// the eight qwords at `rsp`, each in an image named. Nothing allocates.
pub(crate) fn report(fault: &Fault, out: &mut impl Write) {
    let kind = match fault.kind {
        FaultKind::Access => "access",
        FaultKind::IllegalInstruction => "illegal-instruction",
        FaultKind::Breakpoint => "breakpoint",
        FaultKind::Arithmetic => "arithmetic",
    };
    let _ = writeln!(
        out,
        "victory: {kind} fault at {:#x} in title code",
        fault.address
    );
    report_state(fault.registers, out);
}

/// The crash report of an exception in title code that no handler took:
/// its code, then what [`report`] writes after its first line.
pub(crate) fn report_exception(
    code: u32,
    record: &seh::ExceptionRecord,
    fault: &Fault,
    out: &mut impl Write,
) {
    let _ = writeln!(
        out,
        "victory: exception {code:#010x} ({}) at {:#x} in title code",
        seh::status_name(code),
        record.address as usize
    );
    report_state(fault.registers, out);
}

/// The registers, and the eight qwords at `rsp`, each in an image named:
/// the crash report after its first line.
fn report_state(r: &Registers, out: &mut impl Write) {
    let _ = write!(out, "  rip ");
    locate(r.rip, out);
    let _ = writeln!(out, "  rsp {:#x}  rflags {:#x}", r.rsp, r.rflags);
    let _ = writeln!(
        out,
        "  rax {:#x}  rbx {:#x}  rcx {:#x}  rdx {:#x}",
        r.rax, r.rbx, r.rcx, r.rdx
    );
    let _ = writeln!(
        out,
        "  rsi {:#x}  rdi {:#x}  rbp {:#x}",
        r.rsi, r.rdi, r.rbp
    );
    let _ = writeln!(
        out,
        "  r8 {:#x}  r9 {:#x}  r10 {:#x}  r11 {:#x}",
        r.r8, r.r9, r.r10, r.r11
    );
    let _ = writeln!(
        out,
        "  r12 {:#x}  r13 {:#x}  r14 {:#x}  r15 {:#x}",
        r.r12, r.r13, r.r14, r.r15
    );
    let _ = write!(out, "  stack:");
    for i in 0..8 {
        let mut value = [0u8; 8];
        if !Host::read_into(r.rsp as usize + 8 * i, &mut value) {
            break;
        }
        let value = u64::from_le_bytes(value);
        let _ = write!(out, " {value:#x}");
        if crate::images::is_title(value as usize) {
            let _ = write!(out, " (");
            locate(value, out);
            let _ = write!(out, ")");
        }
    }
    let _ = writeln!(out);
}

// The decoder `console_insn.rs` includes on Windows. Here and not in `zen2`:
// an inline module's `path` would start from `faults/zen2/`, which is no
// directory.
#[path = "console_insn_decode.rs"]
mod decode;

/// The Zen 2 instructions titles use and this processor may lack, done in
/// the handler: what `crash.rs` and `console_insn.rs` do on Windows.
mod zen2 {
    use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use super::decode::{self, ConsoleOnly};
    use victory_platform::faults::Registers;
    use victory_platform::{Host, Memory, Protection};

    /// Instructions emulated since the start.
    pub(super) static EMULATED: AtomicU64 = AtomicU64::new(0);

    /// Does the instruction at `regs.rip` if it is one of ours, and steps
    /// over it; false for any other.
    pub(super) fn emulate(regs: &mut Registers) -> bool {
        let rip = regs.rip as usize;
        let mut code = [0u8; 16];
        let code = super::read_code(rip, &mut code);
        if code.is_empty() {
            return false;
        }
        let len = match decode::decode(code) {
            Some((ConsoleOnly::Clzero, len)) => {
                clzero(regs, &code[..len]);
                len
            }
            // The loop around it goes on as the processor would run it: a
            // fault each pass.
            Some((ConsoleOnly::ClzeroLoop { .. }, len)) => {
                clzero(regs, &code[..len - 9]);
                len - 9
            }
            Some((ConsoleOnly::Monitorx, len)) => {
                patch(rip + len - 3, [0x0f, 0x1f, 0x00]);
                len
            }
            Some((ConsoleOnly::Mwaitx, len)) => {
                patch(rip + len - 3, [0xf3, 0x90, 0x90]);
                len
            }
            Some((ConsoleOnly::Clwb, len)) => len,
            Some((ConsoleOnly::Extrq { dst, src, len, idx }, length)) => {
                let dst = usize::from(dst);
                // The register form reads its fields from src's low half.
                let (len, idx) = match src {
                    Some(src) => fields(regs.xmm[usize::from(src)][0]),
                    None => (len, idx),
                };
                regs.xmm[dst][0] = decode::extrq(regs.xmm[dst][0], len, idx);
                length
            }
            Some((
                ConsoleOnly::Insertq {
                    dst,
                    src,
                    reg,
                    len,
                    idx,
                },
                length,
            )) => {
                let (dst, src) = (usize::from(dst), usize::from(src));
                // The register form reads its fields from src's high half.
                let (len, idx) = if reg {
                    fields(regs.xmm[src][1])
                } else {
                    (len, idx)
                };
                regs.xmm[dst][0] = decode::insertq(regs.xmm[dst][0], regs.xmm[src][0], len, idx);
                length
            }
            _ => match movnt_scalar(code, regs) {
                Some(len) => len,
                None => return false,
            },
        };
        regs.rip += len as u64;
        EMULATED.fetch_add(1, SeqCst);
        true
    }

    /// An SSE4a register form's length and index: the low two bytes of a
    /// qword.
    fn fields(f: u64) -> (u8, u8) {
        (f as u8, (f >> 8) as u8)
    }

    /// Writes `bytes` at `at` if they are there to read: the title writes
    /// memory it owns; a read-only page would fault inside the handler and
    /// end the process, as on Windows.
    fn store(at: usize, bytes: &[u8]) -> bool {
        let mut was = [0u8; 64];
        let was = &mut was[..bytes.len()];
        if !Host::read_into(at, was) {
            return false;
        }
        // SAFETY: mapped memory of the title's, which it asked us to write.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
        true
    }

    /// `clzero` (`code` is the instruction): zeroes the 64-byte line at
    /// `rax` (`eax` after an address-size prefix).
    fn clzero(regs: &Registers, code: &[u8]) {
        let rax = if code[..code.len() - 3].contains(&0x67) {
            regs.rax & 0xffff_ffff
        } else {
            regs.rax
        };
        store((rax & !63) as usize, &[0; 64]);
    }

    /// Replaces the 3 bytes of a wait instruction at `at` by `with`, so that
    /// it never faults again: whether it did. The bytes change in one store,
    /// so that a thread running the same instruction sees the old three or
    /// the new three, never a mix: an aligned qword's where they lie in one,
    /// else an aligned 16 bytes' (`cmpxchg16b`, where the processor has it).
    /// Bytes that cross 16 aligned ones are left, and emulated at each fault
    /// as before. The page is made writable for the store and given back
    /// the protection the loader gave it after ([`super::code_protection`]);
    /// left as it is if the host refuses. One patch at a time in the
    /// process ([`PATCHING`]): false, patching nothing, while another
    /// thread's is under way (the instruction is stepped over all the same,
    /// and patched at a later fault).
    fn patch(at: usize, with: [u8; 3]) -> bool {
        let wide = if at % 8 <= 5 {
            false
        } else if at % 16 <= 13 {
            true
        } else {
            return false;
        };
        if wide && !std::arch::is_x86_feature_detected!("cmpxchg16b") {
            return false;
        }
        // Held until the page is read-execute again (dropped last).
        let Some(_patching) = take(&PATCHING) else {
            return false;
        };
        let page = Host::page_size();
        let start = at & !(page - 1);
        let after = super::code_protection(start, page);
        // SAFETY: the title's code page (the bytes lie in 16 aligned ones,
        // so in one page), from `Host::allocate` as the loader maps it;
        // nothing holds a reference into it. The store is to its bytes, made
        // writable, and the processor has `cmpxchg16b` where it is used
        // (asked above).
        unsafe {
            if !Host::protect(start, page, Protection::ReadWriteExecute) {
                return false;
            }
            let patched = if wide {
                store_16(at, with)
            } else {
                store_8(at, with);
                true
            };
            Host::protect(start, page, after);
            patched
        }
    }

    /// Held by the thread patching a page of code ([`patch`]): without it,
    /// two threads patching the same page race, and one stores into the
    /// page the other has just made read-execute again, which faults inside
    /// the signal handler and ends the process (several job workers that
    /// reach the same idle loop together, under Rosetta 2).
    pub(super) static PATCHING: AtomicBool = AtomicBool::new(false);

    /// `flag` taken until the guard drops; `None` while another holds it.
    pub(super) fn take(flag: &AtomicBool) -> Option<Taken<'_>> {
        flag.compare_exchange(false, true, Acquire, Relaxed)
            .is_ok()
            .then_some(Taken(flag))
    }

    /// A flag [`take`] took, given back when dropped.
    pub(super) struct Taken<'a>(&'a AtomicBool);

    impl Drop for Taken<'_> {
        fn drop(&mut self) {
            self.0.store(false, Release);
        }
    }

    /// `with` stored at `at` in one store of the aligned qword it lies in.
    ///
    /// # Safety
    /// The qword is writable memory, and `at % 8 <= 5`.
    unsafe fn store_8(at: usize, with: [u8; 3]) {
        let aligned = at & !7;
        let offset = at % 8;
        // SAFETY: the caller's aligned, writable qword; an atomic over it,
        // as other threads may run the code in it.
        unsafe {
            let mut bytes = std::ptr::read_volatile(aligned as *const u64).to_le_bytes();
            bytes[offset..offset + 3].copy_from_slice(&with);
            let word = &*(aligned as *const AtomicU64);
            word.store(u64::from_le_bytes(bytes), Relaxed);
        }
    }

    /// `with` stored at `at` in one `cmpxchg16b` of the aligned 16 bytes it
    /// lies in; tried again while another thread changes them meanwhile, at
    /// most 8 times: whether it was stored.
    ///
    /// # Safety
    /// The 16 bytes are writable memory, `at % 16 <= 13`, and the processor
    /// has `cmpxchg16b`.
    #[target_feature(enable = "cmpxchg16b")]
    unsafe fn store_16(at: usize, with: [u8; 3]) -> bool {
        let aligned = (at & !15) as *mut u128;
        let offset = at % 16;
        for _ in 0..8 {
            // SAFETY: the caller's aligned, writable 16 bytes.
            let old = unsafe {
                let halves = aligned as *const u64;
                u128::from(std::ptr::read_volatile(halves))
                    | u128::from(std::ptr::read_volatile(halves.add(1))) << 64
            };
            let mut bytes = old.to_le_bytes();
            bytes[offset..offset + 3].copy_from_slice(&with);
            let new = u128::from_le_bytes(bytes);
            // SAFETY: as above; the caller's processor has the instruction.
            if unsafe { cmpxchg16b(aligned, old, new) } == old {
                return true;
            }
        }
        false
    }

    /// `lock cmpxchg16b`: `new` at `dst` if it holds `old`; what it held.
    /// In assembly and not `core::arch::x86_64::cmpxchg16b`: that one, in a
    /// debug build, calls `__atomic_compare_exchange_16`, which nothing
    /// here links. `rbx`, which LLVM keeps for itself, is swapped out
    /// around the instruction and back.
    ///
    /// # Safety
    /// `dst` is 16 aligned, writable bytes, and the processor has
    /// `cmpxchg16b`.
    #[target_feature(enable = "cmpxchg16b")]
    unsafe fn cmpxchg16b(dst: *mut u128, old: u128, new: u128) -> u128 {
        let (low, high): (u64, u64);
        // SAFETY: the caller's 16 bytes and processor.
        unsafe {
            core::arch::asm!(
                "xchg {b}, rbx",
                "lock cmpxchg16b xmmword ptr [{dst}]",
                "mov rbx, {b}",
                dst = in(reg) dst,
                b = inout(reg) new as u64 => _,
                in("rcx") (new >> 64) as u64,
                inout("rax") old as u64 => low,
                inout("rdx") (old >> 64) as u64 => high,
                options(nostack),
            );
        }
        u128::from(low) | u128::from(high) << 64
    }

    /// The general register numbered `n` as instructions encode it.
    fn gpr(regs: &Registers, n: u8) -> u64 {
        match n & 15 {
            0 => regs.rax,
            1 => regs.rcx,
            2 => regs.rdx,
            3 => regs.rbx,
            4 => regs.rsp,
            5 => regs.rbp,
            6 => regs.rsi,
            7 => regs.rdi,
            8 => regs.r8,
            9 => regs.r9,
            10 => regs.r10,
            11 => regs.r11,
            12 => regs.r12,
            13 => regs.r13,
            14 => regs.r14,
            _ => regs.r15,
        }
    }

    /// SSE4a's `movntss m32, xmm` (`F3 [rex] 0F 2B /r`) and `movntsd m64,
    /// xmm` (`F2 ...`), memory operands only: the low dword or qword of the
    /// register stored. Its length, or `None` for any other instruction (or
    /// memory that is not there).
    fn movnt_scalar(code: &[u8], regs: &Registers) -> Option<usize> {
        let wide = match *code.first()? {
            0xf3 => false,
            0xf2 => true,
            _ => return None,
        };
        let (rex, at) = match *code.get(1)? {
            b @ 0x40..=0x4f => (b, 2),
            _ => (0, 1),
        };
        if *code.get(at..at + 2)? != [0x0f, 0x2b] {
            return None;
        }
        let modrm = *code.get(at + 2)?;
        let (mode, rm) = (modrm >> 6, modrm & 7);
        if mode == 3 {
            return None;
        }
        let reg = usize::from((modrm >> 3) & 7 | (rex & 4) << 1);
        let mut len = at + 3;
        let mut rip_relative = false;
        let mut address = 0u64;
        let mut disp32 = mode == 2;
        if rm == 4 {
            let sib = *code.get(len)?;
            len += 1;
            let index = (sib >> 3) & 7 | (rex & 2) << 2;
            if index != 4 {
                address = gpr(regs, index) << (sib >> 6);
            }
            if sib & 7 == 5 && mode == 0 {
                disp32 = true;
            } else {
                address = address.wrapping_add(gpr(regs, sib & 7 | (rex & 1) << 3));
            }
        } else if rm == 5 && mode == 0 {
            rip_relative = true;
            disp32 = true;
        } else {
            address = gpr(regs, rm | (rex & 1) << 3);
        }
        let disp = if disp32 {
            let d = i32::from_le_bytes(code.get(len..len + 4)?.try_into().ok()?);
            len += 4;
            i64::from(d)
        } else if mode == 1 {
            let d = *code.get(len)? as i8;
            len += 1;
            i64::from(d)
        } else {
            0
        };
        if rip_relative {
            address = regs.rip.wrapping_add(len as u64);
        }
        let address = address.wrapping_add(disp as u64) as usize;
        let value = regs.xmm[reg][0].to_le_bytes();
        store(address, if wide { &value } else { &value[..4] }).then_some(len)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn movntss_and_movntsd_store_the_low_part() {
            let mut slot = [0xaau8; 16];
            let mut regs = Registers {
                rcx: slot.as_mut_ptr() as u64,
                ..Registers::default()
            };
            regs.xmm[1] = [0x1122_3344_5566_7788, 0x99];
            // movntss [rcx+4], xmm1
            assert_eq!(
                movnt_scalar(&[0xf3, 0x0f, 0x2b, 0x49, 0x04], &regs),
                Some(5)
            );
            // movntsd [rcx+8], xmm9 (REX.R)
            regs.xmm[9] = [0x0102_0304_0506_0708, 0];
            assert_eq!(
                movnt_scalar(&[0xf2, 0x44, 0x0f, 0x2b, 0x49, 0x08], &regs),
                Some(6)
            );
            assert_eq!(slot[..4], [0xaa; 4]);
            assert_eq!(slot[4..8], 0x5566_7788u32.to_le_bytes());
            assert_eq!(slot[8..], 0x0102_0304_0506_0708u64.to_le_bytes());
            assert_eq!(
                movnt_scalar(&[0xf3, 0x0f, 0x2b, 0xc1], &regs),
                None,
                "a register"
            );
            assert_eq!(movnt_scalar(&[0x0f, 0x2b, 0x01], &regs), None, "movntps");
        }

        #[test]
        fn sse4a_is_done_on_the_saved_registers() {
            let mut regs = Registers::default();
            // Bytes read from our own buffer as the faulting instruction.
            let run = |regs: &mut Registers, bytes: &[u8]| {
                let mut buf = [0x90u8; 16];
                buf[..bytes.len()].copy_from_slice(bytes);
                regs.rip = buf.as_ptr() as u64;
                assert!(emulate(regs), "{bytes:02x?}");
                (regs.rip - buf.as_ptr() as u64) as usize
            };
            // extrq xmm2, 4, 8
            regs.xmm[2] = [0x1234_5678_9abc_def0, 0x77];
            assert_eq!(run(&mut regs, &[0x66, 0x0f, 0x78, 0xc2, 0x04, 0x08]), 6);
            assert_eq!(regs.xmm[2], [0xe, 0x77], "the high half is left");
            // insertq xmm0, xmm1, 8, 16
            regs.xmm[0] = [u64::MAX, 5];
            regs.xmm[1] = [0xab, 0];
            assert_eq!(run(&mut regs, &[0xf2, 0x0f, 0x78, 0xc1, 0x08, 0x10]), 6);
            assert_eq!(regs.xmm[0], [0xffff_ffff_ffab_ffff, 5]);
            // insertq xmm0, xmm1 with the fields in xmm1's high half
            regs.xmm[0] = [0, 0];
            regs.xmm[1] = [0xff, 0x0404];
            assert_eq!(run(&mut regs, &[0xf2, 0x0f, 0x79, 0xc1]), 4);
            assert_eq!(regs.xmm[0][0], 0xf0);
            // clwb [rax]: nothing to do
            assert_eq!(run(&mut regs, &[0x66, 0x0f, 0xae, 0x30]), 4);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::Image;
    use victory_platform::faults::Registers;
    use victory_platform::{Memory, Protection};

    #[test]
    fn the_report_names_title_code() {
        crate::images::register(Image {
            name: "fake".into(),
            base: 0x7000_0000,
            size: 0x1000,
            exception_table: None,
        })
        .unwrap();
        let stack: [u64; 8] = [0x7000_0010, 1, 2, 3, 4, 5, 6, 7];
        let mut registers = Registers {
            rip: 0x7000_0123,
            rsp: stack.as_ptr() as u64,
            rax: 0xa,
            ..Registers::default()
        };
        let fault = Fault {
            kind: FaultKind::Access,
            address: 0x10,
            write: false,
            trap: Some(PAGE_FAULT),
            error: 4,
            registers: &mut registers,
        };
        let mut text = String::new();
        report(&fault, &mut text);
        crate::images::unregister(0x7000_0000);
        assert!(
            text.starts_with("victory: access fault at 0x10 in title code\n"),
            "{text}"
        );
        assert!(text.contains("  rip fake+0x123  rsp "), "{text}");
        assert!(text.contains("  rax 0xa  rbx 0x0"), "{text}");
        assert!(
            text.contains("  stack: 0x70000010 (fake+0x10) 0x1 0x2 0x3 0x4 0x5 0x6 0x7\n"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 7, "{text}");
    }

    #[test]
    fn a_full_buffer_cuts_the_report_off() {
        let mut bytes = [0u8; 8];
        let mut out = Buf {
            bytes: &mut bytes,
            len: 0,
        };
        let _ = write!(out, "victory: {}", 12345);
        assert_eq!(out.len, 8);
        assert_eq!(&bytes, b"victory:");
    }

    /// A page of our own with `code` at its start, made read-execute.
    fn code_page(code: &[u8]) -> usize {
        let page = Host::page_size();
        let at = Host::allocate(page, None).expect("a page");
        // SAFETY: our fresh page, read-write until protected.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, code.len());
            assert!(Host::protect(at, page, Protection::ReadExecute));
        }
        at
    }

    #[repr(align(64))]
    struct Lines([u8; 128]);

    /// Held by the tests that patch: one patch at a time in the process
    /// ([`zen2::PATCHING`]), so that two of them at once would see a patch
    /// refused.
    static PATCH_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn patch_tests() -> std::sync::MutexGuard<'static, ()> {
        PATCH_TESTS.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn zen2_emulates_clzero_monitorx_mwaitx() {
        let _one = patch_tests();
        let mut lines = Lines([0xaa; 128]);
        let page = Host::page_size();
        let code = code_page(&[0x0f, 0x01, 0xfc, 0x0f, 0x01, 0xfa, 0x0f, 0x01, 0xfb, 0xc3]);
        let before = emulated();
        let mut regs = Registers {
            rip: code as u64,
            rax: lines.0.as_mut_ptr() as u64 + 64 + 5,
            ..Registers::default()
        };
        assert!(zen2::emulate(&mut regs));
        assert_eq!(regs.rip, code as u64 + 3);
        assert!(lines.0[64..].iter().all(|&b| b == 0), "the line at rax");
        assert!(lines.0[..64].iter().all(|&b| b == 0xaa), "the line before");
        assert!(zen2::emulate(&mut regs));
        assert_eq!(regs.rip, code as u64 + 6);
        assert!(zen2::emulate(&mut regs));
        assert_eq!(regs.rip, code as u64 + 9);
        // SAFETY: our page, readable.
        let bytes = unsafe { std::slice::from_raw_parts(code as *const u8, 10) };
        assert_eq!(bytes[3..6], [0x0f, 0x1f, 0x00], "monitorx became a NOP");
        assert_eq!(bytes[6..9], [0xf3, 0x90, 0x90], "mwaitx became pause");
        assert!(emulated() >= before + 3);
        assert!(!zen2::emulate(&mut regs), "ret is not ours");
        // SAFETY: our page, no longer used.
        assert!(unsafe { Host::free(code, page) });
    }

    /// A wait instruction is patched in one store where its bytes lie in an
    /// aligned qword (offsets 0 and 5 of 16 aligned bytes) or in the aligned
    /// 16 (6 and 13, with `cmpxchg16b`); across 16 (14) it is left, and
    /// stepped over all the same.
    #[test]
    fn wait_patches_are_single_stores_or_none() {
        const OFFSETS: [usize; 5] = [0, 5, 6, 13, 14];
        let _one = patch_tests();
        let page = Host::page_size();
        let mut code = [0x90u8; 5 * 32];
        for (k, offset) in OFFSETS.iter().enumerate() {
            code[32 * k + offset..32 * k + offset + 3].copy_from_slice(&[0x0f, 0x01, 0xfa]);
        }
        let base = code_page(&code);
        let wide = std::arch::is_x86_feature_detected!("cmpxchg16b");
        for (k, offset) in OFFSETS.into_iter().enumerate() {
            let at = base + 32 * k + offset;
            assert_eq!(at % 16, offset);
            let mut regs = Registers {
                rip: at as u64,
                ..Registers::default()
            };
            assert!(zen2::emulate(&mut regs), "offset {offset}");
            assert_eq!(regs.rip, at as u64 + 3, "offset {offset}");
            // SAFETY: our page, readable.
            let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, 3) };
            let patched = match offset {
                0 | 5 => true,
                6 | 13 => wide,
                _ => false,
            };
            let expected = if patched {
                [0x0f, 0x1f, 0x00]
            } else {
                [0x0f, 0x01, 0xfa]
            };
            assert_eq!(bytes, expected, "offset {offset}");
        }
        // SAFETY: our page, no longer used.
        assert!(unsafe { Host::free(base, page) });
    }

    #[test]
    fn a_patch_waits_for_no_other() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        let held = zen2::take(&flag).expect("a free flag");
        std::thread::scope(|s| {
            let other = s.spawn(|| zen2::take(&flag).is_some());
            assert!(!other.join().unwrap(), "refused while another holds it");
        });
        drop(held);
        let again = std::thread::scope(|s| s.spawn(|| zen2::take(&flag).is_some()).join());
        assert!(again.unwrap(), "taken once given back");
    }

    #[test]
    fn traps_map_to_windows_exceptions() {
        use FaultKind::*;
        let av = |what, at| Raised::Exception(seh::STATUS_ACCESS_VIOLATION, Some([what, at]));
        let nop = [0x90u8];
        // Page faults: read, write, execute (instruction fetch, write clear).
        assert_eq!(raised(Access, Some(14), 0x4, 0x10, &nop), av(0, 0x10));
        assert_eq!(raised(Access, Some(14), 0x6, 0x10, &nop), av(1, 0x10));
        assert_eq!(raised(Access, Some(14), 0x15, 0x10, &nop), av(8, 0x10));
        // `int 0x29` (#GP, error 0x14a): a fail-fast, not a write; whatever
        // the signal.
        let ff = [0xCD, 0x29, 0x90];
        assert_eq!(raised(Access, Some(13), 0x14a, 0, &ff), Raised::FailFast);
        let sigill = raised(IllegalInstruction, None, 0, 0, &ff);
        assert_eq!(sigill, Raised::FailFast);
        let not = [0xCD, 0x2A];
        assert_ne!(raised(Access, Some(13), 0x14a, 0, &not), Raised::FailFast);
        // #GP: privileged instructions, else a non-canonical address.
        let gp = |code: &[u8]| raised(Access, Some(13), 0, 0, code);
        let privileged = Raised::Exception(seh::STATUS_PRIVILEGED_INSTRUCTION, None);
        for code in [
            &[0xF4][..],               // hlt
            &[0xFA],                   // cli
            &[0xEC],                   // in al, dx
            &[0xE6, 0x80],             // out 0x80, al
            &[0x0F, 0x32],             // rdmsr
            &[0x0F, 0x30],             // wrmsr
            &[0x0F, 0x20, 0xC0],       // mov rax, cr0
            &[0x44, 0x0F, 0x22, 0xC0], // mov cr8, rax
            &[0x66, 0xED],             // in ax, dx
        ] {
            assert_eq!(gp(code), privileged, "{code:02x?}");
        }
        assert_eq!(gp(&[0x48, 0x8B, 0x00]), av(0, 0));
        assert_eq!(gp(&[]), av(0, 0));
        // Other traps go by the signal.
        assert_eq!(raised(Access, Some(17), 0, 0x21, &nop), av(0, 0x21));
        assert_eq!(raised(Access, None, 0, 0x21, &nop), av(0, 0x21));
        assert_eq!(
            raised(IllegalInstruction, Some(6), 0, 0, &[0x0F, 0x0B]),
            Raised::Exception(seh::STATUS_ILLEGAL_INSTRUCTION, None)
        );
        assert_eq!(
            raised(Breakpoint, Some(3), 0, 0, &nop),
            Raised::Exception(seh::STATUS_BREAKPOINT, None)
        );
        assert_eq!(
            raised(Arithmetic, Some(0), 0, 0, &nop),
            Raised::Exception(seh::STATUS_INTEGER_DIVIDE_BY_ZERO, None)
        );
    }

    #[test]
    fn a_thread_with_no_teb_is_not_dispatched() {
        let why = std::thread::spawn(|| {
            let stack = [0u64; 8];
            let mut registers = Registers {
                rip: 0x7100_0000,
                rsp: stack.as_ptr() as u64 + 0x100_0000,
                ..Registers::default()
            };
            let before = registers;
            let mut fault = Fault {
                kind: FaultKind::Access,
                address: 0,
                write: false,
                trap: Some(PAGE_FAULT),
                error: 4,
                registers: &mut registers,
            };
            let why = to_exception(&mut fault);
            assert_eq!(registers, before, "nothing changed");
            why
        })
        .join()
        .unwrap();
        assert_eq!(why, Err(NO_TEB));
        assert!(NO_TEB.contains("a thread with no TEB"));
    }

    #[test]
    fn a_patched_page_gets_its_sections_protection_back() {
        // Headers of a fake image: e_lfanew 0x40; two sections, .text
        // (read-execute) at 0x1000 and a writable code section at 0x3000
        // whose VirtualSize is 0 (its raw size, 0x1800, counts).
        let mut image = vec![0u8; 0x400];
        image[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        let nt = 0x40;
        image[nt + 6..nt + 8].copy_from_slice(&2u16.to_le_bytes());
        image[nt + 20..nt + 22].copy_from_slice(&0xF0u16.to_le_bytes());
        let table = nt + 24 + 0xF0;
        let mut section = |i: usize, size: u32, rva: u32, raw: u32, flags: u32| {
            let at = table + 40 * i;
            image[at + 8..at + 12].copy_from_slice(&size.to_le_bytes());
            image[at + 12..at + 16].copy_from_slice(&rva.to_le_bytes());
            image[at + 16..at + 20].copy_from_slice(&raw.to_le_bytes());
            image[at + 36..at + 40].copy_from_slice(&flags.to_le_bytes());
        };
        section(0, 0x1800, 0x1000, 0x1800, 0x6000_0020);
        section(1, 0, 0x3000, 0x1800, 0xE000_0020);
        let base = image.as_ptr() as usize;
        let writable = |rva| page_writable(base, rva, 0x1000);
        assert_eq!(writable(0x1000), Some(false));
        assert_eq!(writable(0x2000), Some(false));
        assert_eq!(writable(0x3000), Some(true));
        assert_eq!(writable(0x4000), Some(true), "the raw size");
        assert_eq!(writable(0x5000), Some(false), "past both");
        // A page in no image of ours: read-execute, as before.
        let protection = code_protection(0x7200_0000, 0x1000);
        assert_eq!(protection, Protection::ReadExecute);
    }
}
