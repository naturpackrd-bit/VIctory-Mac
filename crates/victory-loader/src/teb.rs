//! The TEB (thread environment block) where the host has none for title
//! code. Windows code reaches its thread's TEB through `gs`
//! (`victory_pe::gs`), and on macOS `gs` is the host's own thread data
//! (docs/macos.md, probe 2). So each thread that runs title code gets a TEB
//! of ours, laid out as Windows' ([`Teb`]) with the thread's stack bounds,
//! its ids and its TLS array ([`attach_current_thread`]); and every
//! `gs:[disp]` access in a mapped image's code is patched, at mapping, into
//! a call of a thunk that reads or writes the same field of our TEB (the
//! forms `mov r, gs:[disp]` and `mov gs:[disp], r`), as the Windows
//! loader's TLS redirect does for one field (`tls.rs`). A thunk finds the
//! thread's TEB where the host keeps a pointer for us
//! (`victory_platform::Thread::teb_pointer_disp`) and leaves every register
//! but its own, and the flags, as they were: what the `mov` it replaces did.
//!
//! Static TLS: each image with a TLS directory gets an index; a thread's
//! TLS array holds one block per image (the image's initial data, then its
//! zero fill), and the image's `_tls_index` is written. A thread's blocks
//! for images mapped after it attached are added when it attaches again
//! ([`attach_current_thread`] may be called at any time).
//!
//! Not yet (docs/macos.md): TLS callbacks; a PEB with contents (titles get
//! a zeroed one); `gs` accesses in other forms than a `mov` (mapping refuses
//! the image and names them; `victory inspect --teb` lists an image's).

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::offset_of;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, OnceLock};

use victory_pe::{GsAccess, GsSite, TEB_SIZE};
use victory_platform::faults::Faults;
use victory_platform::{Host, Memory, Protection, Thread};

use crate::map::TLS_IMAGES;

/// The TEB as x64 Windows lays it out; the fields title code reads, named.
#[repr(C)]
pub struct Teb {
    pub exception_list: u64,
    pub stack_base: u64,
    pub stack_limit: u64,
    pub sub_system_tib: u64,
    pub fiber_data: u64,
    pub arbitrary_user_pointer: u64,
    /// `NtCurrentTeb()`: this block's own address.
    pub self_address: u64,
    pub environment_pointer: u64,
    pub process_id: u64,
    pub thread_id: u64,
    pub active_rpc_handle: u64,
    /// `ThreadLocalStoragePointer`: one block per image with static TLS.
    pub tls_array: u64,
    pub peb: u64,
    pub last_error: u32,
    pub owned_critical_sections: u32,
    reserved_70: [u8; 0x1480 - 0x70],
    pub tls_slots: [u64; 64],
    reserved_1680: [u8; 0x1780 - 0x1680],
    pub tls_expansion_slots: u64,
    reserved_1788: [u8; TEB_SIZE as usize - 0x1788],
}

const _: () = {
    assert!(size_of::<Teb>() == TEB_SIZE as usize);
    assert!(offset_of!(Teb, stack_base) == 0x08);
    assert!(offset_of!(Teb, stack_limit) == 0x10);
    assert!(offset_of!(Teb, arbitrary_user_pointer) == 0x28);
    assert!(offset_of!(Teb, self_address) == 0x30);
    assert!(offset_of!(Teb, process_id) == 0x40);
    assert!(offset_of!(Teb, thread_id) == 0x48);
    assert!(offset_of!(Teb, tls_array) == 0x58);
    assert!(offset_of!(Teb, peb) == 0x60);
    assert!(offset_of!(Teb, last_error) == 0x68);
    assert!(offset_of!(Teb, tls_slots) == 0x1480);
    assert!(offset_of!(Teb, tls_expansion_slots) == 0x1780);
};

/// `sizeof(PEB)` on x64 Windows 10 and 11.
const PEB_SIZE: usize = 0x7C8;

/// The one PEB, zeroed: `BeingDebugged` 0, and nothing else in it yet.
fn peb() -> u64 {
    static PEB: OnceLock<Box<[u8; PEB_SIZE]>> = OnceLock::new();
    PEB.get_or_init(|| Box::new([0; PEB_SIZE])).as_ptr() as u64
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// An image with static TLS: its initial data and zero fill, under the
/// index its `_tls_index` holds.
struct TlsImage {
    /// Tells a thread's block for this index from one made for an earlier
    /// image that had the index.
    generation: u64,
    init: Box<[u8]>,
    zero_fill: usize,
}

static TLS_REGISTRY: Mutex<Vec<Option<TlsImage>>> = Mutex::new(Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Registers an image's static TLS and returns its index; `None` when
/// [`TLS_IMAGES`] images have one already.
pub(crate) fn register_tls(init: &[u8], zero_fill: usize) -> Option<u32> {
    let mut images = lock(&TLS_REGISTRY);
    let index = match images.iter().position(Option::is_none) {
        Some(free) => free,
        None if images.len() < TLS_IMAGES => {
            images.push(None);
            images.len() - 1
        }
        None => return None,
    };
    images[index] = Some(TlsImage {
        generation: GENERATION.fetch_add(1, Relaxed),
        init: init.into(),
        zero_fill,
    });
    Some(index as u32)
}

/// Frees an index of [`register_tls`]: the image is unmapped.
pub(crate) fn unregister_tls(index: u32) {
    if let Some(slot) = lock(&TLS_REGISTRY).get_mut(index as usize) {
        *slot = None;
    }
}

/// A thread's TEB and TLS blocks, for its life.
struct ThreadState {
    teb: Box<Teb>,
    /// Slot `i`: the block of the image with TLS index `i`, or 0.
    array: Box<[u64; TLS_IMAGES]>,
    generations: [u64; TLS_IMAGES],
    /// The blocks the slots point into.
    blocks: Vec<Box<[u8]>>,
}

thread_local! {
    static THREAD: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

/// Gives this thread a TEB if it has none, and a TLS block for every image
/// with static TLS it has none for; returns the TEB's address. Called before
/// title code runs on a thread, and again after an image with TLS was
/// mapped.
pub fn attach_current_thread() -> usize {
    THREAD.with(|slot| {
        let mut slot = slot.borrow_mut();
        let state = slot.get_or_insert_with(new_thread_state);
        let images = lock(&TLS_REGISTRY);
        for (i, image) in images.iter().enumerate() {
            let Some(image) = image else { continue };
            if state.generations[i] == image.generation {
                continue;
            }
            let mut block = vec![0u8; image.init.len() + image.zero_fill].into_boxed_slice();
            block[..image.init.len()].copy_from_slice(&image.init);
            state.array[i] = block.as_ptr() as u64;
            state.generations[i] = image.generation;
            state.blocks.push(block);
        }
        state.teb.self_address as usize
    })
}

/// This thread's TEB, if it attached.
pub fn current_teb() -> Option<usize> {
    THREAD.with(|slot| slot.borrow().as_ref().map(|s| s.teb.self_address as usize))
}

fn new_thread_state() -> ThreadState {
    let (low, high) = Host::stack_bounds();
    let array: Box<[u64; TLS_IMAGES]> = Box::new([0; TLS_IMAGES]);
    // SAFETY: integers and byte arrays, for which zero is a value.
    let mut teb: Box<Teb> = Box::new(unsafe { std::mem::zeroed() });
    teb.stack_base = high as u64;
    teb.stack_limit = low as u64;
    teb.self_address = &raw const *teb as usize as u64;
    teb.process_id = u64::from(Host::process_id());
    teb.thread_id = u64::from(Host::thread_id());
    teb.tls_array = array.as_ptr() as u64;
    teb.peb = peb();
    // SAFETY: a TEB with its address at 0x30, kept in this thread's state
    // for the thread's life.
    let kept = unsafe { Host::set_teb_pointer(teb.self_address as usize) };
    assert!(kept, "the host refused to keep this thread's TEB pointer");
    if !Host::alternate_stack() {
        tracing::warn!("no alternate signal stack: a stack overflow on this thread is lost");
    }
    ThreadState {
        teb,
        array,
        generations: [0; TLS_IMAGES],
        blocks: Vec::new(),
    }
}

/// Bytes a thunk may take; the block holds [`BLOCK`] / `THUNK` of them.
const THUNK: usize = 32;
const BLOCK: usize = 64 << 10;
const GRANULE: usize = 64 << 10;

/// The thunks an image's code calls in place of its `gs` accesses: one per
/// (access, register, field), in a block within `rel32` reach of the image.
#[derive(Debug)]
pub(crate) struct Thunks {
    block: usize,
    used: usize,
    made: HashMap<(GsAccess, u8, u32), usize>,
}

/// Why a site was not patched.
#[derive(Debug)]
pub(crate) enum PatchError {
    /// Forms no thunk is made for: anything but a `mov`, and a load into
    /// `rsp`.
    Unpatchable(Vec<GsSite>),
    /// The thunk block is full.
    Full,
}

impl Thunks {
    /// A block within 1 GiB of `[base, base + size)`, so that every call
    /// site reaches it; `None` if no free memory is found there.
    pub(crate) fn near(base: usize, size: usize) -> Option<Thunks> {
        if size >= 1 << 30 {
            return None;
        }
        let above = (base + size).next_multiple_of(GRANULE);
        for k in 0..(1usize << 14) {
            let hints = [
                base.checked_sub((k + 1) * GRANULE),
                above.checked_add(k * GRANULE),
            ];
            for hint in hints.into_iter().flatten() {
                if let Some(block) = Host::allocate(BLOCK, Some(hint)) {
                    return Some(Thunks {
                        block,
                        used: 0,
                        made: HashMap::new(),
                    });
                }
            }
        }
        None
    }

    /// The thunk for `site`, made if there is none yet.
    fn thunk(&mut self, site: &GsSite) -> Result<usize, PatchError> {
        let key = (site.access, site.reg, site.disp);
        if let Some(&at) = self.made.get(&key) {
            return Ok(at);
        }
        let code = encode(site.access, site.reg, site.disp)
            .ok_or_else(|| PatchError::Unpatchable(vec![*site]))?;
        if self.used + THUNK > BLOCK {
            return Err(PatchError::Full);
        }
        let at = self.block + self.used;
        // SAFETY: `THUNK` bytes of our block, still writable; `code` is
        // shorter.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, code.len()) };
        self.used += THUNK;
        self.made.insert(key, at);
        Ok(at)
    }

    /// Makes the block executable; no thunk is added after. The block's
    /// address and size, for an error.
    pub(crate) fn finish(&self) -> Result<(), (usize, usize)> {
        // SAFETY: our block, which nothing references.
        if unsafe { Host::protect(self.block, BLOCK, Protection::ReadExecute) } {
            Ok(())
        } else {
            Err((self.block, BLOCK))
        }
    }
}

impl Drop for Thunks {
    fn drop(&mut self) {
        // SAFETY: our block; the image whose code called into it is being
        // unmapped with it.
        unsafe { Host::free(self.block, BLOCK) };
    }
}

/// Patches the `gs` accesses `sites` of `code`, which lies at address `at`,
/// into `call thunk` and a NOP for the rest of the instruction; returns how
/// many. All or none: an error leaves `code` as it was.
pub(crate) fn patch_sites(
    code: &mut [u8],
    at: usize,
    sites: &[GsSite],
    thunks: &mut Thunks,
) -> Result<usize, PatchError> {
    let unpatchable: Vec<GsSite> = sites
        .iter()
        .filter(|s| encode(s.access, s.reg, s.disp).is_none())
        .copied()
        .collect();
    if !unpatchable.is_empty() {
        return Err(PatchError::Unpatchable(unpatchable));
    }
    let mut targets = Vec::with_capacity(sites.len());
    for site in sites {
        targets.push(thunks.thunk(site)?);
    }
    for (site, thunk) in sites.iter().zip(targets) {
        let (offset, len) = (site.offset, usize::from(site.len));
        let rel = thunk as i64 - (at + offset + 5) as i64;
        let rel = i32::try_from(rel).expect("thunks lie within rel32 reach of the image");
        code[offset] = 0xE8;
        code[offset + 1..offset + 5].copy_from_slice(&rel.to_le_bytes());
        match len {
            9 => code[offset + 5..offset + 9].copy_from_slice(&[0x0F, 0x1F, 0x40, 0x00]),
            8 => code[offset + 5..offset + 8].copy_from_slice(&[0x0F, 0x1F, 0x00]),
            _ => unreachable!("a mov with a gs:[disp32] operand is 8 or 9 bytes"),
        }
    }
    Ok(sites.len())
}

/// The thunk for an access, or `None` for a form no thunk is made for.
/// Flags are left alone throughout, as by the `mov` replaced.
fn encode(access: GsAccess, reg: u8, disp: u32) -> Option<Vec<u8>> {
    let slot = Host::teb_pointer_disp();
    let mut code = Vec::with_capacity(THUNK);
    match access {
        // `mov reg, gs:[slot]` (the TEB), `mov reg, [reg + disp]`, `ret`.
        GsAccess::Load64 | GsAccess::Load32 => {
            if reg == 4 {
                return None; // a load into rsp: never seen, and `call` could not do it
            }
            mov_reg_gs(&mut code, reg, slot);
            mov_reg_mem(&mut code, reg, reg, disp, access == GsAccess::Load64);
        }
        // With a scratch register saved around it: `mov scratch, gs:[slot]`,
        // `mov [scratch + disp], reg`.
        GsAccess::Store64 | GsAccess::Store32 => {
            let scratch = if reg == 0 { 1 } else { 0 };
            code.push(0x50 + scratch); // push
            mov_reg_gs(&mut code, scratch, slot);
            mov_mem_reg(&mut code, scratch, disp, reg, access == GsAccess::Store64);
            code.push(0x58 + scratch); // pop
        }
        GsAccess::Other(_) => return None,
    }
    code.push(0xC3);
    debug_assert!(code.len() <= THUNK);
    Some(code)
}

/// `mov reg, qword ptr gs:[disp]`.
fn mov_reg_gs(code: &mut Vec<u8>, reg: u8, disp: u32) {
    code.extend([
        0x65,
        0x48 | (reg >> 3) << 2,
        0x8B,
        0x04 | (reg & 7) << 3,
        0x25,
    ]);
    code.extend(disp.to_le_bytes());
}

/// `mov reg, [base + disp]`, 64 or 32 bits wide.
fn mov_reg_mem(code: &mut Vec<u8>, reg: u8, base: u8, disp: u32, wide: bool) {
    rex(code, reg, base, wide);
    code.push(0x8B);
    modrm_disp32(code, reg, base, disp);
}

/// `mov [base + disp], reg`, 64 or 32 bits wide.
fn mov_mem_reg(code: &mut Vec<u8>, base: u8, disp: u32, reg: u8, wide: bool) {
    rex(code, reg, base, wide);
    code.push(0x89);
    modrm_disp32(code, reg, base, disp);
}

/// The REX prefix for `reg` (R) and `base` (B), if one is needed.
fn rex(code: &mut Vec<u8>, reg: u8, base: u8, wide: bool) {
    let rex = 0x40 | if wide { 8 } else { 0 } | (reg >> 3) << 2 | (base >> 3);
    if rex != 0x40 {
        code.push(rex);
    }
}

/// ModRM (mod 2: a 32-bit displacement), the SIB `rsp`/`r12` need, and the
/// displacement.
fn modrm_disp32(code: &mut Vec<u8>, reg: u8, base: u8, disp: u32) {
    code.push(0x80 | (reg & 7) << 3 | (base & 7));
    if base & 7 == 4 {
        code.push(0x24);
    }
    code.extend(disp.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The thunk bytes with the TEB slot's displacement masked to `S`.
    fn thunk(access: GsAccess, reg: u8, disp: u32) -> Vec<u8> {
        let slot = Host::teb_pointer_disp().to_le_bytes();
        let mut code = encode(access, reg, disp).expect("a mov is patched");
        // The slot's four bytes follow the first `65 REX 8B ModRM 25`.
        let at = code
            .windows(5)
            .position(|w| w[0] == 0x65 && w[2] == 0x8B && w[4] == 0x25)
            .unwrap()
            + 5;
        assert_eq!(code[at..at + 4], slot);
        code[at..at + 4].copy_from_slice(b"SSSS");
        code
    }

    #[test]
    fn loads_are_encoded_as_the_processor_reads_them() {
        // mov rax, gs:[S]; mov rax, [rax + 0x58]; ret
        assert_eq!(
            thunk(GsAccess::Load64, 0, 0x58),
            [
                0x65, 0x48, 0x8B, 0x04, 0x25, b'S', b'S', b'S', b'S', 0x48, 0x8B, 0x80, 0x58, 0, 0,
                0, 0xC3
            ]
        );
        // mov r11, gs:[S]; mov r11, [r11 + 0x10]; ret
        assert_eq!(
            thunk(GsAccess::Load64, 11, 0x10),
            [
                0x65, 0x4C, 0x8B, 0x1C, 0x25, b'S', b'S', b'S', b'S', 0x4D, 0x8B, 0x9B, 0x10, 0, 0,
                0, 0xC3
            ]
        );
        // mov rcx, gs:[S]; mov ecx, [rcx + 0x48]; ret (no REX for the 32-bit load)
        assert_eq!(
            thunk(GsAccess::Load32, 1, 0x48),
            [
                0x65, 0x48, 0x8B, 0x0C, 0x25, b'S', b'S', b'S', b'S', 0x8B, 0x89, 0x48, 0, 0, 0,
                0xC3
            ]
        );
        // r12 as base needs a SIB; rbp/r13 do not.
        assert_eq!(
            thunk(GsAccess::Load64, 12, 0x30),
            [
                0x65, 0x4C, 0x8B, 0x24, 0x25, b'S', b'S', b'S', b'S', 0x4D, 0x8B, 0xA4, 0x24, 0x30,
                0, 0, 0, 0xC3
            ]
        );
        assert_eq!(
            thunk(GsAccess::Load64, 13, 0x30),
            [
                0x65, 0x4C, 0x8B, 0x2C, 0x25, b'S', b'S', b'S', b'S', 0x4D, 0x8B, 0xAD, 0x30, 0, 0,
                0, 0xC3
            ]
        );
    }

    #[test]
    fn stores_save_a_scratch_register_around_the_teb_load() {
        // push rax; mov rax, gs:[S]; mov [rax + 0x68], ecx; pop rax; ret
        assert_eq!(
            thunk(GsAccess::Store32, 1, 0x68),
            [
                0x50, 0x65, 0x48, 0x8B, 0x04, 0x25, b'S', b'S', b'S', b'S', 0x89, 0x88, 0x68, 0, 0,
                0, 0x58, 0xC3
            ]
        );
        // The source is rax: rcx is the scratch.
        // push rcx; mov rcx, gs:[S]; mov [rcx + 0x28], rax; pop rcx; ret
        assert_eq!(
            thunk(GsAccess::Store64, 0, 0x28),
            [
                0x51, 0x65, 0x48, 0x8B, 0x0C, 0x25, b'S', b'S', b'S', b'S', 0x48, 0x89, 0x81, 0x28,
                0, 0, 0, 0x59, 0xC3
            ]
        );
        // A high source register: REX.R on the store.
        // push rax; mov rax, gs:[S]; mov [rax + 0x28], r9; pop rax; ret
        assert_eq!(
            thunk(GsAccess::Store64, 9, 0x28),
            [
                0x50, 0x65, 0x48, 0x8B, 0x04, 0x25, b'S', b'S', b'S', b'S', 0x4C, 0x89, 0x88, 0x28,
                0, 0, 0, 0x58, 0xC3
            ]
        );
    }

    #[test]
    fn what_gets_no_thunk() {
        assert!(
            encode(GsAccess::Load64, 4, 0x58).is_none(),
            "a load into rsp"
        );
        assert!(encode(GsAccess::Other(0x39), 1, 0x10).is_none(), "cmp");
        assert!(
            encode(GsAccess::Store64, 4, 0x58).is_some(),
            "a store of rsp is a plain store"
        );
    }

    #[test]
    fn every_thunk_fits_its_slot() {
        for access in [
            GsAccess::Load64,
            GsAccess::Load32,
            GsAccess::Store64,
            GsAccess::Store32,
        ] {
            for reg in 0..16u8 {
                if let Some(code) = encode(access, reg, 0x1780) {
                    assert!(
                        code.len() <= THUNK,
                        "{access:?} r{reg}: {} bytes",
                        code.len()
                    );
                    assert_eq!(*code.last().unwrap(), 0xC3);
                }
            }
        }
    }

    #[test]
    fn tls_indices_are_handed_out_and_taken_back() {
        let a = register_tls(&[1, 2, 3], 5).unwrap();
        let b = register_tls(&[], 0).unwrap();
        assert_ne!(a, b);
        unregister_tls(a);
        let c = register_tls(&[9], 0).unwrap();
        assert_eq!(c, a, "a freed index is used again");
        unregister_tls(b);
        unregister_tls(c);
    }

    #[test]
    fn an_attached_thread_has_a_teb_with_its_own_facts() {
        let index = register_tls(&[7, 0, 0, 0], 4).unwrap();
        let teb = attach_current_thread();
        assert_eq!(current_teb(), Some(teb));
        assert_eq!(
            attach_current_thread(),
            teb,
            "attaching again keeps the TEB"
        );
        // SAFETY: our own TEB, alive for this thread.
        let t = unsafe { &*(teb as *const Teb) };
        assert_eq!(t.self_address as usize, teb);
        let (low, high) = Host::stack_bounds();
        assert_eq!((t.stack_limit, t.stack_base), (low as u64, high as u64));
        assert_eq!(t.thread_id, u64::from(Host::thread_id()));
        assert_eq!(t.process_id, u64::from(Host::process_id()));
        assert_ne!(t.peb, 0);
        // SAFETY: the TLS array has TLS_IMAGES slots; our index is below.
        let block = unsafe { *(t.tls_array as *const u64).add(index as usize) } as *const u8;
        assert!(
            !block.is_null(),
            "a block for the image registered before attaching"
        );
        // SAFETY: a block of 8 bytes: the init data, then zero fill.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(block, 8) },
            [7, 0, 0, 0, 0, 0, 0, 0]
        );
        let other = std::thread::spawn(attach_current_thread).join().unwrap();
        assert_ne!(other, teb, "another thread has a TEB of its own");
        unregister_tls(index);
    }
}
