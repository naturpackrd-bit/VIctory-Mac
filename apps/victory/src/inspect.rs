use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use victory_pe::implib::ImportLibrary;
use victory_pe::{GsAccess, ImageInfo, layout};

pub fn run(file: &Path, list_imports: bool, list_exports: bool, list_teb: bool) -> Result<()> {
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let info = ImageInfo::parse(&data).with_context(|| format!("parsing {}", file.display()))?;

    println!("file           {}", file.display());
    println!("file size      {} bytes", data.len());
    println!("blake3         {}", blake3::hash(&data).to_hex());
    if let Some(cv) = &info.codeview {
        println!("pdb            {}  (key {})", cv.pdb_path, cv.symbol_key());
    }
    println!(
        "kind           {}",
        if info.is_dll() { "dll" } else { "exe" }
    );
    println!(
        "subsystem      {} ({})",
        info.subsystem,
        info.subsystem_name()
    );
    println!(
        "image base     {:#x}  size {:#x}",
        info.image_base, info.size_of_image
    );
    println!("entry rva      {:#x}", info.entry_rva);
    println!(
        "stack          reserve {:#x} commit {:#x}",
        info.stack_reserve, info.stack_commit
    );
    println!(
        "dll flags      {:#06x}{}",
        info.dll_characteristics,
        dll_flags(info.dll_characteristics)
    );
    println!(
        "relocations    {}   exception table {}   load config {}",
        yes(info.has_relocations),
        yes(info.has_exception_table),
        yes(info.has_load_config)
    );
    if let Some(tls) = &info.tls {
        println!("tls            {} callback(s)", tls.callback_rvas.len());
    }

    println!("\nsections");
    for s in &info.sections {
        println!(
            "  {:<8} rva {:#010x} vsize {:#010x} raw {:#010x} flags {:#010x}",
            s.name.trim_end_matches('\0'),
            s.rva,
            s.virtual_size,
            s.file_size,
            s.characteristics
        );
    }

    println!(
        "\nimports        {} functions from {} DLLs",
        info.import_count(),
        info.imports.len()
    );
    for m in &info.imports {
        println!("  {:<48} {:>5}", m.dll, m.entries.len());
        if list_imports {
            for e in &m.entries {
                println!("      {}", e.symbol);
            }
        }
    }
    if !info.delay_imports.is_empty() {
        println!("\ndelay-load imports");
        for m in &info.delay_imports {
            println!("  {:<48} {:>5}", m.dll, m.entries.len());
            if list_imports {
                for e in &m.entries {
                    println!("      {}", e.symbol);
                }
            }
        }
    }

    println!("\nexports        {}", info.exports.len());
    if list_exports {
        for e in &info.exports {
            let name = e.name.as_deref().unwrap_or("(by ordinal)");
            match (&e.rva, &e.forward) {
                (_, Some(fwd)) => println!("  {:>5}  {name} -> {fwd}", e.ordinal),
                (Some(rva), _) => println!("  {:>5}  {name}  {rva:#x}", e.ordinal),
                _ => println!("  {:>5}  {name}", e.ordinal),
            }
        }
    }
    if list_teb {
        teb_accesses(&data, &info);
    }
    Ok(())
}

/// The `gs:[...]` accesses in the image's code (how Windows code reaches its
/// thread's TEB), by kind and TEB offset: what the loader patches where the
/// host has no TEB for title code (macOS), and the forms it cannot.
fn teb_accesses(data: &[u8], info: &ImageInfo) {
    const EXECUTE: u32 = 0x2000_0000;
    let mut counts: BTreeMap<(GsAccess, u32), usize> = BTreeMap::new();
    let mut others = Vec::new();
    for s in info
        .sections
        .iter()
        .filter(|s| s.characteristics & EXECUTE != 0)
    {
        let from = s.file_offset as usize;
        let len = (s.file_size.min(s.virtual_size.max(s.file_size)) as usize)
            .min(data.len().saturating_sub(from));
        for site in victory_pe::gs_sites(&data[from..from + len]) {
            *counts.entry((site.access, site.disp)).or_default() += 1;
            if let GsAccess::Other(_) = site.access {
                others.push(format!("{:#x}", s.rva as usize + site.offset));
            }
        }
    }
    println!(
        "\nteb accesses   {} ({} kinds)",
        counts.values().sum::<usize>(),
        counts.len()
    );
    for ((access, disp), n) in &counts {
        let what = match access {
            GsAccess::Load64 => "mov r64, gs:[..]".to_owned(),
            GsAccess::Load32 => "mov r32, gs:[..]".to_owned(),
            GsAccess::Store64 => "mov gs:[..], r64".to_owned(),
            GsAccess::Store32 => "mov gs:[..], r32".to_owned(),
            GsAccess::Other(op) => format!("opcode {op:#x} (not patched)"),
        };
        println!("  {n:>7}  teb+{disp:#06x}  {what}  {}", teb_field(*disp));
    }
    if !others.is_empty() {
        let shown = others.len().min(20);
        println!(
            "  unpatched forms at rva {}{}",
            others[..shown].join(", "),
            if others.len() > shown { ", ..." } else { "" }
        );
    }
}

/// The TEB field at `disp`, for the ones title code is known to read.
fn teb_field(disp: u32) -> &'static str {
    match disp {
        0x00 => "ExceptionList",
        0x08 => "StackBase",
        0x10 => "StackLimit",
        0x28 => "ArbitraryUserPointer",
        0x30 => "Self",
        0x40 => "ClientId.UniqueProcess",
        0x48 => "ClientId.UniqueThread",
        0x58 => "ThreadLocalStoragePointer",
        0x60 => "ProcessEnvironmentBlock",
        0x68 => "LastErrorValue",
        0x1480..=0x167F => "TlsSlots",
        0x1780 => "TlsExpansionSlots",
        _ => "",
    }
}

pub fn lib_exports(lib: &Path, list: bool) -> Result<()> {
    let data = std::fs::read(lib).with_context(|| format!("reading {}", lib.display()))?;
    let parsed =
        ImportLibrary::parse(&data).with_context(|| format!("parsing {}", lib.display()))?;
    let mut by_dll: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for i in &parsed.imports {
        by_dll
            .entry(i.dll.to_ascii_lowercase())
            .or_default()
            .push(i.export.to_string());
    }
    println!(
        "{} exports from {} DLLs ({} other members)",
        parsed.imports.len(),
        by_dll.len(),
        parsed.other_members
    );
    for (dll, names) in &by_dll {
        println!("{dll:<60} {:>5}", names.len());
        if list {
            for n in names {
                println!("    {n}");
            }
        }
    }
    Ok(())
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn dll_flags(v: u16) -> String {
    use layout::dll_flags::*;
    let names = [
        (HIGH_ENTROPY_VA, "high-entropy-va"),
        (DYNAMIC_BASE, "dynamic-base"),
        (FORCE_INTEGRITY, "force-integrity"),
        (NX_COMPAT, "nx"),
        (APPCONTAINER, "appcontainer"),
        (GUARD_CF, "cfg"),
    ];
    let set: Vec<_> = names
        .iter()
        .filter(|(f, _)| v & f != 0)
        .map(|(_, n)| *n)
        .collect();
    if set.is_empty() {
        String::new()
    } else {
        format!("  ({})", set.join(", "))
    }
}
