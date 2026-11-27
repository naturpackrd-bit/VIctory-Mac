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
| M2a | Our own PE loader, the TEB on `gs`, SEH dispatch, fault handlers, `extern "win64"` on every title-facing function: a stand-in title loads and runs | In progress. Done: the runtime crates are x86-64 only (the Apple silicon build is the tools); `extern "win64"` on title-facing code; a stand-in title any host builds (`tests/fixtures/pe-probe`, rust-lld, no Windows SDK); our loader (`victory_loader::MappedImage`) maps, relocates, binds (delay imports ahead of time too), runs TLS callbacks and `DllMain`, and gives every unresolved import a stub that reports its first call (`UnimplementedPolicy`); a TEB of ours per thread, with static TLS, reached through the title's `gs` accesses patched at mapping (`victory_loader::teb`); faults in title code (SIGSEGV, SIGBUS, SIGILL, SIGTRAP, SIGFPE) taken on an alternate stack, the Zen 2 instructions the host lacks emulated (`clzero`, `monitorx`, `mwaitx`, `clwb`, SSE4a, `movntss`/`movntsd`), anything else a crash report without allocating (`victory_loader::faults`); x64 unwind data read and frames undone, handlers located, chained and indirect entries followed (`victory_loader::unwind`); structured exception handling as ntdll does it (`victory_loader::seh`): a fault becomes an exception record and context on the faulting thread's own stack, vectored handlers and then the frames' language handlers are called, `__C_specific_handler` runs `__except` filters and `__finally` blocks, `RtlUnwindEx` unwinds to a target frame, `RaiseException` raises from the title, and the unhandled filter and a crash report take what no handler does; a stand-in title with real scope tables exercises each path in a test; the low 4 GiB reserved at start and `ROSETTA_ADVERTISE_AVX` set by a relaunch (`apps/victory`). Tests pass on Linux and (CI) on Windows and macOS, both targets, except the low-memory test under Rosetta 2, fixed after CI run 9 (`__PAGEZERO` is the floor of the map, not a region). C++ exceptions as vcruntime dispatches them (`ucrt::cxx`): `_CxxThrowException` raises from the caller's context, `__CxxFrameHandler3` and `__CxxFrameHandler4` read the 2015 and the compressed 2019 tables, match the thrown object's catchable types against each try block's handlers, unwind through our `RtlUnwindEx` running the destructors on the way, build the catch object, run the catch funclet on a stack below the frames the exception came through and resume at its continuation; rethrows, `std::uncaught_exceptions`, `_is_exception_typeof` and the exception-object helpers; a stand-in title with hand-written tables throws, catches, destroys on unwind and rethrows in a test. Done, with the limits below |
| M2b | Our replacements for the host DLLs the title calls: its C runtime starts | In progress: `victory-hle`'s `win32` module (built off Windows only; it serves `kernel32`, `kernelbase`, `ntdll` and the `api-ms-win-core-*` sets) has handles and the last error (8 exports), synchronization (75: events, semaphores, mutexes, waitable timers and the waits, critical sections, SRW locks, condition variables, `WaitOnAddress`, `InitOnce`, `SList`), memory (40: `VirtualAlloc` and family with reserve/commit kept here, heaps, `GlobalMemoryStatusEx`) and clocks, system information, processor features and the Windows version (33), threads with TLS and FLS slots (37: `CreateThread` on a host thread with the title's stack size, a TEB and the images told of the thread, `ExitThread` through a jump buffer), the file API over host folders (74: each drive letter a host folder, Windows paths resolved regardless of case, `CreateFile` and its family, find handles, overlapped I/O completing at once, the console streams and `NUL`), exceptions (27: `RaiseException`, `RtlUnwindEx`, `__C_specific_handler`, vectored handlers, the error modes, no debugger); 294 exports, with tests on Linux; the title's memory (`xmem`, 13 exports, off Windows): reservations where the console's flags put them (the low 4 GiB from the runner's reservation of it, else 40 to 46 bits) through our `VirtualAlloc`, physical pages as sections of host memory (`victory_platform::sections`: a memfd on Linux, `mach_vm_remap` on macOS) shown at the title's addresses by views, several at a time, in place of the reservation's pages, the working-set statistics and memory transfers; the C runtime (`ucrt`, 797 exports for `ucrtbase.dll`, the `api-ms-win-crt-*` sets and `vcruntime140.dll`: startup (`_initterm`, the onexit tables, the `exit` hooks and the executable's thread-local destructors run at `exit` (not at `ExitProcess`, as on Windows), the arguments, command line and environment, the invalid-parameter, purecall and new handlers, `signal`, `errno`, the floating-point control word over MXCSR, `_beginthreadex` over `CreateThread`), the heap (`malloc` and family, aligned blocks), strings (192: C and wide string, memory, conversion and character-class functions, `strtod` and family, `qsort`, `bsearch`), math (153), time (44, UTC only) and locale (50); RTTI casts; `setjmp`/`longjmp`; stdio: the `printf` engine (every conversion, flag and length, the `__stdio_common_*` option bits, positional arguments, exact decimal rounding) and the `scanf` engine, `FILE` streams with buffering, text and Unicode modes (109 exports), and 93 file-descriptor, stat, find and path functions over `win32::files`; C++ exception handling (18 exports, see M2a); 94 tests on Linux); the C++ standard library's DLL (`msvcp`, 119 exports for `msvcp140.dll` and its satellites: the C-level functions behind `std::thread`, `std::mutex`, `std::condition_variable`, `std::shared_mutex` and `std::chrono` (`_Mtx_*`, `_Cnd_*`, `_Thrd_*`, `_Smtx_*`, `xtime_get`, `_Xtime_*`, `_Query_perf_*`), the `_Lockit` locks, the functions that throw the standard exceptions (`_Xlength_error` and its kin, `_Throw_C_error`, `_Throw_Cpp_error`, `_Throw_future_error`) as MSVC-layout objects with RTTI our handler matches (`std::exception` 24 bytes with its vtable, `system_error` with the `generic`, `system` and `future` categories), `exception_ptr`, the `errno` and Win32 error maps and the locale facets' C helpers (`_Getcvt`, `_Mbrtowc`, `_Tolower`, `_Getwctype`, `_Stod` and family); 11 tests on Linux); the small DLLs: `bcrypt` (13 exports: the RNG provider over `getentropy`, hashes and ciphers refused), COM (`com`, 27: apartments per thread, `CoCreateInstance` reporting every class unregistered, task memory, GUID strings, `PropVariant`, `CoWaitForMultipleHandles`), Windows Error Reporting stubs (`wer`, 14), named pipes (`win32::pipes`, 13: `CreatePipe` real over `std::io::pipe`, named ones refused), and the kernel's code pages, locales, string comparison, `FormatMessage`, date and time formats, time zones and processor topology (`win32::locale`, 74: 1252 read as Latin-1, UTF-8, en-US data, `GetLocaleInfoEx` with about 100 types, `LCMapStringEx`, `CompareStringEx`, `GetStringTypeW`, `FormatMessageW` with inserts and printf specifications and a table of the common error texts, `GetDateFormatEx`/`GetTimeFormatEx` pictures), `GetModuleHandleExW`/`A` (`kernel.rs`); 23 tests on Linux) |
| M2c | With your dump: the title reaches its frame loop headless (no GPU, no audio) for N minutes; every unimplemented import met is logged; memory use measured | In progress: off Windows `victory run` (`apps/victory/src/run.rs`) reads the executable, keeps the low 4 GiB for the title, installs the fault handlers, mounts the package as `G:` and a temporary folder as `T:`, maps the title's DLLs from its folder (dependencies first, each bound against our runtime and the DLLs before it; a `LoadLibrary` of one not yet mapped maps it then, under a loader lock) and then the executable, records their paths as the title sees them (`G:\game.exe`), tells the images of thread starts and ends, names the title's `__XMemAllocationHooks__` to `xmem`, and runs the entry point on a thread with the title's own stack size after the DLLs' `DllMain`s and the TLS callbacks. A stand-in executable (`tests/fixtures/pe-exe`, built like `pe-probe` with rust-lld) writes to the console, starts and joins a thread, reads a file from `G:`, checks `GetModuleHandle` and `GetProcAddress`, and leaves through `ExitProcess(42)`: `apps/victory/tests/run_exe.rs` runs it end to end through the built `victory` on Linux (CI runs it under Rosetta 2 too). The console-side modules build and run off Windows too: `xsystem`, `lifecycle` (`end_title` posts `WM_CLOSE` to the title's windows), `apu`, `net` (offline), `media` (no Media Foundation), `storage` (a drive of its own) and DirectStorage (`dstorage`, over `win32::files` and `xmem`'s sections, 18 of its 19 tests on Linux). Not yet: the title's own executable (its imports need MSVCP140's `locale` and `iostream` classes and the gaps the census of your dump names) |
| M3 | SDL3 window, input, audio: the title's menu music plays and a pad moves through the menu | In progress: the host-independent half, the Win32 model of window classes, windows, window procedures and per-thread message queues (`victory-hle/src/modules/window.rs`, 196 exports for the `ntuser` API sets off Windows: `RegisterClassEx`, `CreateWindowEx` sending `WM_NCCREATE` and `WM_CREATE`, `ShowWindow` with the activation messages, `SetWindowPos`, `DefWindowProc`, `Post`/`Peek`/`Get`/`Dispatch`/`SendMessage` across threads with the sent messages run inside the receiver's waits, `PostQuitMessage`, timers on a thread of ours, `MsgWaitForMultipleObjects`, system metrics and a fixed 1920x1080 display, cursor, raw input registration, properties, hooks stored; 6 tests on Linux). No host window exists yet: SDL3 feeds this model with the real window and events next. The title's sound goes through `xapu`/`AcpHal`/`chad`, not XAudio2 |
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

- `apps/victory/src/run.rs`, `run` off Windows: none of the Windows
  runner's title patches and aids run (write sites, frame rate and
  resolution patches, descriptor sites, the crash log, console timers, the
  shader cache); a late-loaded DLL whose `DllMain` fails is neither
  detached nor unmapped, and a mapping that fails part-way leaves what it
  mapped; a title DLL's import by ordinal stays unbound (a warning); only
  static imports are followed when the title's DLLs are looked for (Windows
  follows delay imports too); the storage drive (`storage::folder`) is not
  mounted yet; `check_cpu_features` needs AVX2 (Rosetta 2 shows it only
  with `ROSETTA_ADVERTISE_AVX=1`, which `victory` sets by re-running
  itself). The executable's preferred range (`SizeOfImage` at its
  `ImageBase`, read from its headers) is reserved before its bytes and the
  title DLLs are read and mapped, and handed to
  `MappedImage::map_with_reserved`; when something is there already the run
  log says so and the executable is moved, as before. A title DLL's
  forwarder (`dll.func`, `dll.#n`) is followed one level, to our runtime
  first and then to the title DLLs mapped already; a second forward or one
  to a DLL not mapped yet gets a stub and a warning naming it. Off Windows
  `HostPolicy::default()` is empty (no host DLL serves anything there), so a
  DLL the host would serve on Windows (`dbghelp.dll`, `xaudio2_9.dll`, ...)
  is looked for in the title's folder and named among the missing DLLs when
  it is not there; `--strict` changes nothing off Windows. `census` agrees
  with `run` (`apps/victory/src/title.rs` says which DLLs come from the
  folder): an import our runtime or the host does not serve counts as
  `title` when a DLL of the folder exports its name, else as missing, marked
  `(title dll)` (the DLL lacks the name, or the import is by ordinal) or
  `(dll absent)` in `--missing` (grouped by DLL); an import both could serve
  counts as `hle`, as `run` binds it; and the census fails when an image
  could not be read, naming it. M2c.
- `victory-loader`, `map` (our own loader): an import nobody resolves gets,
  by `UnimplementedPolicy`, a refusal of the mapping (`Refuse`), a stub that
  traps (`Trap`) or one that logs its first call and returns 0 (`Zero`);
  `Zero` is what the tests use and what M2c runs the title with, so a title
  that needs the result of such a call goes wrong later rather than at the
  call. Delay imports are bound ahead of time, so `__delayLoadHelper2` is
  never called. An exception directory reaching past `SizeOfImage` fails
  the mapping; a TLS directory with no raw data (`StartAddressOfRawData`
  and `EndAddressOfRawData` 0, as lld links an image with TLS callbacks and
  no `.tls`) is an empty template. `victory-pe` leaves out a CodeView record
  it cannot read (an odd debug directory no longer fails the parse), and
  names the directory in its other errors (`import directory: ...`, `delay
  import directory: ...`, `export directory: ...`, `TLS directory: ...`).
  M2b/M2c.
- `victory-loader`, `teb`: the PEB a title reads through `gs:[0x60]` is a
  zeroed block (`BeingDebugged` 0, no image base, no process parameters).
  `gs` accesses of the absolute form (`gs:[disp32]`) in any instruction
  but a `mov` (`cmp`, `movzx`, ...) make the mapping fail, naming them;
  accesses through a register (`gs:[rcx]`, `__readgsqword` of a variable
  offset, an indexed slot) are neither patched nor reported and read the
  host thread's segment (MSVC emits the absolute form for the TEB fields
  it knows); `victory inspect --teb` lists an image's absolute ones before
  it is run. A `mov` with another legacy prefix before or after `gs`
  (`mov word gs:[d], ax`, `lock inc gs:[d]`, `rep`) is reported as another
  instruction from its first prefix, so the mapping fails naming it; so
  does a store of `rsp` (`mov gs:[d], rsp`, which inside the thunk would
  store the thunk's own rsp; MSVC does not emit it). A thread's TLS blocks
  for an image mapped after the thread attached appear when it attaches
  again. M2a/M2b.
- `victory-loader`, `faults`: a fault in title code that is not a Zen 2
  instruction to emulate is handed to `seh` on the thread's own stack; a
  thread with no room left there (a stack overflow) gets the old crash
  report (registers, the top of the stack) and exit code 128 + the signal,
  a path no test has run; so does a thread with no TEB of ours (not created
  through `CreateThread`, not attached), which has no stack bounds to check
  and no alternate stack. Title code is told from ours by a lock-free copy
  of the images' ranges (at most 64 images mapped at once). The trap number
  decides the exception: `int 0x29` (`__fastfail`, a #GP whose error code
  has the write bit) ends the title with `STATUS_STACK_BUFFER_OVERRUN`
  (the code in its parameter) and a crash report, asking no handler, as
  Windows does, whatever the signal (what Rosetta 2 raises for it is
  unverified); a #GP of a privileged instruction (`hlt`, `cli`, `in`/`out`,
  `rdmsr`/`wrmsr`, a control register move, ...) is
  `STATUS_PRIVILEGED_INSTRUCTION`, any other #GP an access violation with
  no address (a non-canonical one); a page fault's first parameter is 0, 1
  or 8 (an instruction fetch). The Zen 2 patcher of `monitorx`/`mwaitx`
  patches one page at a time in the process (a patch while another is under
  way is skipped, the instruction stepped over, and patched at a later
  fault), and gives the page back the protection the loader gave it, read
  from the image's section table (read-write-execute when a section in it
  is writable, else read-execute). A fault resumed after a fix-up (a
  vectored handler, `EXCEPTION_CONTINUE_EXECUTION`) goes back through
  `RtlRestoreContext` on the thread's own stack, not through the signal
  frame, so the ymm upper halves the host saved (Linux's xsave area,
  macOS's `__darwin_mcontext_avx64`) are kept beside the context and put
  back (`vinsertf128`) when the processor has AVX; the AVX-512 state is
  not. Faults in our own code go to the handler that was installed before
  ours (Rust's, which aborts). Up to 8 handlers of ours (`register`); the
  alternate stack (64 KiB) is leaked when a thread ends. M2a.
- `victory-loader`, `seh`: the context of a fault always carries MXCSR
  `0x1F80`, so resuming drops a rounding mode the title set; every SIGFPE
  is `STATUS_INTEGER_DIVIDE_BY_ZERO` and every SIGTRAP `STATUS_BREAKPOINT`
  (single steps included); dynamic function tables
  (`RtlAddFunctionTable`, `RtlInstallFunctionTableCallback`) are accepted
  with a warning and not kept; `RtlVirtualUnwind` ignores its context
  pointers; `RtlCaptureStackBackTrace` returns no frames (its callers are
  our own frames, which have no unwind data); the `CONTEXT_*` flags are the
  bare bits without `CONTEXT_AMD64`; collided unwinds and noncontinuable
  exceptions have the code paths and no test. Nothing of this has run on
  macOS, where `__es.__trapno` and `__es.__err` give the trap and the write
  bit, and `uc_mcsize` says whether the ymm upper halves are there
  (compile-checked only). M2a/M2c.
- `victory-loader`, `unwind`: `backtrace` reads an image's
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
  refused; an `SLIST_HEADER` is laid out as Windows x64 has it (depth and
  sequence, then the top entry with the header-type and init bits), but
  every list's operations go through one lock, not `cmpxchg16b`; a waitable
  timer's completion routine runs on our timer thread (Windows queues it as
  an APC to the thread that set the timer, run in its alertable waits), and
  a named timer stays armed after its last handle is closed (named objects
  live for the process); `WaitOnAddress` returns on any wake, and a
  `WakeByAddressSingle` may release every waiter on the address (allowed as
  spurious wake-ups); `WakeConditionVariable` wakes every sleeper. Every
  signal wakes every waiter through one condition variable, to be made
  finer when it costs. M2b/M2c.
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
  reported as large as physical memory; `MEM_RESET` (and `MEM_RESET_UNDO`)
  is advice ignored: the pages keep their contents; `VirtualProtect`
  outside our reservations works within mapped images only (not thread
  stacks or the host's memory) and reports the old protection as
  `PAGE_EXECUTE_READWRITE` (the loader keeps no record of its sections').
  M2b/M2c.
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
- `victory-hle/src/modules/win32`, `files`: `SetFileTime` and
  `FileBasicInfo` return success and set nothing; `GetDiskFreeSpaceExW` and
  `GetDiskFreeSpaceW` report a total of twice the free space, and off
  Windows `storage::disk_free` is `u64::MAX`, so neither number is real;
  `MOVEFILE_COPY_ALLOWED` is ignored, so a move between drives works only
  where the host renames between the two folders; `CreateSymbolicLinkW`
  fails; a UNC path fails with `ERROR_PATH_NOT_FOUND`; `X:name` resolves
  against the drive's root (no current directory per drive); trailing dots
  and spaces are not trimmed from names; `GetFinalPathNameByHandleW` gives
  the `\\?\X:\` form for every flag; `ReadFileEx` and `WriteFileEx`
  call their completion routine at once (no APC queue); `LockFileEx` and
  `UnlockFileEx` lock nothing but complete their `OVERLAPPED` (status,
  event); a file handle is always signaled (its I/O completes before the
  call returns), so a wait on it succeeds at once; a find ignores its
  information level's extras, `search_op` and flags; read-only files are
  recognised by the owner's write bit; files are created with host write
  access whatever the title asked, and the title's own access is checked
  after. APFS case folding, `created()` and `ctime` are untested on macOS:
  only the Linux tests have run this. M2b/M2c.
- `victory-hle/src/modules/ucrt` (the C runtime off Windows): stdio's
  limits: sharing modes are passed to `CreateFileW` and not enforced;
  `_locking` locks nothing; `_pipe` fails with `EINVAL` (a warning once);
  `_get_stream_buffer_pointers` gives the STL an empty buffer (fields of
  the stream that stay null and 0), so `std::fstream` goes through
  `fgetc`/`fputc`, one character at a time; at `exit` the streams are
  flushed after the `atexit` functions (static destructors included) have
  run, and `ExitProcess` flushes them too; the descriptor table is one lock
  held during I/O; `setvbuf` after a stream's first operation is `EINVAL`;
  text-mode `ftell` counts every `\n` as two bytes; `printf` to a stream
  formats the whole output before writing it; a UTF-8 read cannot fit a
  4-byte character into a 2-byte request and a surrogate pair split across
  two writes becomes U+FFFD; Unicode output to a device is UTF-8; `fputwc`
  above 255 on an ANSI stream writes `?` (the UCRT fails with `EILSEQ`);
  the `conio` functions (`_getch`, `_cputs`, ...) are absent; an invalid
  format writes nothing (the UCRT writes up to the bad directive). C++
  exceptions: the compressed `__CxxFrameHandler4` tables are decoded as
  vcruntime's sources describe them and tested only against tables built
  by hand, never a compiler's output (the first title throw will tell); an
  exception cannot leave a catch funclet or a destructor run during an
  unwind (our own frames lie between, without unwind data), so such a throw
  ends the process through `terminate` with a message (a destructor
  throwing during an unwind outside any catch reaches the unhandled filter
  instead, so the UCRT's filter terminates, or a crash report follows); for
  the same reason a title exception cannot cross one of our callbacks (a
  `qsort` comparator, a thread procedure, a TLS callback, a `DllMain`, a
  window procedure, an `atexit` function): it is unhandled there even when
  a title frame above us would catch it; `std::uncaught_exceptions` stays
  raised after an `__except` takes a C++ exception; a catch funclet left by
  `longjmp` or `ExitThread` leaves the exception marked as being handled on
  that thread; the tables' RVAs and counts are trusted (a corrupt table
  faults in our code); the `_set_se_translator`
  translator is stored and never called; a `noexcept` function terminates
  when the unwind reaches it, not during the search; `longjmp`
  ignores the frame, so `__finally` blocks between are not run;
  `_seh_filter_exe` runs no `signal` handler and `abort` runs no `SIGABRT`
  handler; `_beginthread` returns a handle already closed (as Windows), so
  it cannot be waited on; local time is UTC (no time zone); the Bessel
  functions return NaN with `EDOM`; `_expand` always fails; `system` runs
  nothing; `rand_s` is not cryptographic; `getenv`'s pointer is inside the
  environment's array, valid until the next `_putenv` (as on Windows);
  `__unDName` copies the decorated name without its `?` (no demangling;
  `type_info::name` undecorates plain classes, structs and enums only);
  `_local_unwind` jumps to our `RtlUnwind` (no record, no return value);
  wide case mapping is the "C" locale's, A-Z only; locales are 16-byte
  zero markers (`_free_locale` does nothing) and narrow strings follow
  ASCII rules for case and classes while the multibyte conversions are
  UTF-8 (the code page is reported as 1252 with `MB_CUR_MAX` 1); `mbrtowc`
  keeps no partial character; `_controlfp` reaches only the SSE control
  word (`__control87_2`
  reports it for x87 too, `_statusfp2` 0 for x87); `rint` and the `lrint`
  family round to nearest even whatever the rounding mode; math functions
  set no `errno` but `tgamma` at negative integers and the Bessel stubs;
  `__RTDynamicCast` ignores `vfdelta`, the source type, accessibility and
  ambiguity (the first base with the name wins) and `__RTCastToVoid` takes
  only `cd_offset == 0`; the `_itoa_s` family gives `ERANGE` for a short
  buffer, not `EINVAL`; `div` by zero gives zeros instead of faulting;
  `exit`, `abort`, `terminate`, `__p___argc`, `__p___argv` and
  `_invalid_parameter_noinfo_noreturn` are `kernel.rs`'s and are served
  under `api-ms-win-crt-runtime-*` only, not `ucrtbase.dll`. Nothing of it
  has run on macOS or under a title. M2b/M2c.
- `victory-hle/src/modules/msvcp.rs` (MSVCP140 off Windows): only the
  C-level surface; the `locale`, `_Locimp`, `ios_base`, `basic_streambuf`,
  `basic_ostream` and facet classes (and so `_Getgloballocale`, `cout`, the
  `ctype<char>` and `codecvt` facets) wait for the names the census of
  your dump gives, as do newer exports such as `_Thrd_sleep_for` and
  `__std_system_error_allocate_message`. The locale helpers know the "C"
  locale alone (code page 1252 read as Latin-1; `_Getwctype`'s digit and
  hex classes are ASCII); `_Gettnames` and `_W_Gettnames` return null (the
  UCRT's `__lc_time_data` layout is not written here); `_Winerror_message`
  gives "Windows error N" (no message table); `_Syserror_map` uses the
  CRT's capitalised `strerror` texts, the STL's own are lower-case;
  `_Random_device` reads `getentropy`. The thrown objects' vtables have no
  complete object locator, so `typeid` and `dynamic_cast` on them do not
  work; a rethrow through `exception_ptr` copies the object (MSVC rethrows
  the shared one) into 1 KiB of the rethrowing frame, a larger object into
  a block never freed; an empty `exception_ptr` rethrows `bad_exception`.
  `_Throw_C_error(_Thrd_success)` ends the title with a message (the STL
  aborts). Taken from memory of the MSVC STL and unchecked against a
  binary: the `error_category` vtable's order of the two `equivalent`
  overloads (MSVC lays overloads out in reverse declaration order, so
  `equivalent(const error_code&, int)` comes first), the `_Addr` identities
  (future 1, generic 3, system 7), `_Thrd_current`'s null handle,
  `_Lockit`'s eight kinds. The 14 throwing exports raise from the caller's
  context through a shim like `_CxxThrowException`'s; the objects, tables,
  matching, copies and destructors are tested, the shims have compiled but
  never run (no Rust test can survive a throw into our frames): the first
  title `std::vector::at` out of range will tell. Nothing of it has run on
  macOS or under a title. M2b/M2c.
- `victory-hle/src/modules/bcrypt.rs`: only the RNG algorithm opens
  (`getentropy`; the `BCRYPT_RNG_ALG_HANDLE` pseudo-handle works too);
  every hash, cipher, key and property call fails with
  `STATUS_NOT_SUPPORTED` (a warning once each). The census says which the
  title wants. M2b/M2c.
- `victory-hle/src/modules/com.rs`: no class registry, so `CoCreateInstance`
  and `CoCreateInstanceEx` fail with `REGDB_E_CLASSNOTREG` for every CLSID
  (a warning once per class); `CoRegisterClassObject` stores nothing;
  `CoSetProxyBlanket`, `CoImpersonateClient` and the library functions do
  nothing; `PropVariantClear`/`Copy` know the string, BSTR, blob, CLSID,
  interface and vector kinds. M2b/M2c.
- `victory-hle/src/modules/wer.rs`: every Windows Error Reporting call
  succeeds and keeps nothing but the flags; our crash report is `seh`'s.
- `victory-hle/src/modules/win32/pipes.rs`: anonymous pipes are real;
  every named-pipe function fails with `ERROR_NOT_SUPPORTED` (a warning
  once each); the result codes of reads and writes on a closed end
  (`ERROR_BROKEN_PIPE`, `ERROR_NO_DATA`) are from memory of Windows.
- `victory-hle/src/modules/win32/locale.rs`: en-US alone (every valid
  locale name gets its data, others `ERROR_INVALID_PARAMETER`); code pages
  1252 and 437 are read as Latin-1, 65001 as UTF-8, no other (and
  `IsValidCodePage` says so); comparisons
  and sort keys are by code unit with simple case folding (no linguistic
  order, no normalization: `FoldStringW` copies); `FormatMessageW`'s system
  table holds the common errors only (others `ERROR_MR_MID_NOT_FOUND`) and
  modules have no message tables; floating-point inserts are copied as
  text; the time zone is UTC (`TzSpecificLocalTimeToSystemTime` applies the
  bias it is given); `GetNumaAvailableMemoryNodeEx` reports physical
  memory. M2b/M2c.
- `victory-hle/src/modules/window.rs`: no host window and no input
  (`GetKeyState`, `GetAsyncKeyState`, `MapVirtualKey`, `ToUnicode`,
  `GetRawInputData` give nothing, `TranslateMessage` translates nothing,
  the cursor moves only when the title moves it); the display is a fixed
  1920 x 1080 at 96 DPI, one monitor, one mode; windows have no frame and
  nothing paints (`BeginPaint` gives a fake device context); hooks are
  stored and never called; `MessageBox` logs and answers `IDOK`;
  `MsgWaitForMultipleObjects` waits in 5 ms slices, checking the queue
  between them; a thread's end takes its windows with it without
  `WM_DESTROY` (Windows sends it) and answers 0 to what was sent to them;
  `SendMessage` converts no strings between the A and W forms;
  `SendNotifyMessage` to another thread posts; `InSendMessage` is 0 inside a
  sent message; `AttachThreadInput` and `IsGUIThread` succeed doing
  nothing; the `Message`, `STATIC` and `BUTTON` system classes exist with
  `DefWindowProc` and no behaviour; other `ext-ms-win-ntuser-*` sets
  (mouse, gui, ...) fall to `win32`'s `ext-ms-win-*` pattern without these
  names. Nothing of it has run on macOS or under a title. M3.
- `victory-hle/src/modules/win32`, `exceptions`: no debugger is ever
  present; `DebugBreak` is an `int3`, which the title's handlers see as
  `STATUS_BREAKPOINT`; the error modes are stored and change nothing. M2b.
- `victory-hle/src/modules/kernel.rs`, the non-Windows `host` module: every
  host call fails (null handles, zero lengths, no stack walk); `exit`,
  `abort` and `TerminateProcess` end our process. `SetThreadPriority`
  changes nothing and fails. The last error is the `win32` module's now
  (`GetLastError`, in the TEB), and `SetUnhandledExceptionFilter` installs
  the filter `seh` calls. M2b.
- `victory-hle/src/host.rs`, non-Windows `imp::resolve`: no host DLL serves
  any import; on macOS this stays so, and our own implementations replace
  them. M2b.
- `victory-hle/src/modules/xmem_sections.rs` (`xmem` off Windows): no GPU
  side (the Windows module's `victory_gpu::title_memory_*` calls,
  `map_alias`, `map_aliases` and `shown_at` are not there, and
  `pages_written` tells nobody: M4's Metal back end reads title memory
  where it is); the title's `__XMemAllocationHooks__` are used once the runner calls
  `set_allocation_hooks` (M2c); every committed page reports graphics memory
  and no tag; use is not split by bandwidth class (GPU-optimal use reads 0);
  `XMemVirtualQuery` ignores the process handle; a physical page is 64 KiB
  of a section and page numbers count 4 KiB frames from `0x10_0000`;
  `pages_known` and `write_physical` wait for the DirectStorage port. On
  macOS `mach_vm_remap` with `VM_FLAGS_OVERWRITE`, `mach_vm_map` with a null
  object (`unmap_in_place`) and `low::give_back` are compile-checked only,
  and the unit tests' 32-bit parts skip there (a 4 GiB `__PAGEZERO` in the
  test binary). M2c/M4.
- `victory-hle/src/modules/d3d12x.rs`, `create_device`: the device keeps its
  own allocator; `use_allocator`, `use_page_mapper` and
  `use_many_page_mapper` are Windows-only until M4. M4.
- `apps/victory/src/run.rs` and `win32/threads.rs`, threads and images: a
  thread's start and end are told to the images under our loader lock, so a
  thread a `DllMain` starts waits until the initialisation ends (and a
  `DllMain` that waits for such a thread deadlocks, as on Windows); only
  images whose `DLL_PROCESS_ATTACH` has run hear of threads; the thread
  that loads a DLL late gets its TLS blocks, other threads get theirs for
  that DLL only when they attach again (at their own start, so a thread
  alive before a late load has none: a fault at `gs:[0x58]`'s block if it
  touches that DLL's `thread_local`s); a DLL importing from the executable
  gets stubs for those imports (the executable is mapped once, after its
  DLLs); DLLs get no `DLL_PROCESS_DETACH` at exit. `CreateThread`'s size is
  the initial commit unless `STACK_SIZE_PARAM_IS_A_RESERVATION` says
  otherwise, and the reservation is the executable's or the commit rounded
  up to 1 MiB; the commit itself is not tracked (the whole stack is host
  memory). M2c.
- `victory-hle/src/modules/xasync.rs` (the GDK's task queues and async
  blocks, every host): the thread-pool, serialised, delayed-callback and
  pending-work threads of ours attach a TEB before running title callbacks
  off Windows (as the `gameinput` poll thread does); a terminate with
  `wait` waits for every task delivered to run to its end, unless called
  from one of the queue's own callbacks (then the callback runs from the
  last task to finish, as without `wait`); a task already handed to the
  thread pool when the queue is terminated runs with `canceled`; an async
  call keeps its own reference to its queue, so closing the handle while
  the call is in flight is safe; `XAsyncGetStatus(wait)` spins (yielding)
  through the moment between a call's cleanup and its status being
  published. M2c.
- `victory-hle/src/modules/lifecycle.rs` off Windows: the window API set
  is `window.rs`'s (below), and `end_title` posts `WM_CLOSE` to the title's
  windows after the suspend routines;
  `end` leaves the process at once (no pipeline cache to keep); a relaunch
  hands the new process the old one's id, which it polls with `kill(pid, 0)`
  for up to 30 s; `xgame`'s `XLaunchNewGame` still relaunches on Windows
  only. M3.
- `victory-hle/src/modules/dstorage.rs` off Windows: reads go through each
  worker's own handle with `pread` (`VICTORY_DSTORAGE_IO=overlapped` and
  `direct` become `handles`, said once); nothing tells a GPU side of memory
  written (`kernel_writing`, `kernel_will_write`, `title_memory_written`,
  `pages_written` do nothing, `stream_notice` is false, `map_costs` none);
  a host error of an unknown kind is `E_FAIL`. M4.
- `victory-hle/src/modules/media.rs` off Windows: no Media Foundation, so
  `MFCreateSourceResolver` reports an unsupported scheme and opening a
  source fails as it does on a host without it; paths on our drives are not
  translated (M3 decides video playback). M3.
- `victory-hle/src/modules/net.rs` off Windows: every call that passes the
  module's checks and would reach the host's Winsock or WinHTTP fails
  instead, with `WSASYSNOTREADY` as the last error and the result type's
  failure value (`SOCKET_ERROR`, `INVALID_SOCKET`, a null handle), said
  once per call: the title is offline, as a console without a network. The
  Winsock and WinHTTP functions the module does not override (`WSAStartup`,
  `WSAGetLastError`, `socket`, ...) are the host's on Windows and nobody's
  off it, so they get the runner's policy stubs until the census of your
  dump names the set to serve. M3.
- `victory-hle/src/modules/xsystem.rs` off Windows: a registry key that is
  not ours is `FILE_NOT_FOUND` to open or query and `INVALID_HANDLE` to
  close (there is no host registry). M2c.
- `victory-hle/src/modules/storage.rs` off Windows: the storage folder is a
  drive of its own (`win32::files::mount`), and the host's free space comes
  from `statvfs`. Done; nothing less than Windows.
- `victory-hle/src/modules/rpcrt4.rs`: identifiers come from a counter mixed
  with the time, not from the system's random generator. M2.
- `victory-hle/src/modules/xgame.rs`, `XLaunchNewGame`: no relaunch. M2.
- `victory-gpu/src/queue.rs`, `system_timestamp`: always 0. M2.
- `victory-gpu/src/device.rs`: the address range reported is a constant
  (`0x10000` to `0x7fff_fffe_ffff`), and descriptions are not read from title
  memory. M2.
- `victory-gpu/src/fence.rs`, `set_event`: events are not signalled. M2.
- `victory-hle/src/modules/kernel.rs`, `GetProcAddress` on one of our
  pseudo modules: searches every module serving that DLL by name only (an
  ordinal is null, with a warning); `runtime::set_image_path` keeps a path
  in the title's form (`X:\...`) as given on every host and finds an image
  by the name after its last `\` or `/`, so the two kernel tests of title
  paths run everywhere now.

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
- The title's mixed audio reaches WASAPI through COM
  (`CoCreateInstance` of `MMDeviceEnumerator`, then `IAudioClient`), as on
  PC; off Windows `com.rs` reports that class unregistered, so the title
  gets no audio device until M3 provides `IMMDeviceEnumerator`,
  `IMMDevice`, `IAudioClient` and `IAudioRenderClient` objects over the
  host's audio output. Media Foundation (`media`) stays absent.

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
- TaynanGT/Vice-Tay (2026-11-26, e2d158a): our `main` (c9c932c) plus 924
  commits, merging several forks' work. Merged here, since the shader
  translator serves the Metal back end as much as the D3D12 one and Apple
  GPUs run 32-lane waves as NVIDIA does: `victory-shader` fixes with their
  unit tests (`ds_bpermute_b32` permutes within each 32-lane half of a
  wave64, as the ISA says, so such programs run in host waves of 32;
  `v_mad_legacy_f32`/`v_mac_legacy_f32` fused with their sources' subnormals
  flushed, as RDNA 2 runs them; `v_fmac_f32`'s VOP3 accumulator modifiers;
  `v_mul_legacy_f32` taking a denormal as zero; `s_pack_{ll,lh,hh}_b32_b16`;
  the thread-group info SGPR; a comparison sampler replacing the reduction
  bits instead of OR-ing them (a minimum sampler became a maximum one); an
  unmatched `saveexec` no longer ending the scan of a block's sections; a
  `v_readlane` of a lane a `v_writelane` set in the same block reading what
  was written; EXEC guards at a join excluding dead lane edges when
  descriptors are classified; `s_bfm_b32` with literals; a scalar select on
  a mask's SCC counted as a data use; console threads of a 2-D group given
  to 32-lane host waves from a group counter, so each is given once (their
  issue 1243); the loose-LDS last-resort form for console wave64 programs a
  32-lane host would otherwise refuse (approximate, `VICTORY_WAVE_LDS_LOOSE=0`
  turns it off); a wave64 program with no 32-lane form refused as
  unsupported instead of failing at pipeline creation; `TRANSLATOR_VERSION`
  2, so cached translations are redone. Also `xasync` completing a call
  without holding its lock during the provider's cleanup and a block begun
  again as soon as its result shows, and `xgamesave` keeping an older intact
  save when the newest version's manifest names a blob that is gone (both
  with tests). Left out: the table-strides root SRV (slot 252, typed d16
  views through descriptor tables), which needs the D3D12 host to bind it
  (their `victory-gpu` changes) and is for the Metal host to do in M4; a
  NaN-to-zero guard on every colour export and typed store (on by default
  there, with an off switch: a visual workaround that may hide translation
  bugs, and a cost per export); two title shader digests hard-coded to pick the
  wave-wide form (game-derived data, against the asset policy); the D3D12
  renderer changes (35 files: 4 KiB placements except on AMD, descriptor,
  texture, queue and residency fixes), `lifecycle`, `dstorage` and the
  Windows `xmem`, none of which runs on macOS or can be run by the user or
  CI on a GPU; XMA refill made the default in `audio` (a field width also
  changed, unexplained; audio is M3's); scripted keys from a file in
  `gameinput` (M3 replaces the input host on macOS); their benchmark
  documentation and its policy allowance.

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

The runner reserves the low 4 GiB as the first thing `victory run` does,
before the title's file is read, hashed or parsed, so that as little as
possible of ours is mapped there by then (`low::reserve`). A free range of
which something else maps a page between the reading of the map and the
allocation is not left whole: the map is read again within it and what is
still free there taken, round after round while a round takes something
(at most 8).
