//! What an import nobody resolves is bound to (docs/macos.md, M2a): a stub
//! that reports the call once (`dll!name`), counts it, and then follows the
//! policy, as the Windows loader's `thunks.rs` does for its mappings. Each
//! stub is a few bytes of machine code that passes its [`ImportSite`] to
//! [`report`] and returns 0; the stubs of one mapping share a block
//! ([`Stubs`]).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_platform::{Host, Memory, Protection};

/// What a stub does when called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnimplementedPolicy {
    /// Mapping fails instead, naming the imports (no stubs are made).
    Refuse,
    /// The first call of each import is reported, then the process aborts.
    Trap,
    /// Reported once, then returns 0 every time.
    Zero,
}

/// One unresolved import and how often it was called.
#[derive(Debug)]
pub struct ImportSite {
    /// The DLL as the image names it.
    pub dll: String,
    /// The function's name, or `#ordinal`.
    pub symbol: String,
    /// Calls so far.
    pub calls: AtomicU64,
    policy: UnimplementedPolicy,
}

/// Bytes a stub may take; the block holds [`BLOCK`] / `STUB` of them.
const STUB: usize = 48;
const BLOCK: usize = 64 << 10;

/// Calls of unresolved imports since the start, process-wide.
static UNIMPLEMENTED_CALLS: AtomicU64 = AtomicU64::new(0);

/// Calls of unresolved imports since the start, process-wide.
pub fn unimplemented_calls() -> u64 {
    UNIMPLEMENTED_CALLS.load(Relaxed)
}

/// The stubs of one mapping. They are reached through the IAT, by absolute
/// address, so unlike the `gs` thunks their block need not be within
/// `rel32` reach of the image: it may be anywhere.
#[derive(Debug)]
pub struct Stubs {
    block: usize,
    used: usize,
    /// Boxed so that each site keeps its address, which its stub holds,
    /// while the vector grows.
    #[allow(clippy::vec_box)]
    sites: Vec<Box<ImportSite>>,
}

impl Stubs {
    /// An empty block of stubs; `None` if there is no memory for it.
    pub fn new() -> Option<Stubs> {
        let block = Host::allocate(BLOCK, None)?;
        Some(Stubs {
            block,
            used: 0,
            sites: Vec::new(),
        })
    }

    /// The address of a new stub for `dll!symbol` that follows `policy`;
    /// `None` when the block is full, or for [`UnimplementedPolicy::Refuse`]
    /// (callers never ask then: refused imports get no stub).
    pub fn stub(&mut self, dll: &str, symbol: &str, policy: UnimplementedPolicy) -> Option<usize> {
        if policy == UnimplementedPolicy::Refuse || self.used + STUB > BLOCK {
            return None;
        }
        let site = Box::new(ImportSite {
            dll: dll.to_owned(),
            symbol: symbol.to_owned(),
            calls: AtomicU64::new(0),
            policy,
        });
        let code = encode(&raw const *site as usize);
        let at = self.block + self.used;
        // SAFETY: `STUB` bytes of our block, still writable; `code` is
        // shorter.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, code.len()) };
        self.used += STUB;
        self.sites.push(site);
        Some(at)
    }

    /// Makes the block read-execute; no stub is added after. The block's
    /// address and size, for an error.
    pub fn finish(&self) -> Result<(), (usize, usize)> {
        // SAFETY: our block, which nothing references.
        if unsafe { Host::protect(self.block, BLOCK, Protection::ReadExecute) } {
            Ok(())
        } else {
            Err((self.block, BLOCK))
        }
    }

    /// Every import that was called at least once: (dll, symbol, calls).
    pub fn called(&self) -> Vec<(&str, &str, u64)> {
        self.sites
            .iter()
            .map(|s| (s.dll.as_str(), s.symbol.as_str(), s.calls.load(Relaxed)))
            .filter(|&(_, _, calls)| calls > 0)
            .collect()
    }
}

impl Drop for Stubs {
    fn drop(&mut self) {
        // SAFETY: our block; the image whose IAT points into it is being
        // unmapped with it.
        unsafe { Host::free(self.block, BLOCK) };
    }
}

/// The machine code of the stub for `site`. It is the callee of a Win64
/// call, so the volatile registers are ours to clobber.
fn encode(site: usize) -> [u8; 33] {
    let mut code = [0u8; 33];
    // mov rcx, site
    code[0..2].copy_from_slice(&[0x48, 0xB9]);
    code[2..10].copy_from_slice(&(site as u64).to_le_bytes());
    // mov rax, report
    code[10..12].copy_from_slice(&[0x48, 0xB8]);
    let report = report as extern "win64" fn(*const ImportSite) as usize;
    code[12..20].copy_from_slice(&(report as u64).to_le_bytes());
    // sub rsp, 40: the shadow space (32) and 8 more, which bring back the
    // 16-byte alignment the caller's call broke.
    code[20..24].copy_from_slice(&[0x48, 0x83, 0xEC, 0x28]);
    // call rax
    code[24..26].copy_from_slice(&[0xFF, 0xD0]);
    // add rsp, 40
    code[26..30].copy_from_slice(&[0x48, 0x83, 0xC4, 0x28]);
    // xor eax, eax; ret
    code[30..33].copy_from_slice(&[0x31, 0xC0, 0xC3]);
    code
}

/// What every stub calls: counts the call, reports the first one, and ends
/// the process if the policy says so.
extern "win64" fn report(site: *const ImportSite) {
    // SAFETY: the stub passes the boxed site, alive as long as the `Stubs`
    // that made the stub.
    let site = unsafe { &*site };
    let n = site.calls.fetch_add(1, Relaxed) + 1;
    UNIMPLEMENTED_CALLS.fetch_add(1, Relaxed);
    if n == 1 {
        tracing::warn!(dll = %site.dll, symbol = %site.symbol, "unimplemented import called");
        eprintln!(
            "victory: unimplemented import {}!{} called",
            site.dll, site.symbol
        );
        if site.policy == UnimplementedPolicy::Trap {
            eprintln!("victory: ending the process (--unimplemented trap)");
            std::process::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    // `Trap` cannot be tested here: it aborts the test process.
    use super::*;

    type Stub = extern "win64" fn() -> u64;

    fn as_fn(at: usize) -> Stub {
        // SAFETY: a finished stub, which takes nothing and returns in rax.
        unsafe { std::mem::transmute::<usize, Stub>(at) }
    }

    #[test]
    fn a_stub_returns_zero_and_counts() {
        let mut stubs = Stubs::new().unwrap();
        let at = stubs.stub("x.dll", "F", UnimplementedPolicy::Zero).unwrap();
        stubs.finish().unwrap();
        let before = unimplemented_calls();
        let f = as_fn(at);
        assert_eq!(f(), 0);
        assert_eq!(f(), 0);
        assert_eq!(stubs.called(), vec![("x.dll", "F", 2)]);
        // Other tests' stubs may run at the same time: at least ours.
        assert!(unimplemented_calls() >= before + 2);
    }

    #[test]
    fn stubs_are_told_apart() {
        let mut stubs = Stubs::new().unwrap();
        let _first = stubs
            .stub("a.dll", "One", UnimplementedPolicy::Zero)
            .unwrap();
        let second = stubs
            .stub("b.dll", "Two", UnimplementedPolicy::Zero)
            .unwrap();
        stubs.finish().unwrap();
        assert_eq!(as_fn(second)(), 0);
        assert_eq!(stubs.called(), vec![("b.dll", "Two", 1)]);
    }
}
