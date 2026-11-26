//! Faults (access, illegal instruction, breakpoint, arithmetic) in this
//! process, and the faulting thread's state, for a handler of ours: where
//! Windows has exception dispatch, Unix has signals.
//!
//! The handler runs inside the signal handler, so it must not allocate,
//! lock or call anything that may; the registers it gets are a copy read
//! from the signal context, written back into it when it resumes.

#[cfg(target_arch = "x86_64")]
use std::cell::Cell;
#[cfg(target_arch = "x86_64")]
use std::sync::OnceLock;
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

#[cfg(target_arch = "x86_64")]
use crate::{Host, Memory};

/// What kind of fault a thread took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// `SIGSEGV` or `SIGBUS`: memory that is not there or not allowed.
    Access,
    /// `SIGILL`: an instruction this processor does not run.
    IllegalInstruction,
    /// `SIGTRAP`: `int3`, or a debug trap.
    Breakpoint,
    /// `SIGFPE`: a division by zero, an overflowing division.
    Arithmetic,
}

/// The faulting thread's registers, read from the signal context before the
/// handler runs and written back into it after, so the handler changes
/// them by changing these.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Registers {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    /// xmm0..xmm15, low then high qword; zero when the host gave no float state.
    pub xmm: [[u64; 2]; 16],
}

/// A fault, as a handler sees it.
pub struct Fault<'a> {
    pub kind: FaultKind,
    /// For `Access`: the address the instruction reached for (`si_addr`); 0 otherwise.
    pub address: usize,
    /// For `Access`: whether the instruction wrote; false when unknown.
    pub write: bool,
    pub registers: &'a mut Registers,
}

/// What a handler did with a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// The thread goes on with the handler's registers.
    Resume,
    /// Not ours: what was installed before takes it.
    Pass,
}

/// A fault handler of ours; it runs in a signal handler (see the module).
pub type Handler = fn(&mut Fault) -> Handled;

/// The alternate signal stack's size.
pub const ALTERNATE_STACK: usize = 64 << 10;

/// Faults in this process, handed to a handler of ours.
pub trait Faults {
    /// Installs `handler` for SIGSEGV, SIGBUS, SIGILL, SIGTRAP and SIGFPE,
    /// once per process: a second call returns false and changes nothing.
    /// On a fault the handler gets the thread's registers; `Resume` returns
    /// to the (possibly changed) registers; `Pass` hands the signal to what
    /// was installed before (the previous handler if there was one, else
    /// the default action by restoring it and returning, so the faulting
    /// instruction runs again under it).
    fn install(handler: Handler) -> bool;
    /// Gives this thread an alternate signal stack of `ALTERNATE_STACK`
    /// bytes, so that a handler runs even on an exhausted stack; once per
    /// thread (a second call returns true and does nothing). The stack is
    /// kept for the process's life.
    fn alternate_stack() -> bool;
}

/// The signals we take, in the order of [`PREVIOUS`].
#[cfg(target_arch = "x86_64")]
const SIGNALS: [libc::c_int; 5] = [
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGILL,
    libc::SIGTRAP,
    libc::SIGFPE,
];

/// The installed [`Handler`] as an address; 0 before [`Faults::install`].
#[cfg(target_arch = "x86_64")]
static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// What was installed for each of [`SIGNALS`] before us.
#[cfg(target_arch = "x86_64")]
static PREVIOUS: OnceLock<[libc::sigaction; 5]> = OnceLock::new();

#[cfg(target_arch = "x86_64")]
thread_local! {
    /// Whether this thread has its alternate signal stack.
    static HAVE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(target_arch = "x86_64")]
impl Faults for Host {
    fn install(handler: Handler) -> bool {
        if HANDLER
            .compare_exchange(0, handler as usize, SeqCst, SeqCst)
            .is_err()
        {
            return false;
        }
        // What is there now is kept before ours goes in, so that a signal
        // taken right after ours is installed finds it.
        // SAFETY: `sigaction` is plain data, for which zero is a value.
        let mut previous: [libc::sigaction; 5] = unsafe { std::mem::zeroed() };
        for (signal, old) in SIGNALS.iter().zip(previous.iter_mut()) {
            // SAFETY: a query into our own variable.
            if unsafe { libc::sigaction(*signal, std::ptr::null(), old) } != 0 {
                return false;
            }
        }
        if PREVIOUS.set(previous).is_err() {
            return false;
        }
        // SAFETY: as above.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        // SAFETY: our own mask, emptied.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        SIGNALS.iter().all(|&signal| {
            // SAFETY: our handler, which takes what SA_SIGINFO passes.
            unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) == 0 }
        })
    }

    fn alternate_stack() -> bool {
        if HAVE.with(Cell::get) {
            return true;
        }
        let Some(stack) = Host::allocate(ALTERNATE_STACK, None) else {
            return false;
        };
        // SAFETY: `stack_t` is plain data, for which zero is a value.
        let mut alternate: libc::stack_t = unsafe { std::mem::zeroed() };
        alternate.ss_sp = stack as *mut libc::c_void;
        alternate.ss_size = ALTERNATE_STACK;
        alternate.ss_flags = 0;
        // SAFETY: fresh read-write memory of that size, never freed: the
        // kernel may run a handler on it at any time for the thread's life,
        // and nothing tells us when the thread ends.
        let made = unsafe { libc::sigaltstack(&alternate, std::ptr::null_mut()) } == 0;
        if made {
            HAVE.with(|have| have.set(true));
        }
        made
    }
}

/// The signal handler: the fault to our handler, then back to the thread or
/// on to what was there before. Nothing here allocates or locks.
#[cfg(target_arch = "x86_64")]
extern "C" fn on_signal(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let kind = match signal {
        libc::SIGSEGV | libc::SIGBUS => FaultKind::Access,
        libc::SIGILL => FaultKind::IllegalInstruction,
        libc::SIGTRAP => FaultKind::Breakpoint,
        _ => FaultKind::Arithmetic,
    };
    let address = if kind == FaultKind::Access && !info.is_null() {
        // SAFETY: the kernel's siginfo for this signal.
        unsafe { fault_address(info) }
    } else {
        0
    };
    // Bit 1 of the page fault's error code: the access was a write.
    // SAFETY: the kernel's context for this signal, or null.
    let write = kind == FaultKind::Access && unsafe { error_code(context) } & 2 != 0;
    let handler = HANDLER.load(SeqCst);
    let mut registers = Registers::default();
    // SAFETY: (both) the kernel's context for this signal, of the type its
    // host passes with SA_SIGINFO.
    let handled = if handler != 0 && unsafe { read_registers(context, &mut registers) } {
        // SAFETY: the address of the `Handler` that `install` stored.
        let handler = unsafe { std::mem::transmute::<usize, Handler>(handler) };
        handler(&mut Fault {
            kind,
            address,
            write,
            registers: &mut registers,
        })
    } else {
        Handled::Pass
    };
    match handled {
        // SAFETY: as above.
        Handled::Resume => unsafe { write_registers(context, &registers) },
        // SAFETY: the kernel's arguments for this signal, passed on.
        Handled::Pass => unsafe { pass_on(signal, info, context) },
    }
}

/// Hands a signal to what was installed for it before us.
///
/// # Safety
/// The arguments are the kernel's for a signal being handled.
#[cfg(target_arch = "x86_64")]
unsafe fn pass_on(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    let index = SIGNALS.iter().position(|&s| s == signal);
    let Some(previous) = PREVIOUS.get().zip(index).map(|(all, i)| &all[i]) else {
        // SAFETY: the default action back for this signal.
        unsafe { libc::signal(signal, libc::SIG_DFL) };
        return;
    };
    match previous.sa_sigaction {
        libc::SIG_DFL => {
            // SAFETY: what was there before us, put back: the instruction
            // runs again under it.
            unsafe { libc::sigaction(signal, previous, std::ptr::null_mut()) };
        }
        libc::SIG_IGN => {}
        f if previous.sa_flags & libc::SA_SIGINFO != 0 => {
            // SAFETY: a handler installed with SA_SIGINFO takes these.
            let f = unsafe {
                std::mem::transmute::<
                    usize,
                    extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void),
                >(f)
            };
            f(signal, info, context);
        }
        f => {
            // SAFETY: a handler installed without SA_SIGINFO takes the signal.
            let f = unsafe { std::mem::transmute::<usize, extern "C" fn(libc::c_int)>(f) };
            f(signal);
        }
    }
}

/// `si_addr`: the address a faulting access reached for.
///
/// # Safety
/// `info` is the kernel's siginfo for SIGSEGV or SIGBUS.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn fault_address(info: *mut libc::siginfo_t) -> usize {
    // SAFETY: the caller's siginfo, of a fault, which has the field.
    unsafe { (*info).si_addr() as usize }
}

/// `si_addr`: the address a faulting access reached for.
///
/// # Safety
/// `info` is the kernel's siginfo for SIGSEGV or SIGBUS.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
unsafe fn fault_address(info: *mut libc::siginfo_t) -> usize {
    // SAFETY: the caller's siginfo.
    unsafe { (*info).si_addr as usize }
}

/// The fault's error code (`REG_ERR`): for a page fault, bit 1 is set for
/// a write; 0 without a context.
///
/// # Safety
/// `context` is null or the kernel's `ucontext_t` for a signal being
/// handled.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn error_code(context: *mut libc::c_void) -> u64 {
    if context.is_null() {
        return 0;
    }
    // SAFETY: the caller's context.
    let mcontext = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext };
    mcontext.gregs[libc::REG_ERR as usize] as u64
}

/// The fault's error code (`__es.__err`): for a page fault, bit 1 is set
/// for a write; 0 without a context.
///
/// # Safety
/// `context` is null or the kernel's `ucontext_t` for a signal being
/// handled.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
unsafe fn error_code(context: *mut libc::c_void) -> u64 {
    if context.is_null() {
        return 0;
    }
    // SAFETY: the caller's context, whose `uc_mcontext` points at the
    // machine state the kernel saved for the signal, or is null.
    unsafe {
        let mcontext = (*context.cast::<libc::ucontext_t>()).uc_mcontext;
        if mcontext.is_null() {
            return 0;
        }
        u64::from((*mcontext).__es.__err)
    }
}

/// The general registers of [`Registers`] in its field order, for the
/// conversions below (each host lists its slots in the same order).
#[cfg(target_arch = "x86_64")]
fn general(r: &mut Registers) -> [&mut u64; 18] {
    [
        &mut r.rax,
        &mut r.rbx,
        &mut r.rcx,
        &mut r.rdx,
        &mut r.rsi,
        &mut r.rdi,
        &mut r.rbp,
        &mut r.rsp,
        &mut r.r8,
        &mut r.r9,
        &mut r.r10,
        &mut r.r11,
        &mut r.r12,
        &mut r.r13,
        &mut r.r14,
        &mut r.r15,
        &mut r.rip,
        &mut r.rflags,
    ]
}

/// `gregs` slots for [`general`]'s order (`<sys/ucontext.h>`).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const GREGS: [libc::c_int; 18] = [
    libc::REG_RAX,
    libc::REG_RBX,
    libc::REG_RCX,
    libc::REG_RDX,
    libc::REG_RSI,
    libc::REG_RDI,
    libc::REG_RBP,
    libc::REG_RSP,
    libc::REG_R8,
    libc::REG_R9,
    libc::REG_R10,
    libc::REG_R11,
    libc::REG_R12,
    libc::REG_R13,
    libc::REG_R14,
    libc::REG_R15,
    libc::REG_RIP,
    libc::REG_EFL,
];

/// An XMM register of the kernel's `fxsave` area as two qwords.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn qwords(element: [u32; 4]) -> [u64; 2] {
    [
        u64::from(element[0]) | u64::from(element[1]) << 32,
        u64::from(element[2]) | u64::from(element[3]) << 32,
    ]
}

/// Reads the registers of `context` into `r`; false if there is none.
///
/// # Safety
/// `context` is null or the kernel's `ucontext_t` for a signal being
/// handled.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn read_registers(context: *mut libc::c_void, r: &mut Registers) -> bool {
    if context.is_null() {
        return false;
    }
    // SAFETY: the caller's context; `fpregs`, where not null, points at the
    // float state the kernel saved in the signal frame.
    unsafe {
        let mcontext = &(*context.cast::<libc::ucontext_t>()).uc_mcontext;
        for (slot, &reg) in general(r).into_iter().zip(GREGS.iter()) {
            *slot = mcontext.gregs[reg as usize] as u64;
        }
        if let Some(fp) = mcontext.fpregs.as_ref() {
            for (xmm, saved) in r.xmm.iter_mut().zip(fp._xmm.iter()) {
                *xmm = qwords(saved.element);
            }
        }
    }
    true
}

/// Writes `r` back into the registers of `context`.
///
/// # Safety
/// As [`read_registers`], with a context it read.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn write_registers(context: *mut libc::c_void, r: &Registers) {
    let mut r = *r;
    // SAFETY: as in `read_registers`.
    unsafe {
        let mcontext = &mut (*context.cast::<libc::ucontext_t>()).uc_mcontext;
        for (slot, &reg) in general(&mut r).into_iter().zip(GREGS.iter()) {
            mcontext.gregs[reg as usize] = *slot as libc::greg_t;
        }
        if let Some(fp) = mcontext.fpregs.as_mut() {
            for (xmm, saved) in r.xmm.iter().zip(fp._xmm.iter_mut()) {
                saved.element = [
                    xmm[0] as u32,
                    (xmm[0] >> 32) as u32,
                    xmm[1] as u32,
                    (xmm[1] >> 32) as u32,
                ];
            }
        }
    }
}

/// The general registers of a thread state in [`general`]'s order.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn thread_state(ss: &mut libc::__darwin_x86_thread_state64) -> [&mut u64; 18] {
    [
        &mut ss.__rax,
        &mut ss.__rbx,
        &mut ss.__rcx,
        &mut ss.__rdx,
        &mut ss.__rsi,
        &mut ss.__rdi,
        &mut ss.__rbp,
        &mut ss.__rsp,
        &mut ss.__r8,
        &mut ss.__r9,
        &mut ss.__r10,
        &mut ss.__r11,
        &mut ss.__r12,
        &mut ss.__r13,
        &mut ss.__r14,
        &mut ss.__r15,
        &mut ss.__rip,
        &mut ss.__rflags,
    ]
}

/// xmm0 of a float state: xmm1..xmm15 follow it, 16 bytes each
/// (`__fpu_xmm0` .. `__fpu_xmm15`, `<mach/i386/_structs.h>`), byte-aligned.
///
/// # Safety
/// `fs` points at a float state.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
unsafe fn xmm0(fs: *mut libc::__darwin_x86_float_state64) -> *mut [u64; 2] {
    // SAFETY: a field's address in the caller's float state.
    unsafe { (&raw mut (*fs).__fpu_xmm0).cast() }
}

/// Reads the registers of `context` into `r`; false if there is none.
///
/// # Safety
/// `context` is null or the kernel's `ucontext_t` for a signal being
/// handled.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
unsafe fn read_registers(context: *mut libc::c_void, r: &mut Registers) -> bool {
    if context.is_null() {
        return false;
    }
    // SAFETY: the caller's context, whose `uc_mcontext` points at the
    // machine state the kernel saved for the signal (its float state, for
    // a process with AVX state too, begins as `__darwin_x86_float_state64`).
    unsafe {
        let mcontext = (*context.cast::<libc::ucontext_t>()).uc_mcontext;
        if mcontext.is_null() {
            return false;
        }
        for (slot, saved) in general(r)
            .into_iter()
            .zip(thread_state(&mut (*mcontext).__ss))
        {
            *slot = *saved;
        }
        let xmm = xmm0(&raw mut (*mcontext).__fs);
        for (i, register) in r.xmm.iter_mut().enumerate() {
            *register = xmm.add(i).read_unaligned();
        }
    }
    true
}

/// Writes `r` back into the registers of `context`.
///
/// # Safety
/// As [`read_registers`], with a context it read.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
unsafe fn write_registers(context: *mut libc::c_void, r: &Registers) {
    let mut r = *r;
    // SAFETY: as in `read_registers`.
    unsafe {
        let mcontext = (*context.cast::<libc::ucontext_t>()).uc_mcontext;
        for (slot, saved) in general(&mut r)
            .into_iter()
            .zip(thread_state(&mut (*mcontext).__ss))
        {
            *saved = *slot;
        }
        let xmm = xmm0(&raw mut (*mcontext).__fs);
        for (i, register) in r.xmm.iter().enumerate() {
            xmm.add(i).write_unaligned(*register);
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::arch::asm;
    use std::sync::atomic::AtomicBool;

    /// The handler the next fault of a test goes to, as an address.
    static TEST_HANDLER: AtomicUsize = AtomicUsize::new(0);
    /// Set by the SIGTRAP handler installed before ours.
    static TRAPPED: AtomicBool = AtomicBool::new(false);

    /// Ours for the whole test process: each test sets [`TEST_HANDLER`].
    fn dispatch(fault: &mut Fault) -> Handled {
        match TEST_HANDLER.load(SeqCst) {
            0 => Handled::Pass,
            f => {
                // SAFETY: a `Handler` a test stored.
                let handler = unsafe { std::mem::transmute::<usize, Handler>(f) };
                handler(fault)
            }
        }
    }

    /// A plain libc SIGTRAP handler, there before ours: `int3` leaves `rip`
    /// past itself, so the thread goes on as it is.
    extern "C" fn trapped(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
        TRAPPED.store(true, SeqCst);
    }

    /// The libc SIGTRAP handler, then ours, once for the process; whether
    /// ours went in.
    fn install_once() -> bool {
        static INSTALLED: OnceLock<bool> = OnceLock::new();
        *INSTALLED.get_or_init(|| {
            // SAFETY: plain data, then a handler that takes what SA_SIGINFO
            // passes.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = trapped as *const () as usize;
                action.sa_flags = libc::SA_SIGINFO;
                libc::sigemptyset(&mut action.sa_mask);
                assert_eq!(
                    libc::sigaction(libc::SIGTRAP, &action, std::ptr::null_mut()),
                    0
                );
            }
            Host::install(dispatch)
        })
    }

    fn set(handler: Handler) {
        TEST_HANDLER.store(handler as usize, SeqCst);
    }

    fn null_read(fault: &mut Fault) -> Handled {
        assert_eq!(fault.kind, FaultKind::Access);
        assert_eq!(fault.address, 0);
        fault.registers.rax = 0x77;
        fault.registers.rip += 7;
        Handled::Resume
    }

    fn illegal(fault: &mut Fault) -> Handled {
        assert_eq!(fault.kind, FaultKind::IllegalInstruction);
        fault.registers.rcx = 0x99;
        fault.registers.rip += 2;
        Handled::Resume
    }

    fn xmm(fault: &mut Fault) -> Handled {
        assert_eq!(fault.kind, FaultKind::IllegalInstruction);
        fault.registers.xmm[0] = [0x1122_3344_5566_7788, 0];
        fault.registers.rip += 2;
        Handled::Resume
    }

    fn pass_breakpoints(fault: &mut Fault) -> Handled {
        assert_eq!(fault.kind, FaultKind::Breakpoint);
        Handled::Pass
    }

    fn noop(_: &mut Fault) -> Handled {
        Handled::Pass
    }

    #[test]
    fn faults_are_handled_and_chained() {
        assert!(install_once());
        assert!(Host::alternate_stack());
        assert!(Host::alternate_stack(), "a second time does nothing");

        set(null_read);
        let value: u64;
        // SAFETY: a load from address 0, which faults; the handler gives
        // the result and steps over it.
        unsafe {
            asm!(".byte 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00", out("rax") value,
                options(nostack))
        };
        assert_eq!(value, 0x77);

        set(illegal);
        let value: u64;
        // SAFETY: `ud2`, which the handler steps over, giving rcx.
        unsafe { asm!(".byte 0x0f, 0x0b", out("rcx") value, options(nostack)) };
        assert_eq!(value, 0x99);

        set(xmm);
        let value: u64;
        // SAFETY: as above; the handler gives xmm0, read in the same block.
        unsafe {
            asm!(".byte 0x0f, 0x0b", "movq {0}, xmm0", out(reg) value, out("xmm0") _,
                options(nostack))
        };
        assert_eq!(value, 0x1122_3344_5566_7788);

        set(pass_breakpoints);
        let value: u64;
        // SAFETY: `int3`, which ours passes to the libc handler before it;
        // then a plain move, to see the thread went on.
        unsafe { asm!("int3", "mov ecx, 0x31", out("rcx") value, options(nostack)) };
        assert!(TRAPPED.load(SeqCst), "the previous handler ran");
        assert_eq!(value, 0x31);
        TEST_HANDLER.store(0, SeqCst);
    }

    #[test]
    fn a_second_install_is_refused() {
        assert!(install_once());
        assert!(!Host::install(noop));
    }
}
