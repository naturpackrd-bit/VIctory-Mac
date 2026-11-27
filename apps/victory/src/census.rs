//! Which imports do titles need, and which do we cover?

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use victory_hle::{HleResolver, HostPolicy};
use victory_loader::Provenance;
use victory_pe::{ImageInfo, ImportSymbol};

use crate::title::title_dll;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum FunctionKey {
    Name(String),
    Ordinal(u16),
}

impl From<&ImportSymbol> for FunctionKey {
    fn from(symbol: &ImportSymbol) -> Self {
        match symbol {
            ImportSymbol::Name { name, .. } => Self::Name(name.clone()),
            ImportSymbol::Ordinal(ordinal) => Self::Ordinal(*ordinal),
        }
    }
}

impl fmt::Display for FunctionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Double a literal leading '#' to distinguish names from ordinals.
            Self::Name(name) if name.starts_with('#') => write!(f, "#{name}"),
            Self::Name(name) => f.write_str(name),
            Self::Ordinal(ordinal) => write!(f, "#{ordinal}"),
        }
    }
}

/// Why an import nobody serves is missing, as `--missing` marks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    /// Neither our runtime nor the host has it, and no title DLL would.
    Nobody,
    /// The title DLL is in the folder but lacks the name, or the import is
    /// by ordinal (which `run` cannot bind from a title DLL).
    TitleDll,
    /// The DLL should come from the title's folder, which lacks it.
    DllAbsent,
}

impl fmt::Display for Mark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mark::Nobody => "",
            Mark::TitleDll => " (title dll)",
            Mark::DllAbsent => " (dll absent)",
        })
    }
}

/// Where an import is bound from, as `run` would bind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Hle,
    Host,
    Title,
    Missing(Mark),
}

/// The title DLLs of one root, as `run` takes them from the title's folder.
struct TitleDlls<'a> {
    /// Whether `run` takes the DLL from the folder (`title::title_dll`).
    wanted: &'a dyn Fn(&str) -> bool,
    /// Each image's lower-case file name, and the names it exports
    /// (forwarders included).
    exports: BTreeMap<String, BTreeSet<String>>,
}

impl TitleDlls<'_> {
    /// Where `dll!symbol` comes from: our runtime and the host first, then
    /// the title DLLs, as `run` binds it.
    fn source(&self, resolver: &HleResolver, dll: &str, symbol: &ImportSymbol) -> Source {
        match resolver.classify(dll, symbol) {
            Some(Provenance::Hle) => return Source::Hle,
            Some(Provenance::Host) => return Source::Host,
            Some(Provenance::Title) => return Source::Title,
            None if !(self.wanted)(dll) => return Source::Missing(Mark::Nobody),
            None => {}
        }
        match (self.exports.get(&dll.to_ascii_lowercase()), symbol) {
            (None, _) => Source::Missing(Mark::DllAbsent),
            (Some(names), ImportSymbol::Name { name, .. }) if names.contains(name) => Source::Title,
            (Some(_), _) => Source::Missing(Mark::TitleDll),
        }
    }
}

/// The export names of each image of `images`, by lower-case file name;
/// of two files of the same name, the first found.
fn title_exports(images: &[(PathBuf, ImageInfo)]) -> BTreeMap<String, BTreeSet<String>> {
    let mut exports = BTreeMap::new();
    for (path, info) in images {
        let Some(name) = path.file_name() else {
            continue;
        };
        let names = || info.exports.iter().filter_map(|e| e.name.clone()).collect();
        exports
            .entry(name.to_string_lossy().to_ascii_lowercase())
            .or_insert_with(names);
    }
    exports
}

/// Each missing `dll!function`: why, and the images that import it.
type MissingImports = BTreeMap<(String, FunctionKey), (Mark, BTreeSet<PathBuf>)>;

#[derive(Default)]
struct DllStats {
    functions: BTreeSet<FunctionKey>,
    hle: usize,
    host: usize,
    title: usize,
    missing: usize,
    images: BTreeSet<PathBuf>,
}

pub fn run(paths: Vec<PathBuf>, strict: bool, list_missing: bool) -> Result<()> {
    let paths = crate::default_paths(paths)?;
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&resolver.host, dll);

    let mut images = 0usize;
    let mut seen = BTreeSet::new();
    let mut failed = Vec::new();
    let mut dlls: BTreeMap<String, DllStats> = BTreeMap::new();
    let mut missing = MissingImports::new();

    for root in &paths {
        // Every image of the root first: the title DLLs among them serve
        // the imports of the others.
        let mut found = Vec::new();
        for entry in walkdir::WalkDir::new(root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.depth() == 0 => {
                    return Err(error).context("cannot scan census input");
                }
                Err(error) => {
                    failed.push(error.to_string());
                    continue;
                }
            };
            let path = entry.path();
            if !entry.file_type().is_file() || !looks_like_pe(path) {
                continue;
            }
            // Use the resolved path for deduplication and keep the original for output.
            let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            if !seen.insert(identity) {
                continue;
            }
            match std::fs::read(path)
                .map_err(anyhow::Error::from)
                .and_then(|d| ImageInfo::parse(&d).map_err(anyhow::Error::from))
            {
                Ok(info) => found.push((path.to_path_buf(), info)),
                Err(e) => failed.push(format!("{}: {e}", path.display())),
            }
        }
        let title = TitleDlls {
            wanted: &wanted,
            exports: title_exports(&found),
        };
        for (path, info) in &found {
            images += 1;
            tally_image(path, info, &resolver, &title, &mut dlls, &mut missing);
        }
    }

    println!("{images} images scanned, {} unreadable", failed.len());
    for error in &failed {
        println!("  skipped {error}");
    }
    println!(
        "an import both VIctory and a title DLL could serve is counted as hle, as `run` binds it"
    );
    println!(
        "\n{:<56} {:>6} {:>6} {:>6} {:>6} {:>8} {:>7}",
        "dll", "funcs", "hle", "host", "title", "missing", "images"
    );
    let (mut f, mut h, mut o, mut t, mut m) = (0, 0, 0, 0, 0);
    for (dll, s) in &dlls {
        println!(
            "{dll:<56} {:>6} {:>6} {:>6} {:>6} {:>8} {:>7}",
            s.functions.len(),
            s.hle,
            s.host,
            s.title,
            s.missing,
            s.images.len()
        );
        f += s.functions.len();
        h += s.hle;
        o += s.host;
        t += s.title;
        m += s.missing;
    }
    println!("{:<56} {f:>6} {h:>6} {o:>6} {t:>6} {m:>8}", "TOTAL");

    if list_missing {
        println!("\nmissing (by dll)");
        let mut by_dll: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for ((dll, function), (mark, _)) in &missing {
            by_dll
                .entry(dll)
                .or_default()
                .push(format!("{dll}!{function}{mark}"));
        }
        for (dll, functions) in by_dll {
            println!("{dll} ({})", functions.len());
            for function in functions {
                println!("  {function}");
            }
        }
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "{} files could not be read: {}",
            failed.len(),
            failed.join("; ")
        );
    }
    Ok(())
}

/// Cheap pre-filter: an `MZ` header, regardless of extension (console
/// packages do not always use .exe/.dll).
fn looks_like_pe(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"MZ"
}

fn tally_image(
    path: &Path,
    info: &ImageInfo,
    resolver: &HleResolver,
    title: &TitleDlls<'_>,
    dlls: &mut BTreeMap<String, DllStats>,
    missing: &mut MissingImports,
) {
    for module in info.imports.iter().chain(&info.delay_imports) {
        let key = module.dll.to_ascii_lowercase();
        let stats = dlls.entry(key.clone()).or_default();
        stats.images.insert(path.to_path_buf());
        for e in &module.entries {
            let source = title.source(resolver, &module.dll, &e.symbol);
            let function = FunctionKey::from(&e.symbol);
            if let Source::Missing(mark) = source {
                missing
                    .entry((key.clone(), function.clone()))
                    .or_insert_with(|| (mark, BTreeSet::new()))
                    .1
                    .insert(path.to_path_buf());
            }
            // Count each dll!function once across all images.
            if stats.functions.insert(function) {
                match source {
                    Source::Hle => stats.hle += 1,
                    Source::Host => stats.host += 1,
                    Source::Title => stats.title += 1,
                    Source::Missing(_) => stats.missing += 1,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_pe::{ImportEntry, ImportModule};

    fn entry(symbol: ImportSymbol) -> ImportEntry {
        ImportEntry {
            symbol,
            iat_slot_rva: 0,
        }
    }

    fn named(hint: u16, name: &str) -> ImportEntry {
        entry(ImportSymbol::Name {
            hint,
            name: name.into(),
        })
    }

    fn ordinal(ord: u16) -> ImportEntry {
        entry(ImportSymbol::Ordinal(ord))
    }

    /// A folder without title DLLs, none wanted from it.
    fn no_title_dlls() -> TitleDlls<'static> {
        TitleDlls {
            wanted: &|_| false,
            exports: BTreeMap::new(),
        }
    }

    fn synthetic_image(imports: Vec<ImportModule>, delay_imports: Vec<ImportModule>) -> ImageInfo {
        ImageInfo {
            machine: 0,
            characteristics: 0,
            timestamp: 0,
            subsystem: 0,
            dll_characteristics: 0,
            image_base: 0,
            size_of_image: 0,
            entry_rva: 0,
            stack_reserve: 0,
            stack_commit: 0,
            sections: Vec::new(),
            imports,
            delay_imports,
            exports: Vec::new(),
            tls: None,
            has_relocations: false,
            has_exception_table: false,
            exception_table: None,
            has_load_config: false,
            codeview: None,
        }
    }

    #[test]
    fn deduplicates_imports_ignoring_hint_across_images_and_delay_imports() {
        let resolver = HleResolver::new(HostPolicy::none());
        let mut dlls: BTreeMap<String, DllStats> = BTreeMap::new();
        let mut missing = MissingImports::new();

        let image_a = synthetic_image(
            vec![ImportModule {
                dll: "XGameRuntime.dll".into(),
                entries: vec![
                    named(1, "XGameRuntimeInitialize"),
                    named(10, "SyntheticMissingApi"),
                    ordinal(42),
                ],
            }],
            vec![ImportModule {
                dll: "XGameRuntime.dll".into(),
                entries: vec![
                    named(2, "XGameRuntimeInitialize"),
                    named(20, "SyntheticMissingApi"),
                ],
            }],
        );
        let image_b = synthetic_image(
            vec![ImportModule {
                dll: "xgameruntime.dll".into(),
                entries: vec![
                    named(99, "XGameRuntimeInitialize"),
                    named(200, "SyntheticMissingApi"),
                    ordinal(42),
                ],
            }],
            vec![ImportModule {
                dll: "XGameRuntime.dll".into(),
                entries: vec![named(300, "SyntheticMissingApi"), ordinal(42)],
            }],
        );

        let title = no_title_dlls();
        tally_image(
            Path::new("image_a.exe"),
            &image_a,
            &resolver,
            &title,
            &mut dlls,
            &mut missing,
        );
        tally_image(
            Path::new("image_b.dll"),
            &image_b,
            &resolver,
            &title,
            &mut dlls,
            &mut missing,
        );

        assert_eq!(dlls.len(), 1);
        let stats = &dlls["xgameruntime.dll"];
        assert_eq!(stats.functions.len(), 3);
        assert_eq!(stats.hle, 1);
        assert_eq!(stats.host, 0);
        assert_eq!(stats.missing, 2);
        assert_eq!(stats.images.len(), 2);
        assert_eq!(
            dlls.values().map(|s| s.missing).sum::<usize>(),
            missing.len()
        );
        let user_count = |symbol| missing[&("xgameruntime.dll".into(), symbol)].1.len();
        assert_eq!(user_count(FunctionKey::Name("SyntheticMissingApi".into())), 2);
        assert_eq!(user_count(FunctionKey::Ordinal(42)), 2);
    }

    #[test]
    fn distinguishes_missing_named_and_ordinal_imports() {
        let resolver = HleResolver::new(HostPolicy::none());
        let mut dlls = BTreeMap::new();
        let mut missing = BTreeMap::new();
        let dll = "census-fixture.dll";
        let module = |entry| ImportModule {
            dll: dll.into(),
            entries: vec![entry],
        };
        let image_a = synthetic_image(vec![module(named(1, "#42"))], vec![module(named(2, "#42"))]);
        let image_b = synthetic_image(vec![module(ordinal(42))], vec![]);
        let title = no_title_dlls();
        for (path, image) in [("a.exe", &image_a), ("b.exe", &image_b)] {
            tally_image(
                Path::new(path),
                image,
                &resolver,
                &title,
                &mut dlls,
                &mut missing,
            );
        }

        assert_eq!(dlls[dll].missing, 2);
        let mut rows: Vec<_> = missing
            .into_iter()
            .map(|((_, symbol), (_, users))| (symbol.to_string(), users))
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            [
                ("##42".into(), BTreeSet::from([PathBuf::from("a.exe")])),
                ("#42".into(), BTreeSet::from([PathBuf::from("b.exe")])),
            ]
        );
    }

    #[test]
    fn counts_distinct_symbols_and_dlls_separately() {
        let resolver = HleResolver::new(HostPolicy::none());
        let mut dlls: BTreeMap<String, DllStats> = BTreeMap::new();
        let mut missing = MissingImports::new();

        let image = synthetic_image(
            vec![
                ImportModule {
                    dll: "XGameRuntime.dll".into(),
                    entries: vec![
                        named(0, "XGameRuntimeInitialize"),
                        named(0, "SyntheticMissingA"),
                        named(0, "SyntheticMissingB"),
                        ordinal(7),
                        ordinal(8),
                    ],
                },
                ImportModule {
                    dll: "SyntheticExtra.dll".into(),
                    entries: vec![named(0, "SyntheticMissingA"), ordinal(7)],
                },
            ],
            Vec::new(),
        );

        tally_image(
            Path::new("single.exe"),
            &image,
            &resolver,
            &no_title_dlls(),
            &mut dlls,
            &mut missing,
        );

        assert_eq!(dlls.len(), 2);
        let xgr = &dlls["xgameruntime.dll"];
        assert_eq!(xgr.functions.len(), 5);
        assert_eq!(xgr.hle, 1);
        assert_eq!(xgr.missing, 4);
        assert_eq!(xgr.images.len(), 1);

        let extra = &dlls["syntheticextra.dll"];
        assert_eq!(extra.functions.len(), 2);
        assert_eq!(extra.hle, 0);
        assert_eq!(extra.missing, 2);
        assert_eq!(extra.images.len(), 1);

        assert_eq!(
            dlls.values().map(|s| s.missing).sum::<usize>(),
            missing.len()
        );
    }

    /// As `run` binds them: our runtime first (even where a title DLL
    /// exports the name too), then the title DLLs in the folder by name.
    #[test]
    fn title_dlls_of_the_folder_serve_what_they_export() {
        let resolver = HleResolver::new(HostPolicy::none());
        let wanted = |dll: &str| title_dll(&resolver.host, dll);
        let exe = synthetic_image(
            vec![
                ImportModule {
                    dll: "XGameRuntime.dll".into(),
                    entries: vec![named(0, "XGameRuntimeInitialize")],
                },
                ImportModule {
                    dll: "SyntheticTitle.DLL".into(),
                    entries: vec![named(0, "Exported"), named(0, "Lacking"), ordinal(5)],
                },
                ImportModule {
                    dll: "SyntheticAbsent.dll".into(),
                    entries: vec![named(0, "Anything")],
                },
            ],
            Vec::new(),
        );
        let mut dll = synthetic_image(Vec::new(), Vec::new());
        dll.exports = ["Exported", "XGameRuntimeInitialize"]
            .into_iter()
            .map(|name| victory_pe::ExportInfo {
                name: Some(name.into()),
                ordinal: 5,
                rva: Some(0x1000),
                forward: None,
            })
            .collect();
        let found = [
            (PathBuf::from("game/game.exe"), exe),
            (PathBuf::from("game/synthetictitle.dll"), dll.clone()),
            (PathBuf::from("game/XGameRuntime.dll"), dll),
        ];
        let title = TitleDlls {
            wanted: &wanted,
            exports: title_exports(&found),
        };
        let mut dlls = BTreeMap::new();
        let mut missing = MissingImports::new();
        tally_image(
            &found[0].0,
            &found[0].1,
            &resolver,
            &title,
            &mut dlls,
            &mut missing,
        );

        let counts = |dll: &str| {
            let s = &dlls[dll];
            (s.hle, s.title, s.missing)
        };
        assert_eq!(counts("xgameruntime.dll"), (1, 0, 0));
        assert_eq!(counts("synthetictitle.dll"), (0, 1, 2));
        assert_eq!(counts("syntheticabsent.dll"), (0, 0, 1));
        let marks: Vec<_> = missing
            .iter()
            .map(|((dll, function), (mark, _))| format!("{dll}!{function}{mark}"))
            .collect();
        assert_eq!(
            marks,
            [
                "syntheticabsent.dll!Anything (dll absent)",
                "synthetictitle.dll!Lacking (title dll)",
                "synthetictitle.dll!#5 (title dll)",
            ]
        );
    }
}
