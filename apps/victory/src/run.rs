use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use crate::{Console, Unimplemented};

const SKIP_CPU_CHECK_ENV: &str = "VICTORY_SKIP_CPU_CHECK";

fn skip_cpu_check(value: Option<&str>) -> bool {
    value == Some("1")
}

fn check_cpu_features_with(has_avx2: bool, skip_env: Option<&str>) -> Result<()> {
    if has_avx2 || skip_cpu_check(skip_env) {
        return Ok(());
    }
    anyhow::bail!(
        "this CPU does not support AVX2, which Xbox Series titles require; \
         set {SKIP_CPU_CHECK_ENV}=1 to bypass this check (for example under CPU emulation such as Intel SDE)"
    );
}

/// Pre-flight check before executing title code: Xbox Series binaries are
/// compiled for Zen 2 and require AVX2.
fn check_cpu_features() -> Result<()> {
    #[cfg(target_arch = "x86_64")]
    let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let has_avx2 = true;
    let skip = std::env::var(SKIP_CPU_CHECK_ENV).ok();
    check_cpu_features_with(has_avx2, skip.as_deref())
}

#[cfg(windows)]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    console: Console,
) -> Result<()> {
    use std::sync::Arc;
    use victory_hle::{HleResolver, HostPolicy};
    use victory_loader::{BindReport, UnimplementedPolicy, WithTitleDlls};

    if execute {
        check_cpu_features()?;
    }
    // A title that relaunched itself: the old process must be gone first.
    victory_hle::modules::lifecycle::wait_for_previous();
    let cfg = victory_core::Config::load()?;
    let original = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let prepared = victory_loader::prepare(&original)?;
    let hash = prepared.original_hash.to_hex();
    match victory_game::identify(&hash) {
        Some(build) => println!("build          {} ({hash})", build.label),
        None => println!("build          unknown ({hash})"),
    }

    let images = cfg.cache_subdir("images")?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let image = victory_loader::load(&prepared, &images)?;
    // The program is mapped from the copy `load` wrote: the file's bytes
    // and their patched copy are not read again, and were held for the
    // whole run (two blocks of 114.7 MB each for the title, in
    // physical memory: run 65's memory map).
    let image_name = prepared.file_name();
    drop(prepared);
    drop(original);
    victory_hle::runtime::set_main_image(image.base());
    victory_hle::runtime::set_image_path(image.base(), exe);
    println!(
        "mapped at      {:#x} (preferred {:#x})",
        image.base(),
        image.info().image_base
    );
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&resolver.host, dll);
    let title_dir = exe.parent().unwrap_or(Path::new("."));
    // A missing manifest leaves id 0 and no storage quota.
    let manifest =
        std::fs::read_to_string(title_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let title = victory_hle::runtime::Title::from_manifest(&manifest);
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    println!(
        "title          {:08X}, storage {} ({} MB)",
        title.id,
        storage.display(),
        title.storage_bytes >> 20
    );
    // The console's fixed drives: the package as `G:`, temporary files `T:`.
    victory_hle::modules::files::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::modules::files::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());
    let dlls = victory_loader::load_title_dlls(image.info(), title_dir, &images, &wanted)?;
    for (name, dll) in &dlls {
        println!("title dll      {name} at {:#x}", dll.base());
        victory_hle::runtime::set_image_path(dll.base(), &title_dir.join(name));
    }
    let imported: Vec<&str> = image
        .info()
        .imports
        .iter()
        .map(|m| m.dll.as_str())
        .collect();
    let mapped: Vec<&str> = dlls.iter().map(|(name, _)| name.as_str()).collect();
    let absent = absent_title_dlls(&imported, &mapped, &wanted);
    if !absent.is_empty() {
        let message = format!(
            "{} not in {}, and neither the host nor VIctory provides {}: the package is \
             incomplete; copy every file of the package next to the executable",
            absent.join(", "),
            title_dir.display(),
            if absent.len() == 1 { "it" } else { "them" },
        );
        println!("missing dlls   {message}");
        // None of their imports can be bound, so the title stops at its first
        // call into one (Grand Theft Auto VI 1.0.2.1 at +0x2237178 during
        // System Init, #930) with nothing saying why. Stubs only if asked for.
        if execute && matches!(unimplemented, Unimplemented::Trap) {
            anyhow::bail!(
                "the title's own DLLs are missing: {message} (`--unimplemented zero` runs it anyway)"
            );
        }
    }

    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };
    let with_dlls = WithTitleDlls {
        inner: &resolver,
        dlls: &dlls,
    };
    let show = |name: &str, report: BindReport| {
        println!(
            "bound          {name}: {} hle, {} host, {} title, {} missing",
            report.hle,
            report.host,
            report.title,
            report.missing.len()
        );
        for m in &report.missing {
            println!("  missing      {}!{}", m.dll, m.symbol);
        }
    };
    for (name, dll) in &dlls {
        show(name, dll.bind_imports(&with_dlls, policy)?);
    }
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    show(&exe_name, image.bind_imports(&with_dlls, policy)?);
    if execute {
        let size = image.info().size_of_image as usize;
        // SAFETY: the image stays mapped, and nothing of it has run yet.
        unsafe { victory_loader::write_sites::apply_asked(&image_name, image.base(), size) };
    }
    // Renderer port #2 (VICTORY_PORT_DESCRIPTORS=1): the title's stores of
    // descriptor records tell the layer their slots.
    #[cfg(windows)]
    if execute && victory_hle::port_descriptors::on() {
        use victory_loader::descriptor_sites::{Hooked, SITES, apply};
        let size = image.info().size_of_image as usize;
        // SAFETY: the image stays mapped, nothing of it has run yet, and the
        // marks are a static.
        let done = unsafe { apply(image.base(), size, victory_hle::port_descriptors::marks_address(), &SITES) };
        let patched = done.iter().filter(|d| **d == Hooked::Done).count();
        println!("renderer port 2: {patched} of {} descriptor record stores of the title patched", SITES.len());
        victory_hle::port_descriptors::hooked(image.base(), size, patched);
        // VICTORY_PORT_COPIES=1: the frame allocator's table copies the six
        // stores miss (its own memcpy, a 128-byte copy) mark their slots too.
        // A third fewer records left to the fallback (runs pop-4/pop-5), no
        // change seen in the picture (POPIN.md): off by default.
        if std::env::var("VICTORY_PORT_COPIES").is_ok_and(|v| v.trim() == "1") {
            use victory_loader::descriptor_sites::{TITLE_MEMCPY, apply_copies};
            let hook = victory_hle::port_descriptors::title_copy_hook(image.base() + TITLE_MEMCPY);
            // SAFETY: as above; the hook is a function of ours.
            let done = unsafe { apply_copies(image.base(), size, victory_hle::port_descriptors::marks_address(), hook) };
            let copies = done.iter().filter(|d| **d == Hooked::Done).count();
            println!("renderer port 2: {copies} of {} table copies of the title patched", done.len());
        }
    }
    // The frame rate (VICTORY_FRAME_INTERVAL_MS, off without it): our frame
    // clock ticks at that interval (victory-gpu), and the title is given
    // the matching rate in place of its setting's 30 (VICTORY_TITLE_FPS
    // overrides it; `off` leaves the title alone).
    #[cfg(windows)]
    if execute && let Some(ms) = victory_loader::frame_rate::asked_interval_ms() {
        use victory_loader::frame_rate::{Patched, SITES, apply, title_interval_us, title_rate};
        let asked = std::env::var("VICTORY_TITLE_FPS").ok();
        match title_rate(ms, asked.as_deref()) {
            Some(rate) => {
                let size = image.info().size_of_image as usize;
                // SAFETY: the image stays mapped and nothing of it has run yet.
                let done = unsafe { apply(image.base(), size, rate, &SITES) };
                let patched = done.iter().filter(|d| **d == Patched::Done).count();
                println!(
                    "frame rate: our clock {ms} ms; the title's rate {rate} ({patched} of {} reads of its setting patched; it asks for {} us at 60 Hz)",
                    SITES.len(),
                    title_interval_us(rate, false)
                );
            }
            None => println!("frame rate: our clock {ms} ms; the title's own rate (30) left as it is"),
        }
    }
    // The output size and render scale (VICTORY_OUTPUT, VICTORY_RENDER_SCALE,
    // VICTORY_UPSCALER; each off without it).
    #[cfg(windows)]
    if execute {
        let size = image.info().size_of_image as usize;
        // SAFETY: the image stays mapped and nothing of it has run yet.
        unsafe { victory_loader::resolution::apply_asked(image.base(), size) };
    }
    if !execute {
        println!("not executing (pass --execute to start the title)");
        return Ok(());
    }
    victory_loader::report_crashes();
    victory_loader::set_before_thread_shutdown(victory_hle::known_pipelines::flush);
    victory_loader::log_crashes_to(temp.with_file_name("crashes.log"));
    victory_hle::runtime::use_console_timers();
    #[cfg(feature = "diagnostics")]
    diagnostics(&image, &cfg);
    let late = LateDlls {
        host: resolver.host.clone(),
        title_dir: title_dir.to_owned(),
        cache_dir: images,
        policy,
        mapped: std::sync::Mutex::new(dlls.clone()),
        lock: LoaderLock::default(),
    };
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        late.load(file, show).map_err(|e| format!("{e:#}"))
    }));
    // SAFETY: imports are bound; from here on we run title code.
    let main = unsafe { Arc::new(image).spawn_main(dlls)? };
    let code = main
        .join()
        .map_err(|_| anyhow::anyhow!("title main thread panicked"))??;
    victory_hle::known_pipelines::flush();
    println!("title exited with {code}");
    Ok(())
}

/// The debugging aids (README): scheduled stack dumps, watched values and
/// memory saved on a crash.
#[cfg(all(windows, feature = "diagnostics"))]
fn diagnostics(image: &victory_loader::LoadedImage, cfg: &victory_core::Config) {
    victory_loader::dump_stacks_on_schedule();
    victory_loader::profile_on_schedule(victory_hle::frame_phase);
    victory_hle::modules::audio::capture::start();
    watch(image.base(), image.info().size_of_image as usize);
    timers(image.base(), image.info().size_of_image as usize);
    poke(image.base(), image.info().size_of_image as usize);
    // Debugging aid: `VICTORY_CRASH_SAVE=<rva>:<len>,...` (hex) writes those
    // title ranges to `crash-<rva>.bin` in the cache dir on a crash.
    let size = image.info().size_of_image as usize;
    let ranges = std::env::var("VICTORY_CRASH_SAVE").unwrap_or_default();
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    victory_loader::save_on_crash(
        ranges
            .split(',')
            .filter_map(|r| r.split_once(':'))
            .filter_map(|(rva, len)| Some((hex(rva)?, hex(len)?)))
            .filter(|&(rva, len)| rva.checked_add(len).is_some_and(|end| end <= size))
            .map(|(rva, len)| {
                let file = cfg.cache_dir.join(format!("crash-{rva:x}.bin"));
                (image.base() + rva, len, file)
            })
            .collect(),
    );
}

/// Debugging aid: `VICTORY_WATCH=<rva>,...` (hex) logs the u32 at each
/// title+rva whenever it changes, polled every 5 ms (an engine's state, say).
#[cfg(all(windows, feature = "diagnostics"))]
fn watch(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_WATCH") else {
        return;
    };
    let rvas: Vec<usize> = list
        .split(',')
        .filter_map(|r| usize::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&rva| rva + 4 <= size)
        .collect();
    std::thread::spawn(move || {
        let mut seen = vec![None; rvas.len()];
        loop {
            for (&rva, seen) in rvas.iter().zip(&mut seen) {
                // SAFETY: four bytes inside the mapped image, checked above.
                let value = unsafe { ((base + rva) as *const u32).read_volatile() };
                if *seen != Some(value) {
                    eprintln!("VIctory: title+{rva:#x} = {value:#x}");
                    *seen = Some(value);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

/// Debugging aid: `VICTORY_TIMER_LOG=1` logs the title's timers once a
/// second (`victory_loader::frame_rate`): its frame time and limiter flag,
/// and each timer's two times, step in seconds and in milliseconds. Whether
/// game time keeps to the wall clock at any frame rate is read from these.
#[cfg(all(windows, feature = "diagnostics"))]
fn timers(base: usize, size: usize) {
    use victory_loader::frame_rate::{FRAME_US, LIMITER_ON, TIMERS};
    if std::env::var("VICTORY_TIMER_LOG").ok().as_deref() != Some("1") {
        return;
    }
    if TIMERS.iter().chain([&FRAME_US, &LIMITER_ON]).any(|&rva| rva + 0x14 > size) {
        return;
    }
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let u32_at = |rva: usize| {
            // SAFETY: inside the mapped image (checked above), title data.
            unsafe { ((base + rva) as *const u32).read_volatile() }
        };
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let timers: Vec<String> = TIMERS
                .iter()
                .map(|&t| {
                    let step = f32::from_bits(u32_at(t + 0xc));
                    format!("{} {} {step:.4} {}", u32_at(t), u32_at(t + 4), u32_at(t + 0x10))
                })
                .collect();
            eprintln!(
                "VIctory: the title's timers at {} ms: frame_us {} limiter {} frames {}; timers (time ms, frames, step s, step ms): {}",
                start.elapsed().as_millis(),
                u32_at(FRAME_US),
                u32_at(LIMITER_ON) & 0xff,
                victory_hle::frames_begun(),
                timers.join(" | ")
            );
        }
    });
}

/// Debugging aid: `VICTORY_POKE=<rva>:<byte>,...` (hex) writes the byte at
/// each title+rva twice a second. A title that ends itself when its main
/// loop has not ticked for a while (Grand Theft Auto VI: 30 s) runs on
/// through slower frames with its tick flag poked. The address must be
/// writable title data.
#[cfg(all(windows, feature = "diagnostics"))]
fn poke(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_POKE") else {
        return;
    };
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    let pokes: Vec<(usize, u8)> = list
        .split(',')
        .filter_map(|p| p.split_once(':'))
        .filter_map(|(rva, byte)| Some((hex(rva)?, u8::try_from(hex(byte)?).ok()?)))
        .filter(|&(rva, _)| rva < size)
        .collect();
    std::thread::spawn(move || {
        loop {
            for &(rva, byte) in &pokes {
                // SAFETY: a byte inside the mapped image, checked above.
                unsafe { ((base + rva) as *mut u8).write_volatile(byte) };
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
}

/// DLLs the host or our runtime provide are never taken from the title.
fn title_dll(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// The DLLs among `imported` (the main image's static imports) that must
/// come from the title's folder (`wanted`) but were not mapped from it:
/// none of their imports can be bound. Each named once, case ignored.
fn absent_title_dlls(
    imported: &[&str],
    mapped: &[&str],
    wanted: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut absent: Vec<String> = Vec::new();
    for &dll in imported {
        let known = |list: &[&str]| list.iter().any(|m| m.eq_ignore_ascii_case(dll));
        if wanted(dll) && !known(mapped) && !absent.iter().any(|a| a.eq_ignore_ascii_case(dll)) {
            absent.push(dll.to_owned());
        }
    }
    absent
}

/// Title DLLs the title loads itself (`LoadLibrary`) that nothing imports,
/// e.g. The title's upscaler behind its FidelityFX loader.
#[cfg(windows)]
struct LateDlls {
    host: victory_hle::HostPolicy,
    title_dir: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    policy: victory_loader::UnimplementedPolicy,
    /// Every title DLL mapped so far, to bind new ones against.
    mapped: std::sync::Mutex<Vec<(String, victory_loader::LoadedImage)>>,
    lock: LoaderLock,
}

#[cfg(windows)]
impl LateDlls {
    /// Every `LoadLibrary` of the title comes here first. Returns a title
    /// image called `file`, mapping it and the title DLLs it needs first,
    /// binding them and running their `DllMain` on the calling thread, as
    /// the Windows loader would; `None` if `file` is no title image.
    fn load(
        &self,
        file: &str,
        show: impl Fn(&str, victory_loader::BindReport),
    ) -> Result<Option<usize>> {
        use victory_hle::runtime::{forget_image, image_named, set_image_path};
        // Waits while another thread initialises a DLL; a `DllMain` that
        // loads another DLL re-enters.
        self.lock.hold(|| {
            if let Some(base) = image_named(file) {
                return Ok(Some(base));
            }
            let new = {
                let mut mapped = self.mapped.lock().unwrap_or_else(|e| e.into_inner());
                let wanted = |dll: &str| title_dll(&self.host, dll) && image_named(dll).is_none();
                let new = victory_loader::load_title_dll(
                    file,
                    &self.title_dir,
                    &self.cache_dir,
                    &wanted,
                )?;
                let hle = victory_hle::HleResolver::new(self.host.clone());
                let old = victory_loader::WithTitleDlls {
                    inner: &hle,
                    dlls: &mapped,
                };
                let resolver = victory_loader::WithTitleDlls {
                    inner: &old,
                    dlls: &new,
                };
                for (name, dll) in &new {
                    println!(
                        "title dll      {name} at {:#x} (loaded by the title)",
                        dll.base()
                    );
                    show(name, dll.bind_imports(&resolver, self.policy)?);
                    set_image_path(dll.base(), &self.title_dir.join(name));
                }
                // Visible before their `DllMain`s run, which may load more.
                mapped.extend(new.iter().cloned());
                new
            };
            let failed = new.iter().enumerate().find_map(|(i, (name, dll))| {
                // SAFETY: bound above, dependencies attached first.
                let attached = unsafe { dll.attach() };
                attached
                    .err()
                    .map(|e| (i, e.context(format!("attaching {name}"))))
            });
            let Some((i, error)) = failed else {
                return Ok(new.last().map(|(_, dll)| dll.base()));
            };
            // As `LoadLibrary` does when a `DllMain` fails: detach what this
            // load attached (the failed one too) and unmap all of it.
            let bases: Vec<_> = new.iter().map(|(_, d)| d.base()).collect();
            self.mapped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(_, d)| !bases.contains(&d.base()));
            for (_, done) in new[..=i].iter().rev() {
                // SAFETY: attached (or failing to) on this thread just now.
                unsafe { done.detach() };
            }
            for (_, dll) in new {
                forget_image(dll.base());
                // SAFETY: detached, and no title code holds it yet.
                unsafe { dll.unload() };
            }
            Err(error)
        })
    }
}

/// The Windows loader lock's shape: one thread at a time, re-entered by
/// the thread that holds it.
#[derive(Default)]
struct LoaderLock {
    owner: std::sync::Mutex<(Option<std::thread::ThreadId>, usize)>,
    free: std::sync::Condvar,
}

impl LoaderLock {
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        let me = std::thread::current().id();
        let lock = || self.owner.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut owner = lock();
            while owner.0.is_some_and(|o| o != me) {
                owner = self.free.wait(owner).unwrap_or_else(|e| e.into_inner());
            }
            *owner = (Some(me), owner.1 + 1);
        }
        let result = f();
        let mut owner = lock();
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            self.free.notify_one();
        }
        result
    }
}

/// The loader lock of the title's process: held while a `DllMain` runs
/// (start-up and late loads) and while a thread's start or end is told to
/// the images, as Windows holds its own, so a thread starting during an
/// initialisation waits for it.
#[cfg(not(windows))]
static LOADER_LOCK: LoaderLock = LoaderLock {
    owner: std::sync::Mutex::new((None, 0)),
    free: std::sync::Condvar::new(),
};

#[cfg(test)]
mod absent_tests {
    use super::absent_title_dlls;

    #[test]
    fn title_dlls_missing_from_the_folder_are_named_once() {
        // The host provides kernel32, our runtime d3d12_xs; the rest are the
        // title's own (#930: the four DLLs beside the executable missing).
        let wanted = |dll: &str| {
            !["KERNEL32.dll", "d3d12_xs.dll"]
                .iter()
                .any(|h| h.eq_ignore_ascii_case(dll))
        };
        let imported = [
            "KERNEL32.dll",
            "d3d12_xs.dll",
            "XboxBvhBuilder.dll",
            "MSVCP140.dll",
            "Bink2Scarlett.dll",
            "xboxbvhbuilder.dll",
        ];
        let mapped = ["msvcp140.dll"];
        assert_eq!(
            absent_title_dlls(&imported, &mapped, &wanted),
            ["XboxBvhBuilder.dll", "Bink2Scarlett.dll"]
        );
    }

    #[test]
    fn a_complete_package_has_nothing_absent() {
        let wanted = |dll: &str| dll.starts_with("Title");
        assert!(absent_title_dlls(&["Title.dll", "Host.dll"], &["TITLE.DLL"], &wanted).is_empty());
        assert!(absent_title_dlls(&[], &[], &wanted).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    #[test]
    fn loader_lock_is_reentrant_and_exclusive() {
        let lock = Arc::new(super::LoaderLock::default());
        assert_eq!(lock.hold(|| lock.hold(|| 7)), 7);
        let order = Arc::new(Mutex::new(Vec::new()));
        let other = lock.hold(|| {
            let (theirs, their_order) = (lock.clone(), order.clone());
            let other = std::thread::spawn(move || {
                theirs.hold(|| their_order.lock().unwrap().push(2));
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            order.lock().unwrap().push(1);
            other
        });
        other.join().unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }

    #[test]
    fn cpu_feature_check_matches_host() {
        let skip = std::env::var(super::SKIP_CPU_CHECK_ENV).ok();
        #[cfg(target_arch = "x86_64")]
        let expected_ok =
            std::arch::is_x86_feature_detected!("avx2") || skip.as_deref() == Some("1");
        #[cfg(not(target_arch = "x86_64"))]
        let expected_ok = true;
        assert_eq!(super::check_cpu_features().is_ok(), expected_ok);
    }
}

#[cfg(test)]
mod cpu_check_tests {
    use super::{SKIP_CPU_CHECK_ENV, check_cpu_features_with};

    #[test]
    fn avx2_present_passes() {
        assert!(check_cpu_features_with(true, None).is_ok());
        assert!(check_cpu_features_with(true, Some("0")).is_ok());
        assert!(check_cpu_features_with(true, Some("1")).is_ok());
    }

    #[test]
    fn avx2_missing_fails_with_clear_error() {
        for skip in [None, Some(""), Some("0"), Some("true")] {
            let err = check_cpu_features_with(false, skip).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("AVX2"), "unexpected error: {msg}");
            assert!(msg.contains("Xbox Series"), "unexpected error: {msg}");
            assert!(msg.contains(SKIP_CPU_CHECK_ENV), "unexpected error: {msg}");
        }
    }

    #[test]
    fn skip_env_var_bypasses_missing_avx2() {
        assert!(check_cpu_features_with(false, Some("1")).is_ok());
    }
}

/// Off Windows (docs/macos.md, M2b) the title is mapped by our own loader
/// (`victory_loader::map`): its DLLs, dependencies first, then the
/// executable, each bound against our runtime and the DLLs before it, and
/// started on a thread of its own with the stack it asks for.
#[cfg(not(windows))]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    console: Console,
) -> Result<()> {
    use victory_hle::{HleResolver, HostPolicy};
    use victory_loader::stubs::UnimplementedPolicy;

    // The title's addresses below 4 GiB are kept for it first, while as
    // little as can be of ours is mapped (a tools build of a test binary may
    // have none free).
    let free: usize = victory_platform::low::reserve().iter().map(|r| r.len).sum();
    if execute {
        check_cpu_features()?;
    }
    let cfg = victory_core::Config::load()?;
    let bytes = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let info = victory_pe::ImageInfo::parse(&bytes)?;
    let hash = blake3::hash(&bytes).to_hex();
    match victory_game::identify(&hash) {
        Some(build) => println!("build          {} ({hash})", build.label),
        None => println!("build          unknown ({hash})"),
    }
    if info.is_dll() {
        anyhow::bail!("{} is a DLL, not a title executable", exe.display());
    }
    println!(
        "low memory     {:.2} GiB reserved below 4 GiB",
        free as f64 / (1u64 << 30) as f64
    );
    if !victory_loader::faults::install() {
        anyhow::bail!("installing the fault handlers failed");
    }

    let hle = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&hle.host, dll);
    let title_dir = exe
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // A missing manifest leaves id 0 and no storage quota.
    let manifest =
        std::fs::read_to_string(title_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let title = victory_hle::runtime::Title::from_manifest(&manifest);
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    println!(
        "title          {:08X}, storage {} ({} MB)",
        title.id,
        storage.display(),
        title.storage_bytes >> 20
    );
    // The console's fixed drives: the package as `G:`, temporary files `T:`.
    victory_hle::modules::win32::files::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::modules::win32::files::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());

    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::Zero,
    };
    let mut dlls = Vec::new();
    for module in &info.imports {
        if wanted(&module.dll) {
            let mut visiting = Vec::new();
            map_dll(
                &module.dll,
                title_dir,
                &hle,
                &wanted,
                policy,
                &mut dlls,
                &mut visiting,
            )?;
        }
    }
    let imported: Vec<&str> = info.imports.iter().map(|m| m.dll.as_str()).collect();
    let mapped: Vec<&str> = dlls.iter().map(|(name, _)| name.as_str()).collect();
    let absent = absent_title_dlls(&imported, &mapped, &wanted);
    if !absent.is_empty() {
        let message = format!(
            "{} not in {}, and neither the host nor VIctory provides {}: the package is \
             incomplete; copy every file of the package next to the executable",
            absent.join(", "),
            title_dir.display(),
            if absent.len() == 1 { "it" } else { "them" },
        );
        println!("missing dlls   {message}");
        if execute && matches!(unimplemented, Unimplemented::Trap) {
            anyhow::bail!(
                "the title's own DLLs are missing: {message} (`--unimplemented zero` runs it anyway)"
            );
        }
    }

    let exe_name = exe
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let resolver = TitleResolver::new(&hle, &dlls);
    let image = MappedImage::map_with(&bytes, &exe_name, &resolver, policy)
        .with_context(|| format!("mapping {}", exe.display()))?;
    resolver.show(&exe_name);
    drop(bytes);
    // Never unmapped: title code may run until the process ends.
    let exe_image: &'static MappedImage = Box::leak(Box::new(image));
    victory_hle::runtime::set_main_image(exe_image.base());
    victory_hle::runtime::set_image_path(exe_image.base(), &title_path(&exe_name));
    println!(
        "mapped at      {:#x} (preferred {:#x})",
        exe_image.base(),
        info.image_base
    );
    for (name, dll) in &dlls {
        println!("title dll      {name} at {:#x}", dll.base());
        victory_hle::runtime::set_image_path(dll.base(), &title_path(name));
    }
    if info.stack_reserve > 0 {
        victory_hle::modules::win32::threads::set_default_stack_size(info.stack_reserve as usize);
    }
    if let Some(at) = exe_image.export("__XMemAllocationHooks__") {
        victory_hle::modules::xmem::set_allocation_hooks(at);
    }
    let images = Images {
        exe: exe_image,
        dlls: dlls.clone(),
        attached: std::collections::HashSet::new(),
    };
    if IMAGES.set(std::sync::Mutex::new(images)).is_err() {
        anyhow::bail!("a title was mapped already");
    }
    victory_hle::modules::win32::threads::set_thread_notifier(notify_images);
    victory_hle::runtime::set_export_lookup(lookup);
    let late_dir = title_dir.to_owned();
    let late_hle = HleResolver::new(hle.host.clone());
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        late_load(file, &LOADER_LOCK, &late_dir, &late_hle, policy).map_err(|e| format!("{e:#}"))
    }));

    if !execute {
        println!("not executing (pass --execute to start the title)");
        return Ok(());
    }
    // Windows gives the main thread `SizeOfStackReserve`; the host's main
    // thread is not ours to resize, so the title gets a thread of its own.
    let stack = info.stack_reserve.max(1 << 20) as usize;
    let main = std::thread::Builder::new()
        .name("title main".into())
        .stack_size(stack)
        .spawn(move || -> Result<u32> {
            use victory_loader::DLL_PROCESS_ATTACH;
            victory_loader::teb::attach_current_thread();
            // Under the loader lock, as Windows initialises: a thread a
            // `DllMain` starts waits to be told of itself until every image
            // is initialised, and only initialised images hear of it.
            LOADER_LOCK.hold(|| -> Result<()> {
                for (name, dll) in &dlls {
                    dll.notify(DLL_PROCESS_ATTACH)
                        .map_err(|e| anyhow::anyhow!("{name}'s DllMain refused: {e}"))?;
                    mark_attached(dll.base());
                }
                // The executable's TLS callbacks; its entry point runs below.
                exe_image.notify(DLL_PROCESS_ATTACH).map_err(|e| {
                    anyhow::anyhow!("{}'s TLS callbacks failed: {e}", exe_image.name())
                })?;
                mark_attached(exe_image.base());
                Ok(())
            })?;
            let entry = exe_image
                .entry_point()
                .ok_or_else(|| anyhow::anyhow!("the executable has no entry point"))?;
            // `mainCRTStartup` takes nothing and returns its exit code.
            Ok(victory_hle::modules::win32::threads::run_on_this_thread(
                entry, 0,
            ))
        })
        .context("starting the title's main thread")?;
    let code = main
        .join()
        .map_err(|_| anyhow::anyhow!("title main thread panicked"))??;
    println!("title exited with {code}");
    Ok(())
}

#[cfg(not(windows))]
use victory_loader::MappedImage;

/// The title's images, mapped by `run` and never unmapped: title code may
/// run until the process ends. Late loads add DLLs.
#[cfg(not(windows))]
struct Images {
    exe: &'static MappedImage,
    /// In the order they were mapped: each after the DLLs it imports.
    dlls: Vec<(String, &'static MappedImage)>,
    /// The bases of the images whose `DLL_PROCESS_ATTACH` has run: the ones
    /// told of threads.
    attached: std::collections::HashSet<usize>,
}

/// Notes that the image at `base` has had its `DLL_PROCESS_ATTACH`.
#[cfg(not(windows))]
fn mark_attached(base: usize) {
    if let Some(mut images) = images() {
        images.attached.insert(base);
    }
}

#[cfg(not(windows))]
impl Images {
    /// The image whose file is `file` (any case).
    fn base_of(&self, file: &str) -> Option<usize> {
        self.all()
            .find(|(name, _)| name.eq_ignore_ascii_case(file))
            .map(|(_, image)| image.base())
    }

    /// Every image, the DLLs in order and then the executable.
    fn all(&self) -> impl Iterator<Item = (&str, &'static MappedImage)> {
        let dlls = self
            .dlls
            .iter()
            .map(|(name, image)| (name.as_str(), *image));
        dlls.chain(std::iter::once((self.exe.name(), self.exe)))
    }
}

#[cfg(not(windows))]
static IMAGES: std::sync::OnceLock<std::sync::Mutex<Images>> = std::sync::OnceLock::new();

#[cfg(not(windows))]
fn images() -> Option<std::sync::MutexGuard<'static, Images>> {
    Some(IMAGES.get()?.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The path the title sees for its file `name`: in the package, `G:`. Off
/// Windows there is no host path a title could use.
#[cfg(not(windows))]
fn title_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("G:\\{name}"))
}

/// Tells every initialised image of a thread's start or end
/// (`DLL_THREAD_*`), the DLLs in order and then the executable, under the
/// loader lock (a thread started by a `DllMain` waits for the
/// initialisation to end, as on Windows). The list is read first, so that
/// the images' lock is not held while title code runs.
#[cfg(not(windows))]
fn notify_images(reason: u32) {
    LOADER_LOCK.hold(|| {
        let Some(list) = images().map(|images| {
            images
                .all()
                .filter(|(_, image)| images.attached.contains(&image.base()))
                .map(|(name, image)| (name.to_owned(), image))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        for (name, image) in list {
            if let Err(error) = image.notify(reason) {
                tracing::warn!(image = name, reason, %error, "notifying a title image failed");
            }
        }
    });
}

/// `GetProcAddress` of a title image: its export `name`.
#[cfg(not(windows))]
fn lookup(base: usize, name: &str) -> Option<usize> {
    let images = images()?;
    let (_, image) = images.all().find(|(_, image)| image.base() == base)?;
    image.export(name)
}

/// The entry of `dir` whose file name is `name`, case ignored (the title
/// imports `Foo.DLL`, the folder has `foo.dll`).
#[cfg(not(windows))]
fn find_file(dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
}

/// Maps the title DLL `name` from `title_dir`, after the title DLLs it
/// imports (`wanted`), each bound against our runtime and the DLLs in
/// `mapped`, to which it is added under its file's own spelling. Nothing
/// is done for one mapped already, one being mapped (a cycle), or one the
/// folder lacks (the caller reports those).
#[cfg(not(windows))]
fn map_dll(
    name: &str,
    title_dir: &Path,
    hle: &victory_hle::HleResolver,
    wanted: &dyn Fn(&str) -> bool,
    policy: victory_loader::stubs::UnimplementedPolicy,
    mapped: &mut Vec<(String, &'static MappedImage)>,
    visiting: &mut Vec<String>,
) -> Result<()> {
    if mapped.iter().any(|(m, _)| m.eq_ignore_ascii_case(name)) {
        return Ok(());
    }
    // A DLL importing from the executable: the executable is mapped once,
    // after its DLLs, never as a second copy here (the import gets a stub).
    if name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        tracing::warn!(
            dll = name,
            "a title DLL imports from the executable: unresolved"
        );
        return Ok(());
    }
    if visiting.iter().any(|v| v.eq_ignore_ascii_case(name)) {
        let from = visiting.last().map_or("", String::as_str);
        tracing::warn!(dll = name, from, "title DLLs import each other: a cycle");
        return Ok(());
    }
    let Some(path) = find_file(title_dir, name) else {
        return Ok(());
    };
    let file_name = path
        .file_name()
        .map_or_else(|| name.to_owned(), |n| n.to_string_lossy().into_owned());
    visiting.push(file_name.clone());
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let info = victory_pe::ImageInfo::parse(&bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    for module in &info.imports {
        if wanted(&module.dll) {
            map_dll(
                &module.dll,
                title_dir,
                hle,
                wanted,
                policy,
                mapped,
                visiting,
            )?;
        }
    }
    let image = {
        let resolver = TitleResolver::new(hle, mapped);
        let image = MappedImage::map_with(&bytes, &file_name, &resolver, policy)
            .with_context(|| format!("mapping {}", path.display()))?;
        resolver.show(&file_name);
        image
    };
    // Never unmapped, as the executable.
    mapped.push((file_name, Box::leak(Box::new(image))));
    visiting.pop();
    Ok(())
}

/// A title image's imports: our runtime first, then the title's DLLs mapped
/// so far; what each import was bound to is counted for the `bound` line.
#[cfg(not(windows))]
struct TitleResolver<'a> {
    hle: &'a victory_hle::HleResolver,
    dlls: &'a [(String, &'static MappedImage)],
    hle_count: std::cell::Cell<u32>,
    title_count: std::cell::Cell<u32>,
    missing: std::cell::RefCell<Vec<String>>,
}

#[cfg(not(windows))]
impl<'a> TitleResolver<'a> {
    fn new(hle: &'a victory_hle::HleResolver, dlls: &'a [(String, &'static MappedImage)]) -> Self {
        TitleResolver {
            hle,
            dlls,
            hle_count: Default::default(),
            title_count: Default::default(),
            missing: Default::default(),
        }
    }

    /// The `bound` line of the image `name`, and one line per import that
    /// nobody provides.
    fn show(&self, name: &str) {
        let missing = self.missing.borrow();
        println!(
            "bound          {name}: {} hle, {} title, {} missing",
            self.hle_count.get(),
            self.title_count.get(),
            missing.len()
        );
        for import in missing.iter() {
            println!("  missing      {import}");
        }
    }

    /// `dll!symbol` from a title DLL mapped already.
    fn title(&self, dll: &str, symbol: &victory_pe::ImportSymbol) -> victory_loader::Resolution {
        use victory_loader::{Provenance, Resolution};
        let Some((_, image)) = self
            .dlls
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(dll))
        else {
            return Resolution::Missing;
        };
        match symbol {
            victory_pe::ImportSymbol::Name { name, .. } => match image.export(name) {
                Some(address) => Resolution::Found {
                    address,
                    provenance: Provenance::Title,
                },
                None => Resolution::Missing,
            },
            // `MappedImage::export` finds exports by name only.
            victory_pe::ImportSymbol::Ordinal(ordinal) => {
                tracing::warn!(dll, ordinal, "a title DLL's export by ordinal: not bound");
                Resolution::Missing
            }
        }
    }
}

#[cfg(not(windows))]
impl victory_loader::ImportResolver for TitleResolver<'_> {
    fn resolve(&self, dll: &str, symbol: &victory_pe::ImportSymbol) -> victory_loader::Resolution {
        use victory_loader::{Provenance, Resolution};
        let resolution = match self.hle.resolve(dll, symbol) {
            Resolution::Missing => self.title(dll, symbol),
            found => found,
        };
        let count = match resolution {
            Resolution::Found {
                provenance: Provenance::Title,
                ..
            } => &self.title_count,
            Resolution::Found { .. } => &self.hle_count,
            Resolution::Missing => {
                self.missing.borrow_mut().push(format!("{dll}!{symbol}"));
                return resolution;
            }
        };
        count.set(count.get() + 1);
        resolution
    }
}

/// What a `LoadLibrary` of a title DLL not mapped yet reaches: maps it and
/// the title DLLs it needs, binds them against those mapped before, and
/// runs their `DllMain`s on the calling thread (a title thread, with a
/// TEB), dependencies first, as the Windows loader would. Its base, or
/// `None` if `file` is no title DLL or not in the folder.
#[cfg(not(windows))]
fn late_load(
    file: &str,
    lock: &LoaderLock,
    title_dir: &Path,
    hle: &victory_hle::HleResolver,
    policy: victory_loader::stubs::UnimplementedPolicy,
) -> Result<Option<usize>> {
    use victory_hle::runtime::set_image_path;
    if !title_dll(&hle.host, file) {
        return Ok(None);
    }
    // Waits while another thread initialises a DLL; a `DllMain` that loads
    // another DLL re-enters.
    lock.hold(|| {
        let mut dlls = {
            let Some(images) = images() else {
                return Ok(None);
            };
            if let Some(base) = images.base_of(file) {
                return Ok(Some(base));
            }
            images.dlls.clone()
        };
        let before = dlls.len();
        let wanted = |dll: &str| title_dll(&hle.host, dll);
        map_dll(
            file,
            title_dir,
            hle,
            &wanted,
            policy,
            &mut dlls,
            &mut Vec::new(),
        )?;
        let new = dlls.split_off(before);
        // Visible before their `DllMain`s run, which may load more.
        if let Some(mut images) = images() {
            images.dlls.extend(new.iter().cloned());
        }
        for (name, dll) in &new {
            set_image_path(dll.base(), &title_path(name));
            println!(
                "title dll      {name} at {:#x} (loaded by the title)",
                dll.base()
            );
        }
        // The loading thread's TLS blocks for the new DLLs, before their
        // `DllMain`s (which may use them) run on it.
        victory_loader::teb::attach_current_thread();
        for (name, dll) in &new {
            dll.notify(victory_loader::DLL_PROCESS_ATTACH)
                .map_err(|e| anyhow::anyhow!("{name}'s DllMain refused: {e}"))?;
            mark_attached(dll.base());
        }
        Ok(new
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(file))
            .map(|(_, dll)| dll.base()))
    })
}
