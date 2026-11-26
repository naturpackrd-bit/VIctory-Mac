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
| M1 | The workspace builds, passes clippy (`-D warnings`) and its tests on macOS, both targets; the binaries start and report the macOS default folders; Windows CI stays green | macOS: passed (CI runs 1 and 3, Apple silicon runner, x86-64 under Rosetta 2). Windows: clippy passes; one test fails, `victory-gpu` `pipeline::host::tests::depth_bounds_pipeline_loads_and_filters_draws`, on the runner's software renderer (WARP), in code this port has not changed; it needs a Windows machine with a GPU to tell whether WARP or the test is at fault |
| M2a | Our own PE loader, the TEB on `gs`, SEH dispatch, fault handlers, `extern "win64"` on every title-facing function: a stand-in title loads and runs | In progress. So far: the runtime crates are x86-64 only (the Apple silicon build is the tools); `extern "win64"` on title-facing code; a stand-in title any host builds (`tests/fixtures/pe-probe`, rust-lld, no Windows SDK); our loader (`victory_loader::MappedImage`) maps, relocates, binds and runs it: tests pass on Linux, on macOS under Rosetta 2 and on Windows (CI run 3). Next: TLS, the TEB on `gs`, exceptions, `ROSETTA_ADVERTISE_AVX`, the low 4 GiB |
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
- `victory-loader`, `map` (our own loader): maps, relocates and binds an
  image; not yet static TLS, the exception table, delay imports, TLS
  callbacks or the entry point, and an import nobody resolves fails the
  mapping instead of getting a reporting stub. M2a.
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

### The Apple silicon build (tools)

`victory-loader`, `victory-hle` and `victory-gpu` run the title's x86-64
code and are built for x86-64 only; for `aarch64-apple-darwin` they are
empty crates. There, `victory run`, `victory census`, `victory shaders` and
`victory translate --warm` stop with "this command is part of the runtime,
which is built for x86-64". `inspect`, `lib-exports`, `shader-info`,
`translate` and `config` run natively. The shader cache moves out of
`victory-gpu` at M4, so that the native build can fill it.

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
- Found (macOS 27.0, 2026-11-26): Rosetta 2 runs AVX2. A probe that reads
  CPUID and then runs `vpaddd ymm` got the right sum both times; CPUID
  reported AVX, OSXSAVE, FMA, AVX2, BMI1 and BMI2 as absent, and present
  with `ROSETTA_ADVERTISE_AVX=1` in the environment. (`sysctl
  machdep.cpu.features` lists none of them either way.) So the runtime
  starts with that variable set, re-launching itself when it is missing, or
  the title and our own AVX2 paths (`victory-gpu`) see no AVX2. M2a.
- Found (probe 2, an x86-64 C program under Rosetta 2, 2026-11-26):
  - Memory below 4 GiB, which the title asks for (`ADDRESS_32BIT`): refused
    with the default 4 GiB `__PAGEZERO`; given at `0x10000000` when linked
    with `-Wl,-pagezero_size,0x1000`. Then only about 600 KB down there is
    mapped (our image, dyld), around `0xe00000`. The runtime links so and
    reserves the low range at its start, before anything else lands in it.
    M2a.
  - A 16 GiB block allocated anywhere lands near `0x2_0000_0000`, below the
    2^40 the title's memory must stay under.
  - A write to a read-only page, taken by a signal handler that opens the
    page, costs about 18 us a round; the two `mprotect` calls in it about
    0.75 us. macOS has nothing like `GetWriteWatch`, so tracking the title's
    writes by faults would cost about 17 us a page: on the Mac, buffers must
    not be tracked at all, Metal reading them where the title writes them
    (M4). M2c's null GPU tracks nothing.
  - `%gs` is macOS's thread data: `gs:0x30`, `0x58` and `0x60` read 0 and
    `gs:0x10` a small number (the MIG reply port). A Windows TEB read left
    unpatched gets a wrong value without faulting, so the loader's patch
    pass must find every one, and refuses to start a title in which it meets
    one it cannot patch. M2a.
  - `clzero` raises SIGILL; a handler that zeroes the line and moves `rip`
    on works, as the Windows emulation does.
- Found (probe 3, an x86-64 C program under Rosetta 2, 2026-11-26):
  - A page can be readable, writable and executable at once, and code in it
    patched in place while it runs: the Windows loader's TLS thunks and code
    patches (`tls.rs`, `write_sites.rs`, `PAGE_EXECUTE_READWRITE`) port as
    they are.
  - One block of memory shown at two addresses (`mach_vm_remap` without
    copying; current protection read-write, maximum read-write-execute): a
    write through one view is read through the other. That is what the
    title's memory (`xmem`, `MapViewOfFile3`) needs. M2c.
  - Each view has its own protection: one view read-only while the other
    stays writable; a write through the read-only one faults, is caught and
    opens it; the writable one never faults. The write guards' per-view
    cover (`watch.rs`) ports as it is, at the 18 us a fault costs.
  - Showing a 64 KiB block at a new address and dropping it: 1.3 us; two
    `mprotect`s of 64 KiB: 0.9 us. Cheap, where Windows' `VirtualQuery` cost
    the title milliseconds (`console_insn.rs`).
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
