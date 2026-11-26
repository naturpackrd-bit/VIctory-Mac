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
| M2a | Our own PE loader, the TEB on `gs`, SEH dispatch, fault handlers, `extern "win64"` on every title-facing function: a stand-in title loads and runs | In progress. Done: the runtime crates are x86-64 only (the Apple silicon build is the tools); `extern "win64"` on title-facing code; a stand-in title any host builds (`tests/fixtures/pe-probe`, rust-lld, no Windows SDK); our loader (`victory_loader::MappedImage`) maps, relocates, binds (delay imports ahead of time too), runs TLS callbacks and `DllMain`, and gives every unresolved import a stub that reports its first call (`UnimplementedPolicy`); a TEB of ours per thread, with static TLS, reached through the title's `gs` accesses patched at mapping (`victory_loader::teb`); faults in title code (SIGSEGV, SIGBUS, SIGILL, SIGTRAP, SIGFPE) taken on an alternate stack, the Zen 2 instructions the host lacks emulated (`clzero`, `monitorx`, `mwaitx`, `clwb`, SSE4a, `movntss`/`movntsd`), anything else a crash report without allocating (`victory_loader::faults`); x64 unwind data read and frames undone, handlers located (`victory_loader::unwind`); the low 4 GiB reserved at start and `ROSETTA_ADVERTISE_AVX` set by a relaunch (`apps/victory`). Tests pass on Linux and (CI) on Windows and macOS, both targets, except the low-memory test under Rosetta 2, fixed after CI run 9 (`__PAGEZERO` is the floor of the map, not a region). Next: SEH dispatch (`RtlDispatchException`'s work on the handlers the unwinder finds, `RaiseException`, `__C_specific_handler`), threads, then the title's entry |
| M2b | Our replacements for the host DLLs the title calls: its C runtime starts | In progress: `victory-hle`'s `win32` module (built off Windows only; it serves `kernel32`, `kernelbase`, `ntdll` and the `api-ms-win-core-*` sets) has handles and the last error (8 exports), synchronization (66: events, semaphores, mutexes and the waits, critical sections, SRW locks, condition variables, `WaitOnAddress`, `InitOnce`, `SList`), memory (40: `VirtualAlloc` and family with reserve/commit kept here, heaps, `GlobalMemoryStatusEx`) and clocks, system information, processor features and the Windows version (33), threads with TLS and FLS slots (37: `CreateThread` on a host thread with the title's stack size, a TEB and the images told of the thread, `ExitThread` through a jump buffer); 184 exports, with tests on Linux. Not yet: files, the C runtime (`ucrt`: sizing needs the census of your dump), `RaiseException` and friends |
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
- `victory-loader`, `map` (our own loader): an import nobody resolves gets,
  by `UnimplementedPolicy`, a refusal of the mapping (`Refuse`), a stub that
  traps (`Trap`) or one that logs its first call and returns 0 (`Zero`);
  `Zero` is what the tests use and what M2c runs the title with, so a title
  that needs the result of such a call goes wrong later rather than at the
  call. Delay imports are bound ahead of time, so `__delayLoadHelper2` is
  never called. M2b/M2c.
- `victory-loader`, `teb`: the PEB a title reads through `gs:[0x60]` is a
  zeroed block (`BeingDebugged` 0, no image base, no process parameters).
  `gs` accesses in any form but a `mov` (`cmp`, `movzx`, ...) make the
  mapping fail, naming them; `victory inspect --teb` lists an image's
  before it is run. A thread's TLS blocks for an image mapped after the
  thread attached appear when it attaches again. M2a/M2b.
- `victory-loader`, `faults`: a fault in title code that is not a Zen 2
  instruction to emulate ends the process with a crash report (registers,
  the top of the stack) and exit code 128 + the signal; the title's own
  handlers (`__try`, vectored handlers, `SetUnhandledExceptionFilter`) are
  not yet called, though `unwind` finds them: SEH dispatch is M2a's next
  step. Faults in our own code go to the handler that was installed before
  ours (Rust's, which aborts). Up to 8 handlers of ours (`register`); the
  alternate stack (64 KiB) is leaked when a thread ends. M2a.
- `victory-loader`, `unwind`: a frame's handler and language-specific data
  are located but never invoked (see `faults`); `backtrace` reads an image's
  exception table again for every frame, and reads an epilogue a byte at a
  time (one checked read a byte: fine for a crash report, to be made one
  read when exceptions are dispatched often). The establisher frame of a
  function with a frame register is computed from that register even when
  `rip` is in the prologue before it is set, which Windows handles. M2a.
- `victory-platform`, `low`: `free_ranges` counts the executable's
  `__PAGEZERO` as mapped (the kernel raises the floor of the address map for
  it, reporting no region); nothing else that is not a region is known. The
  Apple silicon build, whose `__PAGEZERO` is 4 GiB, reports nothing free,
  truthfully. M2a.
- `victory-hle/src/modules/win32`, `sync`: alertable waits are ordinary ones
  (no APCs); a mutex whose owner ends is never reported abandoned (no
  thread's end is seen yet); `PulseEvent` releases nobody (waiters check
  the event again when they wake, by which time it is clear);
  `SignalObjectAndWait` is two steps, not one; a wait-all with the same
  handle twice is not refused; a critical section's spin count is stored
  and not used, and its `LockCount` is -1 free, 0 held (Windows encodes
  waiters in it; nothing known reads it); named objects live for the
  process (Windows frees them with their last handle) and their `Local\`
  and `Global\` prefixes are part of the name; `INIT_ONCE_ASYNC` is
  refused; an `SLIST_HEADER`'s sequence bytes are unused. Every signal
  wakes every waiter through one condition variable, to be made finer when
  it costs. M2b/M2c.
- `victory-hle/src/modules/win32`, `threads`: `SuspendThread` works only on
  a thread that has not begun running (one created suspended); on a running
  one it fails (`ERROR_NOT_SUPPORTED`, a warning once). `TerminateThread`,
  `GetThreadContext`, `SetThreadContext` and `QueueUserAPC` fail the same
  way. `ExitThread` returns from the start routine's call without unwinding
  the title's frames (they need none), so one called from inside a callback
  our Rust code invoked (`InitOnce`, FLS, TLS callbacks) skips that Rust
  frame, which must hold nothing to drop; `ExitThread` on a thread we did
  not start ends the host thread (`pthread_exit`), untested on either host.
  `TlsAlloc` zeroes the new slot in the calling thread only and `TlsFree`
  leaves other threads' values; a thread using an expansion slot (index 64
  and up) gets an 8 KiB array kept for the process's life. `FlsFree` runs
  the slot's callback for the calling thread's value only. Priorities and
  affinities are kept and reported, not applied (`SetThreadPriority` is
  `kernel.rs`'s and does not store here). M2b/M2c.
- `victory-hle/src/modules/win32`, `memory`: `PAGE_GUARD` becomes a page
  with no access (the one-shot guard fault is not kept); `VirtualQuery`
  knows only reservations made through these functions, so what the host,
  Rust or the loader own looks free; a decommitted page keeps its contents
  until it is committed again (then zeroed); `GetWriteWatch` and
  `ResetWriteWatch` fail (the Windows GPU path's, not a title's);
  `HeapDestroy` frees none of the heap's blocks; `HEAP_REALLOC_IN_PLACE_ONLY`
  is not honoured (a block may move); moveable `LocalAlloc`/`GlobalAlloc`
  memory is a plain pointer; `VirtualLock` does nothing; the page file is
  reported as large as physical memory. M2b/M2c.
- `victory-hle/src/modules/win32`, `time`: `GetLocalTime` is UTC and
  `GetTimeZoneInformation` reports no zone (bias 0); `GetSystemTimes` is
  all zeros; `GetCurrentProcessorNumber` is always 0; processors are capped
  at 64 (one group); `GetLogicalProcessorInformationEx` describes no caches
  or groups; Windows is reported as 10, build 19044, the computer as
  `VICTORY`, the directories as `C:\Windows`. On macOS the available
  memory (`GlobalMemoryStatusEx`) comes from the kernel's page counts times
  the kernel's page size (16 KiB under Rosetta 2 too): CI run 10 saw the
  native and the Rosetta build agree (6.8 and 6.7 GiB available of 14), as
  they would not with Rosetta's 4 KiB. M2b.
- `victory-hle/src/modules/kernel.rs`, the non-Windows `host` module: every
  host call fails (null handles, zero lengths, no stack walk); `exit`,
  `abort` and `TerminateProcess` end our process. `SetThreadPriority`
  changes nothing and fails. The last error is the `win32` module's now
  (`GetLastError`, in the TEB). M2b.
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
- How the TEB is reached, per host (`victory_platform::Thread`): on macOS a
  thread-specific key of ours, read by the thunks as `gs:[key * 8]`
  (libpthread's TSD layout; a test holds it to that); on Linux (tests) the
  TEB itself is put at the `gs` base with `arch_prctl`, which nothing of
  Linux uses on x86-64, and the thunks read its `Self` field.
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

## Other forks looked at

- Belobug/VIctory-Belobug-AMD (2026-11-26): our `main` (c9c932c) plus three
  commits, all in the Windows D3D12 renderer: RX 9070 XT fixes with an HUD
  (a filter for garbage light probes, a vectored handler that parks a
  pipeline worker the AMD driver's compiler crashed under, a texture size
  cap under VRAM pressure, pooled indirect-batch buffers, descriptor tables
  released at landing, `lto = "thin"` and one codegen unit in the release
  profile), title textures uploaded on the submission's prelude after the
  refresh (one frame less of latency, no flicker in video rings), and a
  bridge to AMD's PC FidelityFX 4 upscaler (needs its signed DLL). Not
  merged: nothing of it runs on macOS, where M4 replaces that renderer with
  Metal; it is AMD- and Windows-specific in two of three parts; and neither
  the user nor CI has a Windows GPU to run it on (CI's WARP only compiles
  and unit-tests it). For M4, two ideas worth keeping: deciding a texture's
  upload at submission time after its staging refresh, and clamping a
  texture array view's mip range to the host copy's levels (`from_base`).

## Memory

On Windows the title takes about 30 GB of memory and 10.5 GB of the card's.
A Mac shares one 24 GB pool between processor and GPU. Much of the Windows
figure is copies of title memory that a separate card needs (`victory-gpu`,
`watch.rs`); with unified memory Metal can read buffers where the title
writes them, and only textures in the console's tiled layouts need converting.
M2c measures the footprint without a GPU, M4 with Metal.

Found (CI run 9, macOS, 2026-11-26): the kernel maps nothing for an
executable's `__PAGEZERO` segment; it raises the minimum address of the
task's map instead (`vm_map_raise_min_offset` in xnu's `mach_loader.c`). So
`mach_vm_region` reports no region there and a walk of the low 4 GiB finds
it all free, while a fixed allocation in it fails. `free_ranges` reads the
segment's size from the executable's header (`getsegbyname`) and counts it
as mapped. The runtime and the platform crate's test binaries link with a
one-page `__PAGEZERO` (`build.rs`); every other binary keeps the 4 GiB one
and sees nothing free below 4 GiB.
