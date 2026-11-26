//! `victory run` off Windows, end to end: the stand-in executable of
//! tests/fixtures/pe-exe, mapped by our loader and run on our runtime.
#![cfg(all(not(windows), target_arch = "x86_64"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// The fixture's executable, built once: for `x86_64-pc-windows-msvc`,
/// linked by the rust-lld that comes with Rust (no Windows SDK), into the
/// outer build's scratch folder for tests.
fn exe() -> PathBuf {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let manifest = workspace_root().join("tests/fixtures/pe-exe/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pe-exe");
        let status = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", "x86_64-pc-windows-msvc", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            // Flags of the outer build would replace the fixture's own.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", "rust-lld")
            .env(
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
                "-C linker-flavor=lld-link -C link-arg=/NODEFAULTLIB \
                 -C link-arg=/ENTRY:probe_main -C link-arg=/SUBSYSTEM:CONSOLE",
            )
            .status()
            .expect("running cargo");
        assert!(status.success(), "building pe-exe failed");
        let exe = target.join("x86_64-pc-windows-msvc/release/pe-exe.exe");
        assert!(exe.is_file(), "{} was not built", exe.display());
        exe
    })
    .clone()
}

/// A title folder: the fixture and the file it reads.
fn game_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary folder");
    std::fs::copy(exe(), dir.path().join("pe-exe.exe")).expect("copying the fixture");
    std::fs::write(dir.path().join("probe.txt"), "hello from G:").expect("writing probe.txt");
    dir
}

/// `victory run <game_dir>/pe-exe.exe <args>`, with its cache and data in
/// a folder of their own.
fn victory(game_dir: &Path, args: &[&str]) -> Output {
    victory_with(&game_dir.join("pe-exe.exe"), args)
}

/// `victory run <exe> <args>`, as [`victory`].
fn victory_with(exe: &Path, args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("a temporary folder");
    Command::new(env!("CARGO_BIN_EXE_victory"))
        .arg("run")
        .arg(exe)
        .args(args)
        .env("VICTORY_CACHE_DIR", home.path().join("cache"))
        .env("VICTORY_DATA_DIR", home.path().join("data"))
        .env_remove("RUST_LOG")
        .output()
        .expect("running victory")
}

/// Both of the output's streams, whole.
fn streams(output: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn the_stand_in_exe_runs_to_exit_process() {
    let game = game_dir();
    let output = victory(
        game.path(),
        &["--execute", "--strict", "--unimplemented", "trap"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{stdout}");
    assert_eq!(output.status.code(), Some(42), "{}", streams(&output));
    for line in [
        "pe-exe: start",
        "pe-exe: thread 7 value 85",
        "pe-exe: file hello from G:",
        "pe-exe: module ok",
        "pe-exe: kernel32 ExitProcess found",
        "mapped at      ",
        "bound          ",
    ] {
        assert!(stdout.contains(line), "no {line:?}:\n{}", streams(&output));
    }
}

#[test]
fn binding_without_execute_reports_and_stops() {
    let game = game_dir();
    let output = victory(game.path(), &["--strict"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", streams(&output));
    assert!(stdout.contains("not executing"), "{}", streams(&output));
    let bound = stdout
        .lines()
        .find(|l| l.starts_with("bound          pe-exe.exe: "))
        .unwrap_or_else(|| panic!("no bound line:\n{}", streams(&output)));
    assert!(bound.ends_with(" 0 missing"), "{}", streams(&output));
}

#[test]
fn a_missing_file_is_a_clear_error() {
    let game = game_dir();
    let output = victory_with(&game.path().join("nothing.exe"), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{}", streams(&output));
    assert!(stderr.contains("nothing.exe"), "{}", streams(&output));
}
