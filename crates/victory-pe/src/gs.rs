//! The `gs:` accesses of x86-64 Windows code: how it reaches its thread's
//! TEB. `mov rax, gs:[0x30]` is `NtCurrentTeb()`; `gs:[0x58]` the TLS
//! array; `gs:[0x10]` the stack limit `__chkstk` probes down to; `gs:[0x48]`
//! the thread id; `gs:[0x68]` the last error. On a host whose `gs` is not
//! the TEB (macOS), the loader patches each one (`victory-loader`'s `teb`),
//! and `victory inspect --teb` lists an image's beforehand.
//!
//! shortcut: a byte match, not a decode, as the Windows loader's TLS
//! redirect (which found every one of a title's 53,246 `gs:[0x58]` loads at
//! an instruction start, and no false match). The absolute form `gs:[disp32]`
//! is `65 [REX] opcode ModRM(mod 0, rm 4) SIB(0x25) disp32`; a displacement
//! counts only below [`TEB_SIZE`].

/// `sizeof(TEB)` on x64 Windows 10 and 11.
pub const TEB_SIZE: u32 = 0x1838;

/// What an instruction does with its `gs:[disp]` operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GsAccess {
    /// `mov r64, gs:[disp]`.
    Load64,
    /// `mov r32, gs:[disp]`.
    Load32,
    /// `mov gs:[disp], r64`.
    Store64,
    /// `mov gs:[disp], r32`.
    Store32,
    /// Another instruction: its opcode, `0x0F00 | second byte` for a
    /// two-byte one.
    Other(u16),
}

/// One `gs:[disp32]` access in a piece of code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GsSite {
    /// Where the instruction starts in the code given.
    pub offset: usize,
    /// Its length: prefix, REX if any, opcode (two bytes if escaped), ModRM,
    /// SIB and the displacement.
    pub len: u8,
    pub access: GsAccess,
    /// The register (0 `rax` to 15 `r15`): a load's destination, a store's
    /// source; for [`GsAccess::Other`] the ModRM `reg` field as it is.
    pub reg: u8,
    /// The TEB offset accessed.
    pub disp: u32,
}

/// Every `gs:[disp32]` access in `code`, in order.
pub fn gs_sites(code: &[u8]) -> Vec<GsSite> {
    let mut sites = Vec::new();
    let mut i = 0;
    while i < code.len() {
        match gs_site(&code[i..], i) {
            Some(site) => {
                i += usize::from(site.len);
                sites.push(site);
            }
            None => i += 1,
        }
    }
    sites
}

/// The access at the start of `c`, if one starts there.
fn gs_site(c: &[u8], offset: usize) -> Option<GsSite> {
    if *c.first()? != 0x65 {
        return None;
    }
    let (rex, mut at) = match *c.get(1)? {
        r @ 0x40..=0x4F => (r, 2),
        _ => (0, 1),
    };
    let opcode = *c.get(at)?;
    at += 1;
    // Another prefix means a form this does not read (`mov ax, gs:[..]`).
    if matches!(
        opcode,
        0x26 | 0x2E | 0x36 | 0x3E | 0x64..=0x67 | 0xF0 | 0xF2 | 0xF3
    ) {
        return None;
    }
    let access = match opcode {
        0x8B if rex & 8 != 0 => GsAccess::Load64,
        0x8B => GsAccess::Load32,
        0x89 if rex & 8 != 0 => GsAccess::Store64,
        0x89 => GsAccess::Store32,
        0x0F => {
            let second = *c.get(at)?;
            at += 1;
            GsAccess::Other(0x0F00 | u16::from(second))
        }
        other => GsAccess::Other(u16::from(other)),
    };
    let modrm = *c.get(at)?;
    if modrm & 0xC7 != 0x04 || *c.get(at + 1)? != 0x25 {
        return None;
    }
    let disp = u32::from_le_bytes(c.get(at + 2..at + 6)?.try_into().ok()?);
    if disp >= TEB_SIZE {
        return None;
    }
    let reg = (modrm >> 3) & 7 | if rex & 4 != 0 { 8 } else { 0 };
    Some(GsSite {
        offset,
        len: (at + 6) as u8,
        access,
        reg,
        disp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(bytes: &[u8]) -> GsSite {
        let sites = gs_sites(bytes);
        assert_eq!(sites.len(), 1, "{bytes:02x?}: {sites:?}");
        assert_eq!(usize::from(sites[0].len), bytes.len());
        sites[0]
    }

    #[test]
    fn the_forms_windows_code_uses() {
        let s = one(&[0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]); // mov rax, gs:[0x58]
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Load64, 0, 0x58));
        let s = one(&[0x65, 0x4C, 0x8B, 0x1C, 0x25, 0x10, 0, 0, 0]); // mov r11, gs:[0x10]
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Load64, 11, 0x10));
        let s = one(&[0x65, 0x4C, 0x8B, 0x0C, 0x25, 0x28, 0, 0, 0]); // mov r9, gs:[0x28]
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Load64, 9, 0x28));
        let s = one(&[0x65, 0x8B, 0x04, 0x25, 0x48, 0, 0, 0]); // mov eax, gs:[0x48]
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Load32, 0, 0x48));
        let s = one(&[0x65, 0x44, 0x8B, 0x04, 0x25, 0x48, 0, 0, 0]); // mov r8d, gs:[0x48]
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Load32, 8, 0x48));
        let s = one(&[0x65, 0x89, 0x0C, 0x25, 0x68, 0, 0, 0]); // mov gs:[0x68], ecx
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Store32, 1, 0x68));
        let s = one(&[0x65, 0x48, 0x89, 0x04, 0x25, 0x28, 0, 0, 0]); // mov gs:[0x28], rax
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Store64, 0, 0x28));
    }

    #[test]
    fn other_instructions_are_reported_not_mistaken() {
        // movzx eax, byte ptr gs:[0x2]
        let s = one(&[0x65, 0x0F, 0xB6, 0x04, 0x25, 0x02, 0, 0, 0]);
        assert_eq!((s.access, s.disp), (GsAccess::Other(0x0FB6), 2));
        // cmp qword ptr gs:[0x10], rcx
        let s = one(&[0x65, 0x48, 0x39, 0x0C, 0x25, 0x10, 0, 0, 0]);
        assert_eq!((s.access, s.reg, s.disp), (GsAccess::Other(0x39), 1, 0x10));
    }

    #[test]
    fn what_is_not_an_absolute_teb_access() {
        // RIP-relative (mod 0, rm 5), not absolute.
        assert!(gs_sites(&[0x65, 0x48, 0x8B, 0x05, 0x58, 0, 0, 0, 0]).is_empty());
        // Past the TEB.
        assert!(gs_sites(&[0x65, 0x48, 0x8B, 0x04, 0x25, 0, 0, 1, 0]).is_empty());
        // A different segment.
        assert!(gs_sites(&[0x64, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]).is_empty());
        // An operand-size prefix after gs.
        assert!(gs_sites(&[0x65, 0x66, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]).is_empty());
        // Cut short.
        assert!(gs_sites(&[0x65, 0x48, 0x8B, 0x04, 0x25, 0x58]).is_empty());
    }

    #[test]
    fn sites_in_a_stream_keep_their_offsets() {
        let mut code = vec![0x90u8; 3];
        code.extend([0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]);
        code.extend([0xC3, 0x65]); // a lone prefix byte, then
        code.extend([0x65, 0x8B, 0x0C, 0x25, 0x48, 0, 0, 0]);
        let sites = gs_sites(&code);
        assert_eq!(sites.len(), 2);
        assert_eq!((sites[0].offset, sites[0].len), (3, 9));
        assert_eq!((sites[1].offset, sites[1].len, sites[1].reg), (14, 8, 1));
    }
}
