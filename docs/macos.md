# macOS (Apple silicon)

The port of VIctory to macOS on Apple silicon, keeping the Windows build as it
is. Target machine: a MacBook Pro with an M5 Pro and 24 GB of memory, on
macOS 27.0.

## Decisions (2026-11-26)

1. **How the title's x86-64 code runs.** The runtime is built for
   `x86_64-apple-darwin` and runs under Rosetta 2, which translates the
   title's code and ours together. The tools (shader translation, asset
   conversion) are built natively for `aarch64-apple-darwin`. A native arm64
   runtime would need an x86-64 to arm64 translator of our own: not planned.
2. **Graphics.** Native Metal, with the DXIL our translator already emits
   converted by Apple's Metal Shader Converter. Not Vulkan on MoltenVK: the
   title needs mesh shaders.
3. **Platform layer.** `crates/victory-platform`: one trait per subsystem,
   implemented by `Host` on Windows and macOS (and on Linux, only so that the
   portable crates' tests run there; titles do not). A subsystem moves there
   when the macOS port first needs it, with the Windows code it stands for.
   The GPU crate splits into a shared core and `d3d12` / `metal` back ends at
   M4.

Still open, asked when their milestone starts: how the host DLLs the title
calls on Windows are replaced (kernel32, ntdll, the C runtime; M2), and
XAudio2 on FAudio or our own (M3).

## Milestones

| | Pass criterion | State |
|---|---|---|
| M1 | The workspace builds, passes clippy (`-D warnings`) and its tests on macOS, both targets; the binaries start and report the macOS default folders; Windows CI stays green | Code done; the macOS CI job (`.github/workflows/ci.yml`, `macos`) has not run yet |
| M2a | Our own PE loader, the TEB on `gs`, SEH dispatch, fault handlers, `extern "win64"` on every title-facing function: the repository's stand-in title (`tests/fixtures/hle-probe`) loads and runs | |
| M2b | Our replacements for the host DLLs the title calls: its C runtime starts | |
| M2c | With your dump: the title reaches its frame loop headless (no GPU, no audio) for N minutes; every unimplemented import met is logged; memory use measured | |
| M3 | SDL3 window, input, audio (XAudio2): the title's menu music plays and a pad moves through the menu | |
| M4 | Metal back end: the menu, then the city street, render as on Windows; frame times and memory measured | |
| M5 | A signed `.app` that opens with a double-click on a clean account, asks for the game folder and runs | |

One change from the plan: `extern "win64"` moved from M1 to M2a. It cannot be
tested before the loader runs title code, and arm64 has no Win64 calling
convention, so the crates that carry it become x86-64-only on macOS then,
with the tools split from the runtime at the same time.

## Building on a Mac

These are the commands the CI job runs; they have not yet been run on a Mac
by hand.

```bash
xcode-select --install
```

```bash
softwareupdate --install-rosetta --agree-to-license
```

```bash
cargo build --locked -p victory --target x86_64-apple-darwin
```

```bash
cargo test --workspace --locked --target x86_64-apple-darwin
```

The Command Line Tools give the C compiler `blake3` needs. `rustup` installs
the pinned toolchain and both Apple targets (`rust-toolchain.toml`). On Apple
silicon, x86-64 binaries and tests run under Rosetta 2.

Default folders on macOS: derived data in `~/Library/Caches/victory`, what
titles save in `~/Library/Application Support/victory`. On Windows they stay
`%LOCALAPPDATA%\victory\cache` and `%LOCALAPPDATA%\victory\data`.

## Stubs on macOS

Everything below compiles on macOS and does less than on Windows. Each names
the milestone that replaces it.

### Runtime and loader

- `apps/victory/src/run.rs`, `run`: refuses to run a title ("needs Windows
  for now"). M2.
- `victory-loader`: steps 2 to 5 (mapping, binding, TLS, threads, crash
  reporting, Zen 2 instruction emulation, patches of the title's code) are not
  built. M2a.
- `victory-hle/src/modules/kernel.rs`, the non-Windows `host` module: every
  host call fails (null handles, zero lengths, no last error, no stack walk);
  `exit`, `abort` and `TerminateProcess` end our process.
  `SetThreadPriority` changes nothing and fails. M2a/M2b.
- `victory-hle/src/host.rs`, non-Windows `imp::resolve`: no host DLL serves
  any import; on macOS this stays so, and our own implementations replace
  them. M2b.
- `victory-hle/src/modules/d3d12x.rs`, `create_device`: the device keeps its
  own allocator, the title's memory (`xmem`) being Windows-only. M2.
- `victory-hle/src/modules/storage.rs`, `folder`: no persistent-storage
  drive; its path calls fail (`files`, the virtual drives, is Windows-only).
  M2.
- `victory-hle/src/modules/rpcrt4.rs`: identifiers come from a counter mixed
  with the time, not from the system's random generator. M2.
- `victory-hle/src/modules/xgame.rs`, `XLaunchNewGame`: no relaunch. M2.
- `victory-gpu/src/queue.rs`, `system_timestamp`: always 0. M2.
- `victory-gpu/src/device.rs`: the address range reported is a constant
  (`0x10000` to `0x7fff_fffe_ffff`), and descriptions are not read from title
  memory. M2.
- `victory-gpu/src/fence.rs`, `set_event`: events are not signalled. M2.
- Known issue, not a stub: `runtime::set_image_path` reads title paths
  (`C:\title\game.exe`) with the host's path rules, which on macOS treat them
  as relative names. The two kernel tests that show it are Windows-only until
  M2 gives titles Windows-style paths on every host.

### Graphics (a null GPU back end until M4)

- `victory-gpu/src/list.rs`: commands are taken and dropped; copies (buffer,
  resource, texture, memory), clears, barriers, resolves, value writes,
  draws, dispatches and indirect executions do nothing, and pipelines are
  bound to nothing; the non-Windows `state` module ignores every draw state
  call;
  uploads are plain memory nobody reads; a value wait reads title memory
  through the kernel (`victory_platform::Memory`), as on Windows.
- `victory-gpu/src/queue.rs`: presents count frames and show nothing; draws
  and dispatches are reported as 0.
- `victory-gpu/src/resource.rs`, `descriptor.rs`: resources and descriptor
  heaps exist without host copies; descriptor copies are plain memory copies.
- `victory-gpu/src/root.rs`, `host_export`: no host `d3d12.dll`.
- `victory-hle/src/modules/d3d12x.rs`: no shader cache (`precompile`,
  `warm_shader` fail, `stale_shaders` is 0), so `victory shaders` and
  `victory translate --warm` report every shader as failed. M4.
- `apps/victory/src/translate/unhosted.rs`: `victory translate --validate`,
  `--pipelines` and `VICTORY_PAIRS` stop with "needs Windows for now";
  translation itself runs as on Windows. M4 (pipeline checks on Metal).

### Window, input, audio (M3)

- `victory-hle/src/modules/gameinput.rs`, non-Windows `host`: no pads, no
  keys, rumble dropped.
- `victory-hle/src/modules/xgameui.rs`, non-Windows `ask`: text entry is
  cancelled, as when the dialog cannot open.
- `victory-hle/src/modules/display.rs`: the shell object has no display
  deferrals (Windows' power requests).
- Not built: `lifecycle` (the title's window), `apu`, `media`, `dstorage`,
  `net`; XAudio2 and Media Foundation are host DLLs on Windows.

### Not built on macOS

`victory warm` and `victory replay`; the D3D12 back end and everything that
uses it (`victory-gpu`'s `host`, `texture`, `titlewrite` and the rest of its
Windows modules); `victory-shader`'s `emit::host` and `emit::validator`.

Tests that are Windows-only for a reason given beside each:
`victory-pe` and `victory-loader` read the test binary as a PE image (four and
four tests), `victory-hle` `kernel` (two, title paths above) and `storage`
(one, virtual drives).

## To find out on the Mac

- Found (M5 Pro, 2026-11-26): Rosetta 2 gives x86-64 processes 4 KiB pages
  (`arch -x86_64 getconf PAGESIZE`), as Windows does, so the runtime's 4 KiB
  assumptions hold under it. Native processes have 16 KiB pages.
- Not yet known: whether Rosetta 2 runs AVX2, which the title needs. Its
  feature list (`arch -x86_64 sysctl machdep.cpu.features`) stops at SSE4.2
  and AES, with no AVX or FMA, and it has no `machdep.cpu.leaf7_features`
  at all: AVX2 is at least not advertised. Next: a probe that reads CPUID
  and runs one AVX2 instruction, with and without `ROSETTA_ADVERTISE_AVX=1`.
  If Rosetta 2 cannot run AVX2, decision 1 is reopened.
- The alignment Metal asks of memory it uses in place (no-copy buffers)
  under Rosetta.
- How long Apple keeps Rosetta 2 whole: it said in 2025 that it stays through
  macOS 27, the version this Mac runs, and that later versions keep only what
  older games need. Decision 1 may not outlive macOS 27; to check against
  Apple's current word before M5.

## Memory

On Windows the title takes about 30 GB of memory and 10.5 GB of the card's.
A Mac shares one 24 GB pool between processor and GPU. Much of the Windows
figure is copies of title memory that a separate card needs (`victory-gpu`,
`watch.rs`); with unified memory Metal can read buffers where the title
writes them, and only textures in the console's tiled layouts need converting.
M2c measures the footprint without a GPU, M4 with Metal.
