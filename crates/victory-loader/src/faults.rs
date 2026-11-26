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

use victory_platform::Host;
use victory_platform::faults::{Fault, FaultKind, Faults, Handled, Handler, Registers};

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
    if crate::images::with_image(rip, |i| i.is_some()) != Some(true) {
        return Handled::Pass; // our code or the host's, or the registry was locked
    }
    if to_exception(fault) {
        return Handled::Resume;
    }
    let mut buffer = [0u8; 2048];
    let mut out = Buf {
        bytes: &mut buffer,
        len: 0,
    };
    report(fault, &mut out);
    let _ = writeln!(out, "  no stack left to dispatch it on: a stack overflow");
    let len = out.len;
    // SAFETY: our buffer's written bytes to stderr, then the end of the
    // process: both async-signal-safe.
    unsafe {
        write(2, buffer.as_ptr(), len);
        _exit(128 + signal_number(fault.kind))
    }
}

/// Bytes below the faulting rsp for the exception: its record, then its
/// context at [`CONTEXT_AT`].
const EXCEPTION_ROOM: u64 = 0x600;
/// Where the context lies in [`EXCEPTION_ROOM`]: past the record, 16-aligned.
const CONTEXT_AT: u64 = 0xA0;
/// The stack the dispatch needs: the exception and our frames.
const STACK_ROOM: u64 = EXCEPTION_ROOM + (64 << 10);
const _: () = assert!(
    size_of::<seh::ExceptionRecord>() as u64 <= CONTEXT_AT
        && CONTEXT_AT + size_of::<seh::Context>() as u64 <= EXCEPTION_ROOM
);

/// Turns a fault in title code into a Windows exception: its record and
/// context written on the faulting thread's own stack, below its rsp, and
/// the thread sent to `seh::dispatch_entry` there, which it runs once the
/// signal handler returns (with the signal mask restored). False, changing
/// nothing, when the stack has no room left for that (a stack overflow).
fn to_exception(fault: &mut Fault) -> bool {
    let limit = crate::teb::current_teb().map_or(0, |teb| {
        // SAFETY: this thread's TEB, ours, alive for the thread's life; a
        // read of it, which nothing in a signal handler can disturb.
        unsafe { (*(teb as *const crate::teb::Teb)).stack_limit }
    });
    let regs = &mut *fault.registers;
    if regs.rsp < limit.saturating_add(STACK_ROOM) {
        return false;
    }
    let (code, parameters) = match fault.kind {
        FaultKind::Access => (
            seh::STATUS_ACCESS_VIOLATION,
            Some([u64::from(fault.write), fault.address as u64]),
        ),
        FaultKind::IllegalInstruction => (seh::STATUS_ILLEGAL_INSTRUCTION, None),
        FaultKind::Breakpoint => {
            // Windows reports the `int3` itself, not the byte after it.
            let mut byte = [0u8];
            let before = regs.rip.wrapping_sub(1) as usize;
            if <Host as victory_platform::Memory>::read_into(before, &mut byte) && byte[0] == 0xCC {
                regs.rip -= 1;
            }
            (seh::STATUS_BREAKPOINT, None)
        }
        FaultKind::Arithmetic => (seh::STATUS_INTEGER_DIVIDE_BY_ZERO, None),
    };
    let mut record = seh::ExceptionRecord::new(code, 0, regs.rip);
    if let Some(parameters) = parameters {
        record.number_parameters = 2;
        record.information[..2].copy_from_slice(&parameters);
    }
    let base = (regs.rsp - EXCEPTION_ROOM) & !15;
    // SAFETY: the faulting thread's own stack below its rsp, which Win64
    // code keeps nothing in, and which has room (checked above).
    unsafe {
        std::ptr::write(base as *mut seh::ExceptionRecord, record);
        std::ptr::write(
            (base + CONTEXT_AT) as *mut seh::Context,
            seh::Context::from_registers(regs),
        );
    }
    regs.rip = seh::dispatch_entry as *const () as u64;
    regs.rdi = base;
    regs.rsi = base + CONTEXT_AT;
    // As after a call: rsp + 8 is 16-aligned at a SysV function's entry.
    regs.rsp = base - 8;
    // Our code runs with the direction flag clear, as the SysV ABI has it,
    // and without single steps; the context keeps the title's flags.
    regs.rflags &= !(RFLAGS_DF | RFLAGS_TF);
    true
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
        if !<Host as victory_platform::Memory>::read_into(r.rsp as usize + 8 * i, &mut value) {
            break;
        }
        let value = u64::from_le_bytes(value);
        let _ = write!(out, " {value:#x}");
        if crate::images::with_image(value as usize, |i| i.is_some()) == Some(true) {
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
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering::{Relaxed, SeqCst};

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
        // The 16 bytes, else those left in the page (the next one may not
        // be mapped).
        let page = Host::page_size();
        let room = (page - rip % page).min(code.len());
        let n = if Host::read_into(rip, &mut code) {
            code.len()
        } else if Host::read_into(rip, &mut code[..room]) {
            room
        } else {
            return false;
        };
        let code = &code[..n];
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
    /// as before. The page is made writable for the store and read-execute
    /// after; left as it is if the host refuses.
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
        let page = Host::page_size();
        let start = at & !(page - 1);
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
            Host::protect(start, page, Protection::ReadExecute);
            patched
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
        });
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

    #[test]
    fn zen2_emulates_clzero_monitorx_mwaitx() {
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
}
