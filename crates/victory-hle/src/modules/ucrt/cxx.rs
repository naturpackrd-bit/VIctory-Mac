//! C++ exception handling (`vcruntime140.dll`): `_CxxThrowException` and
//! the language handlers MSVC names in the unwind info of every function
//! with C++ EH state, `__CxxFrameHandler3` (the 2015 tables, `FuncInfo`)
//! and `__CxxFrameHandler4` (the compressed tables MSVC emits by default
//! since 2019, `FuncInfo4`), over `victory_loader::seh`'s dispatch and
//! unwind.
//!
//! A throw raises the C++ record (`0xE06D7363`, the thrown object and its
//! `ThrowInfo`) from the title's context. In the search, a frame's handler
//! finds the `catch` around the frame's state that takes one of the
//! object's catchable types, builds the catch object in the frame, and
//! unwinds to the frame: the frames on the way run their destructors (their
//! unwind actions), and the catching frame its own down to the `try`. The
//! unwind resumes the frame in [`catch_trampoline`], which runs the catch
//! funclet on the stack below the dead frames (where the thrown object
//! still lives, as Windows' unwind consolidation does), destroys the
//! object, and goes on at the continuation the funclet names, with the
//! frame's registers as the unwind left them.
//!
//! None of vcruntime's own trampolines (`__CxxCallCatchBlock`,
//! `_CallSettingFrame`, `_UnwindNestedFrames`) is reproduced: the contract
//! is the tables, the record, the funclet conventions and the continuation.
//!
//! Limits:
//! - An exception cannot leave a catch funclet (nor a destructor run by an
//!   unwind): our frames, which have no unwind data, lie between the
//!   funclet and its function, so the search cannot pass them. A throw (or
//!   `throw;`) that a frame inside the funclet catches works; one that would
//!   leave it ends the process with a message (`std::terminate`).
//! - The se translator (`_set_se_translator`) is kept per thread, never
//!   called: a foreign exception is not translated into a C++ one.
//! - The FH4 decoder follows `ehdata4_export.h` as we know it; it is tested
//!   on tables we encoded, not yet on a compiler's output.
//! - A `noexcept` function a C++ exception would leave ends the process when
//!   the unwind reaches it, not in the search.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of};
use std::arch::naked_asm;
use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::ptr;
use std::sync::Once;

use victory_loader::seh::{
    self, Context, DispatcherContext, EXCEPTION_CONTINUE_SEARCH_DISPOSITION,
    EXCEPTION_NONCONTINUABLE, EXCEPTION_TARGET_UNWIND, EXCEPTION_UNWIND, ExceptionPointers,
    ExceptionRecord, RtlCaptureContext,
};

use crate::{HleExport, hle_export};

/// The code of a C++ exception (`EH_EXCEPTION_NUMBER`, 'msc').
pub const EH_EXCEPTION_NUMBER: u32 = 0xE06D_7363;
/// The magic a throw writes in the record's first parameter; the two after
/// it are accepted when reading.
pub const EH_MAGIC_NUMBER1: u32 = 0x1993_0520;
pub const EH_MAGIC_NUMBER2: u32 = 0x1993_0521;
pub const EH_MAGIC_NUMBER3: u32 = 0x1993_0522;

/// `ThrowInfo::attributes`.
pub const TI_IS_CONST: u32 = 1;
pub const TI_IS_VOLATILE: u32 = 2;
pub const TI_IS_UNALIGNED: u32 = 4;
pub const TI_IS_PURE: u32 = 8;
pub const TI_IS_WINRT: u32 = 0x10;

/// `CatchableType::properties`.
pub const CT_IS_SIMPLE_TYPE: u32 = 1;
pub const CT_BY_REFERENCE_ONLY: u32 = 2;
pub const CT_HAS_VIRTUAL_BASE: u32 = 4;
pub const CT_IS_WINRT_HANDLE: u32 = 8;
pub const CT_IS_STD_BAD_ALLOC: u32 = 0x10;

/// `HandlerType::adjectives`.
pub const HT_IS_CONST: u32 = 1;
pub const HT_IS_VOLATILE: u32 = 2;
pub const HT_IS_UNALIGNED: u32 = 4;
pub const HT_IS_REFERENCE: u32 = 8;
pub const HT_IS_RESUMABLE: u32 = 0x10;
pub const HT_IS_STD_DOT_DOT: u32 = 0x40;
pub const HT_IS_BAD_ALLOC_COMPAT: u32 = 0x80;
pub const HT_IS_COMPLUS_EH: u32 = 0x8000_0000;

/// `FuncInfo::eh_flags` (magic `0x19930522` and later).
pub const FI_EHS_FLAG: i32 = 1;
pub const FI_DYNSTKALIGN_FLAG: i32 = 2;
pub const FI_EHNOEXCEPT_FLAG: i32 = 4;

/// What the prologue stores in the unwind-help slot: the state is the ip's.
const UNWIND_HELP_UNSET: i32 = -2;

/// `_ThrowInfo`: what a throw names; RVAs from the record's image base.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ThrowInfo {
    pub attributes: u32,
    /// The destructor, 0 for none.
    pub pmfn_unwind: i32,
    pub p_forward_compat: i32,
    pub p_catchable_type_array: i32,
}

/// `PMD`: how to get from the thrown object to a base of it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pmd {
    pub mdisp: i32,
    /// The vbtable pointer's offset, or -1 when the base is not virtual.
    pub pdisp: i32,
    pub vdisp: i32,
}

/// `CatchableType`: one type a thrown object can be caught as.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CatchableType {
    pub properties: u32,
    /// The `TypeDescriptor`.
    pub p_type: i32,
    pub this_displacement: Pmd,
    pub size_or_offset: i32,
    /// The copy constructor, 0 for none.
    pub copy_function: i32,
}

/// The fixed part of a `TypeDescriptor` (`type_info`); the decorated name
/// (`.H`, `.?AVfoo@@`) follows, NUL-terminated.
#[repr(C)]
pub struct TypeDescriptor {
    pub p_vftable: *const c_void,
    pub spare: *const c_void,
    pub name: [u8; 0],
}

/// The FH3 `FuncInfo` (x64).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FuncInfo3 {
    magic_number: u32,
    max_state: i32,
    disp_unwind_map: i32,
    n_try_blocks: u32,
    disp_try_block_map: i32,
    n_ip_map_entries: u32,
    disp_ip_to_state_map: i32,
    disp_unwind_help: i32,
    disp_es_type_list: i32,
    eh_flags: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UnwindMapEntry3 {
    to_state: i32,
    action: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct TryBlockMapEntry3 {
    try_low: i32,
    try_high: i32,
    catch_high: i32,
    n_catches: i32,
    disp_handler_array: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct HandlerType3 {
    adjectives: u32,
    disp_type: i32,
    disp_catch_obj: i32,
    disp_of_handler: i32,
    disp_frame: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IpToStateMapEntry3 {
    ip: i32,
    state: i32,
}

const _: () = {
    assert!(size_of::<ThrowInfo>() == 16);
    assert!(size_of::<CatchableType>() == 28);
    assert!(offset_of!(TypeDescriptor, name) == 16);
    assert!(size_of::<FuncInfo3>() == 40);
    assert!(size_of::<TryBlockMapEntry3>() == 20);
    assert!(size_of::<HandlerType3>() == 20);
};

// ---------------------------------------------------------------------------
// The tables, decoded.

/// What an unwind state's entry does when the state is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    /// A funclet (`rdx` the frame): FH3's actions, FH4's `RVA`.
    Funclet(u32),
    /// FH4 `DtorWithObj`: a destructor of the object at the frame offset.
    Dtor {
        rva: u32,
        object: i32,
    },
    /// FH4 `DtorWithPtrToObj`: of the object a pointer at the offset names.
    DtorPtr {
        rva: u32,
        object: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnwindEntry {
    pub to_state: i32,
    pub action: Action,
}

/// A `catch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handler {
    pub adjectives: u32,
    /// The `TypeDescriptor`'s RVA, 0 for `catch(...)`.
    pub type_rva: u32,
    /// The catch object's frame offset, 0 for none.
    pub catch_obj: i32,
    /// The catch funclet's RVA.
    pub handler_rva: u32,
    /// FH3: where a catch funclet of this handler keeps its function's frame
    /// (in its own frame).
    pub disp_frame: i32,
    /// FH4: the continuation addresses, as RVAs; the funclet returns an
    /// index into them.
    pub continuations: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TryBlock {
    pub low: i32,
    pub high: i32,
    pub catch_high: i32,
    pub handlers: Vec<Handler>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// The unwind-help slot's frame offset (0: none).
    Fh3 { unwind_help: i32 },
    /// For a catch funclet's `FuncInfo4`: where it keeps its function's
    /// frame (`dispFrame`).
    Fh4 { catch_frame: Option<u32> },
}

/// A function's EH tables, either format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FuncInfo {
    pub format: Format,
    /// Compiled with `/EHs`: foreign exceptions are not caught.
    pub ehs: bool,
    pub noexcept: bool,
    /// Indexed by state.
    pub unwind: Vec<UnwindEntry>,
    /// Inner blocks first.
    pub try_blocks: Vec<TryBlock>,
    /// (RVA, state) by ascending RVA: the state from that address on.
    pub ip_map: Vec<(u32, i32)>,
}

impl FuncInfo {
    /// The state at the RVA `rva`: that of the last map entry at or before
    /// it, -1 before the first.
    pub fn state_at(&self, rva: u32) -> i32 {
        match self.ip_map.partition_point(|&(ip, _)| ip <= rva) {
            0 => -1,
            n => self.ip_map[n - 1].1,
        }
    }

    /// The frame the tables' offsets are from: `frame` itself, or for a
    /// catch funclet's frame its function's (vcruntime's
    /// `GetEstablisherFrame`: FH3 finds the funclet among the handlers of
    /// the try blocks whose catch states hold `state`, by its start
    /// `function_begin`, and reads the frame at its `disp_frame`; FH4 reads
    /// it at the `FuncInfo4`'s `dispFrame`).
    ///
    /// # Safety
    /// `frame` is the establisher frame of the function the tables are for.
    unsafe fn establisher(&self, frame: u64, state: i32, function_begin: u32) -> u64 {
        match self.format {
            // SAFETY: the funclet's frame, which keeps its function's there.
            Format::Fh4 {
                catch_frame: Some(at),
            } => unsafe { read(frame.wrapping_add(u64::from(at))) },
            Format::Fh4 { catch_frame: None } => frame,
            Format::Fh3 { .. } => {
                for block in self.try_blocks.iter().rev() {
                    if state <= block.high || state > block.catch_high {
                        continue;
                    }
                    if let Some(h) = block
                        .handlers
                        .iter()
                        .find(|h| h.handler_rva == function_begin)
                    {
                        // SAFETY: as above.
                        return unsafe { read(frame.wrapping_add_signed(i64::from(h.disp_frame))) };
                    }
                }
                frame
            }
        }
    }

    /// The unwind-help slot's address in the frame `establisher` (FH3).
    fn unwind_help(&self, establisher: u64) -> Option<u64> {
        match self.format {
            Format::Fh3 { unwind_help } if unwind_help != 0 => {
                Some(establisher.wrapping_add_signed(i64::from(unwind_help)))
            }
            _ => None,
        }
    }
}

/// Reads a `T` at `at`, unaligned.
///
/// # Safety
/// `at` holds a `T`.
unsafe fn read<T: Copy>(at: u64) -> T {
    // SAFETY: the caller's.
    unsafe { (at as *const T).read_unaligned() }
}

/// Writes a `T` at `at`, unaligned.
///
/// # Safety
/// `at` is writable for a `T`.
unsafe fn write<T>(at: u64, value: T) {
    // SAFETY: the caller's.
    unsafe { (at as *mut T).write_unaligned(value) }
}

/// `at` plus the RVA `rva` read as unsigned.
fn rva(base: u64, rva: i32) -> u64 {
    base.wrapping_add(u64::from(rva as u32))
}

/// Decodes the FH3 `FuncInfo` at `at` in the image at `base`; None when its
/// magic is not one of the three.
///
/// # Safety
/// `at` is a `FuncInfo` of the image at `base`, its tables mapped.
unsafe fn parse_fh3(base: u64, at: u64) -> Option<FuncInfo> {
    // SAFETY: the caller's tables, here and below.
    let fi: FuncInfo3 = unsafe { read(at) };
    if !(EH_MAGIC_NUMBER1..=EH_MAGIC_NUMBER3).contains(&fi.magic_number) {
        return None;
    }
    let flags = if fi.magic_number >= EH_MAGIC_NUMBER3 {
        fi.eh_flags
    } else {
        0
    };
    let unwind = (0..fi.max_state.max(0) as u64)
        .map(|i| {
            // SAFETY: as above.
            let e: UnwindMapEntry3 = unsafe { read(rva(base, fi.disp_unwind_map) + 8 * i) };
            UnwindEntry {
                to_state: e.to_state,
                action: match e.action {
                    0 => Action::None,
                    a => Action::Funclet(a as u32),
                },
            }
        })
        .collect();
    let try_blocks = (0..u64::from(fi.n_try_blocks))
        .map(|i| {
            // SAFETY: as above.
            let t: TryBlockMapEntry3 = unsafe { read(rva(base, fi.disp_try_block_map) + 20 * i) };
            let handlers = (0..t.n_catches.max(0) as u64)
                .map(|j| {
                    // SAFETY: as above.
                    let h: HandlerType3 = unsafe { read(rva(base, t.disp_handler_array) + 20 * j) };
                    Handler {
                        adjectives: h.adjectives,
                        type_rva: h.disp_type as u32,
                        catch_obj: h.disp_catch_obj,
                        handler_rva: h.disp_of_handler as u32,
                        disp_frame: h.disp_frame,
                        continuations: Vec::new(),
                    }
                })
                .collect();
            TryBlock {
                low: t.try_low,
                high: t.try_high,
                catch_high: t.catch_high,
                handlers,
            }
        })
        .collect();
    let ip_map = (0..u64::from(fi.n_ip_map_entries))
        .map(|i| {
            // SAFETY: as above.
            let e: IpToStateMapEntry3 = unsafe { read(rva(base, fi.disp_ip_to_state_map) + 8 * i) };
            (e.ip as u32, e.state)
        })
        .collect();
    Some(FuncInfo {
        format: Format::Fh3 {
            unwind_help: fi.disp_unwind_help,
        },
        ehs: flags & FI_EHS_FLAG != 0,
        noexcept: flags & FI_EHNOEXCEPT_FLAG != 0,
        unwind,
        try_blocks,
        ip_map,
    })
}

/// FH4's compressed unsigned integer at the start of `bytes`: its value and
/// length (`ehdata4_export.h`'s `ReadUnsigned`). The low bits of the first
/// byte give the length: `x0` one byte (value `>> 1`), `01` two (`>> 2`),
/// `011` three (`>> 3`), `0111` four (`>> 4`), `1111` five (the next four
/// bytes are the value); little-endian. None when `bytes` is too short.
pub(crate) fn decode_unsigned(bytes: &[u8]) -> Option<(u32, usize)> {
    let first = *bytes.first()?;
    let len = encoded_len(first);
    let b = bytes.get(..len)?;
    let le = |b: &[u8]| b.iter().rev().fold(0u32, |v, &x| v << 8 | u32::from(x));
    let value = match len {
        1 => u32::from(first) >> 1,
        2 => le(b) >> 2,
        3 => le(b) >> 3,
        4 => le(b) >> 4,
        _ => le(&b[1..]),
    };
    Some((value, len))
}

/// The length of the compressed integer whose first byte is `first`.
fn encoded_len(first: u8) -> usize {
    match first & 0xF {
        b if b & 1 == 0 => 1,
        b if b & 3 == 1 => 2,
        b if b & 7 == 3 => 3,
        7 => 4,
        _ => 5,
    }
}

/// A cursor over FH4's byte stream in mapped memory.
struct Stream {
    at: u64,
}

impl Stream {
    /// # Safety
    /// For each read: the stream holds what is read.
    unsafe fn byte(&mut self) -> u8 {
        // SAFETY: the caller's.
        let b = unsafe { read::<u8>(self.at) };
        self.at += 1;
        b
    }

    /// `ReadUnsigned`.
    ///
    /// # Safety
    /// As [`Stream::byte`].
    unsafe fn unsigned(&mut self) -> u32 {
        // SAFETY: the caller's: the first byte, then as many as it says.
        let first = unsafe { read::<u8>(self.at) };
        let len = encoded_len(first);
        let mut bytes = [0u8; 5];
        for (i, b) in bytes[..len].iter_mut().enumerate() {
            // SAFETY: as above.
            *b = unsafe { read(self.at + i as u64) };
        }
        self.at += len as u64;
        decode_unsigned(&bytes[..len]).map_or(0, |(v, _)| v)
    }

    /// `ReadInt`: four bytes as they are (RVAs), not compressed.
    ///
    /// # Safety
    /// As [`Stream::byte`].
    unsafe fn int(&mut self) -> i32 {
        // SAFETY: the caller's.
        let v = unsafe { read::<i32>(self.at) };
        self.at += 4;
        v
    }
}

/// FuncInfo4 header bits.
const FI4_IS_CATCH: u8 = 1;
const FI4_IS_SEPARATED: u8 = 2;
const FI4_BBT: u8 = 4;
const FI4_UNWIND_MAP: u8 = 8;
const FI4_TRY_BLOCK_MAP: u8 = 0x10;
const FI4_EHS: u8 = 0x20;
const FI4_NO_EXCEPT: u8 = 0x40;

/// HandlerType4 header bits.
const HT4_ADJECTIVES: u8 = 1;
const HT4_DISP_TYPE: u8 = 2;
const HT4_DISP_CATCH_OBJ: u8 = 4;
const HT4_CONT_IS_RVA: u8 = 8;
const HT4_CONT_COUNT_SHIFT: u8 = 4;

/// Decodes the FH4 `FuncInfo4` at `at` in the image at `base`, for the
/// function (or segment) starting at the RVA `function_start`, as
/// `ehdata4_export.h`'s `DecompFuncInfo`, `UWMap4`, `TryBlockMap4`,
/// `HandlerMap4` and `IPtoStateMap4` read it.
///
/// # Safety
/// `at` is a `FuncInfo4` of the image at `base`, its tables mapped.
pub(crate) unsafe fn parse_fh4(base: u64, at: u64, function_start: u32) -> FuncInfo {
    let mut s = Stream { at };
    // SAFETY: the caller's tables, here and below.
    let header = unsafe { s.byte() };
    if header & FI4_BBT != 0 {
        // SAFETY: as above.
        let _bbt_flags = unsafe { s.unsigned() };
    }
    // SAFETY: as above.
    let unwind_map = (header & FI4_UNWIND_MAP != 0).then(|| unsafe { s.int() });
    // SAFETY: as above.
    let try_map = (header & FI4_TRY_BLOCK_MAP != 0).then(|| unsafe { s.int() });
    // SAFETY: as above.
    let ip_rva = unsafe { s.int() };
    let ip_map_at = if header & FI4_IS_SEPARATED != 0 {
        let mut seg = Stream {
            at: rva(base, ip_rva),
        };
        // SAFETY: as above.
        let count = unsafe { seg.unsigned() };
        let mut found = 0;
        for _ in 0..count {
            // SAFETY: as above.
            let (start, map) = unsafe { (seg.int(), seg.int()) };
            if start as u32 == function_start {
                found = map;
                break;
            }
        }
        found
    } else {
        ip_rva
    };
    // SAFETY: as above.
    let catch_frame = (header & FI4_IS_CATCH != 0).then(|| unsafe { s.unsigned() });

    let mut unwind = Vec::new();
    if let Some(map) = unwind_map.filter(|&m| m != 0) {
        let mut u = Stream { at: rva(base, map) };
        // SAFETY: as above.
        let count = unsafe { u.unsigned() };
        let mut starts: Vec<u64> = Vec::new();
        for _ in 0..count {
            let start = u.at;
            // SAFETY: as above.
            let next = unsafe { u.unsigned() };
            let distance = u64::from(next >> 2);
            let to_state = if distance == 0 {
                -1
            } else {
                starts
                    .iter()
                    .position(|&p| p == start.wrapping_sub(distance))
                    .map_or(-1, |i| i as i32)
            };
            let action = match next & 3 {
                0 => Action::None,
                kind => {
                    // SAFETY: as above.
                    let rva = unsafe { u.int() } as u32;
                    match kind {
                        // SAFETY: as above.
                        1 => Action::Dtor {
                            rva,
                            object: unsafe { u.unsigned() } as i32,
                        },
                        // SAFETY: as above.
                        2 => Action::DtorPtr {
                            rva,
                            object: unsafe { u.unsigned() } as i32,
                        },
                        _ => Action::Funclet(rva),
                    }
                }
            };
            starts.push(start);
            unwind.push(UnwindEntry { to_state, action });
        }
    }

    let mut try_blocks = Vec::new();
    if let Some(map) = try_map.filter(|&m| m != 0) {
        let mut t = Stream { at: rva(base, map) };
        // SAFETY: as above.
        let count = unsafe { t.unsigned() };
        for _ in 0..count {
            // SAFETY: as above.
            let (low, high, catch_high, array) =
                unsafe { (t.unsigned(), t.unsigned(), t.unsigned(), t.int()) };
            let mut h = Stream {
                at: rva(base, array),
            };
            // SAFETY: as above.
            let n = unsafe { h.unsigned() };
            let handlers = (0..n)
                // SAFETY: as above.
                .map(|_| unsafe { read_handler4(&mut h, function_start) })
                .collect();
            try_blocks.push(TryBlock {
                low: low as i32,
                high: high as i32,
                catch_high: catch_high as i32,
                handlers,
            });
        }
    }

    let mut ip_map = Vec::new();
    if ip_map_at != 0 {
        let mut m = Stream {
            at: rva(base, ip_map_at),
        };
        // SAFETY: as above.
        let count = unsafe { m.unsigned() };
        let mut ip = function_start;
        for _ in 0..count {
            // SAFETY: as above.
            let (delta, state) = unsafe { (m.unsigned(), m.unsigned()) };
            ip = ip.wrapping_add(delta);
            ip_map.push((ip, state as i32 - 1));
        }
    }

    FuncInfo {
        format: Format::Fh4 { catch_frame },
        ehs: header & FI4_EHS != 0,
        noexcept: header & FI4_NO_EXCEPT != 0,
        unwind,
        try_blocks,
        ip_map,
    }
}

/// One `HandlerType4` (`DecompHandler`): a header byte, the adjectives,
/// type and catch object when it says so, the funclet's RVA, then none, one
/// or two continuation addresses, RVAs or offsets from the function's start.
///
/// # Safety
/// As [`Stream::byte`].
unsafe fn read_handler4(s: &mut Stream, function_start: u32) -> Handler {
    // SAFETY: the caller's, here and below.
    let header = unsafe { s.byte() };
    // SAFETY: as above.
    let adjectives = (header & HT4_ADJECTIVES != 0).then(|| unsafe { s.unsigned() });
    // SAFETY: as above.
    let type_rva = (header & HT4_DISP_TYPE != 0).then(|| unsafe { s.int() });
    // SAFETY: as above.
    let catch_obj = (header & HT4_DISP_CATCH_OBJ != 0).then(|| unsafe { s.unsigned() });
    // SAFETY: as above.
    let handler_rva = unsafe { s.int() } as u32;
    let count = match (header >> HT4_CONT_COUNT_SHIFT) & 3 {
        1 => 1,
        2 => 2,
        _ => 0,
    };
    let continuations = (0..count)
        .map(|_| {
            if header & HT4_CONT_IS_RVA != 0 {
                // SAFETY: as above.
                unsafe { s.int() as u32 }
            } else {
                // SAFETY: as above.
                function_start.wrapping_add(unsafe { s.unsigned() })
            }
        })
        .collect();
    Handler {
        adjectives: adjectives.unwrap_or(0),
        type_rva: type_rva.unwrap_or(0) as u32,
        catch_obj: catch_obj.unwrap_or(0) as i32,
        handler_rva,
        disp_frame: 0,
        continuations,
    }
}

// ---------------------------------------------------------------------------
// Thrown objects and their types.

/// What a C++ record carries: the object, its `ThrowInfo`, and the base
/// the `ThrowInfo`'s RVAs are from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Thrown {
    object: u64,
    info: u64,
    base: u64,
}

/// Whether `r` is a C++ exception's record (rethrow records included).
fn is_cxx(r: &ExceptionRecord) -> bool {
    r.code == EH_EXCEPTION_NUMBER
        && r.number_parameters >= 3
        && (u64::from(EH_MAGIC_NUMBER1)..=u64::from(EH_MAGIC_NUMBER3)).contains(&r.information[0])
}

/// The thrown object of the C++ record `r`, as its parameters name it.
fn thrown_in(r: &ExceptionRecord) -> Option<Thrown> {
    (is_cxx(r) && r.information[2] != 0).then(|| Thrown {
        object: r.information[1],
        info: r.information[2],
        base: if r.number_parameters >= 4 {
            r.information[3]
        } else {
            0
        },
    })
}

/// The thrown object of `r`: its own, or for a rethrow record (null
/// parameters) the current exception's.
fn thrown_of(r: &ExceptionRecord) -> Option<Thrown> {
    if !is_cxx(r) {
        return None;
    }
    thrown_in(r).or_else(|| HANDLING.with(|h| h.borrow().last().and_then(|x| thrown_in(&x.record))))
}

impl Thrown {
    /// # Safety
    /// The record's `ThrowInfo` and its tables are mapped.
    unsafe fn throw_info(&self) -> ThrowInfo {
        // SAFETY: the caller's.
        unsafe { read(self.info) }
    }

    /// The catchable types, in their array's order.
    ///
    /// # Safety
    /// As [`Thrown::throw_info`].
    unsafe fn catchables(&self) -> Vec<CatchableType> {
        // SAFETY: the caller's, here and below.
        let array = rva(
            self.base,
            unsafe { self.throw_info() }.p_catchable_type_array,
        );
        // SAFETY: as above.
        let n: i32 = unsafe { read(array) };
        (0..n.max(0) as u64)
            // SAFETY: as above.
            .map(|i| unsafe { read(rva(self.base, read(array + 4 + 4 * i))) })
            .collect()
    }

    /// Runs the object's destructor, if it has one.
    ///
    /// # Safety
    /// As [`Thrown::throw_info`]; the object is alive.
    unsafe fn destroy(&self) {
        // SAFETY: the caller's.
        let unwind = unsafe { self.throw_info() }.pmfn_unwind;
        if unwind != 0 && self.object != 0 {
            // SAFETY: the title's destructor, given its object.
            unsafe {
                let dtor = std::mem::transmute::<u64, Destructor>(rva(self.base, unwind));
                dtor(self.object);
            }
        }
    }
}

/// The decorated name of the `TypeDescriptor` at `at`, without its NUL.
///
/// # Safety
/// `at` is a `TypeDescriptor`, its name NUL-terminated.
unsafe fn type_name<'a>(at: u64) -> &'a [u8] {
    // SAFETY: the caller's.
    unsafe { CStr::from_ptr((at + 16) as *const c_char) }.to_bytes()
}

/// Whether a handler is `catch(...)`: no type, a type with an empty name,
/// or `HT_IsStdDotDot`.
fn is_ellipsis(adjectives: u32, name: Option<&[u8]>) -> bool {
    adjectives & HT_IS_STD_DOT_DOT != 0 || name.is_none_or(<[u8]>::is_empty)
}

/// vcruntime's `TypeMatchHelper`: whether a handler of `adjectives` and
/// type `name` (None: no type) takes a catchable type `ct_name` of
/// `ct_properties`, of a throw of `ti_attributes`. `catch(...)` takes all;
/// `HT_IsBadAllocCompat` takes `CT_IsStdBadAlloc`; else the names must be
/// equal (or the handler `void*`, `.PEAX`, and the type a simple pointer),
/// a by-reference-only type caught by reference, and the throw's const,
/// volatile and unaligned qualifiers held by the handler.
pub(crate) fn type_matches(
    adjectives: u32,
    name: Option<&[u8]>,
    ct_properties: u32,
    ct_name: &[u8],
    ti_attributes: u32,
) -> bool {
    if is_ellipsis(adjectives, name) {
        return true;
    }
    if adjectives & HT_IS_BAD_ALLOC_COMPAT != 0 && ct_properties & CT_IS_STD_BAD_ALLOC != 0 {
        return true;
    }
    let name = name.unwrap_or_default();
    let void_pointer =
        name == b".PEAX" && ct_properties & CT_IS_SIMPLE_TYPE != 0 && ct_name.starts_with(b".P");
    if name != ct_name && !void_pointer {
        return false;
    }
    let held = |ti: u32, ht: u32| ti_attributes & ti == 0 || adjectives & ht != 0;
    (ct_properties & CT_BY_REFERENCE_ONLY == 0 || adjectives & HT_IS_REFERENCE != 0)
        && held(TI_IS_CONST, HT_IS_CONST)
        && held(TI_IS_UNALIGNED, HT_IS_UNALIGNED)
        && held(TI_IS_VOLATILE, HT_IS_VOLATILE)
}

/// vcruntime's `__AdjustPointer`: the address of the base `pmd` names in
/// the object at `object`: `mdisp` from it, and for a virtual base the
/// offset its vbtable (at `pdisp`) has at `vdisp`, from the vbtable pointer.
///
/// # Safety
/// For a virtual base, the object's vbtable pointer and entry are readable.
pub(crate) unsafe fn adjust_pointer(object: u64, pmd: Pmd) -> u64 {
    let mut at = object.wrapping_add_signed(i64::from(pmd.mdisp));
    if pmd.pdisp >= 0 {
        let vbptr = object.wrapping_add_signed(i64::from(pmd.pdisp));
        // SAFETY: the caller's.
        let offset: i32 =
            unsafe { read(read::<u64>(vbptr).wrapping_add_signed(i64::from(pmd.vdisp))) };
        at = at
            .wrapping_add_signed(i64::from(offset))
            .wrapping_add_signed(i64::from(pmd.pdisp));
    }
    at
}

/// The first of the thrown object's catchable types `h` takes (None:
/// none); `Some(None)` for `catch(...)`.
///
/// # Safety
/// As [`Thrown::throw_info`]; `h`'s type is in the image at `base`.
unsafe fn handler_takes(h: &Handler, base: u64, t: &Thrown) -> Option<Option<CatchableType>> {
    // SAFETY: the caller's descriptors, here and below.
    let name = (h.type_rva != 0).then(|| unsafe { type_name(base + u64::from(h.type_rva)) });
    if is_ellipsis(h.adjectives, name) {
        return Some(None);
    }
    // SAFETY: as above.
    let attributes = unsafe { t.throw_info() }.attributes;
    // SAFETY: as above.
    unsafe { t.catchables() }
        .into_iter()
        .find(|ct| {
            // SAFETY: as above.
            let ct_name = unsafe { type_name(rva(t.base, ct.p_type)) };
            type_matches(h.adjectives, name, ct.properties, ct_name, attributes)
        })
        .map(Some)
}

/// vcruntime's `BuildCatchObjectHelper`: the catch object of `h` in the
/// frame `establisher`, from the thrown object as `ct`: by reference, its
/// address (adjusted to the base); a simple type, its bytes (a pointer
/// adjusted); else the copy constructor, or its bytes without one.
///
/// # Safety
/// The frame and the object are alive; the tables are mapped.
unsafe fn build_catch_object(establisher: u64, h: &Handler, t: &Thrown, ct: &CatchableType) {
    if h.catch_obj == 0 {
        return;
    }
    let slot = establisher.wrapping_add_signed(i64::from(h.catch_obj));
    let size = ct.size_or_offset.max(0) as usize;
    // SAFETY: the caller's frame and object, the title's copy constructor.
    unsafe {
        if h.adjectives & HT_IS_REFERENCE != 0 {
            write(slot, adjust_pointer(t.object, ct.this_displacement));
        } else if ct.properties & CT_IS_SIMPLE_TYPE != 0 {
            ptr::copy(t.object as *const u8, slot as *mut u8, size);
            if size == 8 && read::<u64>(slot) != 0 {
                write(slot, adjust_pointer(read(slot), ct.this_displacement));
            }
        } else if ct.copy_function == 0 {
            let from = adjust_pointer(t.object, ct.this_displacement);
            ptr::copy(from as *const u8, slot as *mut u8, size);
        } else {
            let from = adjust_pointer(t.object, ct.this_displacement);
            let copy = rva(t.base, ct.copy_function);
            if ct.properties & CT_HAS_VIRTUAL_BASE != 0 {
                std::mem::transmute::<u64, CopyVirtual>(copy)(slot, from, 1);
            } else {
                std::mem::transmute::<u64, CopyConstructor>(copy)(slot, from);
            }
        }
    }
}

/// A funclet: (unused, the frame); a catch funclet returns the
/// continuation.
type Funclet = unsafe extern "win64" fn(u64, u64) -> u64;
/// A destructor: `this`.
type Destructor = unsafe extern "win64" fn(u64);
/// A copy constructor: (`this`, the source).
type CopyConstructor = unsafe extern "win64" fn(u64, u64) -> u64;
/// A copy constructor of a class with virtual bases: (`this`, the source,
/// 1: construct them).
type CopyVirtual = unsafe extern "win64" fn(u64, u64, i32) -> u64;

// ---------------------------------------------------------------------------
// Per-thread state.

/// An exception being handled (a catch running, or one registered by
/// `__CxxRegisterExceptionObject`).
struct Handling {
    record: ExceptionRecord,
    context: *mut Context,
    /// Rethrown (`throw;`) while it was the current exception.
    rethrown: bool,
}

/// A catch the unwind is going to: what [`catch_trampoline`] gets in rax.
#[repr(C)]
pub(crate) struct Catch {
    /// Where the funclet runs: below the frames of the dispatch, which hold
    /// the thrown object. 16-aligned.
    stack: u64,
    /// The catching frame (the unwind's target).
    frame: u64,
    /// The frame the funclet is given.
    establisher: u64,
    /// The state the catching frame is unwound to.
    try_low: i32,
    funclet: u64,
    continuations: [u64; 2],
    n_continuations: usize,
    /// The exception, its parameters resolved for a rethrow.
    record: ExceptionRecord,
    context: *mut Context,
    /// The catching frame's unwind-help slot (FH3), 0 for none.
    unwind_help: u64,
}

thread_local! {
    static CURRENT_EXCEPTION: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
    static CURRENT_EXCEPTION_CONTEXT: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
    /// Throws not yet caught (`__ProcessingThrow`), which is also the count
    /// `std::uncaught_exceptions` gives.
    static PROCESSING_THROW: Cell<i32> = const { Cell::new(0) };
    /// The exceptions being handled, the current one last; boxed, so that
    /// the current-exception slot's pointer to a record stays good.
    #[allow(clippy::vec_box)]
    static HANDLING: RefCell<Vec<Box<Handling>>> = const { RefCell::new(Vec::new()) };
    /// The catches being unwound to, the latest last.
    static PENDING: RefCell<Vec<*mut Catch>> = const { RefCell::new(Vec::new()) };
    /// Catch funclets running.
    static RUNNING: Cell<u32> = const { Cell::new(0) };
    static TRANSLATOR: Cell<usize> = const { Cell::new(0) };
}

/// Points the current-exception slots at the top of the stack.
fn sync_current() {
    HANDLING.with(|h| {
        let h = h.borrow();
        let (record, context) = h.last().map_or((ptr::null_mut(), ptr::null_mut()), |x| {
            (
                (&raw const x.record).cast_mut().cast(),
                x.context.cast::<c_void>(),
            )
        });
        CURRENT_EXCEPTION.with(|c| c.set(record));
        CURRENT_EXCEPTION_CONTEXT.with(|c| c.set(context));
    });
}

fn push_handling(record: ExceptionRecord, context: *mut Context) -> *const Handling {
    let handling = Box::new(Handling {
        record,
        context,
        rethrown: false,
    });
    let at: *const Handling = &*handling;
    HANDLING.with(|h| h.borrow_mut().push(handling));
    sync_current();
    at
}

/// Takes the handling `at` off the stack.
fn remove_handling(at: *const Handling) -> Option<Box<Handling>> {
    let removed = HANDLING.with(|h| {
        let mut h = h.borrow_mut();
        let i = h.iter().rposition(|x| ptr::eq(&**x, at))?;
        Some(h.remove(i))
    });
    sync_current();
    removed
}

/// Whether an exception on the thread's stack holds `object`.
fn held(object: u64) -> bool {
    HANDLING.with(|h| {
        h.borrow()
            .iter()
            .any(|x| thrown_in(&x.record).is_some_and(|t| t.object == object))
    })
}

/// `std::terminate`, after saying why.
fn terminate_because(why: &str) -> ! {
    tracing::error!("{why}: std::terminate");
    __std_terminate()
}

/// Warns once of tables we cannot read.
fn bad_tables(what: &str) {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| tracing::warn!("C++ EH tables not understood ({what}); frame skipped"));
}

// ---------------------------------------------------------------------------
// The throw.

/// The frame of the caller-context shim, as `seh.rs`'s: the four register
/// arguments below the saved rbp, a [`Context`] at `rsp + 32`, the shadow
/// space below it; a multiple of 16.
const SHIM_FRAME: usize = 0x510;
const _: () =
    assert!(32 + size_of::<Context>() + 32 <= SHIM_FRAME && SHIM_FRAME.is_multiple_of(16));

/// `_CxxThrowException`: throws `object` as `throw_info` describes it, from
/// the caller's context; both null: rethrows the current exception
/// (`throw;`).
///
/// # Safety
/// Called from title code, with a thrown object and its `ThrowInfo`.
#[unsafe(naked)]
pub unsafe extern "win64" fn _CxxThrowException(object: *mut c_void, throw_info: *const c_void) {
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
        "lea rdx, [rsp + 32]",
        "call {throw}",
        "ud2",
        frame = const SHIM_FRAME,
        capture = sym RtlCaptureContext,
        throw = sym cxx_throw,
    )
}

/// `_CxxThrowException`'s work, with the caller's context: the C++ record,
/// raised. Inside a catch funclet, an exception no frame inside it takes
/// ends the process (see the limits).
///
/// # Safety
/// As [`_CxxThrowException`]; `ctx` is the shim's captured context.
unsafe extern "sysv64" fn cxx_throw(
    object: *mut c_void,
    throw_info: *const c_void,
    ctx: *mut Context,
) -> ! {
    // SAFETY: the shim's context, alive in its frame, which never returns.
    let ctx = unsafe { &mut *ctx };
    let mut record = ExceptionRecord::new(EH_EXCEPTION_NUMBER, EXCEPTION_NONCONTINUABLE, ctx.rip);
    record.number_parameters = 4;
    if throw_info.is_null() {
        let current = HANDLING.with(|h| {
            h.borrow_mut().last_mut().map(|x| {
                x.rethrown = true;
                x.record
            })
        });
        match current {
            Some(r) if is_cxx(&r) => record.information[..4].copy_from_slice(&r.information[..4]),
            Some(mut foreign) => {
                // A foreign exception caught by `catch(...)`: raised again.
                PROCESSING_THROW.with(|p| p.set(p.get() + 1));
                raise_from(&mut foreign, ctx)
            }
            None => terminate_because("`throw;` with no exception being handled"),
        }
    } else {
        // SAFETY: an address, looked up.
        let base = unsafe { seh::RtlPcToFileHeader(throw_info.cast_mut(), ptr::null_mut()) };
        record.information[..4].copy_from_slice(&[
            u64::from(EH_MAGIC_NUMBER1),
            object as u64,
            throw_info as u64,
            base as u64,
        ]);
    }
    PROCESSING_THROW.with(|p| p.set(p.get() + 1));
    raise_from(&mut record, ctx)
}

/// Raises `record` in `ctx`; inside a catch funclet, the search alone, and
/// the end of the process when it ends (it reached our frames).
fn raise_from(record: &mut ExceptionRecord, ctx: &mut Context) -> ! {
    if RUNNING.with(Cell::get) == 0 {
        seh::raise(record, ctx)
    }
    if seh::dispatch_exception(record, ctx) {
        // SAFETY: the context a handler has the thread go on in.
        unsafe { seh::RtlRestoreContext(ctx, ptr::null()) }
    }
    terminate_because(
        "a C++ exception would leave a catch block, which our frames cannot pass (a known limit)",
    )
}

// ---------------------------------------------------------------------------
// The frame handlers.

/// `__CxxFrameHandler3`: the FH3 tables (`FuncInfo`) at the RVA the frame's
/// handler data holds.
pub extern "win64" fn __CxxFrameHandler3(
    record: *mut ExceptionRecord,
    frame: u64,
    _context: *mut Context,
    dispatcher: *mut DispatcherContext,
) -> u32 {
    // SAFETY: the dispatcher's context, whose handler data is the RVA of the
    // frame's tables in its image.
    let info = unsafe {
        let dc = &*dispatcher;
        let at = dc.image_base + u64::from(read::<u32>(dc.handler_data as u64));
        parse_fh3(dc.image_base, at)
    };
    match info {
        // SAFETY: the dispatcher's arguments.
        Some(info) => unsafe { frame_handler(record, frame, dispatcher, &info) },
        None => {
            bad_tables("FuncInfo magic");
            EXCEPTION_CONTINUE_SEARCH_DISPOSITION
        }
    }
}

/// `__CxxFrameHandler4`: the FH4 tables (`FuncInfo4`) at the RVA the
/// frame's handler data holds.
pub extern "win64" fn __CxxFrameHandler4(
    record: *mut ExceptionRecord,
    frame: u64,
    _context: *mut Context,
    dispatcher: *mut DispatcherContext,
) -> u32 {
    // SAFETY: as in `__CxxFrameHandler3`; the function entry is the frame's.
    let info = unsafe {
        let dc = &*dispatcher;
        let at = dc.image_base + u64::from(read::<u32>(dc.handler_data as u64));
        parse_fh4(dc.image_base, at, function_begin(dc))
    };
    // SAFETY: the dispatcher's arguments.
    unsafe { frame_handler(record, frame, dispatcher, &info) }
}

/// The RVA of the start of the frame's function, 0 when unknown.
fn function_begin(dc: &DispatcherContext) -> u32 {
    // SAFETY: the dispatcher's entry, in the image's table, or null.
    unsafe { dc.function_entry.as_ref() }.map_or(0, |f| f.begin)
}

/// The language handler's work, either format: in the unwind, the frame's
/// local unwind; in the search, the catch that takes the exception, if any,
/// unwound to (never returning).
///
/// # Safety
/// The dispatcher's arguments, for a frame whose tables are `info`.
unsafe fn frame_handler(
    record: *mut ExceptionRecord,
    frame: u64,
    dispatcher: *mut DispatcherContext,
    info: &FuncInfo,
) -> u32 {
    // SAFETY: the dispatcher's.
    let (rec, dc) = unsafe { (&mut *record, &mut *dispatcher) };
    let base = dc.image_base;
    // `ControlPcIsUnwound`: a return address, whose call is the instruction
    // before it.
    let pc = dc.control_pc - u64::from(dc.fill0 & 0xFF != 0);
    let state = info.state_at(pc.wrapping_sub(base) as u32);
    // SAFETY: the frame is the function's.
    let establisher = unsafe { info.establisher(frame, state, function_begin(dc)) };

    if rec.flags & EXCEPTION_UNWIND != 0 {
        let target_unwind = rec.flags & EXCEPTION_TARGET_UNWIND != 0;
        if info.noexcept && !target_unwind && is_cxx(rec) {
            terminate_because("a C++ exception leaves a noexcept function");
        }
        let target = if target_unwind {
            PENDING
                .with(|p| {
                    p.borrow().iter().rev().find_map(|&c| {
                        // SAFETY: a pending catch, alive until it runs.
                        let c = unsafe { &*c };
                        (c.frame == frame).then_some(c.try_low)
                    })
                })
                .unwrap_or_else(|| info.state_at(dc.target_ip.wrapping_sub(base) as u32))
        } else {
            -1
        };
        let current = match info.unwind_help(establisher) {
            // SAFETY: the frame's slot.
            Some(slot) => match unsafe { read::<i32>(slot) } {
                UNWIND_HELP_UNSET => state,
                s => s,
            },
            None => state,
        };
        // SAFETY: the frame's tables and actions.
        unsafe { unwind_to(info, base, establisher, current, target) };
        return EXCEPTION_CONTINUE_SEARCH_DISPOSITION;
    }

    let thrown = thrown_of(rec);
    if thrown.is_none() && (is_cxx(rec) || info.ehs) {
        return EXCEPTION_CONTINUE_SEARCH_DISPOSITION;
    }
    for block in &info.try_blocks {
        if state < block.low || state > block.high {
            continue;
        }
        for h in &block.handlers {
            let taken = match &thrown {
                // SAFETY: the record's tables and the frame's.
                Some(t) => unsafe { handler_takes(h, base, t) },
                None => {
                    // A foreign exception (`/EHa`): `catch(...)` that is not
                    // `std`'s.
                    // SAFETY: as above.
                    let name = (h.type_rva != 0)
                        .then(|| unsafe { type_name(base + u64::from(h.type_rva)) });
                    (is_ellipsis(h.adjectives, name) && h.adjectives & HT_IS_STD_DOT_DOT == 0)
                        .then_some(None)
                }
            };
            let Some(catchable) = taken else {
                continue;
            };
            if let (Some(t), Some(ct)) = (&thrown, &catchable) {
                // SAFETY: the frame, alive until the catch ends; the object.
                unsafe { build_catch_object(establisher, h, t, ct) };
            }
            // SAFETY: the dispatcher's arguments.
            unsafe { catch_it(record, frame, establisher, dc, info, block, h, thrown) }
        }
    }
    EXCEPTION_CONTINUE_SEARCH_DISPOSITION
}

/// Unwinds the frame `establisher` from `state` down to (not including)
/// `target`: each state's action runs, FH3 storing the state it goes to in
/// the unwind-help slot first.
///
/// # Safety
/// The frame's tables and actions, the frame alive.
unsafe fn unwind_to(info: &FuncInfo, base: u64, establisher: u64, mut state: i32, target: i32) {
    while state > target {
        let Some(entry) = info.unwind.get(state as usize).copied() else {
            bad_tables("an unwind state out of range");
            return;
        };
        if let Some(slot) = info.unwind_help(establisher) {
            // SAFETY: the frame's slot.
            unsafe { write(slot, entry.to_state) };
        }
        let object = |offset: i32| establisher.wrapping_add_signed(i64::from(offset));
        // SAFETY: the title's funclets and destructors, given their frame or
        // object.
        unsafe {
            match entry.action {
                Action::None => {}
                Action::Funclet(at) => {
                    std::mem::transmute::<u64, Funclet>(base + u64::from(at))(0, establisher);
                }
                Action::Dtor { rva, object: at } => {
                    std::mem::transmute::<u64, Destructor>(base + u64::from(rva))(object(at));
                }
                Action::DtorPtr { rva, object: at } => {
                    let this = read::<u64>(object(at));
                    std::mem::transmute::<u64, Destructor>(base + u64::from(rva))(this);
                }
            }
        }
        if entry.to_state >= state {
            bad_tables("an unwind map that does not go down");
            return;
        }
        state = entry.to_state;
    }
}

/// Takes the exception in the catch `h` of `block`: the catch is made
/// pending and the stack unwound to its frame, which resumes in
/// [`catch_trampoline`].
///
/// # Safety
/// The dispatcher's arguments.
#[allow(clippy::too_many_arguments)]
unsafe fn catch_it(
    record: *mut ExceptionRecord,
    frame: u64,
    establisher: u64,
    dc: &DispatcherContext,
    info: &FuncInfo,
    block: &TryBlock,
    h: &Handler,
    thrown: Option<Thrown>,
) -> ! {
    // SAFETY: the dispatcher's record.
    let mut copy = unsafe { *record };
    if let Some(t) = thrown {
        copy.number_parameters = copy.number_parameters.max(4);
        copy.information[1..4].copy_from_slice(&[t.object, t.info, t.base]);
    }
    let mut continuations = [0; 2];
    for (c, &r) in continuations.iter_mut().zip(&h.continuations) {
        *c = dc.image_base + u64::from(r);
    }
    let stack: u64;
    // SAFETY: reads rsp.
    unsafe {
        std::arch::asm!("mov {}, rsp", out(reg) stack, options(nomem, nostack, preserves_flags))
    };
    let catch = Box::into_raw(Box::new(Catch {
        // Below this frame: what the unwind leaves behind, dead, and what
        // the trampoline may use.
        stack: stack & !15,
        frame,
        establisher,
        try_low: block.low,
        funclet: dc.image_base + u64::from(h.handler_rva),
        continuations,
        n_continuations: h.continuations.len().min(2),
        record: copy,
        context: dc.context_record,
        unwind_help: info.unwind_help(establisher).unwrap_or(0),
    }));
    PENDING.with(|p| p.borrow_mut().push(catch));
    // The walk on a copy: the exception's context stays as it was, for
    // `__current_exception_context`.
    // SAFETY: the dispatcher's context.
    let mut walk = unsafe { *dc.context_record };
    // SAFETY: the frame is on this thread's stack, the walk starts where the
    // exception happened.
    unsafe {
        seh::unwind_ex(
            frame,
            catch_trampoline as *const () as u64,
            record,
            catch as u64,
            &mut walk,
        )
    }
}

/// Where the unwind resumes the catching frame (its rsp and nonvolatile
/// registers as the unwind has them), rax the [`Catch`]: [`run_catch`] runs
/// on the catch's stack (below the dead frames), then the frame goes on at
/// the continuation, rsp restored.
#[unsafe(naked)]
unsafe extern "win64" fn catch_trampoline() {
    naked_asm!(
        "mov rcx, rax",
        "mov r11, rsp",
        "mov rsp, [rax + {stack}]",
        "push r11",
        "sub rsp, 40",
        "call {run}",
        "add rsp, 40",
        "pop rsp",
        "jmp rax",
        stack = const offset_of!(Catch, stack),
        run = sym run_catch,
    )
}

/// The catch: the exception becomes the current one, the funclet runs
/// (`rdx` the frame), the exception is popped and its object destroyed
/// unless another exception being handled holds it; the continuation (an
/// FH4 index resolved through the handler's table).
extern "win64" fn run_catch(catch: *mut Catch) -> u64 {
    PENDING.with(|p| p.borrow_mut().retain(|&c| c != catch));
    // SAFETY: the catch `catch_it` made, no longer pending: ours.
    let catch = unsafe { Box::from_raw(catch) };
    let cxx = is_cxx(&catch.record);
    let handling = push_handling(catch.record, catch.context);
    if cxx {
        PROCESSING_THROW.with(|p| p.set((p.get() - 1).max(0)));
    }
    RUNNING.with(|r| r.set(r.get() + 1));
    // SAFETY: the title's catch funclet, given its frame.
    let returned =
        unsafe { std::mem::transmute::<u64, Funclet>(catch.funclet)(0, catch.establisher) };
    RUNNING.with(|r| r.set(r.get() - 1));
    let continuation = if (returned as usize) < catch.n_continuations {
        catch.continuations[returned as usize]
    } else {
        returned
    };
    if let Some(done) = remove_handling(handling)
        && let Some(t) = thrown_in(&done.record)
        && !held(t.object)
    {
        // SAFETY: the object, which nothing holds any more.
        unsafe { t.destroy() };
    }
    if catch.unwind_help != 0 {
        // The frame's state is its ip's again.
        // SAFETY: the frame's slot.
        unsafe { write(catch.unwind_help, UNWIND_HELP_UNSET) };
    }
    continuation
}

// ---------------------------------------------------------------------------
// The rest of the exports.

/// `std::terminate`: `kernel.rs`'s `terminate`.
pub extern "win64" fn __std_terminate() -> ! {
    crate::modules::kernel::terminate()
}

/// The calling thread's slot for the exception being handled (its
/// `ExceptionRecord*`).
pub extern "win64" fn __current_exception() -> *mut *mut c_void {
    CURRENT_EXCEPTION.with(Cell::as_ptr)
}

/// The calling thread's slot for the context of the exception being handled.
pub extern "win64" fn __current_exception_context() -> *mut *mut c_void {
    CURRENT_EXCEPTION_CONTEXT.with(Cell::as_ptr)
}

/// The calling thread's count of throws in progress.
pub extern "win64" fn __processing_throw() -> *mut i32 {
    PROCESSING_THROW.with(Cell::as_ptr)
}

/// `std::uncaught_exception`: whether a throw is in progress.
pub extern "win64" fn __uncaught_exception() -> u8 {
    u8::from(PROCESSING_THROW.with(Cell::get) > 0)
}

/// `std::uncaught_exceptions`: how many throws are in progress.
pub extern "win64" fn __uncaught_exceptions() -> i32 {
    PROCESSING_THROW.with(Cell::get)
}

/// Whether the record in `pointers` is a C++ exception one of whose
/// catchable types is named as `ty` (a `type_info`).
pub extern "win64" fn _is_exception_typeof(
    ty: *const TypeDescriptor,
    pointers: *const ExceptionPointers,
) -> i32 {
    if ty.is_null() || pointers.is_null() {
        return 0;
    }
    // SAFETY: the title's pointers, record and tables.
    unsafe {
        let Some(record) = (*pointers).record.as_ref() else {
            return 0;
        };
        let Some(t) = thrown_of(record) else {
            return 0;
        };
        let name = type_name(ty as u64);
        i32::from(
            t.catchables()
                .iter()
                .any(|ct| type_name(rva(t.base, ct.p_type)) == name),
        )
    }
}

/// 1 unless an exception being handled on this thread holds `object`.
pub extern "win64" fn _IsExceptionObjectToBeDestroyed(object: *mut c_void) -> i32 {
    i32::from(!held(object as u64))
}

/// Runs the destructor of the C++ record's object.
pub extern "win64" fn __DestructExceptionObject(record: *mut ExceptionRecord, _abnormal: u8) {
    // SAFETY: the title's record.
    if let Some(t) = unsafe { record.as_ref() }.and_then(thrown_in) {
        // SAFETY: the record's object and tables.
        unsafe { t.destroy() };
    }
}

/// 1 when `*record` is a rethrow's (a C++ record with null parameters) and
/// an exception is being handled, which is marked rethrown; a throw is in
/// progress again.
pub extern "win64" fn __CxxDetectRethrow(record: *mut *mut ExceptionRecord) -> i32 {
    // SAFETY: the title's pointers.
    let Some(r) = (unsafe { record.as_ref().and_then(|r| r.as_ref()) }) else {
        return 0;
    };
    if !is_cxx(r) || r.information[2] != 0 {
        return 0;
    }
    let marked = HANDLING.with(|h| {
        h.borrow_mut()
            .last_mut()
            .map(|x| x.rethrown = true)
            .is_some()
    });
    if marked {
        PROCESSING_THROW.with(|p| p.set(p.get() + 1));
    }
    i32::from(marked)
}

/// The size of the record of a C++ exception.
pub extern "win64" fn __CxxQueryExceptionSize() -> u32 {
    size_of::<ExceptionRecord>() as u32
}

/// What `__CxxRegisterExceptionObject` keeps in the title's storage (at its
/// start) when nothing was registered.
const NOT_REGISTERED: u64 = u64::MAX;

/// `/EHa`'s filter helper, as vcruntime's: the exception in `pointers` (a
/// rethrow's, the current one) becomes the current one, and the storage
/// names it for [`__CxxUnregisterExceptionObject`]; no exception, nothing.
pub extern "win64" fn __CxxRegisterExceptionObject(
    pointers: *const ExceptionPointers,
    storage: *mut c_void,
) -> i32 {
    if storage.is_null() {
        return 0;
    }
    // SAFETY: the title's pointers.
    let pointers = unsafe { pointers.as_ref() };
    // SAFETY: as above.
    let record = pointers.and_then(|p| unsafe { p.record.as_ref() });
    let (Some(p), Some(r)) = (pointers, record) else {
        // SAFETY: the title's storage, at least two pointers.
        unsafe { write(storage as u64, NOT_REGISTERED) };
        return 1;
    };
    let mut record = *r;
    if let Some(t) = thrown_of(r) {
        record.information[1..4].copy_from_slice(&[t.object, t.info, t.base]);
        record.number_parameters = record.number_parameters.max(4);
    }
    let at = push_handling(record, p.context);
    // SAFETY: as above.
    unsafe { write(storage as u64, at as u64) };
    1
}

/// Undoes [`__CxxRegisterExceptionObject`]: the exception it registered is
/// no longer current, and unless `rethrow`, its object is destroyed when
/// nothing else holds it. 0.
pub extern "win64" fn __CxxUnregisterExceptionObject(storage: *mut c_void, rethrow: i32) -> i32 {
    if storage.is_null() {
        return 0;
    }
    // SAFETY: the title's storage, which the register call wrote.
    let at = unsafe { read::<u64>(storage as u64) };
    if at == NOT_REGISTERED {
        return 0;
    }
    if let Some(done) = remove_handling(at as *const Handling)
        && rethrow == 0
        && let Some(t) = thrown_in(&done.record)
        && !held(t.object)
    {
        // SAFETY: the object, which nothing holds any more.
        unsafe { t.destroy() };
    }
    0
}

/// `_se_translator_function`.
pub type SeTranslator = Option<unsafe extern "win64" fn(u32, *mut ExceptionPointers)>;

/// Sets the thread's se translator (kept, never called); the old one.
pub extern "win64" fn _set_se_translator(translator: SeTranslator) -> SeTranslator {
    static WARNED: Once = Once::new();
    if translator.is_some() {
        WARNED.call_once(|| {
            tracing::warn!("the title sets an se translator: kept, never called");
        });
    }
    let new = translator.map_or(0, |f| f as usize);
    let old = TRANSLATOR.with(|t| t.replace(new));
    // SAFETY: 0 or a translator stored above.
    unsafe { std::mem::transmute::<usize, SeTranslator>(old) }
}

pub(crate) fn exports() -> Vec<HleExport> {
    vec![
        hle_export!(__std_terminate),
        hle_export!(_CxxThrowException),
        // Older vcruntimes' name for it.
        HleExport {
            name: "__CxxFrameHandler2",
            address: __CxxFrameHandler3 as *const () as usize,
        },
        hle_export!(__CxxFrameHandler3),
        hle_export!(__CxxFrameHandler4),
        hle_export!(__current_exception),
        hle_export!(__current_exception_context),
        hle_export!(__processing_throw),
        hle_export!(__uncaught_exception),
        hle_export!(__uncaught_exceptions),
        hle_export!(_is_exception_typeof),
        hle_export!(_IsExceptionObjectToBeDestroyed),
        hle_export!(__DestructExceptionObject),
        hle_export!(__CxxDetectRethrow),
        hle_export!(__CxxQueryExceptionSize),
        hle_export!(__CxxRegisterExceptionObject),
        hle_export!(__CxxUnregisterExceptionObject),
        hle_export!(_set_se_translator),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inverse of [`decode_unsigned`].
    fn enc(v: u32) -> Vec<u8> {
        match v {
            0..0x80 => vec![(v << 1) as u8],
            0x80..0x4000 => ((v << 2) | 1).to_le_bytes()[..2].to_vec(),
            0x4000..0x20_0000 => ((v << 3) | 3).to_le_bytes()[..3].to_vec(),
            0x20_0000..0x1000_0000 => ((v << 4) | 7).to_le_bytes().to_vec(),
            _ => {
                let mut b = vec![0x0F];
                b.extend(v.to_le_bytes());
                b
            }
        }
    }

    #[test]
    fn compressed_integers_of_every_length() {
        for (v, len) in [
            (0, 1),
            (5, 1),
            (127, 1),
            (128, 2),
            (0x3FFF, 2),
            (0x4000, 3),
            (0x1F_FFFF, 3),
            (0x20_0000, 4),
            (0x0FFF_FFFF, 4),
            (0x1000_0000, 5),
            (u32::MAX, 5),
        ] {
            let bytes = enc(v);
            assert_eq!(bytes.len(), len, "{v:#x}");
            assert_eq!(decode_unsigned(&bytes), Some((v, len)), "{v:#x}");
        }
        // By hand: 0x0A is 5 in one byte; 0x01 0x02 is (0x0201 >> 2) = 0x80.
        assert_eq!(decode_unsigned(&[0x0A]), Some((5, 1)));
        assert_eq!(decode_unsigned(&[0x01, 0x02]), Some((0x80, 2)));
        assert_eq!(decode_unsigned(&[0x0F, 1, 0, 0, 0]), Some((1, 5)));
        assert_eq!(decode_unsigned(&[0x01]), None, "too short");
    }

    /// An image of hand-made FH4 tables: what is put where.
    #[derive(Default)]
    struct Image {
        bytes: Vec<u8>,
    }

    impl Image {
        /// Appends `b`; its RVA.
        fn put(&mut self, b: &[u8]) -> i32 {
            // Never at RVA 0: that means "none".
            if self.bytes.is_empty() {
                self.bytes.extend([0; 16]);
            }
            let at = self.bytes.len() as i32;
            self.bytes.extend_from_slice(b);
            at
        }

        fn base(&self) -> u64 {
            self.bytes.as_ptr() as u64
        }
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// The function the tables are for, and a catch funclet's segment.
    const START: u32 = 0x1000;
    const SEGMENT: u32 = 0x3000;

    /// Unwind map: state 0 (`DtorWithObj` at 0x30, root), state 1 (`RVA`,
    /// parent 0), state 2 (`DtorWithPtrToObj` at 0x38, parent 1), state 3
    /// (`NoUW`, parent 0); one try block 0..=2, catch state 3, with a
    /// `catch(int&)` (two continuations, offsets) and a `catch(...)` (one
    /// continuation, an RVA); an ip map with -1, 0, 2, -1.
    fn fh4_tables(image: &mut Image, separated: bool) -> (i32, Vec<UnwindEntry>) {
        let td = image.put(&cat(&[&[0; 16], b".H\0"]));
        // Unwind entries and their offsets from the map's start.
        let mut map = enc(4);
        let e0 = map.len();
        map.extend(cat(&[&enc(1), &0x2000i32.to_le_bytes(), &enc(0x30)]));
        let e1 = map.len();
        map.extend(cat(&[
            &enc((((e1 - e0) as u32) << 2) | 3),
            &0x2100i32.to_le_bytes(),
        ]));
        let e2 = map.len();
        map.extend(cat(&[
            &enc((((e2 - e1) as u32) << 2) | 2),
            &0x2200i32.to_le_bytes(),
            &enc(0x38),
        ]));
        let e3 = map.len();
        map.extend(enc(((e3 - e0) as u32) << 2));
        let unwind_map = image.put(&map);
        let handlers = image.put(&cat(&[
            &enc(2),
            // adjectives, type, catch object, two offset continuations.
            &[1 | 2 | 4 | (2 << 4)],
            &enc(HT_IS_REFERENCE),
            &td.to_le_bytes(),
            &enc(0x28),
            &0x2400i32.to_le_bytes(),
            &enc(0x40),
            &enc(0x50),
            // `catch(...)`: no type, one continuation as an RVA.
            &[8 | (1 << 4)],
            &0x2500i32.to_le_bytes(),
            &0x1060i32.to_le_bytes(),
        ]));
        let try_map = image.put(&cat(&[
            &enc(1),
            &enc(0),
            &enc(2),
            &enc(3),
            &handlers.to_le_bytes(),
        ]));
        // (delta, state + 1): -1 from the start, 0 at +0x10, 2 at +0x20,
        // -1 at +0x30.
        let ip = image.put(&cat(&[
            &enc(4),
            &enc(0),
            &enc(0),
            &enc(0x10),
            &enc(1),
            &enc(0x10),
            &enc(3),
            &enc(0x10),
            &enc(0),
        ]));
        let ip_ref = if separated {
            let other = image.put(&cat(&[&enc(1), &enc(0), &enc(4)]));
            image.put(&cat(&[
                &enc(2),
                &(SEGMENT as i32).to_le_bytes(),
                &other.to_le_bytes(),
                &(START as i32).to_le_bytes(),
                &ip.to_le_bytes(),
            ]))
        } else {
            ip
        };
        let header = FI4_UNWIND_MAP
            | FI4_TRY_BLOCK_MAP
            | FI4_EHS
            | if separated { FI4_IS_SEPARATED } else { 0 };
        let info = image.put(&cat(&[
            &[header],
            &unwind_map.to_le_bytes(),
            &try_map.to_le_bytes(),
            &ip_ref.to_le_bytes(),
        ]));
        let unwind = vec![
            UnwindEntry {
                to_state: -1,
                action: Action::Dtor {
                    rva: 0x2000,
                    object: 0x30,
                },
            },
            UnwindEntry {
                to_state: 0,
                action: Action::Funclet(0x2100),
            },
            UnwindEntry {
                to_state: 1,
                action: Action::DtorPtr {
                    rva: 0x2200,
                    object: 0x38,
                },
            },
            UnwindEntry {
                to_state: 0,
                action: Action::None,
            },
        ];
        (info, unwind)
    }

    #[test]
    fn a_funcinfo4_decodes() {
        for separated in [false, true] {
            let mut image = Image::default();
            let (info, unwind) = fh4_tables(&mut image, separated);
            // SAFETY: tables in `image`, alive here.
            let decoded = unsafe { parse_fh4(image.base(), image.base() + info as u64, START) };
            assert_eq!(decoded.format, Format::Fh4 { catch_frame: None });
            assert!(decoded.ehs && !decoded.noexcept);
            assert_eq!(decoded.unwind, unwind, "separated: {separated}");
            assert_eq!(decoded.try_blocks.len(), 1);
            let block = &decoded.try_blocks[0];
            assert_eq!((block.low, block.high, block.catch_high), (0, 2, 3));
            assert_eq!(block.handlers.len(), 2);
            let (int, all) = (&block.handlers[0], &block.handlers[1]);
            assert_eq!(int.adjectives, HT_IS_REFERENCE);
            assert_ne!(int.type_rva, 0);
            // SAFETY: as above.
            assert_eq!(
                unsafe { type_name(image.base() + u64::from(int.type_rva)) },
                b".H"
            );
            assert_eq!(int.catch_obj, 0x28);
            assert_eq!(int.handler_rva, 0x2400);
            assert_eq!(int.continuations, vec![START + 0x40, START + 0x50]);
            assert_eq!(
                (all.type_rva, all.catch_obj, all.handler_rva),
                (0, 0, 0x2500)
            );
            assert_eq!(all.continuations, vec![0x1060]);
            assert_eq!(
                decoded.ip_map,
                vec![
                    (START, -1),
                    (START + 0x10, 0),
                    (START + 0x20, 2),
                    (START + 0x30, -1)
                ]
            );
            assert_eq!(decoded.state_at(START - 1), -1);
            assert_eq!(decoded.state_at(START + 0x1F), 0);
            assert_eq!(decoded.state_at(START + 0x20), 2);
            assert_eq!(decoded.state_at(START + 0x100), -1);
        }
    }

    #[test]
    fn a_catch_funclets_funcinfo4_names_its_frame() {
        let mut image = Image::default();
        let ip = image.put(&cat(&[&enc(1), &enc(0), &enc(4)]));
        let info = image.put(&cat(&[
            &[FI4_IS_CATCH | FI4_BBT | FI4_NO_EXCEPT],
            &enc(0x1234),
            &ip.to_le_bytes(),
            &enc(0x58),
        ]));
        // SAFETY: tables in `image`.
        let decoded = unsafe { parse_fh4(image.base(), image.base() + info as u64, SEGMENT) };
        assert_eq!(
            decoded.format,
            Format::Fh4 {
                catch_frame: Some(0x58)
            }
        );
        assert!(decoded.noexcept && !decoded.ehs);
        assert!(decoded.unwind.is_empty() && decoded.try_blocks.is_empty());
        assert_eq!(decoded.ip_map, vec![(SEGMENT, 3)]);
        let parent = 0x7777_0000u64;
        let frame = [0u64, parent];
        let at = frame.as_ptr() as u64 - 0x50;
        // SAFETY: `frame`, read at `at + 0x58`.
        assert_eq!(unsafe { decoded.establisher(at, 3, SEGMENT) }, parent);
    }

    #[test]
    fn type_matching_follows_vcruntime() {
        let int = Some(&b".H"[..]);
        let simple = CT_IS_SIMPLE_TYPE;
        assert!(type_matches(0, int, simple, b".H", 0));
        assert!(!type_matches(0, int, simple, b".M", 0), "another type");
        assert!(type_matches(0, None, simple, b".M", 0), "catch(...)");
        assert!(
            type_matches(0, Some(b""), simple, b".M", 0),
            "an empty name"
        );
        assert!(type_matches(HT_IS_STD_DOT_DOT, int, simple, b".M", 0));
        // `void*` takes simple pointers.
        assert!(type_matches(0, Some(b".PEAX"), simple, b".PEAD", 0));
        assert!(!type_matches(0, Some(b".PEAX"), simple, b".H", 0));
        assert!(!type_matches(0, Some(b".PEAX"), 0, b".PEAD", 0));
        // A const pointer is not caught by a handler that drops the const.
        assert!(!type_matches(
            0,
            Some(b".PEAD"),
            simple,
            b".PEAD",
            TI_IS_CONST
        ));
        assert!(type_matches(
            HT_IS_CONST,
            Some(b".PEAD"),
            simple,
            b".PEAD",
            TI_IS_CONST
        ));
        assert!(!type_matches(
            0,
            Some(b".PEAD"),
            simple,
            b".PEAD",
            TI_IS_VOLATILE
        ));
        // By reference only.
        let class = Some(&b".?AVfoo@@"[..]);
        assert!(!type_matches(
            0,
            class,
            CT_BY_REFERENCE_ONLY,
            b".?AVfoo@@",
            0
        ));
        assert!(type_matches(
            HT_IS_REFERENCE,
            class,
            CT_BY_REFERENCE_ONLY,
            b".?AVfoo@@",
            0
        ));
        // `std::bad_alloc` compatibility.
        assert!(type_matches(
            HT_IS_BAD_ALLOC_COMPAT,
            class,
            CT_IS_STD_BAD_ALLOC,
            b".?AVbad_alloc@std@@",
            0
        ));
        assert!(!type_matches(
            0,
            class,
            CT_IS_STD_BAD_ALLOC,
            b".?AVbad_alloc@std@@",
            0
        ));
    }

    #[test]
    fn pointers_are_adjusted_to_the_base() {
        let plain = Pmd {
            mdisp: 8,
            pdisp: -1,
            vdisp: 0,
        };
        // SAFETY: no virtual base: nothing read.
        assert_eq!(unsafe { adjust_pointer(0x1000, plain) }, 0x1008);
        // An object whose vbtable pointer is at +0x10; the vbtable's entry
        // at +4 is 0x20: the base is at 0x10 + 0x20 (+ mdisp 0).
        let vbtable: [i32; 2] = [0, 0x20];
        let mut object = [0u64; 4];
        object[2] = vbtable.as_ptr() as u64;
        let at = object.as_ptr() as u64;
        let virtual_base = Pmd {
            mdisp: 0,
            pdisp: 0x10,
            vdisp: 4,
        };
        // SAFETY: `object` and `vbtable`, alive here.
        assert_eq!(unsafe { adjust_pointer(at, virtual_base) }, at + 0x30);
        let both = Pmd {
            mdisp: 4,
            ..virtual_base
        };
        // SAFETY: as above.
        assert_eq!(unsafe { adjust_pointer(at, both) }, at + 0x34);
    }

    #[test]
    fn fh3_tables_decode() {
        // A FuncInfo with one unwind state, one try block of one handler and
        // two ip map entries, at RVAs in a buffer.
        let mut image = Image::default();
        let unwind = image.put(&cat(&[&(-1i32).to_le_bytes(), &0x2000i32.to_le_bytes()]));
        let handler = image.put(
            &[8u32, 0x40, 0x20, 0x2100, 0x38]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let tries = image.put(
            &[0i32, 0, 1, 1, handler]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let ip = image.put(
            &[0x1000i32, -1, 0x1010, 0]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let info = image.put(
            &[
                EH_MAGIC_NUMBER3 as i32,
                1,
                unwind,
                1,
                tries,
                2,
                ip,
                0x28,
                0,
                FI_EHS_FLAG | FI_EHNOEXCEPT_FLAG,
            ]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
        );
        // SAFETY: tables in `image`.
        let decoded = unsafe { parse_fh3(image.base(), image.base() + info as u64) }.unwrap();
        assert_eq!(decoded.format, Format::Fh3 { unwind_help: 0x28 });
        assert!(decoded.ehs && decoded.noexcept);
        assert_eq!(
            decoded.unwind,
            vec![UnwindEntry {
                to_state: -1,
                action: Action::Funclet(0x2000)
            }]
        );
        let h = &decoded.try_blocks[0].handlers[0];
        assert_eq!(
            (
                h.adjectives,
                h.type_rva,
                h.catch_obj,
                h.handler_rva,
                h.disp_frame
            ),
            (8, 0x40, 0x20, 0x2100, 0x38)
        );
        assert_eq!(decoded.state_at(0x100F), -1);
        assert_eq!(decoded.state_at(0x1010), 0);
        // A bad magic is not read.
        let bad = image.put(&[0u8; 40]);
        // SAFETY: as above.
        assert!(unsafe { parse_fh3(image.base(), image.base() + bad as u64) }.is_none());
    }

    #[test]
    fn records_are_recognised() {
        let mut r = ExceptionRecord::new(EH_EXCEPTION_NUMBER, EXCEPTION_NONCONTINUABLE, 0);
        r.number_parameters = 4;
        r.information[..4].copy_from_slice(&[u64::from(EH_MAGIC_NUMBER1), 0x10, 0x20, 0x30]);
        assert!(is_cxx(&r));
        assert_eq!(
            thrown_in(&r),
            Some(Thrown {
                object: 0x10,
                info: 0x20,
                base: 0x30
            })
        );
        r.information[0] = u64::from(EH_MAGIC_NUMBER3);
        assert!(is_cxx(&r));
        r.information[0] = 0x1993_0523;
        assert!(!is_cxx(&r));
        assert_eq!(__CxxQueryExceptionSize(), 152);
        assert_eq!(__uncaught_exceptions(), 0);
        assert_eq!(_IsExceptionObjectToBeDestroyed(ptr::null_mut()), 1);
    }

    #[test]
    fn every_export_is_listed() {
        assert_eq!(exports().len(), 18);
    }
}
