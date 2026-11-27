//! x64 unwind data (`.pdata`'s RUNTIME_FUNCTION table, UNWIND_INFO) read and
//! a frame undone: what Windows' `RtlLookupFunctionEntry` and
//! `RtlVirtualUnwind` do, for stack walks (crash reports) and the dispatch
//! of the title's own exceptions, on a host that has neither
//! (docs/macos.md, M2a).
//!
//! All memory is read through `victory_platform::Memory::read_into`
//! (kernel-checked), so a bad pointer is an error, never a fault.

use victory_platform::{Host, Memory};

/// UNW_FLAG_EHANDLER: the function has an exception handler.
const FLAG_EHANDLER: u8 = 1;
/// UNW_FLAG_UHANDLER: the function has a termination handler.
const FLAG_UHANDLER: u8 = 2;
/// UNW_FLAG_CHAININFO: the unwind info continues in a parent entry's.
const FLAG_CHAININFO: u8 = 4;
/// How many chained entries are followed before the chain is malformed.
const MAX_CHAIN: usize = 16;

/// A thread's registers for unwinding: the same fields as
/// `victory_platform::faults::Registers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Context {
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
    /// xmm0..xmm15, low then high qword.
    pub xmm: [[u64; 2]; 16],
}

impl Context {
    /// The register x64 unwind codes number `n` by: 0 rax, 1 rcx, 2 rdx,
    /// 3 rbx, 4 rsp, 5 rbp, 6 rsi, 7 rdi, 8..15 r8..r15. Only the low four
    /// bits of `n` count.
    pub fn register_mut(&mut self, n: u8) -> &mut u64 {
        match n & 15 {
            0 => &mut self.rax,
            1 => &mut self.rcx,
            2 => &mut self.rdx,
            3 => &mut self.rbx,
            4 => &mut self.rsp,
            5 => &mut self.rbp,
            6 => &mut self.rsi,
            7 => &mut self.rdi,
            8 => &mut self.r8,
            9 => &mut self.r9,
            10 => &mut self.r10,
            11 => &mut self.r11,
            12 => &mut self.r12,
            13 => &mut self.r13,
            14 => &mut self.r14,
            _ => &mut self.r15,
        }
    }

    /// The value of the register numbered `n`, as [`Context::register_mut`]
    /// numbers them.
    pub fn register(&self, n: u8) -> u64 {
        match n & 15 {
            0 => self.rax,
            1 => self.rcx,
            2 => self.rdx,
            3 => self.rbx,
            4 => self.rsp,
            5 => self.rbp,
            6 => self.rsi,
            7 => self.rdi,
            8 => self.r8,
            9 => self.r9,
            10 => self.r10,
            11 => self.r11,
            12 => self.r12,
            13 => self.r13,
            14 => self.r14,
            _ => self.r15,
        }
    }
}

/// A `.pdata` entry: a function's extent and its unwind info, as RVAs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin: u32,
    pub end: u32,
    pub unwind_info: u32,
}

impl RuntimeFunction {
    fn parse(bytes: &[u8]) -> Option<RuntimeFunction> {
        let word = |i: usize| -> Option<u32> {
            Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?))
        };
        Some(RuntimeFunction {
            begin: word(0)?,
            end: word(4)?,
            unwind_info: word(8)?,
        })
    }
}

/// The entries of an exception table (12 bytes each, little-endian; a
/// trailing partial entry is ignored).
pub fn functions(pdata: &[u8]) -> Vec<RuntimeFunction> {
    pdata
        .chunks_exact(12)
        .filter_map(RuntimeFunction::parse)
        .collect()
}

/// The entry whose function holds `rva` (`begin <= rva < end`), by binary
/// search: the table is sorted by `begin`.
pub fn lookup(pdata: &[u8], rva: u32) -> Option<RuntimeFunction> {
    let count = pdata.len() / 12;
    let entry = |i: usize| RuntimeFunction::parse(&pdata[i * 12..i * 12 + 12]);
    // The number of entries that begin at or before `rva`.
    let (mut low, mut high) = (0, count);
    while low < high {
        let middle = low + (high - low) / 2;
        if entry(middle)?.begin <= rva {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    let candidate = entry(low.checked_sub(1)?)?;
    (rva < candidate.end).then_some(candidate)
}

/// A function's exception or termination handler, from its unwind info.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handler {
    /// UNW_FLAG_EHANDLER (1) and/or UNW_FLAG_UHANDLER (2).
    pub flags: u8,
    /// The handler's address.
    pub address: u64,
    /// The language-specific data following the handler RVA in the unwind
    /// info: its address.
    pub data: u64,
}

/// What [`virtual_unwind`] found of the frame it undid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unwound {
    /// The entry of the function the frame was of.
    pub function: RuntimeFunction,
    /// RSP as it was after the function's prologue (frame-register based
    /// when the function has one).
    pub establisher_frame: u64,
    /// The function's exception handler, when it has one and `rip` was in
    /// the function's body (not its prologue or an epilogue).
    pub handler: Option<Handler>,
}

/// Why a frame could not be undone.
#[derive(Debug, thiserror::Error)]
pub enum UnwindError {
    #[error("unreadable memory at {0:#x}")]
    Unreadable(u64),
    #[error("malformed unwind info: {0}")]
    Malformed(&'static str),
    #[error("unsupported unwind code {0}")]
    UnsupportedCode(u8),
}

fn read_bytes(at: u64, to: &mut [u8]) -> Result<(), UnwindError> {
    let src = usize::try_from(at).map_err(|_| UnwindError::Unreadable(at))?;
    if Host::read_into(src, to) {
        Ok(())
    } else {
        Err(UnwindError::Unreadable(at))
    }
}

fn read_u64(at: u64) -> Result<u64, UnwindError> {
    let mut bytes = [0; 8];
    read_bytes(at, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32(at: u64) -> Result<u32, UnwindError> {
    let mut bytes = [0; 4];
    read_bytes(at, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u16(at: u64) -> Result<u16, UnwindError> {
    let mut bytes = [0; 2];
    read_bytes(at, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u8(at: u64) -> Result<u8, UnwindError> {
    let mut bytes = [0; 1];
    read_bytes(at, &mut bytes)?;
    Ok(bytes[0])
}

/// An UNWIND_INFO, read.
struct Info {
    flags: u8,
    prolog_size: u8,
    frame_register: u8,
    /// Already scaled: in bytes.
    frame_offset: u64,
    codes: Vec<u16>,
    /// Where what follows the (padded) codes array lies: the handler RVA or
    /// the parent's RUNTIME_FUNCTION.
    tail: u64,
}

impl Info {
    fn read(at: u64) -> Result<Info, UnwindError> {
        let mut header = [0; 4];
        read_bytes(at, &mut header)?;
        let version = header[0] & 7;
        let flags = header[0] >> 3;
        if version != 1 && version != 2 {
            return Err(UnwindError::Malformed("version"));
        }
        if flags & (FLAG_EHANDLER | FLAG_UHANDLER) != 0 && flags & FLAG_CHAININFO != 0 {
            return Err(UnwindError::Malformed("both a handler and chained info"));
        }
        let count = u64::from(header[2]);
        let codes = (0..count)
            .map(|i| read_u16(at + 4 + i * 2))
            .collect::<Result<Vec<_>, _>>()?;
        // The version only tells ops 6 and 7 apart, and neither changes the
        // frame in either version, so it is checked and not kept.
        Ok(Info {
            flags,
            prolog_size: header[1],
            frame_register: header[3] & 15,
            frame_offset: u64::from(header[3] >> 4) * 16,
            codes,
            tail: at + 4 + count.next_multiple_of(2) * 2,
        })
    }

    /// The frame register's value less the frame offset: where SET_FPREG
    /// puts rsp back.
    fn frame_base(&self, ctx: &Context) -> u64 {
        ctx.register(self.frame_register)
            .wrapping_sub(self.frame_offset)
    }

    /// Undoes this info's codes in `ctx`, in the order they are stored;
    /// with `ran` (the offset of rip in a prologue), only those of the
    /// instructions that had run. True when a PUSH_MACHFRAME was undone.
    fn apply(&self, ctx: &mut Context, ran: Option<u32>) -> Result<bool, UnwindError> {
        let mut machine_frame = false;
        let mut i = 0;
        while i < self.codes.len() {
            let here = i;
            let code = self.codes[here];
            let offset = u32::from(code & 0xff);
            let op = ((code >> 8) & 15) as u8;
            let info = (code >> 12) as u8;
            let nodes = match op {
                0 | 2 | 3 | 10 => 1,
                1 if info == 0 => 2,
                1 if info == 1 => 3,
                1 => return Err(UnwindError::Malformed("large allocation")),
                4 | 6 | 8 => 2,
                5 | 7 | 9 => 3,
                _ => return Err(UnwindError::UnsupportedCode(op)),
            };
            let node = |n: usize| -> Result<u64, UnwindError> {
                self.codes
                    .get(here + n)
                    .map(|&v| u64::from(v))
                    .ok_or(UnwindError::Malformed("codes end inside a code"))
            };
            let far = |ctx: &Context| -> Result<u64, UnwindError> {
                Ok(ctx.rsp.wrapping_add(node(1)? | (node(2)? << 16)))
            };
            // The last node an op has is checked up front, so that a code
            // that is skipped is checked too.
            node(nodes - 1)?;
            i += nodes;
            if ran.is_some_and(|ran| offset > ran) {
                continue;
            }
            match op {
                0 => {
                    *ctx.register_mut(info) = read_u64(ctx.rsp)?;
                    ctx.rsp = ctx.rsp.wrapping_add(8);
                }
                1 => {
                    let size = if info == 0 {
                        node(1)? * 8
                    } else {
                        node(1)? | (node(2)? << 16)
                    };
                    ctx.rsp = ctx.rsp.wrapping_add(size);
                }
                2 => ctx.rsp = ctx.rsp.wrapping_add(u64::from(info) * 8 + 8),
                3 => ctx.rsp = self.frame_base(ctx),
                4 => *ctx.register_mut(info) = read_u64(ctx.rsp.wrapping_add(node(1)? * 8))?,
                5 => *ctx.register_mut(info) = read_u64(far(ctx)?)?,
                // Version 1: SAVE_XMM and SAVE_XMM_FAR; version 2: EPILOG
                // and SPARE_CODE. None of them changes the frame.
                6 | 7 => {}
                8 => {
                    let at = ctx.rsp.wrapping_add(node(1)? * 16);
                    ctx.xmm[usize::from(info)] = [read_u64(at)?, read_u64(at.wrapping_add(8))?];
                }
                9 => {
                    let at = far(ctx)?;
                    ctx.xmm[usize::from(info)] = [read_u64(at)?, read_u64(at.wrapping_add(8))?];
                }
                _ => {
                    // PUSH_MACHFRAME: [error code], rip, cs, rflags, rsp, ss.
                    let at = ctx.rsp.wrapping_add(8 * u64::from(info));
                    ctx.rip = read_u64(at)?;
                    ctx.rflags = read_u64(at.wrapping_add(16))?;
                    ctx.rsp = read_u64(at.wrapping_add(24))?;
                    machine_frame = true;
                }
            }
        }
        Ok(machine_frame)
    }
}

/// An instruction of an epilogue, decoded from the bytes at rip.
enum Step {
    /// `add rsp, imm`: the immediate is sign-extended, as the instruction's
    /// (imm8 by `83 /0 ib`, imm32 by `81 /0 id`).
    Add(i64),
    /// `lea rsp, [reg + disp]`.
    Lea(u8, i64),
    /// `pop reg`.
    Pop(u8),
}

/// The bytes at `at`, read one at a time as the decoder needs them, so
/// that nothing past the epilogue need be readable.
struct Cursor {
    at: u64,
}

impl Cursor {
    fn peek(&self, n: u64) -> Result<u8, UnwindError> {
        read_u8(self.at.wrapping_add(n))
    }

    fn take(&mut self) -> Result<u8, UnwindError> {
        let byte = self.peek(0)?;
        self.at = self.at.wrapping_add(1);
        Ok(byte)
    }

    fn disp8(&mut self) -> Result<i64, UnwindError> {
        Ok(i64::from(self.take()? as i8))
    }

    fn imm32(&mut self) -> Result<u32, UnwindError> {
        let value = read_u32(self.at)?;
        self.at = self.at.wrapping_add(4);
        Ok(value)
    }
}

/// The steps of the epilogue that starts at `rip`, if the bytes there are
/// one: `add rsp, imm` or `lea rsp, [reg + disp]`, optionally, then pops,
/// then a `ret` or a `jmp` out of the function `[begin, end)`.
fn epilogue(rip: u64, begin: u64, end: u64) -> Result<Option<Vec<Step>>, UnwindError> {
    let mut steps = Vec::new();
    let mut c = Cursor { at: rip };
    let rex = c.peek(0)?;
    if rex == 0x48 && c.peek(1)? == 0x83 && c.peek(2)? == 0xc4 {
        c.at += 3;
        steps.push(Step::Add(c.disp8()?));
    } else if rex == 0x48 && c.peek(1)? == 0x81 && c.peek(2)? == 0xc4 {
        c.at += 3;
        steps.push(Step::Add(i64::from(c.imm32()? as i32)));
    } else if (rex == 0x48 || rex == 0x49) && c.peek(1)? == 0x8d {
        let modrm = c.peek(2)?;
        let (mode, reg, rm) = (modrm >> 6, (modrm >> 3) & 7, modrm & 7);
        if (mode == 1 || mode == 2) && reg == 4 {
            c.at += 3;
            if rm == 4 && c.take()? != 0x24 {
                return Ok(None);
            }
            let disp = if mode == 1 {
                c.disp8()?
            } else {
                i64::from(c.imm32()? as i32)
            };
            let base = rm + if rex == 0x49 { 8 } else { 0 };
            steps.push(Step::Lea(base, disp));
        }
    }
    loop {
        let byte = c.peek(0)?;
        if (0x58..0x60).contains(&byte) {
            c.at += 1;
            steps.push(Step::Pop(byte - 0x58));
        } else if byte == 0x41 && (0x58..0x60).contains(&c.peek(1)?) {
            steps.push(Step::Pop(c.peek(1)? - 0x58 + 8));
            c.at += 2;
        } else {
            break;
        }
    }
    let outside = |target: u64| !(begin..end).contains(&target);
    let ends = match c.take()? {
        0xc3 => true,
        0xf3 => c.take()? == 0xc3,
        0xeb => {
            let disp = c.disp8()?;
            outside(c.at.wrapping_add_signed(disp))
        }
        0xe9 => {
            let disp = i64::from(c.imm32()? as i32);
            outside(c.at.wrapping_add_signed(disp))
        }
        0xff => c.take()? == 0x25,
        0x48 => c.take()? == 0xff && c.take()? == 0x25,
        _ => false,
    };
    Ok(ends.then_some(steps))
}

/// Pops the return address: rip from [rsp], rsp past it.
fn pop_return(ctx: &mut Context) -> Result<(), UnwindError> {
    ctx.rip = read_u64(ctx.rsp)?;
    ctx.rsp = ctx.rsp.wrapping_add(8);
    Ok(())
}

/// Undoes the frame of the function `ctx.rip` lies in, in the image at
/// `base` whose exception table is `pdata`: afterwards `ctx` is the
/// caller's state (rip the return address, rsp just above it). A `rip` in
/// no entry is a leaf function: rip = [rsp], rsp += 8, and `Ok(None)`.
pub fn virtual_unwind(
    base: u64,
    pdata: &[u8],
    ctx: &mut Context,
) -> Result<Option<Unwound>, UnwindError> {
    let function = u32::try_from(ctx.rip.wrapping_sub(base))
        .ok()
        .and_then(|rva| lookup(pdata, rva));
    let Some(function) = function else {
        pop_return(ctx)?;
        return Ok(None);
    };
    let primary = Info::read(base + u64::from(function.unwind_info))?;
    let offset_in_function = (ctx.rip - base) as u32 - function.begin;
    let establisher_frame = if primary.frame_register != 0 {
        primary.frame_base(ctx)
    } else {
        ctx.rsp
    };
    let in_body = offset_in_function >= u32::from(primary.prolog_size);
    if in_body {
        let (begin, end) = (
            base + u64::from(function.begin),
            base + u64::from(function.end),
        );
        if let Some(steps) = epilogue(ctx.rip, begin, end)? {
            for step in steps {
                match step {
                    Step::Add(size) => ctx.rsp = ctx.rsp.wrapping_add_signed(size),
                    Step::Lea(reg, disp) => {
                        ctx.rsp = ctx.register(reg).wrapping_add_signed(disp);
                    }
                    Step::Pop(reg) => {
                        *ctx.register_mut(reg) = read_u64(ctx.rsp)?;
                        ctx.rsp = ctx.rsp.wrapping_add(8);
                    }
                }
            }
            pop_return(ctx)?;
            return Ok(Some(Unwound {
                function,
                establisher_frame,
                handler: None,
            }));
        }
    }
    let ran = (!in_body).then_some(offset_in_function);
    let mut machine_frame = primary.apply(ctx, ran)?;
    let mut info = &primary;
    let mut parent;
    let mut links = 0;
    while info.flags & FLAG_CHAININFO != 0 {
        links += 1;
        if links > MAX_CHAIN {
            return Err(UnwindError::Malformed("chain"));
        }
        let mut entry = [0; 12];
        read_bytes(info.tail, &mut entry)?;
        let chained =
            RuntimeFunction::parse(&entry).ok_or(UnwindError::Malformed("chained entry"))?;
        parent = Info::read(base + u64::from(chained.unwind_info))?;
        machine_frame |= parent.apply(ctx, None)?;
        info = &parent;
    }
    // The handler is the primary's (the last info of the chain): a chained
    // fragment (a shrink-wrapped save, a cold part) has none of its own, and
    // Windows' `RtlVirtualUnwind` reports the function's. Whether the pc is
    // in a prologue or epilogue is the fragment's own business, above.
    let handler = if in_body && info.flags & (FLAG_EHANDLER | FLAG_UHANDLER) != 0 {
        Some(Handler {
            flags: info.flags & 3,
            address: base + u64::from(read_u32(info.tail)?),
            data: info.tail + 4,
        })
    } else {
        None
    };
    if !machine_frame {
        pop_return(ctx)?;
    }
    Ok(Some(Unwound {
        function,
        establisher_frame,
        handler,
    }))
}

/// The return addresses from `ctx` outward, `ctx.rip` first, at most `max`:
/// each frame's image is found with `crate::images::find` and its exception
/// table read (`Memory::read_into` of the table's bytes); the walk ends at
/// an address in no image, an unreadable stack, an rsp that does not grow,
/// or an error.
pub fn backtrace(mut ctx: Context, max: usize) -> Vec<u64> {
    let mut addresses = Vec::new();
    while addresses.len() < max {
        addresses.push(ctx.rip);
        let Some(image) = usize::try_from(ctx.rip).ok().and_then(crate::images::find) else {
            break;
        };
        let mut pdata = Vec::new();
        if let Some((at, len)) = image.exception_table {
            pdata.resize(len, 0);
            if !Host::read_into(at, &mut pdata) {
                break;
            }
        }
        let rsp = ctx.rsp;
        if virtual_unwind(image.base as u64, &pdata, &mut ctx).is_err() || ctx.rsp <= rsp {
            break;
        }
    }
    addresses
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 64 << 10;

    /// A block of ours standing in for an image: unwind info and code are
    /// written into it; given back when dropped.
    struct FakeImage {
        base: usize,
    }

    impl FakeImage {
        fn new() -> FakeImage {
            FakeImage {
                base: Host::allocate(BLOCK, None).expect("a block"),
            }
        }

        fn base(&self) -> u64 {
            self.base as u64
        }

        fn put(&self, rva: usize, bytes: &[u8]) {
            assert!(rva + bytes.len() <= BLOCK);
            // SAFETY: inside our writable block, checked above.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    (self.base + rva) as *mut u8,
                    bytes.len(),
                )
            };
        }
    }

    impl Drop for FakeImage {
        fn drop(&mut self) {
            // SAFETY: our whole allocation, which nothing uses any more.
            unsafe { Host::free(self.base, BLOCK) };
        }
    }

    fn code(offset: u8, op: u8, info: u8) -> u16 {
        u16::from(offset) | (u16::from(op) << 8) | (u16::from(info) << 12)
    }

    fn unwind_info(
        version: u8,
        flags: u8,
        prolog: u8,
        frame_reg: u8,
        frame_off: u8,
        codes: &[u16],
        tail: &[u8],
    ) -> Vec<u8> {
        let mut bytes = vec![
            version | (flags << 3),
            prolog,
            codes.len() as u8,
            frame_reg | (frame_off << 4),
        ];
        for c in codes {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        if codes.len() % 2 == 1 {
            bytes.extend_from_slice(&[0, 0]);
        }
        bytes.extend_from_slice(tail);
        bytes
    }

    fn entry(begin: u32, end: u32, unwind_info: u32) -> Vec<u8> {
        [begin, end, unwind_info]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect()
    }

    fn address(stack: &[u64], slot: usize) -> u64 {
        stack[slot..].as_ptr() as u64
    }

    /// The prologue `push rbp; push rbx; sub rsp, 0x40` at 0x1000..0x1100,
    /// its info at 0x200; the stack from rsp up: locals, rbx, rbp, return.
    fn pushes_and_alloc(image: &FakeImage, flags: u8, tail: &[u8]) -> (Vec<u8>, Vec<u64>) {
        let codes = [code(6, 2, 7), code(2, 0, 3), code(1, 0, 5)];
        image.put(0x200, &unwind_info(1, flags, 6, 0, 0, &codes, tail));
        let mut stack = vec![0; 8];
        stack.extend([0x1111, 0x2222, 0x4000_1234]);
        (entry(0x1000, 0x1100, 0x200), stack)
    }

    #[test]
    fn a_frame_of_pushes_and_an_alloc_is_undone() {
        let image = FakeImage::new();
        let (pdata, stack) = pushes_and_alloc(&image, 0, &[]);
        let start = address(&stack, 0);
        let mut ctx = Context {
            rip: image.base() + 0x1000 + 10,
            rsp: start,
            ..Default::default()
        };
        let unwound = virtual_unwind(image.base(), &pdata, &mut ctx)
            .unwrap()
            .unwrap();
        assert_eq!((ctx.rbx, ctx.rbp), (0x1111, 0x2222));
        assert_eq!(ctx.rsp, start + 0x40 + 24);
        assert_eq!(ctx.rip, 0x4000_1234);
        assert_eq!(unwound.establisher_frame, start);
        assert_eq!(unwound.function.begin, 0x1000);
        assert_eq!(unwound.handler, None);
    }

    #[test]
    fn in_the_prologue_only_what_ran_is_undone() {
        let image = FakeImage::new();
        let (pdata, _) = pushes_and_alloc(&image, 0, &[]);
        // After `push rbp` only: the saved rbp, then the return address.
        let stack = [0x2222u64, 0x4000_1234];
        let start = address(&stack, 0);
        let mut ctx = Context {
            rip: image.base() + 0x1000 + 1,
            rsp: start,
            rbx: 0x77,
            ..Default::default()
        };
        virtual_unwind(image.base(), &pdata, &mut ctx)
            .unwrap()
            .unwrap();
        assert_eq!(ctx.rbp, 0x2222);
        assert_eq!(ctx.rbx, 0x77, "push rbx had not run");
        assert_eq!(ctx.rsp, start + 16);
        assert_eq!(ctx.rip, 0x4000_1234);
    }

    #[test]
    fn a_frame_register_gives_rsp_back() {
        let image = FakeImage::new();
        // push rbp (offset 1); mov rbp, rsp (offset 4).
        let codes = [code(4, 3, 0), code(1, 0, 5)];
        image.put(0x200, &unwind_info(1, 0, 4, 5, 0, &codes, &[]));
        let pdata = entry(0x1000, 0x1100, 0x200);
        // Further pushes and allocas below the frame, then the saved rbp
        // and the return address.
        let stack = [0u64, 0, 0, 0, 0x3333, 0x4000_5678];
        let frame = address(&stack, 4);
        let mut ctx = Context {
            rip: image.base() + 0x1000 + 0x20,
            rsp: address(&stack, 0),
            rbp: frame,
            ..Default::default()
        };
        let unwound = virtual_unwind(image.base(), &pdata, &mut ctx)
            .unwrap()
            .unwrap();
        assert_eq!(unwound.establisher_frame, frame);
        assert_eq!(ctx.rbp, 0x3333);
        assert_eq!(ctx.rip, 0x4000_5678);
        assert_eq!(ctx.rsp, frame + 16);
    }

    #[test]
    fn saved_registers_and_xmm_come_from_their_slots() {
        let image = FakeImage::new();
        // sub rsp, 0x38 (offset 4); mov [rsp+0x10], rsi (offset 9);
        // movaps [rsp+0x20], xmm6 (offset 14).
        let codes = [code(14, 8, 6), 2, code(9, 4, 6), 2, code(4, 2, 6)];
        image.put(0x200, &unwind_info(1, 0, 14, 0, 0, &codes, &[]));
        let pdata = entry(0x1000, 0x1100, 0x200);
        let stack = [0u64, 0, 0x5151, 0, 0x6666, 0x6767, 0, 0x4000_9abc];
        let start = address(&stack, 0);
        let mut ctx = Context {
            rip: image.base() + 0x1000 + 0x30,
            rsp: start,
            ..Default::default()
        };
        virtual_unwind(image.base(), &pdata, &mut ctx)
            .unwrap()
            .unwrap();
        assert_eq!(ctx.rsi, 0x5151);
        assert_eq!(ctx.xmm[6], [0x6666, 0x6767]);
        assert_eq!(ctx.rsp, start + 0x38 + 8);
        assert_eq!(ctx.rip, 0x4000_9abc);
    }

    #[test]
    fn an_epilogue_is_run_forward() {
        let image = FakeImage::new();
        let (pdata, stack) = pushes_and_alloc(&image, FLAG_EHANDLER, &0x3000u32.to_le_bytes());
        // add rsp, 0x40; pop rbx; pop rbp; ret.
        image.put(0x1050, &[0x48, 0x83, 0xc4, 0x40, 0x5b, 0x5d, 0xc3]);
        let start = address(&stack, 0);
        let mut ctx = Context {
            rip: image.base() + 0x1050,
            rsp: start,
            ..Default::default()
        };
        let unwound = virtual_unwind(image.base(), &pdata, &mut ctx)
            .unwrap()
            .unwrap();
        assert_eq!((ctx.rbx, ctx.rbp), (0x1111, 0x2222));
        assert_eq!(ctx.rsp, start + 0x40 + 24);
        assert_eq!(ctx.rip, 0x4000_1234);
        assert_eq!(unwound.handler, None, "no handler in an epilogue");
    }

    #[test]
    fn chained_info_adds_the_parents_codes() {
        let image = FakeImage::new();
        // The parent: sub rsp, 0x20, with a handler.
        let parent = unwind_info(
            1,
            FLAG_EHANDLER,
            4,
            0,
            0,
            &[code(4, 2, 3)],
            &[0, 0x30, 0, 0],
        );
        image.put(0x300, &parent);
        // The child, a later part of the same function: push rbx, chained to
        // the parent's entry.
        let chained = entry(0x1000, 0x1100, 0x300);
        let child = unwind_info(1, FLAG_CHAININFO, 2, 0, 0, &[code(2, 0, 3)], &chained);
        image.put(0x200, &child);
        let mut pdata = chained.clone();
        pdata.extend(entry(0x1100, 0x1200, 0x200));
        let stack = [0x1111u64, 0, 0, 0, 0, 0x4000_4321];
        let start = address(&stack, 0);
        let mut ctx = Context {
            rip: image.base() + 0x1100 + 5,
            rsp: start,
            ..Default::default()
        };
        let unwound = virtual_unwind(image.base(), &pdata, &mut ctx)
            .unwrap()
            .unwrap();
        assert_eq!(unwound.function.begin, 0x1100);
        assert_eq!(ctx.rbx, 0x1111);
        assert_eq!(ctx.rsp, start + 8 + 0x20 + 8);
        assert_eq!(ctx.rip, 0x4000_4321);
        // A chained entry has no handler of its own: the parent's (the
        // function's) is reported, as Windows reports it.
        assert_eq!(
            unwound.handler.map(|h| (h.flags, h.address)),
            Some((FLAG_EHANDLER, image.base() + 0x3000))
        );
    }

    #[test]
    fn a_machine_frame_sets_rip_and_rsp() {
        for error_code in [0u8, 1] {
            let image = FakeImage::new();
            image.put(
                0x200,
                &unwind_info(1, 0, 0, 0, 0, &[code(0, 10, error_code)], &[]),
            );
            let pdata = entry(0x1000, 0x1100, 0x200);
            let mut stack = vec![0xeeu64; usize::from(error_code)];
            stack.extend([0x4000_aaaa, 0x33, 0x246, 0x7000_0000, 0x2b]);
            let mut ctx = Context {
                rip: image.base() + 0x1000 + 8,
                rsp: address(&stack, 0),
                ..Default::default()
            };
            virtual_unwind(image.base(), &pdata, &mut ctx)
                .unwrap()
                .unwrap();
            assert_eq!(ctx.rip, 0x4000_aaaa, "error code {error_code}");
            assert_eq!(ctx.rflags, 0x246);
            assert_eq!(ctx.rsp, 0x7000_0000, "no return address popped");
        }
    }

    #[test]
    fn lookup_finds_the_function_or_nothing() {
        let mut pdata = entry(0x1000, 0x1010, 0x200);
        pdata.extend(entry(0x1020, 0x1080, 0x210));
        pdata.extend(entry(0x1100, 0x1104, 0x220));
        pdata.push(0xff);
        assert_eq!(functions(&pdata).len(), 3, "the partial entry is ignored");
        let begin = |rva| lookup(&pdata, rva).map(|f| f.begin);
        assert_eq!(begin(0x1000), Some(0x1000));
        assert_eq!(begin(0x100f), Some(0x1000));
        assert_eq!(begin(0x1020), Some(0x1020));
        assert_eq!(begin(0x107f), Some(0x1020));
        assert_eq!(begin(0x1103), Some(0x1100));
        assert_eq!(begin(0xfff), None);
        assert_eq!(begin(0x1010), None);
        assert_eq!(begin(0x10ff), None);
        assert_eq!(begin(0x1104), None);
        assert_eq!(begin(0xffff_ffff), None);
        assert_eq!(lookup(&[], 0x1000), None);
    }

    #[test]
    fn a_leaf_pops_the_return_address() {
        let image = FakeImage::new();
        let pdata = entry(0x1000, 0x1100, 0x200);
        let stack = [0x4000_beefu64];
        let start = address(&stack, 0);
        let mut ctx = Context {
            rip: image.base() + 0x2000,
            rsp: start,
            ..Default::default()
        };
        assert_eq!(
            virtual_unwind(image.base(), &pdata, &mut ctx).unwrap(),
            None
        );
        assert_eq!(ctx.rip, 0x4000_beef);
        assert_eq!(ctx.rsp, start + 8);
    }

    #[test]
    fn handler_and_its_data_are_located() {
        let image = FakeImage::new();
        let mut tail = 0x3000u32.to_le_bytes().to_vec();
        tail.extend([1, 2, 3, 4, 5, 6, 7, 8]);
        // One code, padded to two: the handler RVA at 0x208.
        let info = unwind_info(1, FLAG_EHANDLER, 1, 0, 0, &[code(1, 0, 5)], &tail);
        image.put(0x200, &info);
        let pdata = entry(0x1000, 0x1100, 0x200);
        let stack = [0x2222u64, 0x4000_1234];
        let mut ctx = Context {
            rip: image.base() + 0x1010,
            rsp: address(&stack, 0),
            ..Default::default()
        };
        let unwound = virtual_unwind(image.base(), &pdata, &mut ctx)
            .unwrap()
            .unwrap();
        let handler = unwound.handler.unwrap();
        assert_eq!(handler.flags, FLAG_EHANDLER);
        assert_eq!(handler.address, image.base() + 0x3000);
        assert_eq!(handler.data, image.base() + 0x20c);
        assert_eq!(read_u64(handler.data).unwrap(), 0x0807_0605_0403_0201);
    }

    #[test]
    fn a_bad_pointer_is_an_error() {
        let image = FakeImage::new();
        let pdata = entry(0x1000, 0x1100, 0x200);
        let mut ctx = Context {
            rip: image.base() + 0x2000,
            rsp: 8,
            ..Default::default()
        };
        assert!(matches!(
            virtual_unwind(image.base(), &pdata, &mut ctx),
            Err(UnwindError::Unreadable(8))
        ));
    }
}
